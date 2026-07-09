// COMPOSE / REVIEW — the send ceremony. OWNED BY: view-agent-2 (actions).
//
// Send is the one irreversible action, so it gets friction proportional to that
// (UX-DIRECTIONS "Action feel"):
//   EDIT   — to / subject / body. Enter (or ⌘Enter in the body) -> REVIEW.
//   REVIEW — recipients + body preview + outbound-guard verdict. We submit ONCE
//            without override_guard to get the verdict:
//              * clean pass  -> the send actually fired; we close on success.
//              * 422 guard   -> show redacted guard kinds + a distinct
//                               "override and send" affordance (shift+Enter or
//                               the danger button). A second plain Enter without
//                               a surfaced guard just re-fires.
//   403    — no write credential: render the `squelchd auth --write` hint.
// Esc backs REVIEW -> EDIT, and EDIT -> cancel.

import { useMemo, useRef, useEffect } from "react";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import { api, ApiError } from "../api";

export function ComposeReview() {
  const compose = useStore((s) => s.compose);
  if (!compose) return null;
  return <ComposeInner />;
}

function ComposeInner() {
  const compose = useStore((s) => s.compose)!;
  const update = useStore((s) => s.updateCompose);
  const close = useStore((s) => s.closeCompose);
  const pushToast = useStore((s) => s.pushToast);

  useKeyContext("modal");

  const toRef = useRef<HTMLInputElement>(null);
  const inReview = compose.phase === "review";

  // Focus the "to" field when the edit pane opens.
  useEffect(() => {
    if (!inReview) toRef.current?.focus();
  }, [inReview]);

  async function fire(override: boolean) {
    const c = useStore.getState().compose;
    if (!c || c.sending) return;
    update({ sending: true, error: null });
    try {
      await api.actionSend({
        body: c.body,
        replyToMessageId: c.replyToMessageId,
        to: c.to || undefined,
        subject: c.subject || undefined,
        overrideGuard: override,
      });
      pushToast("sent", "success");
      close();
    } catch (e) {
      if (e instanceof ApiError && e.kind === "guard_blocked") {
        // Surface the redacted verdict; stay in review, offer explicit override.
        update({
          phase: "review",
          guardKinds: e.guardKinds ?? [],
          sending: false,
          error: null,
        });
        return;
      }
      if (e instanceof ApiError && e.kind === "forbidden") {
        update({
          sending: false,
          error: "no write credential — run `squelchd auth --write`",
        });
        return;
      }
      update({
        sending: false,
        error: e instanceof ApiError ? e.message : "send failed",
      });
    }
  }

  function toReview() {
    // Guard against an empty body — nothing to review.
    const c = useStore.getState().compose;
    if (!c) return;
    if (!c.body.trim()) {
      update({ error: "body is empty" });
      return;
    }
    update({ phase: "review", error: null, guardKinds: [] });
  }

  function primaryEnter() {
    const c = useStore.getState().compose;
    if (!c) return;
    if (c.phase === "edit") {
      toReview();
    } else {
      // In review: a plain Enter fires WITHOUT override (phase 1 = get verdict).
      void fire(false);
    }
  }

  const bindings = useMemo(
    () => [
      {
        key: "Escape",
        description: inReview ? "back to edit" : "cancel",
        allowInInput: true,
        handler: () => {
          const c = useStore.getState().compose;
          if (c?.phase === "review") update({ phase: "edit", error: null });
          else close();
        },
      },
      {
        key: "Enter",
        description: inReview ? "send" : "review",
        allowInInput: true,
        handler: (e: KeyboardEvent) => {
          const target = e.target as HTMLElement | null;
          // In the body textarea, plain Enter is a newline; require ⌘/Ctrl+Enter.
          if (
            !inReview &&
            target?.tagName === "TEXTAREA" &&
            !(e.metaKey || e.ctrlKey)
          ) {
            return false; // let the textarea insert a newline
          }
          primaryEnter();
        },
      },
      {
        // Explicit override in review only.
        key: "shift+Enter",
        description: "override guard and send",
        allowInInput: true,
        handler: () => {
          const c = useStore.getState().compose;
          if (c?.phase === "review" && c.guardKinds.length > 0) void fire(true);
          else return false;
        },
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [inReview],
  );
  useKeys("modal", bindings, [bindings]);

  const guarded = compose.guardKinds.length > 0;

  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(0,0,0,0.55)",
        display: "grid",
        placeItems: "center",
        zIndex: 100,
      }}
    >
      <div
        style={{
          width: 600,
          maxWidth: "90vw",
          background: "var(--bg-raised)",
          border: "1px solid var(--border)",
          borderRadius: 6,
          padding: 18,
        }}
      >
        <div
          style={{
            display: "flex",
            justifyContent: "space-between",
            alignItems: "baseline",
            marginBottom: 12,
          }}
        >
          <span style={{ color: "var(--fg)", fontSize: 13, letterSpacing: 0.5 }}>
            {inReview ? "review · confirm send" : "compose"}
          </span>
          <span style={{ color: "var(--fg-faint)", fontSize: 11 }}>
            {compose.replyToMessageId ? "reply" : "new message"}
          </span>
        </div>

        {inReview ? (
          <ReviewPane
            to={compose.to}
            subject={compose.subject}
            body={compose.body}
            guardKinds={compose.guardKinds}
          />
        ) : (
          <EditPane toRef={toRef} />
        )}

        {compose.error && <div className="err">{compose.error}</div>}

        <div
          style={{
            display: "flex",
            gap: 8,
            justifyContent: "flex-end",
            alignItems: "center",
            marginTop: 4,
          }}
        >
          <span style={{ color: "var(--fg-faint)", fontSize: 11, marginRight: "auto" }}>
            {inReview ? (
              guarded ? (
                <>
                  <kbd>esc</kbd> back · <kbd>shift+enter</kbd> override + send
                </>
              ) : (
                <>
                  <kbd>esc</kbd> back · <kbd>enter</kbd> send
                </>
              )
            ) : (
              <>
                <kbd>esc</kbd> cancel · <kbd>⌘enter</kbd> review
              </>
            )}
          </span>

          {inReview ? (
            <>
              <button onClick={() => update({ phase: "edit", error: null })}>
                esc back
              </button>
              {guarded ? (
                <button
                  onClick={() => void fire(true)}
                  disabled={compose.sending}
                  style={{ borderColor: "var(--red)", color: "var(--red)" }}
                >
                  {compose.sending ? "sending…" : "override + send"}
                </button>
              ) : (
                <button onClick={() => void fire(false)} disabled={compose.sending}>
                  {compose.sending ? "sending…" : "send"}
                </button>
              )}
            </>
          ) : (
            <>
              <button onClick={() => close()}>esc cancel</button>
              <button onClick={() => toReview()}>review →</button>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function EditPane({ toRef }: { toRef: React.RefObject<HTMLInputElement> }) {
  const compose = useStore((s) => s.compose)!;
  const update = useStore((s) => s.updateCompose);
  return (
    <>
      <div className="field">
        <label>to</label>
        <input
          ref={toRef}
          value={compose.to}
          onChange={(e) => update({ to: e.target.value })}
          placeholder="recipient@example.com"
        />
      </div>
      <div className="field">
        <label>subject</label>
        <input
          value={compose.subject}
          onChange={(e) => update({ subject: e.target.value })}
        />
      </div>
      <div className="field">
        <label>body</label>
        <textarea
          rows={9}
          value={compose.body}
          onChange={(e) => update({ body: e.target.value })}
          placeholder="write your reply… ⌘enter to review"
        />
      </div>
    </>
  );
}

function ReviewPane({
  to,
  subject,
  body,
  guardKinds,
}: {
  to: string;
  subject: string;
  body: string;
  guardKinds: string[];
}) {
  return (
    <div style={{ fontSize: 12, marginBottom: 12 }}>
      <ReviewRow label="to" value={to || "(none)"} />
      <ReviewRow label="subject" value={subject || "(none)"} />
      <div
        style={{
          marginTop: 8,
          padding: "8px 10px",
          background: "#0f1416",
          border: "1px solid var(--border)",
          borderRadius: 3,
          maxHeight: 220,
          overflow: "auto",
          whiteSpace: "pre-wrap",
          color: "var(--fg-dim)",
          userSelect: "text",
        }}
      >
        {body}
      </div>

      <div style={{ marginTop: 10 }}>
        {guardKinds.length > 0 ? (
          <div
            style={{
              border: "1px solid var(--red)",
              borderRadius: 3,
              padding: "8px 10px",
              color: "var(--red)",
            }}
          >
            outbound guard blocked · matched (redacted):{" "}
            <span className="num">{guardKinds.join(", ")}</span>
            <div style={{ color: "var(--fg-dim)", marginTop: 4 }}>
              review the recipients and body, then override to send anyway.
            </div>
          </div>
        ) : (
          <div style={{ color: "var(--fg-faint)" }}>
            outbound guard: not yet checked · <kbd>enter</kbd> submits for the
            verdict
          </div>
        )}
      </div>
    </div>
  );
}

function ReviewRow({ label, value }: { label: string; value: string }) {
  return (
    <div style={{ display: "flex", gap: 8, padding: "2px 0" }}>
      <span
        style={{
          color: "var(--fg-faint)",
          textTransform: "uppercase",
          fontSize: 11,
          width: 64,
          flexShrink: 0,
        }}
      >
        {label}
      </span>
      <span className="mono" style={{ color: "var(--fg)", userSelect: "text" }}>
        {value}
      </span>
    </div>
  );
}
