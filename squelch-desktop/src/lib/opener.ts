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
 * Open `url` externally. No-op (resolves) for non-http(s) URLs so callers can
 * pass a possibly-null/odd tracking_url through a guard upstream and still be
 * safe here. Never throws for a bad scheme.
 */
export async function openExternal(url: string): Promise<void> {
  if (!url || !isHttpUrl(url)) return;

  if (inTauri()) {
    const { openUrl } = await import("@tauri-apps/plugin-opener");
    await openUrl(url);
    return;
  }

  window.open(url, "_blank", "noopener,noreferrer");
}
