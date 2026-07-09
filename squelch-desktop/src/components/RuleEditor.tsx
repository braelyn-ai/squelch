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
import type { Disposition } from "../api";

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
  onClose,
}: {
  sender: string;
  onClose: () => void;
}) {
  const pushToast = useStore((s) => s.pushToast);
  const [pattern, setPattern] = useState(() => patternFromSender(sender));
  const [want, setWant] = useState("");
  const [disposition, setDisposition] = useState<Disposition>("squelch");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const wantRef = useRef<HTMLInputElement>(null);

  useKeyContext("modal");
  useEffect(() => {
    wantRef.current?.focus();
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
      await api.createRule({
        match_pattern: pattern.trim(),
        want: want.trim(),
        disposition,
      });
      pushToast(`rule saved · ${pattern.trim()} → ${disposition}`, "success");
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
          tune sender
        </div>
        <div style={{ color: "var(--fg-faint)", fontSize: 11, marginBottom: 14 }}>
          from <span className="mono">{sender}</span>
        </div>

        <div className="field">
          <label>match pattern</label>
          <input
            className="mono"
            value={pattern}
            onChange={(e) => setPattern(e.target.value)}
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
            {saving ? "saving…" : "save rule"}
          </button>
        </div>
      </div>
    </div>
  );
}
