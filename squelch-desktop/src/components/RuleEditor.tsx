// RULE EDITOR — the `t` (tune sender) modal. OWNED BY: view-agent-2 (actions).
//
// Prefilled with `*@domain` derived from the selected sender, a free want-text
// field describing the desired behavior, and a disposition the user cycles with
// Tab (surface -> squelch -> filtered). Save POSTs /client/rules. Mounted by
// ActionLayer, opened via the ruleEditorBus (openRuleEditor(sender)).

import { useMemo, useState, useRef, useEffect } from "react";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import { api, ApiError } from "../api";
import type { Disposition, SenderRule } from "../api";

const DISPOSITIONS: Disposition[] = ["surface", "squelch", "filtered"];

const DISPOSITION_HINT: Record<Disposition, string> = {
  surface: "always surface — never squelch this sender",
  squelch: "squelch — keep out of the sitrep unless it escalates",
  filtered: "filter — drop before triage entirely",
};

/** Turn a sender ("Sarah Chen <sarah@acme.com>" / "sarah@acme.com") into *@domain. */
export function patternFromSender(sender: string): string {
  const m = sender.match(/[<\s]([^<>\s@]+@[^<>\s]+)>?\s*$/) ??
    sender.match(/([^<>\s@]+@[^<>\s]+)/);
  const addr = m ? m[1] : sender.trim();
  const at = addr.lastIndexOf("@");
  if (at >= 0) return `*@${addr.slice(at + 1)}`;
  return addr;
}

export function RuleEditor({
  sender,
  editRule,
  onSaved,
  onClose,
}: {
  /** Present for the `t` tune flow: prefill *@domain from this sender. */
  sender?: string;
  /** Present for the edit flow: prefill from this rule; save = create+delete. */
  editRule?: SenderRule | null;
  /** Called after a successful save (opener re-fetches its list). */
  onSaved?: () => void;
  onClose: () => void;
}) {
  const pushToast = useStore((s) => s.pushToast);
  const [pattern, setPattern] = useState(() =>
    editRule ? editRule.match_pattern : sender ? patternFromSender(sender) : "",
  );
  const [want, setWant] = useState(() => editRule?.want_text ?? "");
  const [disposition, setDisposition] = useState<Disposition>(
    () => editRule?.disposition ?? "squelch",
  );
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const wantRef = useRef<HTMLInputElement>(null);
  const patternRef = useRef<HTMLInputElement>(null);

  const mode: "tune" | "create" | "edit" = editRule
    ? "edit"
    : sender
      ? "tune"
      : "create";

  useKeyContext("modal");
  useEffect(() => {
    // From-scratch create starts on the (empty) pattern field; otherwise the
    // pattern is prefilled so focus lands on the want text.
    if (mode === "create") patternRef.current?.focus();
    else wantRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function cycleDisposition(dir: 1 | -1 = 1) {
    setDisposition((d) => {
      const i = DISPOSITIONS.indexOf(d);
      const n = (i + dir + DISPOSITIONS.length) % DISPOSITIONS.length;
      return DISPOSITIONS[n];
    });
  }

  async function save() {
    if (saving) return;
    if (!pattern.trim()) {
      setError("match pattern is empty");
      return;
    }
    setSaving(true);
    setError(null);
    try {
      // Edit = create-new THEN delete-old, in that order so a mid-flight
      // failure can never lose the rule (worst case: a transient duplicate).
      // TODO: a PUT /client/rules/{id} endpoint would make this atomic; the
      // server owner has no update route yet, so we emulate it client-side.
      await api.createRule({
        match_pattern: pattern.trim(),
        want: want.trim(),
        disposition,
      });
      if (editRule) {
        await api.deleteRule(editRule.id);
      }
      pushToast(
        `${editRule ? "rule updated" : "rule saved"} · ${pattern.trim()} → ${disposition}`,
        "success",
      );
      onSaved?.();
      onClose();
    } catch (e) {
      if (e instanceof ApiError && e.kind === "forbidden") {
        setError("no write credential — run `squelchd auth --write`");
      } else {
        setError(e instanceof ApiError ? e.message : "save failed");
      }
      setSaving(false);
    }
  }

  const bindings = useMemo(
    () => [
      { key: "Escape", description: "cancel", allowInInput: true, handler: () => onClose() },
      {
        key: "Tab",
        description: "cycle disposition",
        allowInInput: true,
        handler: () => cycleDisposition(1),
      },
      {
        key: "shift+Tab",
        description: "cycle disposition (back)",
        allowInInput: true,
        handler: () => cycleDisposition(-1),
      },
      {
        key: "Enter",
        description: "save rule",
        allowInInput: true,
        handler: () => void save(),
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [pattern, want, disposition, saving],
  );
  useKeys("modal", bindings, [bindings]);

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
          width: 460,
          background: "var(--bg-raised)",
          border: "1px solid var(--border)",
          borderRadius: 6,
          padding: 18,
        }}
      >
        <div style={{ color: "var(--fg)", fontSize: 13, marginBottom: 4, letterSpacing: 0.5 }}>
          {mode === "edit" ? "edit rule" : mode === "create" ? "new rule" : "tune sender"}
        </div>
        <div style={{ color: "var(--fg-faint)", fontSize: 11, marginBottom: 14 }}>
          {mode === "tune" ? (
            <>
              from <span className="mono">{sender}</span>
            </>
          ) : mode === "edit" ? (
            <>
              editing <span className="mono">{editRule?.match_pattern}</span> · save
              replaces it
            </>
          ) : (
            "define a sender rule from scratch"
          )}
        </div>

        <div className="field">
          <label>match pattern</label>
          <input
            ref={patternRef}
            className="mono"
            value={pattern}
            onChange={(e) => setPattern(e.target.value)}
            placeholder="*@example.com"
          />
        </div>
        <div className="field">
          <label>want (what should happen)</label>
          <input
            ref={wantRef}
            value={want}
            onChange={(e) => setWant(e.target.value)}
            placeholder="e.g. only surface if it mentions an invoice"
          />
        </div>

        <div className="field">
          <label>disposition · tab to cycle</label>
          <div style={{ display: "flex", gap: 6 }}>
            {DISPOSITIONS.map((d) => (
              <button
                key={d}
                onClick={() => setDisposition(d)}
                style={{
                  borderColor: d === disposition ? "var(--accent)" : "var(--border)",
                  color: d === disposition ? "var(--accent)" : "var(--fg-dim)",
                }}
              >
                {d}
              </button>
            ))}
          </div>
          <div style={{ color: "var(--fg-faint)", fontSize: 11, marginTop: 4 }}>
            {DISPOSITION_HINT[disposition]}
          </div>
        </div>

        {error && <div className="err">{error}</div>}

        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end", marginTop: 4 }}>
          <button onClick={() => onClose()}>esc cancel</button>
          <button onClick={() => void save()} disabled={saving}>
            {saving ? "saving…" : editRule ? "update rule" : "save rule"}
          </button>
        </div>
      </div>
    </div>
  );
}
