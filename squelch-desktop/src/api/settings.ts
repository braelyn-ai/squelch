// Bridge to the thin Rust shell's two keyring-backed commands. The token lives
// only in the OS keyring at rest; we hold it in memory for the fetch layer.

import { invoke } from "@tauri-apps/api/core";

export interface Settings {
  server_url: string;
  api_token: string;
}

/** Load stored settings, or null on first run (before Connect). */
export async function getSettings(): Promise<Settings | null> {
  return await invoke<Settings | null>("get_settings");
}

/** Persist settings into the OS keyring via the shell. */
export async function setSettings(settings: Settings): Promise<void> {
  await invoke("set_settings", { settings });
}
