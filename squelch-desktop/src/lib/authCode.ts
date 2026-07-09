// Client-side 2FA code extraction. The "present, don't read" flow: when a login
// code arrives we auto-reveal the body (server-side audited) and pull the code
// out here so the human never has to open the email — the code just appears.
//
// Pure + DOM-free so it can be unit-tested directly (see authCode.test.ts).
//
// Strategy (in order of confidence):
//   1. Prefer a 4-8 digit run that sits NEAR a code word (code / verification /
//      OTP / one-time / pin / passcode), within ±80 chars — this is almost
//      always the real code and avoids grabbing years, phone fragments, etc.
//   2. Fallback: the longest standalone 4-8 digit run in the body.
// Returns null when nothing plausible is found (caller shows an "Open Auth"
// affordance instead of a code).

/** Kinds that warrant the code modal. Others (resets/alerts) get the ring only. */
export const CODE_KINDS = new Set(["otp", "login_code", "verification"]);

/** True if this sealed kind should pop the code modal (vs. ring-only). */
export function isCodeKind(kind: string | null | undefined): boolean {
  return kind != null && CODE_KINDS.has(kind);
}

// Words that tend to sit right next to the actual code.
const CODE_WORDS =
  /\b(?:one[-\s]?time|verification|verify|passcode|pass[-\s]?code|security\s+code|access\s+code|auth(?:entication)?\s+code|login\s+code|sign[-\s]?in\s+code|confirmation\s+code|OTP|PIN|code)\b/gi;

/** Char distance from `index` to the nearest code-word match, or Infinity. */
function nearestCodeWord(text: string, index: number): number {
  let best = Infinity;
  CODE_WORDS.lastIndex = 0;
  for (const m of text.matchAll(CODE_WORDS)) {
    const wi = m.index ?? 0;
    const d = wi <= index ? index - (wi + m[0].length) : wi - index;
    best = Math.min(best, Math.max(0, d));
  }
  return best;
}

// A standalone 4-8 digit run. Allows an optional single space/hyphen split in
// the middle of longer codes (e.g. "123 456" / "123-456") which some providers
// format for readability; we strip the separator when we return it.
const DIGIT_RUN = /(?<![\w-])(\d{4,8}|\d{3}[\s-]\d{3}|\d{2}[\s-]\d{2}[\s-]\d{2})(?![\w-])/g;

/** Normalize a matched run to bare digits, or null if it isn't 4-8 digits. */
function cleanRun(raw: string): string | null {
  const digits = raw.replace(/[\s-]/g, "");
  return digits.length >= 4 && digits.length <= 8 ? digits : null;
}

/**
 * Extract the most likely login code from a revealed body. See module header.
 * @returns the bare-digit code, or null if none is confidently found.
 */
export function extractCode(body: string | null | undefined): string | null {
  if (!body) return null;
  const text = body.slice(0, 4000); // codes live near the top; bound the work

  // Collect every candidate run with its index.
  const runs: { code: string; index: number }[] = [];
  for (const m of text.matchAll(DIGIT_RUN)) {
    const code = cleanRun(m[0]);
    if (code) runs.push({ code, index: m.index ?? 0 });
  }
  if (runs.length === 0) return null;

  // (1) Prefer runs within ±80 chars of a code word, ranked by how CLOSE they
  // sit to one — proximity beats length so "login code is 55231" wins over a
  // longer order number elsewhere in the same neighborhood.
  const near = runs
    .map((r) => ({ ...r, dist: nearestCodeWord(text, r.index) }))
    .filter((r) => r.dist <= 80);
  if (near.length > 0) {
    near.sort((a, b) => a.dist - b.dist || b.code.length - a.code.length);
    return near[0].code;
  }

  // (2) No code word anywhere: fall back to the longest standalone run, earliest
  // position breaking ties (codes tend to lead the body).
  runs.sort((a, b) => b.code.length - a.code.length || a.index - b.index);
  return runs[0].code;
}
