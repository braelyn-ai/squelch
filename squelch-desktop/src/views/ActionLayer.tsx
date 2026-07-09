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

import { useMemo, useState, useEffect } from "react";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { useActions } from "../actions/useActions";
import { ComposeReview } from "../components/ComposeReview";
import { RuleEditor } from "../components/RuleEditor";
import { ProcessMode } from "../components/ProcessMode";
import {
  onOpenRuleEditor,
  onOpenProcessMode,
  openProcessMode,
} from "../components/ruleEditorBus";

export function ActionLayer() {
  const undos = useStore((s) => s.undos);
  const toasts = useStore((s) => s.toasts);
  const fireUndo = useStore((s) => s.fireUndo);
  const dismissToast = useStore((s) => s.dismissToast);
  const selectedUpdate = useStore((s) => s.selectedUpdate);
  const compose = useStore((s) => s.compose);
  const act = useActions();

  // Overlay state for the two store-less action modals (rule editor + process).
  const [ruleSender, setRuleSender] = useState<string | null>(null);
  const [processOpen, setProcessOpen] = useState(false);

  useEffect(() => {
    const off1 = onOpenRuleEditor(({ sender }) => setRuleSender(sender));
    const off2 = onOpenProcessMode(() => setProcessOpen(true));
    return () => {
      off1();
      off2();
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
      {/* Toast stack (undo + notices), bottom-left, terminal-adjacent. */}
      <div
        style={{
          position: "fixed",
          left: 12,
          bottom: 12,
          display: "flex",
          flexDirection: "column",
          gap: 6,
          zIndex: 50,
          maxWidth: 340,
        }}
      >
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

      {/* Rule editor (t) — store-less overlay via the bus. */}
      {ruleSender !== null && (
        <RuleEditor sender={ruleSender} onClose={() => setRuleSender(null)} />
      )}

      {/* Process mode (p) — card-by-card triage deck. */}
      {processOpen && <ProcessMode onClose={() => setProcessOpen(false)} />}
    </>
  );
}
