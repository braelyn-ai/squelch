// Renders ONE message's server-sanitized HTML in a hard-sandboxed iframe.
//
// SECURITY MODEL:
//   - The HTML was already sanitized server-side (ammonia) at ingest: no
//     <script>, on* handlers, javascript:/data:text URLs, forms, iframes, etc.
//   - sandbox="allow-same-origin" and NOTHING else. Crucially allow-scripts is
//     ABSENT, and that is the property that matters: with it absent the browser
//     refuses to EXECUTE script in the frame no matter what the document
//     contains or what origin it has. Three independent layers keep the frame
//     script-free: (1) the sandbox execution block, (2) ammonia stripped every
//     script vector server-side, (3) the frame CSP is default-src 'none' with
//     no script-src. Content is delivered via srcdoc.
//   - WHAT WAS TRADED (owner decision, 2026-07): the frame used to be
//     sandbox="" — an OPAQUE origin, so even our own parent code could not see
//     inside it. That opacity was belt-and-suspenders, not a load-bearing
//     layer, and it made the frame unmeasurable: it forced fixed-height boxes
//     with internal scrollbars. allow-same-origin makes the srcdoc document
//     same-origin with the app so the PARENT can read contentDocument and size
//     each frame to its exact content height — which is what enables the
//     stacked, single-scroll thread view. The frame content still cannot act
//     on that shared origin itself, because nothing inside it can ever run.
//   - A <meta http-equiv="Content-Security-Policy"> is injected as the FIRST
//     child of <head> so it applies before any resource is fetched:
//       default-src 'none'; style-src 'unsafe-inline'; img-src http: https: data:
//     REMOTE IMAGES LOAD BY DEFAULT (product decision, 2026-07): real images are
//     the point of HTML mail, so img-src permits http:/https:/data: unless the
//     Settings "load on demand" pref is on — then network images are cut from
//     the frame CSP until the reader opts in per message (the remote-bar).
//     NOTE: srcdoc frames INHERIT the parent app CSP, so tauri.conf.json's
//     img-src must (and does) permit http:/https: for any of this to work —
//     the frame's own CSP line is the per-message gate.
//     Inline style url() fetches resolve under img-src per CSP, so the same
//     grant covers both. http: is included because a lot of real-world marketing
//     mail references plain http:// image hosts (and protocol-relative //host
//     paths, which under the srcdoc origin resolve against http/https).
//   - CSP is host-agnostic — it can't tell a tracking pixel from a hero image.
//     So trackers are stripped in a PREPROCESSING pass (src/lib/trackers) BEFORE
//     the srcdoc is built, and referrerPolicy="no-referrer" (below) suppresses
//     the Referer on every remote fetch. The bias is deliberately toward keeping
//     images: a missed tracker is a bounded, referrer-less GET; a wrongly-nuked
//     image visibly breaks the mail. When blocked > 0 we show a passive chip.
//
// LINKS: because the sandbox omits allow-popups AND allow-top-navigation (and
// has no allow-scripts), a link click INSIDE the frame is INERT — it cannot
// navigate the top page or open a window. We still set <base target="_blank">
// as belt-and-suspenders (it turns any in-frame navigation attempt into a
// popup attempt, which the sandbox denies).
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
// FOCUS/KEYS: a script-less iframe still steals keyboard focus if it is
// tabbable, and keydowns landing inside it never reach the parent window's
// listener (the keymap in ../keys). We keep the iframe OUT of the tab order and
// non-focusable-by-pointer as much as the platform allows (tabIndex={-1}); the
// message card around it stays the focus/selection target. See ThreadViewer for
// the j/k/Esc guarantee — those keys are handled on window and never enter the
// frame because the frame is never focused programmatically.
//
// HEIGHT: allow-same-origin (above) lets the parent read contentDocument, so
// on the iframe's load event we measure documentElement.scrollHeight (body as
// fallback) and inline-set it as the frame's height — the frame is always
// EXACTLY content-height and NEVER scrolls itself (scrolling="no" + CSS
// overflow: hidden hide any mid-measure scrollbar). Scripts can't run inside,
// so the only thing that changes content height after load is images arriving:
// we attach load/error listeners to every <img> in the contentDocument and
// re-measure on each, plus one requestAnimationFrame settle pass after load.
// The stacked column in ThreadViewer is then the single scroll surface. If
// contentDocument is somehow unreadable we fall back to a fixed 65vh scrolling
// box so the mail is still readable.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { openExternal } from "../lib/opener";
import { usePref } from "../lib/prefs";
import { stripTrackers } from "../lib/trackers";
import { findQuoteNodes } from "../lib/quotes";
import { getFrameHeight, setFrameHeight } from "../lib/frameHeights";

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

