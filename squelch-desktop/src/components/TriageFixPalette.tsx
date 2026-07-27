// TRIAGE FIX PALETTE — `v` on any focused email.
//
// Type a few letters, hit Enter, the mail moves and the correction is recorded
// as training data. That second half is the actual point: a human overruling
// triage is the highest-quality signal there is for improving it, and until now
// it evaporated. See lib/triageTargets for the value set and the matcher, and
// the `triage_feedback` block in schema.sql for what gets stored and why.
//
// AMBIGUITY IS SHOWN, NOT GUESSED. "bill" legitimately matches both Invoice and
// Autopay bill, so the list stays on screen and the highlighted row is always
// the one Enter will pick. Resolving that silently would write a label the
// human never chose into a dataset whose entire value is being ground truth.
//
// Follows the canonical overlay contract: conditional-mount (the parent renders
// it only while a fix is in progress), its own "modal" KeyContext, Esc closes,
// backdrop click closes.

import { useEffect, useMemo, useRef, useState } from "react";
import { Wand2 } from "lucide-react";
import { api, ApiError } from "../api";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import { matchTargets, targetLabel, type TriageTarget } from "../lib/triageTargets";
import "../styles/triagefix.css";

export interface TriageFixTarget {
  messageId: number;
  /** Shown so you can see what you are reclassifying. */
  sender: string;
  subject: string;
  /** Current values, for the "was" labels. Optional — omit when unknown. */
  tier?: string | null;
  category?: string | null;
}

export function TriageFixPalette({
  target,
  onClose,
  onApplied,
}: {
  target: TriageFixTarget;
  onClose: () => void;
  /** Fired after a successful correction, so the list can refresh. */
  onApplied?: (t: TriageTarget) => void;
}) {
  const pushToast = useStore((s) => s.pushToast);
  const [query, setQuery] = useState("");
  const [sel, setSel] = useState(0);
  const [busy, setBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  const hits = useMemo(() => matchTargets(query), [query]);
  // Keep the highlight in range as the query narrows the list.
  useEffect(() => setSel((i) => Math.max(0, Math.min(i, hits.length - 1))), [hits.length]);
  useEffect(() => inputRef.current?.focus(), []);

  async function apply(t: TriageTarget) {
    if (busy) return;
    setBusy(true);
    try {
      await api.correctTriage({
        messageId: target.messageId,
        dimension: t.axis,
        toValue: t.value,
      });
      pushToast(`moved to ${t.label} · recorded`, "success");
      onApplied?.(t);
      onClose();
    } catch (e) {
      pushToast(
        e instanceof ApiError ? e.message : "could not record the correction",
        "error",
      );
      setBusy(false);
    }
  }

  useKeyContext("modal");
  const bindings = useMemo(
    () => [
      { key: "Escape", description: "cancel", handler: () => onClose() },
      {
        key: "ArrowDown",
        description: "next",
        allowInInput: true,
        handler: () => setSel((i) => Math.min(hits.length - 1, i + 1)),
      },
      {
        key: "ArrowUp",
        description: "prev",
        allowInInput: true,
        handler: () => setSel((i) => Math.max(0, i - 1)),
      },
      {
        key: "Enter",
        description: "apply",
        allowInInput: true,
        handler: () => {
          const t = hits[sel];
          if (t) void apply(t);
        },
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [hits, sel, busy, onClose],
  );
  useKeys("modal", bindings, [bindings]);

  return (
    <div className="tfix-overlay" onClick={onClose}>
      <div className="tfix" onClick={(e) => e.stopPropagation()}>
        <div className="tfix-head">
          <Wand2 size={14} />
          <span className="tfix-title">Fix triage</span>
          <span className="tfix-sub" title={`${target.sender} — ${target.subject}`}>
            {target.subject}
          </span>
        </div>

        {/* What it is NOW, so the correction reads as a before/after. A
            dimension the caller does not know is OMITTED rather than shown as
            "unset" — claiming a value is unset when we simply never fetched it
            would be a small lie in the one place accuracy matters. */}
        {(target.tier !== undefined || target.category !== undefined) && (
          <div className="tfix-was">
            {target.tier !== undefined && (
              <span>
                tier <b>{targetLabel("tier", target.tier)}</b>
              </span>
            )}
            {target.category !== undefined && (
              <span>
                category <b>{targetLabel("category", target.category)}</b>
              </span>
            )}
          </div>
        )}

        <input
          ref={inputRef}
          className="tfix-input"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setSel(0);
          }}
          placeholder="where should this have gone? (bill, noise, statement…)"
          spellCheck={false}
          disabled={busy}
        />

        <div className="tfix-list">
          {hits.length === 0 ? (
            <div className="tfix-none">
              nothing matches “{query}”. these are the only values the triage
              pipeline itself uses — anything else could not be learned from.
            </div>
          ) : (
            hits.map((t, i) => (
              <button
                key={`${t.axis}:${t.value}`}
                type="button"
                className={`tfix-row${i === sel ? " sel" : ""}`}
                onMouseEnter={() => setSel(i)}
                onClick={() => void apply(t)}
                disabled={busy}
              >
                <span className={`tfix-axis ${t.axis}`}>{t.axis}</span>
                <span className="tfix-label">{t.label}</span>
                <span className="tfix-hint">{t.hint}</span>
              </button>
            ))
          )}
        </div>

        <div className="tfix-foot">
          <span>
            <kbd>↑</kbd>
            <kbd>↓</kbd> pick · <kbd>↵</kbd> apply · <kbd>esc</kbd> cancel
          </span>
          <span className="tfix-why">stored to refine triage</span>
        </div>
      </div>
    </div>
  );
}
