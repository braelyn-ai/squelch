// RANKING — the pure scorer for the Sitrep "For your eyes" zone. Kept as a small
// dependency-free module (no React, no store) so the algorithm is unit-testable
// and swappable without touching the view.
//
// A standing item is scored by a configurable blend of URGENCY (how soon it's
// due) and SEVERITY (how important it is):
//
//   urgency(u)  = 1.0                      if overdue (deadline in the past)
//               = 1 / (1 + daysUntilDue/7) if it has a future deadline
//               = 0                         if it has no deadline
//   severity(u) = importance / 100          (clamped to 0..1)
//   score       = w * urgency + (1 - w) * severity     // w = rankWeight
//
// w (rankWeight) slides the blend between time (w→1) and severity (w→0); the
// product default is 0.6 (see prefs).

/** Minimal shape the scorer needs — a subset of AttentionUpdate. */
export interface Rankable {
  deadline: string | null | undefined;
  importance: number;
}

/** The default blend weight when no preference is set (time-leaning). */
export const DEFAULT_RANK_WEIGHT = 0.6;

/** Clamp helper. */
function clamp01(n: number): number {
  if (Number.isNaN(n)) return 0;
  if (n < 0) return 0;
  if (n > 1) return 1;
  return n;
}

/**
 * Urgency in 0..1. Overdue pins to 1.0; a future deadline decays with distance
 * (half-life-ish at a week); no/invalid deadline is 0.
 */
export function urgency(item: Rankable, now: number = Date.now()): number {
  if (!item.deadline) return 0;
  const t = new Date(item.deadline).getTime();
  if (Number.isNaN(t)) return 0;
  if (t <= now) return 1;
  const daysUntilDue = (t - now) / 86_400_000;
  return 1 / (1 + daysUntilDue / 7);
}

/** Severity in 0..1 from importance (0..100). */
export function severity(item: Rankable): number {
  return clamp01(item.importance / 100);
}

/**
 * Blended score in 0..1. `weight` (0..1) is the urgency share; the remainder is
 * severity. Out-of-range weights are clamped so a bad pref can't invert the sign.
 */
export function score(
  item: Rankable,
  weight: number = DEFAULT_RANK_WEIGHT,
  now: number = Date.now(),
): number {
  const w = clamp01(weight);
  return w * urgency(item, now) + (1 - w) * severity(item);
}

/**
 * Return a NEW array of the items sorted by descending score (highest first).
 * Stable-ish: equal scores keep their original relative order (the ids fall back
 * so a re-render doesn't reshuffle ties). Does not mutate the input.
 */
export function rankItems<T extends Rankable>(
  items: T[],
  weight: number = DEFAULT_RANK_WEIGHT,
  now: number = Date.now(),
): T[] {
  return items
    .map((item, i) => ({ item, i, s: score(item, weight, now) }))
    .sort((a, b) => (b.s !== a.s ? b.s - a.s : a.i - b.i))
    .map((r) => r.item);
}
