// Small pure formatters for the read side. Dense, terminal-adjacent output:
// relative time as compact tokens ("2h", "5d", "3w"), deadline chips, importance
// coloring, and age weighting for the STILL OPEN escalation.

import type { Tier } from "../api";

/** Compact relative age from an ISO timestamp, e.g. "12m", "4h", "5d", "3w". */
export function relAge(iso: string | null | undefined): string {
  if (!iso) return "";
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "";
  const ms = Date.now() - then;
  if (ms < 0) return "now";
  const min = ms / 60_000;
  if (min < 1) return "now";
  if (min < 60) return `${Math.round(min)}m`;
  const h = min / 60;
  if (h < 24) return `${Math.round(h)}h`;
  const d = h / 24;
  if (d < 14) return `${Math.round(d)}d`;
  const w = d / 7;
  if (w < 8) return `${Math.round(w)}w`;
  const mo = d / 30;
  return `${Math.round(mo)}mo`;
}

/** Louder relative age used in the STILL OPEN band, e.g. "2 WEEKS", "5 DAYS". */
export function loudAge(iso: string | null | undefined): string {
  if (!iso) return "";
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "";
  const ms = Date.now() - then;
  if (ms < 0) return "NOW";
  const min = ms / 60_000;
  if (min < 60) return `${Math.round(min)} MIN`;
  const h = min / 60;
  if (h < 24) return unit(Math.round(h), "HOUR");
  const d = h / 24;
  if (d < 14) return unit(Math.round(d), "DAY");
  const w = Math.round(d / 7);
  return unit(w, "WEEK");
}

function unit(n: number, word: string): string {
  return `${n} ${word}${n === 1 ? "" : "S"}`;
}

/**
 * Whole hours since an ISO timestamp (0 if missing/invalid/future). Used to gate
 * the STILL OPEN aging badge so it only appears once an item is genuinely aging
 * (age > 48h) rather than on every open row.
 */
export function ageHours(iso: string | null | undefined): number {
  if (!iso) return 0;
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return 0;
  const ms = Date.now() - then;
  if (ms <= 0) return 0;
  return ms / 3_600_000;
}

/** True once an item has aged past the 48h threshold where the badge earns its place. */
export const AGING_THRESHOLD_H = 48;
export function isAging(iso: string | null | undefined): boolean {
  return ageHours(iso) > AGING_THRESHOLD_H;
}

/** "last checked: Xh ago" tail for the header. */
export function lastChecked(iso: string | null | undefined): string {
  if (!iso) return "never";
  const a = relAge(iso);
  return a ? `${a} ago` : "just now";
}

/**
 * Age weight 0..1 for the STILL OPEN escalation. Combines raw age with
 * importance so a nudged old high-importance thread visually shouts. Caps at
 * ~30 days so ancient items don't all pin to max identically.
 */
export function openWeight(iso: string | null | undefined, importance: number): number {
  if (!iso) return 0;
  const ms = Date.now() - new Date(iso).getTime();
  if (Number.isNaN(ms) || ms <= 0) return 0;
  const days = ms / 86_400_000;
  const ageComponent = Math.min(1, days / 30); // saturate at a month
  const impComponent = Math.min(1, Math.max(0, importance) / 100);
  // Age dominates (this band's whole point is rot), importance nudges.
  return Math.min(1, ageComponent * 0.7 + impComponent * 0.3);
}

/** Deadline chip text. Past-due shows the overdue span; upcoming shows a date. */
export function deadlineChip(iso: string | null | undefined): {
  text: string;
  overdue: boolean;
} | null {
  if (!iso) return null;
  const t = new Date(iso).getTime();
  if (Number.isNaN(t)) return null;
  const ms = t - Date.now();
  if (ms < 0) {
    return { text: `${loudAge(iso)} PAST DUE`, overdue: true };
  }
  return { text: `due ${shortDate(iso)}`, overdue: false };
}

/** Compact date like "Jul 11" (local). */
export function shortDate(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

/** Date + time like "Jul 11 14:32" for thread messages / audit. */
export function dateTime(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** CSS var name for a tier's accent color (used by browse-all + chips). */
export function tierColor(tier: Tier): string {
  switch (tier) {
    case "past_due":
      return "var(--red)";
    case "deadline":
      return "var(--amber)";
    case "signal":
      return "var(--accent)";
    case "noise":
      return "var(--fg-faint)";
    default:
      return "var(--fg-dim)";
  }
}

/** Importance -> color bucket. High importance leans hot. */
export function importanceColor(n: number): string {
  if (n >= 85) return "var(--red)";
  if (n >= 70) return "var(--amber)";
  if (n >= 40) return "var(--fg)";
  return "var(--fg-dim)";
}

/**
 * Importance (0..100) as a 5-block meter glyph, e.g. "▰▰▰▱▱" — the machined
 * readout used consistently in rows AND obligation cards (Precision-instrument
 * identity). Filled blocks = ceil(n/20), clamped 0..5; the number itself is the
 * title/aria text so the glyph stays glanceable. Render in Plex Mono.
 */
export const METER_SEGMENTS = 5;
export function importanceMeter(n: number): string {
  const clamped = Math.max(0, Math.min(100, n));
  const filled = Math.min(
    METER_SEGMENTS,
    Math.ceil((clamped / 100) * METER_SEGMENTS),
  );
  return "▰".repeat(filled) + "▱".repeat(METER_SEGMENTS - filled);
}