/** True when the sanitized html references at least one network image. */
function hasRemoteImages(html: string): boolean {
  return /<img\b[^>]*\bsrc\s*=\s*["'](?:https?:)?\/\//i.test(html);
}

function buildSrcdoc(html: string, allowRemote: boolean): string {
  // The frame's meta CSP is the per-message image gate. NOTE the parent app
  // CSP is inherited by srcdoc frames, so the effective policy is the
  // intersection — the app CSP (tauri.conf.json) permits http:/https: images
  // precisely so this per-frame line is the deciding one. When remote is off
  // (Settings → load on demand, before the per-email opt-in) only embedded
  // data: images render. Trackers are stripped upstream, not here.
  const csp = allowRemote
    ? "default-src 'none'; style-src 'unsafe-inline'; img-src http: https: data:"
    : "default-src 'none'; style-src 'unsafe-inline'; img-src data:";
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

/** Frame height (px) shown before the first successful measurement. */
const PLACEHOLDER_HEIGHT = 120;

export function EmailFrame({
  html,
  cacheKey,
}: {
  html: string;
  /** Stable id (message id) for the height memory — a frame with a remembered
   *  height renders at its final size instantly, zero resize on reopen. */
  cacheKey?: string;
}) {
  // Strip tracking pixels before render; keep the count for the passive chip.
  const { html: cleaned, blocked } = useMemo(() => stripTrackers(html), [html]);

  // Remote images: the Settings pref is the default; when it's off, a
  // per-message opt-in flips this frame only.
  const autoImages = usePref("loadRemoteImages");
  const [optedIn, setOptedIn] = useState(false);
  const allowRemote = autoImages || optedIn;
  const remoteCandidates = useMemo(() => hasRemoteImages(cleaned), [cleaned]);

  const srcdoc = useMemo(
    () => buildSrcdoc(cleaned, allowRemote),
    [cleaned, allowRemote],
  );
  // Links can't navigate out of the sandbox (see header) — surface them below
  // the frame as openExternal buttons instead. Extract from the cleaned html so
  // stripped trackers don't contribute (they're <img>, not <a>, but stay tidy).
  const links = useMemo(() => extractLinks(cleaned), [cleaned]);
  const shownLinks = links.slice(0, MAX_LINKS);
  const extraLinks = links.length - shownLinks.length;

  // ---- auto-sizing (see HEIGHT in the file header) ----
  const frameRef = useRef<HTMLIFrameElement>(null);
  // Seed from the height memory: a previously-measured message renders at its
  // final size on the FIRST paint. null => never measured (paint-held below).
  const [height, setHeight] = useState<number | null>(() =>
    cacheKey ? getFrameHeight(cacheKey) : null,
  );
  // contentDocument unreadable => fixed 65vh scrolling fallback.
  const [measureFailed, setMeasureFailed] = useState(false);
  // Teardown for the listeners of the CURRENT load; replaced on reload,
  // invoked on unmount.
  const teardownRef = useRef<(() => void) | null>(null);

  // ---- quoted-history collapse ----
  // The frame can never run script (sandbox), so the PARENT hides/shows the
  // quoted-history nodes directly in contentDocument (same-origin, see header)
  // and re-measures. Collapsed by default; a chip below the frame toggles it.
  const quoteNodesRef = useRef<HTMLElement[]>([]);
  const [hasQuoted, setHasQuoted] = useState(false);
  const [quotedHidden, setQuotedHidden] = useState(true);
  // The height memory stores the DEFAULT (collapsed) state only; this ref lets
  // measure() know the live state without going stale in the callback.
  const quotedHiddenRef = useRef(true);
  quotedHiddenRef.current = quotedHidden;

  const measure = useCallback(() => {
    const frame = frameRef.current;
    if (!frame) return;
    try {
      const doc = frame.contentDocument;
      if (!doc) {
        setMeasureFailed(true);
        return;
      }
      const h =
        doc.documentElement?.scrollHeight || doc.body?.scrollHeight || 0;
      if (h > 0) {
        setHeight(h);
        if (cacheKey && quotedHiddenRef.current) setFrameHeight(cacheKey, h);
      }
    } catch {
      setMeasureFailed(true);
    }
  }, [cacheKey]);

  const onFrameLoad = useCallback(() => {
    // A reload replaces the contentDocument — drop listeners bound to the
    // previous one before wiring the new one.
    teardownRef.current?.();
    teardownRef.current = null;

    let doc: Document | null = null;
    try {
      doc = frameRef.current?.contentDocument ?? null;
    } catch {
      doc = null;
    }
    if (!doc) {
      setMeasureFailed(true);
      return;
    }

    // Hide the quoted history BEFORE the first measure so the frame sizes to
    // the collapsed content and never flashes the full chain.
    const quotes = findQuoteNodes(doc);
    quoteNodesRef.current = quotes;
    if (quotes.length > 0) {
      for (const n of quotes) n.style.display = "none";
      setHasQuoted(true);
      setQuotedHidden(true);
    }

    measure();
    // Images arriving are the ONLY post-load height change (no scripts can
    // run inside) — re-measure as they load, BATCHED to one measure per frame
    // (a burst of cached images landing together must cause at most one
    // layout pass, not one per image).
    let imgRaf = 0;
    const onImg = () => {
      if (imgRaf) return;
      imgRaf = requestAnimationFrame(() => {
        imgRaf = 0;
        measure();
      });
    };
    const imgs = Array.from(doc.querySelectorAll("img"));
    for (const img of imgs) {
      // Async decode keeps a large image's decode off the main thread so the
      // (single) height adjustment doesn't stutter.
      img.decoding = "async";
      img.addEventListener("load", onImg);
      img.addEventListener("error", onImg);
    }
    // One settle pass after layout (fonts/first paint can nudge the height).
    const raf = requestAnimationFrame(measure);

    teardownRef.current = () => {
      cancelAnimationFrame(raf);
      if (imgRaf) cancelAnimationFrame(imgRaf);
      for (const img of imgs) {
        img.removeEventListener("load", onImg);
        img.removeEventListener("error", onImg);
      }
    };
  }, [measure]);

  // Unmount: release whatever the last load wired up.
  useEffect(() => () => teardownRef.current?.(), []);

  // DOM writes + re-measure live OUTSIDE the state updater: updaters can be
  // double-invoked (StrictMode), and a double display-flip would desync the
  // frame from the chip label.
  const toggleQuoted = useCallback(() => {
    const next = !quotedHidden;
    for (const n of quoteNodesRef.current) {
      n.style.display = next ? "none" : "";
    }
    setQuotedHidden(next);
    measure();
  }, [quotedHidden, measure]);

  return (
    <div className="email-frame">
      {!allowRemote && remoteCandidates ? (
        <button
          type="button"
          className="remote-bar"
          onClick={() => setOptedIn(true)}
          title="remote images are off by default (Settings → Mail); load them for this email only"
        >
          remote images blocked — load for this email
        </button>
      ) : blocked > 0 ? (
        <div
          className="remote-bar loaded"
          aria-live="polite"
          title="tracking pixels removed before render; links stay inert and no referrer is ever sent"
        >
          {blocked} tracker{blocked === 1 ? "" : "s"} blocked
        </div>
      ) : null}
      <iframe
        ref={frameRef}
        // allow-same-origin ONLY — allow-scripts stays absent, so nothing can
        // ever execute inside; same-origin is what lets us measure. See header.
        sandbox="allow-same-origin"
        // Kept out of the tab order so it never steals keyboard focus from the
        // parent window keymap (j/k/Esc). See file header.
        tabIndex={-1}
        title="email content"
        className="email-iframe"
        srcDoc={srcdoc}
        onLoad={onFrameLoad}
        // The frame never scrolls itself: it is sized to its exact content.
        // (In the measure-failed fallback we re-enable scrolling so a long
        // mail is still fully readable inside the fixed 65vh box.)
        scrolling={measureFailed ? "yes" : "no"}
        // PAINT-HOLD: an unmeasured frame keeps its placeholder space but stays
        // invisible until the first measurement lands (same tick as load), so
        // the reader never sees a half-laid-out document snap to size. A frame
        // with a remembered height paints immediately at its final size.
        style={
          measureFailed
            ? { height: "65vh", overflowY: "auto" }
            : {
                height: `${height ?? PLACEHOLDER_HEIGHT}px`,
                visibility: height === null ? "hidden" : "visible",
              }
        }
        // referrerPolicy suppresses the Referer on every remote image fetch, so
        // hosts learn nothing about which mail (or reader) requested the image.
        referrerPolicy="no-referrer"
      />
      {hasQuoted && (
        <button
          type="button"
          className="quote-toggle"
          onClick={toggleQuoted}
          title="the quoted reply chain below this message"
        >
          {quotedHidden ? "··· show quoted history" : "hide quoted history"}
        </button>
      )}
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
