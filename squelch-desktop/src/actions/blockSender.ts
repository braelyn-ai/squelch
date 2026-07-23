// Shared "block this exact sender" primitive. A squelch rule is created on the
// EXACT sender address (not *@domain) — this one sender abused the situation, not
// necessarily its whole domain. Used by both the unsubscribe-violation prompt
// (ActionLayer) and the no-link fallback in the thread viewer, so the rule shape
// lives in exactly one place.
//
// The address is canonicalized with .trim().toLowerCase() to mirror the server's
// canonical `sender` form.

import { api } from "../api";

/** Create a squelch rule matching `sender` exactly. Throws ApiError on failure. */
export async function createBlockRule(sender: string): Promise<void> {
  await api.createRule({
    match_pattern: sender.trim().toLowerCase(),
    want: "",
    disposition: "squelch",
  });
}
