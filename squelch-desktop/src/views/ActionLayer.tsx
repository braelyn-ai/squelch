// ACTION LAYER — the global action surface. OWNED BY: view-agent-2 (actions).
//
// Renders the always-on action overlays and wires the action-side keys:
//   - undo + notice toast stack (bottom-left), 5s undo with `u` / click
//   - compose/review send ceremony (ComposeReview, driven by store.compose)
//   - rule editor modal (`t` tune sender), opened via ruleEditorBus
//   - process mode deck (`p`), opened via ruleEditorBus
//
// The read views (SitrepView) own their list keymap; this layer EXTENDS the same
// "list" context with the two action verbs that need action-side overlays (t, p)
// and the modal-context keys live inside each overlay component. All intelligence
// is server-side; nothing here logs the token or any sealed body.

import { useMemo, useState, useEffect, useRef } from "react";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { api, ApiError } from "../api";
import type { UnsubscribeRecord } from "../api";
import { relAge } from "../lib/format";
import { senderDisplayName } from "../lib/avatar";
import { useActions } from "../actions/useActions";
import { createBlockRule } from "../actions/blockSender";
import { ComposeReview } from "../components/ComposeReview";
import { RuleEditor } from "../components/RuleEditor";
import { ProcessMode } from "../components/ProcessMode";
import { AuthCodeModal } from "../components/AuthCodeModal";
import { AskBar } from "../components/AskBar";
import {
  onOpenRuleEditor,
  onOpenProcessMode,
  openProcessMode,
  type RuleEditorRequest,
} from "../components/ruleEditorBus";
import { onOpenAskBar } from "../components/askBarBus";

