// Sealed section: lock-chip rows from /client/sealed metadata. Bodies are NEVER
// fetched here — only on an explicit reveal keypress (`r` on a selected sealed
// row), handled by the parent which owns the RevealPanel. This component is
// metadata-only and purely presentational.

import type { SealedMeta } from "../api";
import { relAge } from "../lib/format";

export interface SealedSectionProps {
  items: SealedMeta[];
  selectedId: number | null;
  onSelect: (id: number) => void;
  onReveal: (m: SealedMeta) => void;
}

export function SealedSection({
  items,
  selectedId,
  onSelect,
  onReveal,
}: SealedSectionProps) {
  if (items.length === 0) return null;

  return (
    <section className="sealed">
      <div className="sealed-head">🔒 sealed ({items.length}) · [r] reveal</div>
      {items.map((m) => (
        <div
          key={m.id}
          className={`lock-row num${m.id === selectedId ? " sel" : ""}`}
          onClick={() => onSelect(m.id)}
          onDoubleClick={() => onReveal(m)}
          role="button"
          tabIndex={-1}
        >
          <span className="lock">🔒</span>
          <span className="sender" title={m.sender}>
            {m.sender}
          </span>
          <span className="one-line" title={m.subject}>
            {m.subject}
          </span>
          <span className="meta">
            {m.kind && <span className="kind">{m.kind}</span>}
            <span className="age">{relAge(m.received_at)}</span>
          </span>
        </div>
      ))}
    </section>
  );
}
