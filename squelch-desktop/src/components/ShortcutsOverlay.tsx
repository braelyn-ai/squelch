// Keyboard-shortcuts help overlay, opened with '?' from the sitrep. A curated,
// grouped cheat-sheet (clearer than dumping the raw keymap registry, which
// carries per-context duplicates). Pushes a modal context so Esc/'?' close it
// without triggering list bindings underneath.

import { useMemo } from "react";
import { useKeys, useKeyContext } from "../keys";

interface Shortcut {
  keys: string[];
  desc: string;
}
interface Group {
  title: string;
  items: Shortcut[];
}

const GROUPS: Group[] = [
  {
    title: "Sitrep",
    items: [
      { keys: ["j", "k"], desc: "move selection" },
      { keys: ["Enter"], desc: "open thread" },
      { keys: ["r"], desc: "reply" },
      { keys: ["e"], desc: "archive" },
      { keys: ["d"], desc: "done" },
      { keys: ["t"], desc: "tune sender rule" },
      { keys: ["p"], desc: "process mode" },
      { keys: ["u"], desc: "undo last action" },
    ],
  },
  {
    title: "Navigate",
    items: [
      { keys: ["a"], desc: "browse all mail" },
      { keys: ["T"], desc: "rules audit" },
      { keys: ["g"], desc: "auth messages" },
      { keys: ["/"], desc: "search" },
    ],
  },
  {
    title: "App",
    items: [
      { keys: ["\\"], desc: "toggle light / dark theme" },
      { keys: ["?"], desc: "this help" },
      { keys: ["Esc"], desc: "close panel / overlay" },
    ],
  },
];

export function ShortcutsOverlay({ onClose }: { onClose: () => void }) {
  useKeyContext("modal");
  const bindings = useMemo(
    () => [
      { key: "Escape", description: "close help", handler: () => onClose() },
      { key: "?", description: "close help", handler: () => onClose() },
    ],
    [onClose],
  );
  useKeys("modal", bindings, [bindings]);

  return (
    <div className="shortcuts-panel" onClick={onClose}>
      <div className="shortcuts-card" onClick={(e) => e.stopPropagation()}>
        <h2>Keyboard shortcuts</h2>
        {GROUPS.map((g) => (
          <div key={g.title} className="sc-group">
            <div className="sc-group-title">{g.title}</div>
            {g.items.map((s) => (
              <div key={s.desc} className="sc-row">
                <span className="sc-desc">{s.desc}</span>
                <span>
                  {s.keys.map((k) => (
                    <kbd key={k}>{k}</kbd>
                  ))}
                </span>
              </div>
            ))}
          </div>
        ))}
        <div className="sc-foot">
          <kbd>?</kbd> or <kbd>Esc</kbd> to close
        </div>
      </div>
    </div>
  );
}
