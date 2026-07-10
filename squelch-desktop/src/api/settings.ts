// Bridge to the thin Rust shell's two keyring-backed commands. The token lives
// only in the OS keyring at rest; we hold it in memory for the fetch layer.
//
// DEV FALLBACK (permanent affordance): when the app is opened in a plain browser
// (e.g. `bun run dev` in Chrome for keyboard/UI testing) there is no Tauri
// runtime, so `window.__TAURI_INTERNALS__` is undefined and `invoke` throws.
// In that case we transparently fall back to localStorage for {server_url,
// api_token} and log a one-time console.warn. This lets the whole keyboard-first
// UI be exercised in a real browser without the Rust shell. In a packaged Tauri
// build __TAURI_INTERNALS__ is present, so the real keyring path is always used.

import { invoke } from "@tauri-apps/api/core";

export interface Settings {
  server_url: string;
  api_token: string;
}

const LS_KEY = "squelch.settings";

/** True inside the Tauri shell; false in a plain browser (dev). */
function hasTauri(): boolean {
  return (
    typeof window !== "undefined" &&
    // Tauri v2 exposes this internals bag on the window when the shell is present.
    (window as unknown as { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__ !== undefined
  );
}

let warnedOnce = false;
function warnDevFallback(): void {
  if (warnedOnce) return;
  warnedOnce = true;
  console.warn(
    "[squelch] Tauri runtime not detected — using localStorage for settings " +
      "(browser dev fallback). Settings are NOT stored in the OS keyring here.",
  );
}

/** Load stored settings, or null on first run (before Connect). */
export async function getSettings(): Promise<Settings | null> {
  if (!hasTauri()) {
    warnDevFallback();
    try {
      const raw = localStorage.getItem(LS_KEY);
      if (!raw) return null;
      const parsed = JSON.parse(raw) as Partial<Settings>;
      if (parsed && parsed.server_url && parsed.api_token) {
        return { server_url: parsed.server_url, api_token: parsed.api_token };
      }
      return null;
    } catch {
      return null;
    }
  }
  return await invoke<Settings | null>("get_settings");
}

/** Persist settings into the OS keyring via the shell. */
export async function setSettings(settings: Settings): Promise<void> {
  if (!hasTauri()) {
    warnDevFallback();
    localStorage.setItem(LS_KEY, JSON.stringify(settings));
    return;
  }
  await invoke("set_settings", { settings });
}

/**
 * Clear stored settings (Disconnect). In the browser dev fallback we drop the
 * localStorage entry; under Tauri we overwrite the keyring with empties (the
 * shell exposes no delete command, and empty {server_url, api_token} is treated
 * as "no settings" by getSettings on next boot). Best-effort: swallow errors so
 * a disconnect always returns the UI to the Connect gate.
 */
export async function clearSettings(): Promise<void> {
  if (!hasTauri()) {
    warnDevFallback();
    try {
      localStorage.removeItem(LS_KEY);
    } catch {
      // storage unavailable — nothing persisted to clear.
    }
    return;
  }
  try {
    await invoke("set_settings", {
      settings: { server_url: "", api_token: "" },
    });
  } catch {
    // Keyring write failed; the in-memory disconnect still takes effect.
  }
}