export function ActionLayer() {
  const undos = useStore((s) => s.undos);
  const toasts = useStore((s) => s.toasts);
  const fireUndo = useStore((s) => s.fireUndo);
  const dismissToast = useStore((s) => s.dismissToast);
  const selectedUpdate = useStore((s) => s.selectedUpdate);
  const compose = useStore((s) => s.compose);
  const hasAuthCode = useStore((s) => s.authQueue.length > 0);
  const lastRefresh = useStore((s) => s.lastRefresh);
  const pushToast = useStore((s) => s.pushToast);
  const act = useActions();

  // Unsubscribe-violation prompt: a sender still mailing >72h after the human
  // asked to unsubscribe (violation_count > 0, resolution === null). We surface
  // ONE at a time as a card in the toast stack. `actedRef` remembers senders the
  // human already blocked/kept THIS session so a lagging read model can't re-nag.
  const [blockPrompt, setBlockPrompt] = useState<UnsubscribeRecord | null>(null);
  const [blockBusy, setBlockBusy] = useState(false);
  const actedRef = useRef<Set<string>>(new Set());

  // Re-scan whenever the read model ticks (same subscribe pattern as the views).
  useEffect(() => {
    let alive = true;
    api
      .getUnsubscribes()
      .then((rows) => {
        if (!alive) return;
        const next = rows.find(
          (r) =>
            r.violation_count > 0 &&
            r.resolution === null &&
            !actedRef.current.has(r.sender),
        );
        setBlockPrompt(next ?? null);
      })
      .catch(() => {
        /* transient — leave whatever prompt (if any) is up */
      });
    return () => {
      alive = false;
    };
  }, [lastRefresh]);

  async function blockSender(rec: UnsubscribeRecord) {
    if (blockBusy) return;
    setBlockBusy(true);
    // Mark acted immediately so a refresh mid-flight can't re-surface it.
    actedRef.current.add(rec.sender);
    try {
      // Squelch rule on the EXACT sender address (shared with the thread viewer's
      // no-link fallback — see actions/blockSender).
      await createBlockRule(rec.sender);
      await api.setUnsubResolution(rec.sender, "blocked");
      pushToast(`blocked ${rec.sender}`, "success");
    } catch (e) {
      pushToast(e instanceof ApiError ? e.message : "block failed", "error");
    } finally {
      setBlockBusy(false);
      setBlockPrompt(null);
    }
  }

  async function keepSender(rec: UnsubscribeRecord) {
    if (blockBusy) return;
    setBlockBusy(true);
    actedRef.current.add(rec.sender);
    try {
      await api.setUnsubResolution(rec.sender, "dismissed");
    } catch {
      /* best-effort: the session-level actedRef already suppresses re-nag */
    } finally {
      setBlockBusy(false);
      setBlockPrompt(null);
    }
  }

  // Overlay state for the store-less action modals (rule editor + process +
  // ⌘K ask bar).
  const [ruleReq, setRuleReq] = useState<RuleEditorRequest | null>(null);
  const [processOpen, setProcessOpen] = useState(false);
  const [askOpen, setAskOpen] = useState(false);

  useEffect(() => {
    const off1 = onOpenRuleEditor((req) => setRuleReq(req));
    const off2 = onOpenProcessMode(() => setProcessOpen(true));
    const off3 = onOpenAskBar(() => setAskOpen(true));
    return () => {
      off1();
      off2();
      off3();
    };
  }, []);

  // Extend the shared "list" keymap with the action verbs whose overlays live
  // here. SitrepView owns j/k/Enter/r/e/d/a/T/'/'/u; we add t (tune) and p.
  const bindings = useMemo(
    () => [
      {
        key: "t",
        description: "tune sender",
        handler: () => {
          const u = selectedUpdate();
          if (u) act.tune(u.sender);
        },
      },
      {
        key: "p",
        description: "process mode",
        handler: () => openProcessMode(),
      },
    ],
    [selectedUpdate, act],
  );
  useKeys("list", bindings, [bindings]);

  return (
    <>
      {/* Toast stack (undo + notices), bottom-left, terminal-adjacent. Position
          and layer live in global.css (.toast-stack) — it sits above every
          fullscreen surface, including the thread viewer, so an undo is never
          buried by the email it was fired from. */}
      <div className="toast-stack">
        {/* Unsubscribe-violation prompt — a flat action card above the toasts.
            sender strings are email-derived → rendered as text only, never HTML. */}
        {blockPrompt && (
          <div className="block-prompt">
            <div className="block-prompt-text">
              <strong>{senderDisplayName(blockPrompt.sender)}</strong> is
              ignoring your unsubscribe — {blockPrompt.violation_count} email
              {blockPrompt.violation_count === 1 ? "" : "s"} since you asked{" "}
              {relAge(blockPrompt.requested_at)} ago.
            </div>
            <div className="block-prompt-actions">
              <button
                className="primary"
                disabled={blockBusy}
                onClick={() => void blockSender(blockPrompt)}
              >
                Block sender
              </button>
              <button
                disabled={blockBusy}
                onClick={() => void keepSender(blockPrompt)}
              >
                Keep
              </button>
            </div>
          </div>
        )}
        {toasts.map((t) => (
          <div
            key={t.id}
            onClick={() => dismissToast(t.id)}
            style={{
              background: "var(--bg-raised)",
              border: "1px solid var(--border)",
              borderLeft: `2px solid ${
                t.tone === "error"
                  ? "var(--red)"
                  : t.tone === "success"
                    ? "var(--accent)"
                    : "var(--fg-faint)"
              }`,
              borderRadius: 3,
              padding: "5px 10px",
              fontSize: 12,
              cursor: "pointer",
              color:
                t.tone === "error"
                  ? "var(--red)"
                  : t.tone === "success"
                    ? "var(--accent)"
                    : "var(--fg)",
            }}
          >
            {t.text}
          </div>
        ))}
        {undos.map((u) => (
          <div
            key={u.id}
            onClick={() => void fireUndo(u.id)}
            style={{
              background: "var(--bg-raised)",
              border: "1px solid var(--border)",
              borderLeft: "2px solid var(--amber)",
              borderRadius: 3,
              padding: "5px 10px",
              fontSize: 12,
              cursor: "pointer",
              display: "flex",
              justifyContent: "space-between",
              gap: 12,
            }}
          >
            <span>{u.label}</span>
            <span style={{ color: "var(--fg-faint)" }}>
              <kbd>u</kbd> undo
            </span>
          </div>
        ))}
      </div>

      {/* Send ceremony (edit -> review -> guard verdict -> fire). */}
      {compose && <ComposeReview />}

      {/* Rule editor — store-less overlay via the bus. Serves the `t` tune flow
          (sender prefill) plus the rules-view create (`n`) and edit (Enter/e). */}
      {ruleReq !== null && (
        <RuleEditor
          sender={ruleReq.sender}
          editRule={ruleReq.rule ?? null}
          onSaved={ruleReq.onSaved}
          onClose={() => setRuleReq(null)}
          initialDisposition={ruleReq.disposition}
          initialWant={ruleReq.want}
          initialPattern={ruleReq.pattern}
        />
      )}

      {/* Process mode (p) — card-by-card triage deck. */}
      {processOpen && <ProcessMode onClose={() => setProcessOpen(false)} />}

      {/* 2FA present-don't-read: the big code modal (auto-revealed on arrival).
          Conditional-mount on the queue so the code lives only while shown. */}
      {hasAuthCode && <AuthCodeModal />}

      {/* ⌘K ask-your-inbox assistant. Conditional-mount so its "modal" context is
          only on the stack while open (same contract as the other overlays). */}
      {askOpen && <AskBar onClose={() => setAskOpen(false)} />}
    </>
  );
}
