// Strip tracking pixels from email HTML BEFORE it becomes iframe srcdoc.
//
// TRADE-OFF (product decision, 2026-07): images load by DEFAULT. Real images
// are the whole point of rendering HTML mail; trackers are collateral we remove
// only when we can identify them with confidence. The iframe's CSP is
// host-agnostic (default-src 'none' + img-src http:/https:/data:) so it cannot
// tell a tracker apart from a hero image — this preprocessing pass is the only
// seam where that distinction can be made.
//
// The bias is deliberately asymmetric: a FALSE-KEEP (a tracker we miss) is
// acceptable — referrerPolicy="no-referrer" means no Referer leaks, and the
// frame's links are inert, so the blast radius of a missed pixel is one opaque
// GET with no context. A FALSE-STRIP (nuking a real image) is NOT acceptable —
// it visibly breaks the email. So when in doubt we KEEP the image, and we only
// strip on strong signals: declared pixel-tiny render size, CSS-hidden, or a
// src that matches a known open-tracking endpoint. General heuristics do the
// heavy lifting; the KNOWN_TRACKERS list is intentionally short and honest.

/** Known open-tracking endpoints. Matched (case-insensitive) against the img
 *  src URL. Kept deliberately small — the dimension/hidden heuristics catch the
 *  long tail; this list only covers a few high-volume, unambiguous endpoints. */
const KNOWN_TRACKERS: RegExp[] = [
  /list-manage\.com\/track\/open/i, // mailchimp
  /ct\.sendgrid\.net\/wf\/open/i, // sendgrid
  /email\.mailgun\.net\/o\//i, // mailgun
  /pstmrk\.it/i, // postmark
  /t\.hubspotemail\.net/i, // hubspot
  /track\.hubspot\.com\/__ptq\.gif/i, // hubspot
  /pi\.pardot\.com\/open/i, // pardot
  /track\.customer\.io\/e\/o/i, // customer.io
  /google-analytics\.com\/collect/i, // GA measurement protocol
  /doubleclick\.net/i, // doubleclick
];

/** True only for srcs we're willing to URL-match trackers against: http(s) or
 *  protocol-relative. data:/cid:/blob:/relative srcs are never tracker-matched
 *  (a known endpoint is always a real network host). */
function isNetworkSrc(src: string): boolean {
  return /^(?:https?:)?\/\//i.test(src.trim());
}

function matchesKnownTracker(src: string): boolean {
  if (!isNetworkSrc(src)) return false;
  return KNOWN_TRACKERS.some((re) => re.test(src));
}

/** Parse a declared dimension ("1", "1px", " 2 ") to a number, or null if it
 *  isn't a plain pixel length (%, auto, em, calc(), missing, etc. → not tiny). */
function pxDim(raw: string | null | undefined): number | null {
  if (raw == null) return null;
  const s = raw.trim();
  if (s === "") return null;
  const m = /^(\d+(?:\.\d+)?)(?:px)?$/i.exec(s);
  return m ? parseFloat(m[1]) : null;
}

/** Read a property value out of an inline style string (best-effort). */
function styleProp(style: string, prop: string): string | null {
  const re = new RegExp(`(?:^|;)\\s*${prop}\\s*:\\s*([^;]+)`, "i");
  const m = re.exec(style);
  return m ? m[1].trim() : null;
}

/** A pixel is "tiny" only when BOTH declared width and height are present and
 *  ≤ 2px. DECLARED render size only — attributes first, then inline style. A
 *  1×1-source image stretched to width=600 is a layout element, not a tracker,
 *  so it is NOT tiny (its declared render size is 600). */
function isTinyDeclared(img: Element): boolean {
  const style = img.getAttribute("style") ?? "";
  const w =
    pxDim(img.getAttribute("width")) ?? pxDim(styleProp(style, "width"));
  const h =
    pxDim(img.getAttribute("height")) ?? pxDim(styleProp(style, "height"));
  return w != null && h != null && w <= 2 && h <= 2;
}

/** Inline style hides the element outright. */
function isHidden(img: Element): boolean {
  const style = (img.getAttribute("style") ?? "").toLowerCase();
  const display = styleProp(style, "display");
  const visibility = styleProp(style, "visibility");
  return display === "none" || visibility === "hidden";
}

function shouldStrip(img: Element): boolean {
  const src = img.getAttribute("src") ?? "";
  return isTinyDeclared(img) || isHidden(img) || matchesKnownTracker(src);
}

/**
 * Remove tracking-pixel <img>s from `html`, returning the cleaned html plus a
 * count of how many were stripped. Only <img> elements are ever removed; every
 * other node is left byte-for-byte as parsed. Uses DOMParser (present in the
 * app webview + the happy-dom test env); if DOMParser is unavailable it
 * degrades gracefully — the input is returned unchanged with blocked: 0 (same
 * fallback posture as extractLinks in EmailFrame).
 */
export function stripTrackers(html: string): { html: string; blocked: number } {
  if (typeof DOMParser === "undefined") return { html, blocked: 0 };
  try {
    const doc = new DOMParser().parseFromString(html, "text/html");
    let blocked = 0;
    for (const img of Array.from(doc.querySelectorAll("img"))) {
      if (shouldStrip(img)) {
        img.remove();
        blocked++;
      }
    }
    if (blocked === 0) return { html, blocked: 0 };
    return { html: doc.body.innerHTML, blocked };
  } catch {
    // Parsing blew up — keep the original html untouched (false-keep is safe).
    return { html, blocked: 0 };
  }
}
