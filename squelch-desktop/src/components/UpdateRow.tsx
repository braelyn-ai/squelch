// One dense update row: importance · sender · one_line · relative time ·
// matched-rule hint · deadline chip. Used by every band. Mouse click selects;
// double-click drills in. All action affordances remain keyboard-first — the
// [r][e][d] verb hint only shows on the selected row.

import type { AttentionUpdate } from "../api";
import {
  relAge,
  loudAge,
  deadlineChip,
  importanceColor,
} from "../lib/format";

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
  // Escalation: brighter text + heavier rail as weight climbs.
  const railWidth = aging ? 2 + Math.round(weight * 3) : 2;
  const oneLineColor = aging
    ? `rgba(240, 198, 116, ${0.55 + weight * 0.45})`
    : "var(--fg-dim)";

  return (
    <div
      className={`row num${selected ? " sel" : ""}${aging ? " aging" : ""}`}
      style={aging ? { borderLeftWidth: railWidth } : undefined}
      onClick={() => onSelect(u.id)}
      onDoubleClick={() => onOpen(u)}
      role="button"
      tabIndex={-1}
    >
      <span className="imp" style={{ color: importanceColor(u.importance) }}>
        {u.importance}
      </span>
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

        {aging ? (
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
