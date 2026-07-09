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
//       img-src https: data:
//     for THIS message only — inline style url() fetches would need img-src too
//     (they resolve under img-src per CSP), so the same relaxation covers both.
//
// LINKS: no opener/shell plugin is wired in src-tauri (checked: only the two
// keyring commands + core window perms). Because the sandbox omits
// allow-popups AND allow-top-navigation, a link click is INERT — it cannot
// navigate the opaque frame or open a window. We still set <base target="_blank">
// so that IF allow-popups is ever added, links open in a new context rather
// than replacing the frame. The visible href is preserved for hover/status.
// TODO(v2): wire @tauri-apps/plugin-opener + a capability, then intercept
// clicks to open in the system browser. Not possible today without allow-scripts.
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
    ? "default-src 'none'; style-src 'unsafe-inline'; img-src https: data:"
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

  const allow = () => {
    if (onAllowRemote) onAllowRemote();
    else setLocalAllow(true);
  };

  return (
    <div className="email-frame">
      {remoteRefs && !allowRemote && (
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
      )}
      <iframe
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
    </div>
  );
}
