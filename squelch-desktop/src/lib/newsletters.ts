// NEWSLETTERS derivation for the Sitrep dashboard zone. The newsletters zone is
// the rule-onboarding surface: it surfaces recurring noise-tier senders and — if
// no rule governs them yet — invites the human to define one (the Minga "choose
// what you want to see" flow).
//
// Calibrated against squelch-core's Stage-1 rung-5 reason strings (read-only peek
// at src/triage/{rules,mod}.rs). The engine emits, on Tier::Noise:
//   • "bulk/list mail (unsubscribe footer)"        → newsletter/marketing (INCLUDE)
//   • "order confirmation / receipt"               → receipt/order (EXCLUDE)
//   • "cold-outbound / sales language …"           → cold sales (not a newsletter)
//   • "matched squelch/filtered rule …"            → user-muted (still shows so the
//                                                     rule chip can render)
// We treat the newsletter reason as the strong signal, and additionally admit
// robot/brand senders that show up ≥2× in the window (recurring machine mail).
// Senders whose window is entirely receipts are excluded.

import type { AttentionUpdate, SenderRule } from "../api";
import { faviconDomain, isRobotSender, isBrandSender } from "./avatar";

/** Exact rung-5 reason literals we key off (substring-matched, case-insensitive). */
const NEWSLETTER_REASON = "unsubscribe footer"; // from "bulk/list mail (unsubscribe footer)"
const RECEIPT_REASON = "order confirmation / receipt";

/** Backstop reason shapes in case the exact literal drifts server-side. */
const NEWSLETTER_HINT =
  /\b(unsubscribe|newsletter|bulk\/list|mailing list|marketing|promotional|digest)\b/i;
const RECEIPT_HINT =
  /\b(order confirmation|receipt|your order|shipment|shipped|tracking)\b/i;

function isNewsletterReason(reason: string): boolean {
  const r = reason.toLowerCase();
  return r.includes(NEWSLETTER_REASON) || NEWSLETTER_HINT.test(r);
}
function isReceiptReason(reason: string): boolean {
  const r = reason.toLowerCase();
  return r.includes(RECEIPT_REASON.toLowerCase()) || RECEIPT_HINT.test(r);
}

/** Bare address (lowercased) from a sender string, for grouping + rule matching. */
export function senderAddress(sender: string): string {
  const m =
    sender.match(/[<\s]([^<>\s@]+@[^<>\s]+)>?\s*$/) ??
    sender.match(/([^<>\s@]+@[^<>\s]+)/);
  return (m ? m[1] : sender).trim().toLowerCase();
}

/** ms-epoch date proxy for a noise update (no received_at on the wire model). */
function dateOf(u: AttentionUpdate): number {
  const iso = u.surfaced_at ?? u.resolved_at;
  if (!iso) return 0;
  const t = new Date(iso).getTime();
  return Number.isNaN(t) ? 0 : t;
}

/** A newsletter card: one recurring noise sender for the window. */
export interface Newsletter {
  /** Grouping key = bare lowercased address. */
  address: string;
  /** A representative raw sender string (for avatar + display name). */
  sender: string;
  /** Count of qualifying noise messages in the window. */
  count: number;
  /** Latest one_line in the window (the summary line). */
  summary: string;
  /** Latest message date (ms) — cards sort newest-first. */
  latest: number;
  /** The rule governing this sender, if any (drives the chip vs. CTA). */
  rule: SenderRule | null;
}

/**
 * Glob match for a rule's match_pattern (e.g. "*@acme.com", "*@*.acme.com",
 * "billing@acme.com") against a bare address. `*` matches any run; matching is
 * case-insensitive. Mirrors the server's pragmatic glob shape.
 */
export function ruleMatchesAddress(pattern: string, address: string): boolean {
  const pat = pattern.trim().toLowerCase();
  if (!pat) return false;
  // Fast path: a bare "*@domain" — compare the domain tail directly.
  const rx =
    "^" +
    pat.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*") +
    "$";
  try {
    return new RegExp(rx).test(address.toLowerCase());
  } catch {
    // Pragmatic fallback: does the pattern's domain appear in the address?
    const dom = pat.split("@").pop() ?? pat;
    return address.toLowerCase().includes(dom.replace(/\*/g, ""));
  }
}

