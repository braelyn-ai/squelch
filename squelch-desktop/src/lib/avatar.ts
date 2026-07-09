// Sender avatars — deterministic, LOCAL-ONLY. No network avatar/asset service is
// ever contacted (no Gravatar, no favicon fetch): the correspondent graph is
// private and must never leak off-device. Initials come from the display name
// (fallback: first letter of the address local-part); the background color is a
// stable hash of the address over a small palette tuned to read on both themes.

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
