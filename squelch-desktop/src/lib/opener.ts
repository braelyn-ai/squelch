// Open an external URL in the user's real system browser.
//
// TWO RUNTIMES:
//   - Inside Tauri (the shipped desktop app) we route through the
//     @tauri-apps/plugin-opener `openUrl`, gated by the `opener:allow-open-url`
//     capability in src-tauri/capabilities/default.json. The webview's own
//     navigation is locked down by CSP, so this is the ONLY sanctioned way out.
//   - In browser-dev (vite on :1420, no Tauri) we fall back to window.open with
//     noopener/noreferrer so links still work while iterating.
//
// SECURITY: only http/https URLs are ever opened. Anything else (mailto:, tel:,
// javascript:, data:, file:, custom schemes) is ignored — we never hand an
// arbitrary scheme to the OS shell.

/** Detect the Tauri runtime without importing the API (safe in plain browser). */
function inTauri(): boolean {
  return (
    typeof window !== "undefined" &&
    "__TAURI_INTERNALS__" in (window as unknown as Record<string, unknown>)
  );
}

/** True only for http:/https: — the sole schemes we hand to the shell. */
function isHttpUrl(url: string): boolean {
  try {
    const p = new URL(url).protocol;
    return p === "http:" || p === "https:";
  } catch {
    return false;
  }
}

/**
 * The URL's HOST only — never the path/query. Unsubscribe links are mail-derived
 * and routinely carry per-recipient tokens in the path/query, so a failure log
 * must never echo the full URL. Returns "?" if it won't parse.
 */
function safeHost(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return "?";
  }
}

/**
 * Open `url` externally. No-op (resolves) for non-http(s) URLs so callers can
 * pass a possibly-null/odd tracking_url through a guard upstream and still be
 * safe here. Never throws for a bad scheme.
 */
export async function openExternal(url: string): Promise<void> {
  if (!url || !isHttpUrl(url)) return;

  if (inTauri()) {
    try {
      const { openUrl } = await import("@tauri-apps/plugin-opener");
      await openUrl(url);
    } catch (err) {
      // Never swallow silently: a rejected openUrl (e.g. a ForbiddenUrl from a
      // missing/mis-scoped `opener:allow-open-url` capability) looks like a
      // dead button to the user. Surface it so it's diagnosable — but log a
      // STATIC message + the error + at most the host, never the full (mail-
      // derived, token-bearing) URL.
      console.error(
        `openExternal: failed to open external URL (host: ${safeHost(url)})`,
        err,
      );
    }
    return;
  }

  window.open(url, "_blank", "noopener,noreferrer");
}
