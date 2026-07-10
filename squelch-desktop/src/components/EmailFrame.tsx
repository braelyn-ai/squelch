// Renders ONE message's server-sanitized HTML in a hard-sandboxed iframe.
//
// SECURITY MODEL (locked design):
//   - The HTML was already sanitized server-side (ammonia) at ingest: no
//     <script>, on* handlers, javascript:/data:text URLs, forms, iframes, etc.
//     img src is KEPT deliberately — the CLIENT is the boundary that blocks it
//     from loading, via CSP below.
//   - The iframe is the real jail: sandbox="" (empty) grants NOTHING. In
//     particular NO allow-scripts (so no JS runs even if sanitization missed
//     something) and NO allow-same-origin (opaque origin — the frame cannot
//     touch parent DOM, cookies, or storage). Content is delivered via srcdoc.
//   - A <meta http-equiv="Content-Security-Policy"> is injected as the FIRST
//     child of <head> so it applies before any resource is fetched:
//       default-src 'none'; style-src 'unsafe-inline'
//     Remote content is off by default. When `allowRemote` is true we add
//       img-src http: https: data:
//     for THIS message only — inline style url() fetches would need img-src too
//     (they resolve under img-src per CSP), so the same relaxation covers both.
//     http: is included because a lot of real-world marketing mail references
//     plain http:// image hosts (and protocol-relative //host paths, which under
//     the opaque srcdoc origin resolve against http/https); https:-only silently
//     blocked those, which is what made "load images" appear to do nothing.
//
// LINKS: because the sandbox omits allow-popups AND allow-top-navigation (and
// has no allow-scripts), a link click INSIDE the frame is INERT — it cannot
// navigate the opaque frame, open a window, or postMessage out. We still set
// <base target="_blank"> as belt-and-suspenders.
//
// CHOSEN FIX (2026-07): extract the http(s) hrefs from the sanitized html and
// render them as a compact list BELOW the frame — real <button>s that call
// openExternal() (Tauri opener plugin / window.open fallback). This keeps the
// jail fully intact: we do NOT touch the sandbox or CSP, and we never inject
// script. The hrefs come from the SAME server-sanitized html the frame renders
// (ammonia already stripped javascript:/data: and other dangerous schemes), and
// openExternal itself re-guards to http/https only — so nothing unsafe reaches
// the shell. We de-dupe + cap the list so a marketing mail with 40 tracking
// links doesn't swamp the pane. If the html has no links, nothing renders.
//
// FOCUS/KEYS: an opaque, script-less iframe still steals keyboard focus if it
// is tabbable, and keydowns landing inside it never reach the parent window's
// listener (the keymap in ../keys). We keep the iframe OUT of the tab order and
// non-focusable-by-pointer as much as the platform allows (tabIndex={-1}); the
// message row around it stays the focus/selection target. See ThreadPane for
// the j/k/Esc guarantee — those keys are handled on window and never enter the
// frame because the frame is never focused programmatically.
//
// HEIGHT: no postMessage is possible (opaque origin, no scripts), so the frame
// cannot report its content height. We give it a fixed viewport-relative box
// that grows to a max and scrolls internally. Good enough; documented.

import { useMemo, useState } from "react";
import { openExternal } from "../lib/opener";

/** An extracted, de-duped outbound link: the http(s) href + its visible text. */
interface EmailLink {
  href: string;
  text: string;
}

/** How many links we surface below a frame before collapsing to "+N more". */
const MAX_LINKS = 8;

/**
 * Pull http/https anchor hrefs out of the sanitized html, in document order,
 * de-duped by href. Uses DOMParser (available in the app webview + happy-dom
 * test env) with a regex fallback so it degrades gracefully. The visible link
 * text is captured for a readable label; empty/echo-the-url text falls back to
 * the href host. Only http/https survive — everything else is dropped here and
 * re-guarded in openExternal.
 */