/** Find the first rule that governs an address (exact-local beats wildcard-ish). */
export function ruleForAddress(
  rules: SenderRule[],
  address: string,
): SenderRule | null {
  // Prefer the most specific (fewest wildcards) match for a stable chip.
  const hits = rules.filter((r) => ruleMatchesAddress(r.match_pattern, address));
  if (hits.length === 0) return null;
  hits.sort(
    (a, b) =>
      (a.match_pattern.split("*").length - 1) -
      (b.match_pattern.split("*").length - 1),
  );
  return hits[0];
}

export interface DeriveOpts {
  /** Only include messages at/after this ms-epoch (default: last 7 days). */
  since?: number;
  /** Max cards to return (default: 24). */
  limit?: number;
}

const WEEK_MS = 7 * 86_400_000;

/**
 * Derive newsletter cards from a batch of noise-tier updates. See module header
 * for the heuristic. Pure + testable.
 */
export function deriveNewsletters(
  updates: AttentionUpdate[],
  rules: SenderRule[],
  opts: DeriveOpts = {},
): Newsletter[] {
  const since = opts.since ?? Date.now() - WEEK_MS;
  const limit = opts.limit ?? 24;

  // Bucket by address, tracking newsletter/receipt evidence + robot/brand shape.
  interface Bucket {
    sender: string;
    total: number;
    newsletterHits: number;
    receiptHits: number;
    robot: boolean;
    latest: number;
    summary: string;
  }
  const byAddr = new Map<string, Bucket>();

  for (const u of updates) {
    // EXCLUDE RECEIPTS. The server AUTO-RESOLVES receipt-classified mail to
    // status='done' at ingest (it lives only in the Receipts category, never the
    // inbox). A done row is a settled record, not recurring noise to onboard a
    // rule for — so it must never surface as a "newsletter". This is what keeps
    // Bay Wheels (a ride receipt) out of Newsletters even when the /client/updates
    // noise feed still carries it. Belt-and-suspenders with the receipt-reason
    // exclusion below (which still catches any receipt-shaped sender not yet
    // auto-resolved).
    if (u.status === "done") continue;
    if (dateOf(u) < since) continue;
    const address = senderAddress(u.sender);
    if (!address.includes("@")) continue;

    let b = byAddr.get(address);
    if (!b) {
      b = {
        sender: u.sender,
        total: 0,
        newsletterHits: 0,
        receiptHits: 0,
        robot: isRobotSender(u.sender) || isBrandSender(u.sender),
        latest: 0,
        summary: "",
      };
      byAddr.set(address, b);
    }
    b.total += 1;
    if (isNewsletterReason(u.reason)) b.newsletterHits += 1;
    if (isReceiptReason(u.reason)) b.receiptHits += 1;
    const d = dateOf(u);
    if (d >= b.latest) {
      b.latest = d;
      if (u.one_line) b.summary = u.one_line;
    }
  }

  const out: Newsletter[] = [];
  for (const [address, b] of byAddr) {
    // Exclude senders whose window is entirely receipts (order updates, not a
    // newsletter) with no newsletter signal at all.
    const allReceipts = b.receiptHits > 0 && b.newsletterHits === 0;
    if (allReceipts) continue;

    // Qualify: a newsletter-shaped reason present, OR a recurring robot/brand
    // sender (≥2 noise messages in the window).
    const qualifies =
      b.newsletterHits > 0 || (b.robot && b.total >= 2);
    if (!qualifies) continue;

    out.push({
      address,
      sender: b.sender,
      count: b.total,
      summary: b.summary,
      latest: b.latest,
      rule: ruleForAddress(rules, address),
    });
  }

  // Newest activity first; ties break on higher volume.
  out.sort((a, b) => b.latest - a.latest || b.count - a.count);
  return out.slice(0, limit);
}

/** The `*@domain` pattern a newsletter CTA prefills into the rule editor. */
export function domainPattern(address: string): string {
  const domain = faviconDomain(address) ?? address.split("@").pop() ?? address;
  return `*@${domain}`;
}
