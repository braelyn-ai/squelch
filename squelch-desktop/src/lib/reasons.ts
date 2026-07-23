// Per-property triage reason lookup for hover tooltips.
//
// The wire model (Update) carries an optional `field_reasons` map with a short
// human-readable justification per triaged property (importance / deadline /
// tier). This helper resolves the tooltip text for a given property with a
// graceful cascade: the property-specific reason, then the update's global
// `reason`, then the caller's static fallback. Tooltips render via the plain
// `title` attribute only — the returned string is plain text.

import type { FieldReasons, Update } from "../api";

/**
 * Resolve tooltip text for one triaged property. Falls back to the update's
 * global `reason`, then the caller-supplied static `fallback`, so call sites
 * stay one-liners and never surface an empty title.
 */
export function reasonFor(
  u: Pick<Update, "field_reasons" | "reason">,
  field: keyof FieldReasons,
  fallback: string,
): string {
  const specific = u.field_reasons?.[field];
  if (specific && specific.trim()) return specific;
  if (u.reason && u.reason.trim()) return u.reason;
  return fallback;
}