export function extractLinks(html: string): EmailLink[] {
  const out: EmailLink[] = [];
  const seen = new Set<string>();
  const push = (href: string, text: string) => {
    const h = href.trim();
    if (!/^https?:\/\//i.test(h) || seen.has(h)) return;
    seen.add(h);
    const label = text.replace(/\s+/g, " ").trim();
    out.push({ href: h, text: label || hostOf(h) });
  };

  if (typeof DOMParser !== "undefined") {
    try {
      const doc = new DOMParser().parseFromString(html, "text/html");
      for (const a of Array.from(doc.querySelectorAll("a[href]"))) {
        push(a.getAttribute("href") ?? "", a.textContent ?? "");
      }
      return out;
    } catch {
      // fall through to the regex path
    }
  }

  // Fallback: coarse anchor scan (label unavailable → host is used).
  const re = /<a\b[^>]*\bhref\s*=\s*["']([^"']+)["'][^>]*>/gi;
  let m: RegExpExecArray | null;
  while ((m = re.exec(html)) !== null) push(m[1], "");
  return out;
}

/** Best-effort host label for a url, or the raw url if it won't parse. */
function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

/** Cheap scan: does this html reference any remote/network content? */
export function hasRemoteRefs(html: string): boolean {
  // remote <img src="http(s)/protocol-relative"> or any url(...) in styles.
  return (
    /<img\b[^>]*\bsrc\s*=\s*["']?(?:https?:|\/\/)/i.test(html) ||
    /\burl\(\s*["']?(?:https?:|\/\/|data:)/i.test(html) ||
    // data: images are also "remote content" for gating purposes.
    /<img\b[^>]*\bsrc\s*=\s*["']?data:/i.test(html)
  );
}

/** Count remote img refs for the "N blocked" affordance (approximate). */
function countRemoteImgs(html: string): number {
  const m = html.match(
    /<img\b[^>]*\bsrc\s*=\s*["']?(?:https?:|\/\/|data:)/gi,
  );
  return m ? m.length : 0;
}

function buildSrcdoc(html: string, allowRemote: boolean): string {
  const csp = allowRemote
    ? "default-src 'none'; style-src 'unsafe-inline'; img-src http: https: data:"
    : "default-src 'none'; style-src 'unsafe-inline'";
  // The CSP meta MUST be the first thing in <head> so it governs every
  // subsequent resource. <base target="_blank"> keeps any (currently inert)
  // link from trying to navigate the frame itself.
  return (
    "<!doctype html><html><head>" +
    `<meta http-equiv="Content-Security-Policy" content="${csp}">` +
    '<meta charset="utf-8">' +
    '<base target="_blank">' +
    "<style>" +
    "html,body{margin:0;padding:12px;background:#fff;color:#111;" +
    "font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;" +
    "word-break:break-word;overflow-wrap:anywhere;}" +
    "img{max-width:100%;height:auto;}" +
    "a{color:#0b57d0;}" +
    "table{max-width:100%;}" +
    "</style>" +
    "</head><body>" +
    html +
    "</body></html>"
  );
}

export function EmailFrame({
  html,
  selected,
  // Parent-controlled remote-content gate (so the 'i' keybind on the selected
  // message can flip it). Falls back to a local toggle if not provided.
  remoteAllowed,
  onAllowRemote,
}: {
  html: string;
  selected: boolean;
  remoteAllowed?: boolean;
  onAllowRemote?: () => void;
}) {
  const [localAllow, setLocalAllow] = useState(false);
  const allowRemote = remoteAllowed ?? localAllow;

  const remoteRefs = useMemo(() => hasRemoteRefs(html), [html]);
  const blockedCount = useMemo(
    () => (remoteRefs ? countRemoteImgs(html) : 0),
    [html, remoteRefs],
  );
  const srcdoc = useMemo(
    () => buildSrcdoc(html, allowRemote),
    [html, allowRemote],
  );
  // Links can't navigate out of the sandbox (see header) — surface them below
  // the frame as openExternal buttons instead.
  const links = useMemo(() => extractLinks(html), [html]);
  const shownLinks = links.slice(0, MAX_LINKS);
  const extraLinks = links.length - shownLinks.length;

  const allow = () => {
    if (onAllowRemote) onAllowRemote();
    else setLocalAllow(true);
  };

  return (
    <div className="email-frame">
      {remoteRefs &&
        (allowRemote ? (
          <div className="remote-bar loaded" aria-live="polite">
            remote images loaded
          </div>
        ) : (
          <button
            type="button"
            className="remote-bar"
            onClick={allow}
            title={
              selected
                ? "press i to load remote images for this message"
                : "load remote images for this message"
            }
          >
            remote images blocked
            {blockedCount > 0 ? ` (${blockedCount})` : ""} — load
            {selected ? " (i)" : ""}
          </button>
        ))}
      <iframe
        // Force a full remount when the allow state flips so the browser reloads
        // the srcdoc under the new CSP (updating srcDoc in place is enough in
        // practice, but a keyed remount removes any ambiguity about stale frames).
        key={String(allowRemote)}
        // sandbox="" => no capabilities at all (opaque origin, no scripts).
        sandbox=""
        // Kept out of the tab order so it never steals keyboard focus from the
        // parent window keymap (j/k/Esc). See file header.
        tabIndex={-1}
        title="email content"
        className="email-iframe"
        srcDoc={srcdoc}
        // referrerPolicy hardens remote fetches once the user opts in.
        referrerPolicy="no-referrer"
      />
      {shownLinks.length > 0 && (
        <div className="email-links" aria-label="links in this message">
          <span className="email-links-label">links open externally</span>
          {shownLinks.map((l) => (
            <button
              key={l.href}
              type="button"
              className="email-link"
              onClick={() => void openExternal(l.href)}
              title={l.href}
            >
              {l.text}
            </button>
          ))}
          {extraLinks > 0 && (
            <span className="email-links-more">+{extraLinks} more</span>
          )}
        </div>
      )}
    </div>
  );
}
