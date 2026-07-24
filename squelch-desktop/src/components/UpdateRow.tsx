// One dense update row: importance · sender · one_line · relative time ·
// matched-rule hint · deadline chip. Mouse click selects AND opens the thread
// (gmail semantics). Action affordances remain keyboard-first — the [r][e][d]
// verb hint only shows on the selected row.

import { memo } from "react";
import { Paperclip } from "lucide-react";
import type { AttentionUpdate } from "../api";
import {
  relAge,
  loudAge,
  isAging,
  deadlineChip,
  importanceColor,
  importanceMeter,
} from "../lib/format";
import { Avatar } from "./Avatar";
import { senderDisplayName } from "../lib/avatar";

export interface UpdateRowProps {
  update: AttentionUpdate;
  selected: boolean;
  /** STILL OPEN band: escalating left-rail weight + age note. */
  aging?: boolean;
  /** 0..1 escalation weight for the STILL OPEN visual ramp. */
  weight?: number;
  onSelect: (id: number) => void;
  onOpen: (u: AttentionUpdate) => void;
}

// Memoized: the inbox renders up to ~500 of these and moves the selection on
// every hover — without memo each hover re-rendered the whole list. Parents
// must pass identity-stable onSelect/onOpen for this to bite.
export const UpdateRow = memo(function UpdateRow({
  update: u,
  selected,
  aging,
  weight = 0,
  onSelect,
  onOpen,
}: UpdateRowProps) {
  const chip = deadlineChip(u.deadline);
  // The aging BADGE ("← 2 WEEKS") only earns its place once an item is genuinely
  // aging (age > 48h). Under that, the STILL OPEN row is still "open" but shows
  // the plain relative time like any other band — no shouty badge on fresh items.
  const showAgeBadge = aging && isAging(u.surfaced_at ?? u.resolved_at);
  // Escalation: heavier rail + text that leans toward amber as weight climbs.
  // The escalating weight still ramps for multi-day/week items; we key it off the
  // badge so pre-48h open rows read calm. color-mix keeps it theme-aware.
  const railWidth = showAgeBadge ? 3 + Math.round(weight * 3) : 3;
  const oneLineColor = showAgeBadge
    ? `color-mix(in srgb, var(--amber) ${Math.round(45 + weight * 55)}%, var(--fg-dim))`
    : "var(--fg-dim)";

  return (
    <div
      className={`row${selected ? " sel" : ""}${showAgeBadge ? " aging" : ""}`}
      style={showAgeBadge ? { borderLeftWidth: railWidth } : undefined}
      onMouseEnter={() => onSelect(u.id)}
      onClick={() => {
        onSelect(u.id);
        onOpen(u);
      }}
      role="button"
      tabIndex={-1}
    >
      <span
        className="imp meter"
        style={{ color: importanceColor(u.importance) }}
        aria-label={`importance ${u.importance}`}
      >
        {importanceMeter(u.importance)}
      </span>
      <Avatar sender={u.sender} />
      <span className="sender">
        {senderDisplayName(u.sender)}
      </span>
      {u.has_attachments && (
        <Paperclip
          size={12}
          className="att-clip"
          aria-label="has attachments"
        />
      )}
      <span className="one-line" style={{ color: oneLineColor }}>
        {u.one_line}
      </span>

      <span className="meta">
        {u.matched_rule !== null && (
          <span className="rule-hint">
            ·rule
          </span>
        )}

        {chip && (
          <span className={`chip ${chip.overdue ? "overdue" : "upcoming"}`}>
            {chip.text}
          </span>
        )}

        {showAgeBadge ? (
          <span className="open-note">
            <span>← {loudAge(u.surfaced_at ?? u.resolved_at)}</span>
          </span>
        ) : (
          <span className="age">{relAge(u.surfaced_at)}</span>
        )}

        {selected && <span className="verbs">[r][e][d]</span>}
      </span>
    </div>
  );
});
