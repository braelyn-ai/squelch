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

/** Up to two initials from a display name; fallback to the address local-part. */
export function initialsFor(sender: string): string {
  const { name, addr } = parseSender(sender);
  const source = name || addr;
  // Prefer word-based initials from a human name.
  const words = source
    .split(/[\s._-]+/)
    .filter((w) => /[a-z0-9]/i.test(w));
  if (words.length >= 2) {
    return (words[0][0] + words[1][0]).toUpperCase();
  }
  if (words.length === 1 && words[0].length >= 2) {
    return words[0].slice(0, 2).toUpperCase();
  }
  const local = addr.split("@")[0] ?? source;
  return (local[0] ?? "?").toUpperCase();
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
  /^(no-?reply|do-?not-?reply|notifications?|alerts?|updates?|news(letter)?|marketing|mailer|billing|receipts?|orders?|team|hello|info|support|accounts?|security|admin|service|contact|help|feedback|noreply-\S*)$/i;

// Mail-ish subdomain prefixes to peel so notifications.github.com resolves the
// github.com favicon. First label only; naive but sufficient.
const MAIL_SUBDOMAIN =
  /^(mail|email|e|em|mg|mta|smtp|news|info|mailer|marketing|notifications?|alerts?|sfmail|bounce|reply|link|click|go|m)$/i;

/** True if the sender's local-part (pre-"+tag") is a known robot shape. */
export function isRobotSender(sender: string): boolean {
  const { addr } = parseSender(sender);
  const local = addr.split("@")[0] ?? "";
  const base = local.split("+")[0]; // segment before any +tag
  return ROBOT_LOCAL.test(base);
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

/** DuckDuckGo icon service URL for a base domain. */
export function faviconUrl(domain: string): string {
  return `https://icons.duckduckgo.com/ip3/${domain}.ico`;
}

// Per-domain verdict cache: each domain resolves at most once. "ok" = the img
// loaded; "failed" = error / blank / tiny — fall back to initials forever.
type Verdict = "ok" | "failed";
const LS_KEY = "squelch.favicons";
const mem = new Map<string, Verdict>();

function loadStore(): Record<string, Verdict> {
  try {
    return JSON.parse(localStorage.getItem(LS_KEY) || "{}");
  } catch {
    return {};
  }
}

// Warm the in-memory map from localStorage once at module load.
try {
  for (const [d, v] of Object.entries(loadStore())) {
    if (v === "ok" || v === "failed") mem.set(d, v);
  }
} catch {
  /* no localStorage (e.g. SSR/tests) — in-memory only */
}

/** Cached verdict for a domain, or null if not yet resolved. */
export function faviconVerdict(domain: string): Verdict | null {
  return mem.get(domain) ?? null;
}

/** Record a domain verdict in both the in-memory map and localStorage. */
export function setFaviconVerdict(domain: string, verdict: Verdict): void {
  if (mem.get(domain) === verdict) return;
  mem.set(domain, verdict);
  try {
    const store = loadStore();
    store[domain] = verdict;
    localStorage.setItem(LS_KEY, JSON.stringify(store));
  } catch {
    /* ignore persistence failures — the in-memory verdict still holds */
  }
}
