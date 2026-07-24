// FULLSCREEN THREAD VIEWER — the email/thread reading surface.
//
// Fetches GET /client/thread/{id} (ClientThreadView: sanitized text + optional
// server-sanitized `html`) and renders ALL messages stacked NEWEST-FIRST — the
// mail you came for is at the top, the history reads downward. j/k move the
// selected message (j = older) and scroll it into view; click a card to
// select it. Bodies are server-sanitized; html renders in a hard-sandboxed
// iframe (see EmailFrame), plain text falls back to a selectable-text card.
// Quoted reply history (the indented chain each client appends) is collapsed
// by default in both body types — see lib/quotes for the shared heuristic.
// Remote images load by default; EmailFrame strips tracking pixels first.
//
// ONE SCROLL SURFACE: the outer .tv-body column is the single scroll. Each
// EmailFrame measures its contentDocument (sandbox allows same-origin, never
// scripts — see EmailFrame's header) and inline-sizes itself to its exact
// content height, so frames NEVER scroll internally and the stacked cards
// flow like one long document.
//
// LAYERED / ESC RETURNS YOU: this is a fixed fullscreen overlay ABOVE everything
// (including the browse/search side panels — see store.threadId, orthogonal to
// sideView). The surface underneath stays mounted, so opening a thread from
// search keeps the search panel + results under it and Esc drops you right back
// where you were. Opening from Sitrep opens the viewer directly.
//
// FOCUS-STEALING: on mount we focus the container so keystrokes stop landing in
// any input that still holds DOM focus underneath (e.g. the search box). j/k are
// suppressed while an editable is focused, so grabbing focus is what makes them
// work; Esc is allowInInput so it fires regardless.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, ApiError } from "../api";
import type { ClientThreadView, UnsubscribeRecord } from "../api";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import { dateTime, relAge } from "../lib/format";
import { senderDisplayName } from "../lib/avatar";
import { splitQuotedText } from "../lib/quotes";
import { openExternal } from "../lib/opener";
import { usePref } from "../lib/prefs";
import { dispatchDone } from "../lib/dispatch";
import { createBlockRule } from "../actions/blockSender";
import { EmailFrame } from "./EmailFrame";
import { AttachmentStrip } from "./AttachmentStrip";
import {
  getPrefetchedThread,
  noteFetchedThread,
  prefetchThread,
} from "../lib/threadPrefetch";

export function ThreadViewer() {
  const threadId = useStore((s) => s.threadId);
  if (!threadId) return null;
  // Key by threadId so switching threads remounts the body (fresh fetch/idx).
  return <ViewerBody key={threadId} threadId={threadId} />;
}

/**
 * A plain-text body with its trailing quoted history collapsed behind a chip.
 * Mirrors EmailFrame's html-side collapse; the split heuristic is shared
 * (lib/quotes) and conservative — when in doubt the full text renders.
 */
function PlainBody({ content }: { content: string }) {
  const { visible, quoted } = useMemo(() => splitQuotedText(content), [content]);
  const [open, setOpen] = useState(false);
  if (quoted === null) {
    return <div className="msg-body">{content}</div>;
  }
  return (
    <div className="msg-body">
      {visible}
      <button
        type="button"
        className="quote-toggle"
        onClick={() => setOpen((o) => !o)}
        title="the quoted reply chain below this message"
      >
        {open ? "hide quoted history" : "··· show quoted history"}
      </button>
      {open && <div className="msg-quoted">{quoted}</div>}
    </div>
  );
}

