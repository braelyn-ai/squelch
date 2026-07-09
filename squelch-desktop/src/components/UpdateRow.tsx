// One dense update row: importance · sender · one_line · relative time ·
// matched-rule hint · deadline chip. Used by every band. Mouse click selects;
// double-click drills in. All action affordances remain keyboard-first — the
// [r][e][d] verb hint only shows on the selected row.

import type { AttentionUpdate } from "../api";
import {
  relAge,
  loudAge,
  isAging,
  deadlineChip,
  importanceColor,
} from "../lib/format";
import { Avatar } from "./Avatar";

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

export function UpdateRow({
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
      className={`row num${selected ? " sel" : ""}${showAgeBadge ? " aging" : ""}`}
      style={showAgeBadge ? { borderLeftWidth: railWidth } : undefined}
      onClick={() => onSelect(u.id)}
      onDoubleClick={() => onOpen(u)}
      role="button"
      tabIndex={-1}
    >
      <span className="imp" style={{ color: importanceColor(u.importance) }}>
        {u.importance}
      </span>
      <Avatar sender={u.sender} />
      <span className="sender" title={u.sender}>
        {u.sender}
      </span>
      <span className="one-line" style={{ color: oneLineColor }} title={u.one_line}>
        {u.one_line}
      </span>

      <span className="meta">
        {u.matched_rule !== null && (
          <span className="rule-hint" title={`matched rule #${u.matched_rule}`}>
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
}
