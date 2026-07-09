// AUTH side view ('g'). The dedicated home for auth-related mail — login codes,
// password resets, sign-in alerts, verifications (the /client/sealed metadata,
// but "sealed" is internal jargon and never shown here). Metadata-only: bodies
// are NEVER fetched in the list, only on an explicit reveal (r/Enter), which
// opens the one-time RevealPanel the parent owns.
//
// Follows the SidePanel conditional-mount contract: registers into the existing
// "modal" context via useKeys — it must NOT push a second context (SideViews'
// SidePanel already pushed "modal" while open).

import { useEffect, useMemo, useState } from "react";
import type { SealedMeta } from "../api";
import { useStore } from "../state";
import { useKeys } from "../keys";
import { relAge } from "../lib/format";
import { authKindLabel, authKindIcon } from "../lib/authCopy";
import { Avatar } from "./Avatar";
import { senderDisplayName } from "../lib/avatar";
import { RevealPanel } from "./RevealPanel";

export function AuthView() {
  const items = useStore((s) => s.sitrep.sealed);
  const [idx, setIdx] = useState(0);
  const [revealing, setRevealing] = useState<SealedMeta | null>(null);

  // Keep selection in range as the list refreshes underneath us.
  useEffect(() => {
    setIdx((i) => Math.max(0, Math.min(i, Math.max(0, items.length - 1))));
  }, [items.length]);

  const bindings = useMemo(
    () => [
      {
        key: "j",
        description: "next",
        handler: () => setIdx((i) => Math.min(items.length - 1, i + 1)),
      },
      { key: "k", description: "prev", handler: () => setIdx((i) => Math.max(0, i - 1)) },
      {
        key: "r",
        description: "reveal",
        handler: () => {
          const m = items[idx];
          if (m) setRevealing(m);
        },
      },
      {
        key: "Enter",
        description: "reveal",
        handler: () => {
          const m = items[idx];
          if (m) setRevealing(m);
        },
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [items, idx],
  );
  useKeys("modal", bindings, [bindings]);

  if (items.length === 0) {
    return (
      <div className="side-empty">
        no auth messages right now — login codes, password resets and sign-in
        alerts show up here.
      </div>
    );
  }

  return (
    <div className="auth-list">
      {items.map((m, i) => {
        const sel = i === idx;
        const KindIcon = authKindIcon(m.kind);
        return (
          <div
            key={m.id}
            className={`auth-row${sel ? " sel" : ""}`}
            onClick={() => setIdx(i)}
            onDoubleClick={() => setRevealing(m)}
            role="button"
            tabIndex={-1}
          >
            <Avatar sender={m.sender} />
            <span className="sender" title={m.sender}>
              {senderDisplayName(m.sender)}
            </span>
            <span className="auth-subject" title={m.subject}>
              {m.subject}
            </span>
            <span className="meta">
              <span className="auth-kind">
                <KindIcon size={13} /> {authKindLabel(m.kind)}
              </span>
              <span className="age">{relAge(m.received_at)}</span>
              {sel && <span className="verbs">[r] reveal</span>}
            </span>
          </div>
        );
      })}

      <div className="auth-foot">
        <kbd>j</kbd>/<kbd>k</kbd> select · <kbd>r</kbd>/<kbd>↵</kbd> reveal
      </div>

      {revealing && (
        <RevealPanel meta={revealing} onClose={() => setRevealing(null)} />
      )}
    </div>
  );
}
