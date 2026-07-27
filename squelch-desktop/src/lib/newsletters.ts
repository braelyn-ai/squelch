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
// SUPERSEDED (mostly): `marketing` is a real triage category now, and the
// pipeline's own classification is the qualifier whenever any is available
// (see DeriveOpts.marketingIds). The reason-string heuristic below survives
// only as a migration bridge for stores whose mail predates the category.
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
  /** The latest message's thread — clicking the card opens this email. */
  latest_thread_id: string;
  /** The window's aggregated updates, NEWEST FIRST — the viewer's horizontal
   *  queue (h/l between this sender's emails) and the bulk-done target. */
  items: AttentionUpdate[];
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
  /**
   * Message ids the PIPELINE classified `marketing` (GET /client/marketing).
   *
   * When this is non-empty it is the ONLY qualifier: a sender makes the zone
   * because the LLM said its mail is marketing, not because we pattern-matched
   * prose. When it is empty — nothing has been categorized yet, e.g. before a
   * re-triage — we fall back to the legacy heuristic so the zone is not simply
   * blank during the migration. See the qualification block below.
   */
  marketingIds?: Set<number>;
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
  const marketingIds = opts.marketingIds ?? new Set<number>();

  // Bucket by address, tracking newsletter/receipt evidence + robot/brand shape.
  interface Bucket {
    sender: string;
    total: number;
    newsletterHits: number;
    receiptHits: number;
    /** Messages of this sender the pipeline categorized `marketing`. */
    marketingHits: number;
    robot: boolean;
    latest: number;
    summary: string;
    latest_thread_id: string;
    items: AttentionUpdate[];
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
        marketingHits: 0,
        robot: isRobotSender(u.sender) || isBrandSender(u.sender),
        latest: 0,
        summary: "",
        latest_thread_id: "",
        items: [],
      };
      byAddr.set(address, b);
    }
    b.total += 1;
    b.items.push(u);
    if (marketingIds.has(u.id)) b.marketingHits += 1;
    if (isNewsletterReason(u.reason)) b.newsletterHits += 1;
    if (isReceiptReason(u.reason)) b.receiptHits += 1;
    const d = dateOf(u);
    if (d >= b.latest) {
      b.latest = d;
      if (u.one_line) b.summary = u.one_line;
      b.latest_thread_id = u.thread_id;
    }
  }

  const out: Newsletter[] = [];
  for (const [address, b] of byAddr) {
    // Exclude senders whose window is entirely receipts (order updates, not a
    // newsletter) with no newsletter signal at all.
    const allReceipts =
      b.receiptHits > 0 && b.newsletterHits === 0 && b.marketingHits === 0;
    if (allReceipts) continue;

    // QUALIFICATION.
    //
    // Preferred: the pipeline classified at least one of this sender's messages
    // as `marketing`. That is a real classification, not an inference.
    //
    // Legacy fallback (only while NOTHING has been categorized yet): the old
    // heuristic — a newsletter-shaped `reason` string, or a recurring
    // robot/brand sender. The second half of that was the leak: robot/brand
    // tests only ask whether the local part looks automated, which is true of
    // CI, alerts and system mail, none of which is marketing. It stays ONLY as
    // a migration bridge and disappears the moment real data exists.
    const qualifies =
      marketingIds.size > 0
        ? b.marketingHits > 0
        : b.newsletterHits > 0 || (b.robot && b.total >= 2);
    if (!qualifies) continue;

    out.push({
      address,
      sender: b.sender,
      count: b.total,
      summary: b.summary,
      latest: b.latest,
      latest_thread_id: b.latest_thread_id,
      items: [...b.items].sort((a2, b2) => dateOf(b2) - dateOf(a2)),
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

/**
 * Pick the HERO image src from a newsletter's sanitized html: the first http(s)
 * <img> that plausibly isn't chrome — declared width (when present) must be
 * >= 80px (skips social icons / spacer gifs; tracking pixels are already
 * stripped upstream). Protocol-relative srcs resolve to https. Returns null
 * when nothing qualifies — the card simply renders without a thumb.
 */
export function extractHeroSrc(html: string): string | null {
  if (typeof DOMParser === "undefined") return null;
  let doc: Document;
  try {
    doc = new DOMParser().parseFromString(html, "text/html");
  } catch {
    return null;
  }
  for (const img of Array.from(doc.querySelectorAll("img[src]"))) {
    let src = (img.getAttribute("src") ?? "").trim();
    if (src.startsWith("//")) src = "https:" + src;
    if (!/^https?:\/\//i.test(src)) continue;
    const w = parseInt(img.getAttribute("width") ?? "", 10);
    if (!Number.isNaN(w) && w < 80) continue;
    const h = parseInt(img.getAttribute("height") ?? "", 10);
    if (!Number.isNaN(h) && h < 40) continue;
    return src;
  }
  return null;
}

/**
 * Strip redundant genre labels from a newsletter summary — "Promotional email
 * from X:", "Event promotion for ...", "Newsletter: ..." — the Newsletters
 * section already says what these are. Conservative: only recognized leading
 * label shapes are removed; anything else passes through unchanged. The first
 * surviving letter is re-capitalized.
 */
export function cleanSummary(summary: string): string {
  let out = summary.trim();
  // "Promotional email (from X)(:|-|,) ", "Marketing email: ", "Newsletter: "
  out = out.replace(
    /^(promotional email|marketing email|newsletter|promo(?:tion)?)\s*(from\s+[^:,-]+)?[:,-]?\s+/i,
    "",
  );
  // "(Event|Sale|Summer sale|Product) promotion (for|from|of) " -> drop lead-in
  out = out.replace(/^\w[\w ]{0,24}?\bpromotion\s+(for|from|of)\s+/i, "");
  out = out.trim();
  if (!out) return summary.trim();
  return out.charAt(0).toUpperCase() + out.slice(1);
}