function ViewerBody({ threadId }: { threadId: string }) {
  const closeThread = useStore((s) => s.closeThread);
  const openThread = useStore((s) => s.openThread);
  const threadQueue = useStore((s) => s.threadQueue);
  const pushToast = useStore((s) => s.pushToast);
  // Seed from the prefetch cache (inbox hover/selection warms it): a cached
  // thread renders on the FIRST paint — no loading flash, no refetch.
  const [thread, setThread] = useState<ClientThreadView | null>(() =>
    getPrefetchedThread(threadId),
  );
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(thread === null);
  // Selected message: j/k move it, click selects, `.sel` highlights it.
  const [idx, setIdx] = useState(0);
  // Existing unsubscribe record for THIS thread's sender (checked once on load,
  // re-checked after a fresh request). Drives the header hint copy; null = none.
  const [unsub, setUnsub] = useState<UnsubscribeRecord | null>(null);
  // Confirm flow: null = closed; "ask" = "Unsubscribe from X?"; "noLink" = the
  // 422 fallback "no link — block instead?". A small modal card (UnsubConfirm)
  // that gates the thread keys while open.
  const [confirmMode, setConfirmMode] = useState<null | "ask" | "noLink">(null);
  const [confirmBusy, setConfirmBusy] = useState(false);
  // DEV re-triage (developerMode pref): reset THIS email's LLM verdicts and
  // re-run the pipeline. Uses the newest message id (same target as unsubscribe).
  const devMode = usePref("developerMode");
  const [retriaging, setRetriaging] = useState(false);
  const retriageThis = async () => {
    const newest = thread?.messages[thread.messages.length - 1];
    if (!newest || retriaging) return;
    setRetriaging(true);
    try {
      const { reset } = await api.retriage({ messageId: newest.id });
      pushToast(
        reset > 0 ? "re-triaging this email…" : "nothing to re-triage here",
        "info",
      );
    } catch (e) {
      pushToast(e instanceof ApiError ? e.message : "re-triage failed", "error");
    } finally {
      setRetriaging(false);
    }
  };
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    // Fresh prefetch hit → render it and skip the network round-trip entirely
    // (the cache is at most 60s old; e/d/refresh paths repopulate it).
    const cached = getPrefetchedThread(threadId);
    if (cached) {
      setThread(cached);
      setError(null);
      setLoading(false);
      setIdx(0);
      return;
    }
    setLoading(true);
    setError(null);
    api
      .getThread(threadId)
      .then((t) => {
        if (!alive) return;
        noteFetchedThread(threadId, t); // warm images + instant reopen
        setThread(t);
        setIdx(0); // newest renders first — land on it
      })
      .catch((e) => {
        if (alive)
          setError(e instanceof ApiError ? e.message : "thread load failed");
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [threadId]);

  // Steal focus from any input underneath (e.g. the search box) so j/k land here
  // rather than being typed into a field. Esc still works either way.
  useEffect(() => {
    containerRef.current?.focus();
  }, []);

  // Server order is chronological; display order is newest-first.
  const messages = useMemo(
    () => [...(thread?.messages ?? [])].reverse(),
    [thread],
  );
  const count = messages.length;

  // The NEWEST message is what `u` acts on (server derives the sender from it)
  // and whose from_addr keys the existing-record lookup. trim().toLowerCase() to
  // mirror the server's canonical `sender` (see the wire contract's rule).
  const newest = messages[0] ?? null;
  const newestSender = newest ? newest.from_addr.trim().toLowerCase() : null;
  // Human-readable name for the confirm card / block toast.
  const senderName = newest
    ? senderDisplayName(
        newest.from_name
          ? `${newest.from_name} <${newest.from_addr}>`
          : newest.from_addr,
      )
    : "";

  // Refresh the cached unsubscribe record for this sender. Best-effort: a failed
  // lookup just leaves the hint in its default "u unsubscribe" state.
  const refreshUnsub = useCallback(() => {
    if (!newestSender) return;
    api
      .getUnsubscribes()
      .then((rows) => {
        setUnsub(rows.find((r) => r.sender === newestSender) ?? null);
      })
      .catch(() => {
        /* keep whatever we had; the hint degrades to the default affordance */
      });
  }, [newestSender]);

  // Check for an existing record once the sender is known (per-mount cache).
  useEffect(() => {
    let alive = true;
    if (!newestSender) {
      setUnsub(null);
      return;
    }
    api
      .getUnsubscribes()
      .then((rows) => {
        if (alive) setUnsub(rows.find((r) => r.sender === newestSender) ?? null);
      })
      .catch(() => {
        /* no-op: default affordance still shows */
      });
    return () => {
      alive = false;
    };
  }, [newestSender]);

  // Confirm-first unsubscribe: `u` / the header affordance open the confirm card
  // (setConfirmMode("ask")) rather than firing immediately.
  const openConfirm = useCallback(() => {
    if (!newest) return;
    setConfirmMode("ask");
  }, [newest]);

  // Confirmed unsubscribe: POST /client/unsubscribe (always the browser shape).
  // 200 -> open the url + toast + refresh the hint + close. 422 -> swap the card
  // to the "no link — block instead?" fallback. Any other error -> toast + close.
  const runUnsubscribe = useCallback(async () => {
    if (!newest || confirmBusy) return;
    setConfirmBusy(true);
    try {
      const res = await api.unsubscribe(newest.id);
      await openExternal(res.url);
      pushToast(`opened unsubscribe page — ${res.sender}`, "success");
      refreshUnsub();
      setConfirmMode(null);
    } catch (e) {
      if (e instanceof ApiError && e.status === 422) {
        // No http(s) unsubscribe link — offer to block the sender instead.
        setConfirmMode("noLink");
      } else {
        pushToast(
          e instanceof ApiError ? e.message : "unsubscribe failed",
          "error",
        );
        setConfirmMode(null);
      }
    } finally {
      setConfirmBusy(false);
    }
  }, [newest, confirmBusy, pushToast, refreshUnsub]);

  // No-link fallback: block the EXACT sender (shared squelch-rule primitive).
  const runBlock = useCallback(async () => {
    if (!newestSender || confirmBusy) return;
    setConfirmBusy(true);
    try {
      await createBlockRule(newestSender);
      pushToast(`blocked ${newestSender}`, "success");
      setConfirmMode(null);
    } catch (e) {
      pushToast(e instanceof ApiError ? e.message : "block failed", "error");
      setConfirmMode(null);
    } finally {
      setConfirmBusy(false);
    }
  }, [newestSender, confirmBusy, pushToast]);

  const cancelConfirm = useCallback(() => {
    if (confirmBusy) return;
    setConfirmMode(null);
  }, [confirmBusy]);

  // e/d — "done + next": mark the current thread's update done (via the shared
  // dispatchDone, which keeps its 5s undo toast), then advance to the NEXT queued
  // update in place; if none remain, close the viewer. The current update is
  // located in the EmailsView-supplied queue by thread_id — a no-op when this
  // thread was opened without a queue (Sitrep/search/browse).
  // Warm the NEXT queued thread while this one is being read, so e/d's
  // done+advance opens it instantly (thread + images already cached).
  useEffect(() => {
    const cidx = threadQueue.findIndex((u) => u.thread_id === threadId);
    const next = threadQueue[cidx + 1];
    if (next) prefetchThread(next.thread_id);
  }, [threadQueue, threadId]);

  const doneAndNext = useCallback(() => {
    const cidx = threadQueue.findIndex((u) => u.thread_id === threadId);
    if (cidx < 0) return; // not opened from a queue — nothing to advance through
    void dispatchDone(threadQueue[cidx]);
    const next = threadQueue[cidx + 1] ?? null;
    if (next) {
      openThread(next.thread_id, threadQueue);
    } else {
      closeThread();
    }
  }, [threadQueue, threadId, openThread, closeThread]);

  // Keep the selected card visible: fires on j/k moves AND once right after
  // load (setIdx→newest triggers it; a single-message thread is already at the
  // top, where idx 0 renders in view anyway).
  useEffect(() => {
    if (loading) return;
    const el = containerRef.current?.querySelector<HTMLElement>(
      `[data-mi="${idx}"]`,
    );
    el?.scrollIntoView({ block: "nearest" });
  }, [idx, loading]);

  // Mounted only while the viewer is open, so it pushes/pops the "thread"
  // context correctly — same pattern & rationale as SidePanel in SideViews:
  // an always-mounted useKeyContext would pin the context on the stack forever.
  useKeyContext("thread");
  const bindings = useMemo(
    () => [
      {
        // allowInInput matters: a search input underneath may still hold DOM
        // focus (before/if our focus-steal loses the race).
        key: "Escape",
        description: "back",
        allowInInput: true,
        handler: () => closeThread(),
      },
      {
        key: "j",
        description: "older message",
        handler: () => setIdx((i) => Math.min(count - 1, i + 1)),
      },
      {
        key: "k",
        description: "newer message",
        handler: () => setIdx((i) => Math.max(0, i - 1)),
      },
      {
        key: "e",
        description: "done + next",
        handler: () => doneAndNext(),
      },
      {
        key: "d",
        description: "done + next",
        handler: () => doneAndNext(),
      },
      {
        key: "u",
        description: "unsubscribe",
        handler: () => openConfirm(),
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [count, closeThread, openConfirm, doneAndNext],
  );
  useKeys("thread", bindings, [bindings]);

  return (
    <div className="thread-view" ref={containerRef} tabIndex={-1}>
      {loading ? (
        <div className="side-loading">loading thread…</div>
      ) : error ? (
        <div className="side-error">{error}</div>
      ) : !thread ? (
        <div className="side-empty">no thread.</div>
      ) : count === 0 ? (
        <div className="side-empty">no messages in this thread.</div>
      ) : (
        <>
          {/* data-tauri-drag-region: the header's empty space doubles as a
              window-drag handle (Tauri drags only when the mousedown target
              IS this element, so the title/Esc children are unaffected). */}
          <header className="tv-head" data-tauri-drag-region>
            <div className="tv-title">
              <h2>{thread.subject}</h2>
            </div>
            {/* Discoverable unsubscribe affordance — same kbd+label language as
                the Esc hint. Shows the prior-request age once a record exists;
                clicking always (re-)fires the request. */}
            {devMode && (
              <button
                type="button"
                className="close tv-unsub"
                onClick={() => void retriageThis()}
                disabled={retriaging}
                title="dev: reset this email's LLM verdicts and re-run triage"
              >
                {retriaging ? "re-triaging…" : "re-triage"}
              </button>
            )}
            <button
              type="button"
              className="close tv-unsub"
              onClick={() => openConfirm()}
              title="unsubscribe from this sender"
            >
              {unsub ? (
                <>unsubscribe requested {relAge(unsub.requested_at)} ago</>
              ) : (
                <>
                  <kbd>u</kbd> unsubscribe
                </>
              )}
            </button>
            <span className="close">
              <kbd>Esc</kbd> back
            </span>
          </header>
          <div className="tv-body">
            {messages.map((m, i) => (
              <div
                key={m.id}
                data-mi={i}
                className={`msg${i === idx ? " sel" : ""}`}
                onClick={() => setIdx(i)}
              >
                <div className="msg-head num">
                  <span className="msg-from">
                    {senderDisplayName(
                      m.from_name
                        ? `${m.from_name} <${m.from_addr}>`
                        : m.from_addr,
                    )}
                  </span>
                  <span className="msg-when">{dateTime(m.received_at)}</span>
                </div>
                {m.html ? (
                  <EmailFrame html={m.html} cacheKey={String(m.id)} />
                ) : (
                  <PlainBody content={m.content} />
                )}
                {/* Attachment strip under the body. `attachments` is always
                    present on the human-door shape; the strip self-hides when
                    empty. */}
                <AttachmentStrip attachments={m.attachments ?? []} />
              </div>
            ))}
          </div>
        </>
      )}

      {/* Confirm-first unsubscribe card. Conditional-mount so its "modal"
          KeyContext is only on the stack while shown — that is what gates
          j/k/e/d/u from leaking to the thread underneath. */}
      {confirmMode && (
        <UnsubConfirm
          mode={confirmMode}
          senderName={senderName}
          busy={confirmBusy}
          onConfirm={confirmMode === "ask" ? runUnsubscribe : runBlock}
          onCancel={cancelConfirm}
        />
      )}
    </div>
  );
}

/**
 * The unsubscribe confirm card — a small flat modal matching the authcode-card /
 * block-prompt language. Pushes the "modal" KeyContext (Enter confirms, Esc
 * cancels) so the thread's j/k/e/d/u never fire while it's open. `mode` swaps the
 * copy between the initial confirm and the 422 "no link — block instead?" state.
 */
function UnsubConfirm({
  mode,
  senderName,
  busy,
  onConfirm,
  onCancel,
}: {
  mode: "ask" | "noLink";
  senderName: string;
  busy: boolean;
  onConfirm: () => void | Promise<void>;
  onCancel: () => void;
}) {
  useKeyContext("modal");
  const bindings = useMemo(
    () => [
      { key: "Escape", description: "cancel", handler: () => onCancel() },
      { key: "Enter", description: "confirm", handler: () => void onConfirm() },
    ],
    [onConfirm, onCancel],
  );
  useKeys("modal", bindings, [bindings]);

  return (
    <div className="unsub-confirm-overlay" onClick={() => onCancel()}>
      <div className="unsub-confirm" onClick={(e) => e.stopPropagation()}>
        <div className="unsub-confirm-text">
          {mode === "ask" ? (
            <>
              Unsubscribe from <strong>{senderName}</strong>?
            </>
          ) : (
            <>
              No unsubscribe link on this email. Block{" "}
              <strong>{senderName}</strong> instead?
            </>
          )}
        </div>
        <div className="unsub-confirm-actions">
          <button
            type="button"
            className="primary"
            disabled={busy}
            onClick={() => void onConfirm()}
          >
            {mode === "ask" ? "Unsubscribe" : "Block sender"}
          </button>
          <button type="button" disabled={busy} onClick={() => onCancel()}>
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}
