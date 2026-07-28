// Sender avatars — deterministic and initials-based by default.
//
// PRIVACY MODEL: human correspondents are NEVER resolved over the network — no
// Gravatar, no favicon fetch — because the human correspondent graph must never
// leak off-device. The ONLY exception is ROBOT senders (no-reply@, notifications@,
// billing@, …), whose local-parts identify a service, not a person. For those we
// fetch the DOMAIN's favicon once (cached per-domain, see faviconVerdict), which
// leaks nothing about who a human talks to. Everything else stays local: initials
// from the display name (fallback: first letter of the local-part), over a stable
// address-hashed background from a small theme-aware palette.

/** Palette index CSS vars (defined in global.css) — 10 theme-aware pairs. */
export const AVATAR_SLOTS = 10;

/** Extract a display name and address from a sender string. */
function parseSender(sender: string): { name: string; addr: string } {
  const s = (sender ?? "").trim();
  // "Sarah Chen <sarah@acme.com>" -> name "Sarah Chen", addr "sarah@acme.com"
  const m = s.match(/^(.*?)[<\s]*([^<>\s@]+@[^<>\s]+)>?\s*$/);
  if (m) {
    const name = m[1].replace(/["']/g, "").trim();
    return { name, addr: m[2] };
  }
  return { name: s, addr: s };
}

/**
 * Up to two initials for a sender.
 *
 * THE SOURCE IS NEVER THE FULL ADDRESS. It used to be, and the domain leaked
 * into the result: "bboynton97@gmail.com" split to ["bboynton97@gmail","com"]
 * and rendered "BC" — that second letter is the C of ".com". Almost every bare
 * address produced a "?C" monogram, which is why a column of avatars read RC,
 * IC, BC, MC, SC.
 *
 * Order: a real display name, then the resolved brand/robot label (so a row
 * labelled "Corpnet" shows CO rather than the IC of "info@corpnet.com"), then
 * the local-part alone.
 */
export function initialsFor(sender: string): string {
  const { name, addr } = parseSender(sender);
  const local = (addr.split("@")[0] ?? "").split("+")[0];

  let source = name;
  if (!source) {
    const shown = senderDisplayName(sender);
    source = shown && shown.toLowerCase() !== addr.toLowerCase() ? shown : local;
  }

  const words = source.split(/[\s._-]+/).filter((w) => /[a-z0-9]/i.test(w));
  if (words.length >= 2) {
    return (words[0][0] + words[1][0]).toUpperCase();
  }
  if (words.length === 1 && words[0].length >= 2) {
    return words[0].slice(0, 2).toUpperCase();
  }
  return (local[0] ?? source[0] ?? "?").toUpperCase();
}

/** Deterministic small hash of the address (stable across sessions). */
function hashAddr(addr: string): number {
  const a = parseSender(addr).addr.toLowerCase();
  let h = 5381;
  for (let i = 0; i < a.length; i++) {
    h = ((h << 5) + h + a.charCodeAt(i)) >>> 0;
  }
  return h;
}

/** Palette slot 0..AVATAR_SLOTS-1 for a sender, deterministic by address. */
export function avatarSlot(sender: string): number {
  return hashAddr(sender) % AVATAR_SLOTS;
}

// ---- Robot senders: favicon avatars (the one network exception) -----------

// Robot local-part shapes (segment BEFORE any "+tag"). These are automated
// service mailboxes, not people — safe to resolve a favicon for. Humans never
// match, so humans never trigger a fetch.
const ROBOT_LOCAL =
  /^(no-?reply|do-?not-?reply|notifications?|alerts?|updates?|news(letter)?|marketing|mailer|billing|receipts?|orders?|team|hello|info|support|accounts?|security|admin|service|contact|help|feedback|noreply-\S*|e?mail|invoices?|statements?|confirmations?|tracking|delivery|digest|bulletin)$/i;

// The above must match the WHOLE local-part, which real senders very often
// fail: "no.reply.alerts@chase.com", "no_reply@discord.com",
// "billing-noreply@stripe.com" and "no-reply-aws@amazon.com" are all obviously
// machines, and all fell through to initials. So separators are squashed and
// the result is scanned for an UNAMBIGUOUS automation marker.
//
// Deliberately narrow: only markers that no human is ever behind. The
// human-capable words in ROBOT_LOCAL (hello, info, support, team, contact) stay
// whole-local-part matches ONLY — segment-matching those would classify
// "jane.support@acme.com" as a robot and fetch a favicon for a domain a HUMAN
// corresponds with, which is exactly the leak the privacy model forbids.
const ROBOT_MARKER = /(noreply|donotreply|mailerdaemon|automailer|automated|autoconfirm)/;

// Mail-ish subdomain prefixes to peel so notifications.github.com resolves the
// github.com favicon. First label only; naive but sufficient.
const MAIL_SUBDOMAIN =
  /^(mail|email|e|em|mg|mta|smtp|news|info|mailer|marketing|notifications?|alerts?|sfmail|bounce|reply|link|click|go|m)$/i;

/** True if the sender's local-part (pre-"+tag") is a known robot shape. */
export function isRobotSender(sender: string): boolean {
  const { addr } = parseSender(sender);
  const local = addr.split("@")[0] ?? "";
  const base = local.split("+")[0]; // segment before any +tag
  if (ROBOT_LOCAL.test(base)) return true;
  // "no.reply.alerts" / "no_reply" / "billing-noreply" -> "...noreply..."
  return ROBOT_MARKER.test(base.toLowerCase().replace(/[^a-z0-9]/g, ""));
}

/**
 * The brand's base label — the first label of the favicon domain (the domain
 * after stripping a mail-ish subdomain, two-label minimum). e.g. ebay.com ->
 * "ebay", sfmail.corpnet.com -> "corpnet". Null when there's no usable host.
 * Shared by isBrandSender and the display-name normalizer.
 */
export function baseLabel(sender: string): string | null {
  const domain = faviconDomain(sender);
  if (!domain) return null;
  return domain.split(".")[0] || null;
}

/**
 * True if the local-part equals the domain's base label (case-insensitive),
 * e.g. "eBay@eBay.com" or "corpnet@sfmail.corpnet.com". These are brand
 * mailboxes that name a service, not a person — safe to resolve a favicon for
 * and to display as the bare brand name.
 */
export function isBrandSender(sender: string): boolean {
  const { addr } = parseSender(sender);
  const local = (addr.split("@")[0] ?? "").split("+")[0]; // pre-"+tag"
  const base = baseLabel(sender);
  if (!local || !base) return false;
  return local.toLowerCase() === base.toLowerCase();
}

/**
 * Base domain for a favicon lookup: strip ONE leading mail-ish subdomain label,
 * keeping a two-label minimum (never strips example.com down to com). Naive by
 * design — good enough to map bulk-mail hosts back to the brand domain.
 */
export function faviconDomain(sender: string): string | null {
  const { addr } = parseSender(sender);
  const host = (addr.split("@")[1] ?? "").toLowerCase().replace(/\.$/, "");
  if (!host || !host.includes(".")) return null;
  const labels = host.split(".");
  if (labels.length > 2 && MAIL_SUBDOMAIN.test(labels[0])) {
    labels.shift();
  }
  return labels.length >= 2 ? labels.join(".") : null;
}

/**
 * The name to SHOW for a sender, per the 2026-07-09 display rules:
 *  1. If a display name exists and differs from the raw address, use it.
 *  2. Else if a BRAND sender ("eBay@eBay.com"), show the local-part as given
 *     ("eBay") — no @domain tail.
 *  3. Else if a ROBOT sender ("no-reply@stripe.com"), show the capitalized base
 *     domain label ("Stripe").
 *  4. Else (a human with no display name) show the address as-is.
 * Never emits "x@x.com"-style redundancy.
 */
export function senderDisplayName(sender: string): string {
  const { name, addr } = parseSender(sender);
  if (name && name.toLowerCase() !== addr.toLowerCase()) return name;

  if (isBrandSender(sender)) {
    const local = (addr.split("@")[0] ?? "").split("+")[0];
    if (local) return local; // "eBay" — as given
  }
  if (isRobotSender(sender)) {
    const base = baseLabel(sender);
    if (base) return base.charAt(0).toUpperCase() + base.slice(1);
  }
  return addr;
}

/** DuckDuckGo icon service URL for a base domain. */
export function faviconUrl(domain: string): string {
  return `https://icons.duckduckgo.com/ip3/${domain}.ico`;
}

// ---- Per-domain verdict cache ---------------------------------------------
//
// "ok" is PERMANENT — an icon that loaded once will load again.
//
// A FAILURE IS NOT. This cache used to store "failed" forever, with no expiry
// and no retry, which quietly broke the feature over time: `<img>` reports one
// undifferentiated `onerror`, so being offline for a moment, a DDG rate-limit,
// a DNS blip or a slow cold start all looked identical to "this domain has no
// icon" — and every one of them was written down as permanent. A real cache
// inspected on 2026-07-27 had 50 domains marked failed, including github.com,
// paypal.com, google.com, ebay.com, venmo.com and schwab.com, every one of
// which served a valid icon when re-tested. The user saw initials everywhere
// and reasonably concluded favicons were broken.
//
// So failures now carry the time they happened and are retried after
// FAILED_RETRY_MS. A domain that genuinely has no icon costs one request a
// week; a domain that failed transiently heals itself.
type Verdict = "ok" | "failed";

/** How long a failure is trusted before the domain is worth another attempt. */
const FAILED_RETRY_MS = 7 * 24 * 60 * 60 * 1000; // 7 days

/** On-disk shape. Legacy entries are bare strings (see the migration below). */
type StoredVerdict = Verdict | { v: "failed"; t: number };
type Entry = { v: "ok" } | { v: "failed"; t: number };

const LS_KEY = "squelch.favicons";
const mem = new Map<string, Entry>();

function loadStore(): Record<string, StoredVerdict> {
  try {
    return JSON.parse(localStorage.getItem(LS_KEY) || "{}");
  } catch {
    return {};
  }
}

// Warm the in-memory map from localStorage once at module load.
try {
  for (const [d, raw] of Object.entries(loadStore())) {
    if (raw === "ok") {
      mem.set(d, { v: "ok" });
    } else if (raw && typeof raw === "object" && raw.v === "failed") {
      if (typeof raw.t === "number") mem.set(d, { v: "failed", t: raw.t });
    }
    // A LEGACY bare "failed" (written before failures expired) is deliberately
    // NOT loaded: it carries no timestamp, so there is no honest way to age it,
    // and the odds are high it was a transient failure recorded as permanent.
    // Dropping it retries the domain once and then re-records it properly. This
    // is what un-poisons an existing install.
  }
} catch {
  /* no localStorage (e.g. SSR/tests) — in-memory only */
}

/**
 * Cached verdict for a domain. Returns null when the domain is unresolved OR
 * when its recorded failure has aged out — both mean "try again".
 */
export function faviconVerdict(domain: string, now = Date.now()): Verdict | null {
  const e = mem.get(domain);
  if (!e) return null;
  if (e.v === "ok") return "ok";
  return now - e.t < FAILED_RETRY_MS ? "failed" : null;
}

/** Record a domain verdict in both the in-memory map and localStorage. */
export function setFaviconVerdict(
  domain: string,
  verdict: Verdict,
  now = Date.now(),
): void {
  const prev = mem.get(domain);
  if (prev?.v === "ok" && verdict === "ok") return;
  const entry: Entry = verdict === "ok" ? { v: "ok" } : { v: "failed", t: now };
  mem.set(domain, entry);
  try {
    const store = loadStore();
    // "ok" persists as the bare legacy string (an older build reads it fine);
    // only failures need the timestamp that makes them expirable.
    store[domain] = entry.v === "ok" ? "ok" : entry;
    localStorage.setItem(LS_KEY, JSON.stringify(store));
  } catch {
    /* ignore persistence failures — the in-memory verdict still holds */
  }
}
