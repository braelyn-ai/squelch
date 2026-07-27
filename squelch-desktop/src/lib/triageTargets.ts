// Correction targets for the triage-fix palette (`v`), and the prefix matcher
// that turns what you type into one of them.
//
// The values here MIRROR the pipeline exactly — TriageAxis::allowed in
// squelch-core/src/types.rs is the server-side gate, and it rejects anything
// else with a 400. That is deliberate: the whole point of the feedback dataset
// is "the model said X, the human said Y", and Y has to be a label the model
// could itself have produced or the pair means nothing. If you add a category
// to the pipeline, add it here too; if these drift, the server refuses the
// write rather than storing a label nothing can learn from.
//
// TYPING. You should be able to hit v, type "bill", and hit Enter. The aliases
// below are what make that work: "bill" is not a category name, but it is
// obviously what a person types when they mean one. Matches are RANKED rather
// than resolved to a single answer, because some words genuinely are ambiguous
// — "bill" fits both an invoice you owe and an autopay notice — and silently
// guessing between them would write a wrong label into the training set. The
// list stays on screen so an ambiguous prefix is a visible choice, not a
// coin-flip.

export type TriageAxis = "tier" | "category" | "sensitivity";

export interface TriageTarget {
  axis: TriageAxis;
  /** The wire value. Must be in TriageAxis::allowed server-side. */
  value: string;
  /** What the human sees. */
  label: string;
  /** One line on when this is the right answer. */
  hint: string;
  /** Extra words that should match this target. */
  aliases: string[];
}

export const TRIAGE_TARGETS: TriageTarget[] = [
  // --- categories: what KIND of mail this is -------------------------------
  {
    axis: "category",
    value: "invoice",
    label: "Invoice",
    hint: "a bill you owe and have to pay",
    aliases: ["bill", "billing", "invoice", "owe", "payment", "due"],
  },
  {
    axis: "category",
    value: "autopay_bill",
    label: "Autopay bill",
    hint: "a bill that pays itself; a record, not a task",
    aliases: ["autopay", "auto", "bill", "billing", "subscription", "recurring"],
  },
  {
    axis: "category",
    value: "banking_statement",
    label: "Bank statement",
    hint: "a periodic statement — a record",
    aliases: ["statement", "bank", "banking", "balance"],
  },
  {
    axis: "category",
    value: "transaction_alert",
    label: "Transaction alert",
    hint: "a charge or activity notice",
    aliases: ["transaction", "charge", "alert", "spend", "purchase"],
  },
  {
    axis: "category",
    value: "marketing",
    label: "Marketing",
    hint: "a sale, offer, newsletter or promo blast",
    aliases: [
      "marketing",
      "newsletter",
      "promo",
      "promotional",
      "ad",
      "advertising",
      "sale",
      "offer",
      "deal",
    ],
  },
  {
    axis: "category",
    value: "general",
    label: "General",
    hint: "none of the money categories",
    aliases: ["general", "none", "other", "plain"],
  },

  // --- auth: the sealed axis -----------------------------------------------
  // Auth is NOT a category — it is `triage.sensitivity`, and it is the axis with
  // real consequences. Sealed mail is what the Auth page lists and is
  // structurally absent from the agent door (/mcp), so moving a message here
  // RESTRICTS what any agent can ever see of it, and moving it out EXPOSES it.
  // Both directions are offered because both are genuine triage mistakes worth
  // recording: a missed login code landed in the normal inbox, or ordinary mail
  // got locked away (seal.rs carries explicit guards against over-sealing, so
  // its false positives are exactly the signal worth collecting).
  {
    axis: "sensitivity",
    value: "sealed",
    label: "Auth",
    hint: "a code, reset or sign-in alert; hides it from agents",
    aliases: [
      "auth",
      "sealed",
      "seal",
      "code",
      "otp",
      "2fa",
      "mfa",
      "login",
      "signin",
      "verification",
      "password",
      "reset",
    ],
  },
  {
    axis: "sensitivity",
    value: "normal",
    label: "Not auth",
    hint: "wrongly sealed; unhides it from agents",
    aliases: ["notauth", "unseal", "unsealed", "normal", "notsealed"],
  },

  // --- tiers: how much it should DEMAND of you -----------------------------
  {
    axis: "tier",
    value: "past_due",
    label: "Past due",
    hint: "a deadline that has already passed",
    aliases: ["pastdue", "past", "overdue", "late"],
  },
  {
    axis: "tier",
    value: "deadline",
    label: "Deadline",
    hint: "has a date you must act by",
    aliases: ["deadline", "due", "date"],
  },
  {
    axis: "tier",
    value: "signal",
    label: "Signal",
    hint: "worth your attention, no deadline",
    aliases: ["signal", "important", "attention"],
  },
  {
    axis: "tier",
    value: "noise",
    label: "Noise",
    hint: "should not have surfaced at all",
    // The marketing words deliberately do NOT live here any more: marketing is
    // a real category now, and the two say different things. "This is
    // marketing" is a statement about what the mail IS; "this is noise" is a
    // statement about whether it should have surfaced. Conflating them would
    // teach the dataset that every promo is unwanted, which is exactly the
    // assumption the marketing category exists to stop making.
    aliases: ["noise", "junk", "ignore", "spam", "quiet"],
  },
];

/** Normalize for matching: lowercase, and underscores/spaces are the same. */
function norm(s: string): string {
  return s.toLowerCase().replace(/[\s_-]+/g, "");
}

/**
 * Rank targets against what the user typed. Higher score = better match.
 * Returns 0 when the target should not appear at all.
 *
 * The ordering is deliberately boring: an exact hit beats a prefix, a prefix
 * beats a mid-word substring, and a match on the real value beats a match on a
 * convenience alias. Anything cleverer (fuzzy distance, typo tolerance) would
 * make it harder to predict which label you are about to write, and writing the
 * wrong label is the one failure mode this feature cannot afford.
 */
export function scoreTarget(target: TriageTarget, query: string): number {
  const q = norm(query);
  if (!q) return 1; // empty query: everything shows, in declaration order

  const value = norm(target.value);
  const label = norm(target.label);
  if (value === q || label === q) return 100;
  if (value.startsWith(q) || label.startsWith(q)) return 80;

  let best = 0;
  for (const alias of target.aliases) {
    const a = norm(alias);
    if (a === q) best = Math.max(best, 60);
    else if (a.startsWith(q)) best = Math.max(best, 40);
  }
  if (best > 0) return best;

  // Last resort so a mid-word guess still finds something.
  if (value.includes(q) || label.includes(q)) return 20;
  return 0;
}

/** The ranked, filtered target list for a query. Stable within equal scores. */
export function matchTargets(query: string): TriageTarget[] {
  return TRIAGE_TARGETS.map((t, i) => ({ t, i, s: scoreTarget(t, query) }))
    .filter((r) => r.s > 0)
    .sort((a, b) => b.s - a.s || a.i - b.i)
    .map((r) => r.t);
}

/** Human-facing label for a raw wire value, for showing what it WAS. */
export function targetLabel(axis: TriageAxis, value: string | null | undefined): string {
  if (!value) return "unset";
  const hit = TRIAGE_TARGETS.find((t) => t.axis === axis && t.value === value);
  return hit ? hit.label : value;
}
