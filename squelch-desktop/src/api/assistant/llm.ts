// Bridge to the Rust `llm_complete` proxy and the assistant-key keyring slots.
//
// The BYOK assistant's key lives ONLY in the OS keyring (set via the shell's
// `set_assistant_key`). It is never read back into JS: this module can ask
// whether a key is present and which provider it routes to, and it can fire a
// completion — but the actual Anthropic/OpenAI HTTP call (and the key) stay
// Rust-side in `llm_complete`. That both keeps the secret out of the JS heap and
// dodges webview CORS.
//
// In a plain browser (dev, no Tauri) there is no proxy, so the assistant is
// unavailable — `assistantKeyStatus` reports absent and calls throw a friendly
// error. The rest of the UI still runs for keyboard/layout testing.

import { invoke } from "@tauri-apps/api/core";

/** True inside the Tauri shell; false in a plain browser (dev). */
function hasTauri(): boolean {
  return (
    typeof window !== "undefined" &&
    (window as unknown as { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__ !== undefined
  );
}

export type AssistantProvider = "anthropic" | "openai";

export interface AssistantKeyStatus {
  present: boolean;
  provider: AssistantProvider | null;
}

/** Error thrown when the assistant is used without the desktop shell present. */
export class AssistantUnavailableError extends Error {
  constructor() {
    super("The inbox assistant needs the desktop app (no proxy in the browser).");
    this.name = "AssistantUnavailableError";
  }
}

/** Whether a key is stored and which provider it routes to. Never the secret. */
export async function assistantKeyStatus(): Promise<AssistantKeyStatus> {
  if (!hasTauri()) return { present: false, provider: null };
  return await invoke<AssistantKeyStatus>("assistant_key_status");
}

/** Store the user's assistant key in the keyring (Settings field). */
export async function setAssistantKey(key: string): Promise<void> {
  if (!hasTauri()) throw new AssistantUnavailableError();
  await invoke("set_assistant_key", { key });
}

/** Forget the stored assistant key. */
export async function clearAssistantKey(): Promise<void> {
  if (!hasTauri()) return;
  await invoke("clear_assistant_key");
}

/** One completion round-trip through the Rust proxy. */
export interface LlmResponse<T = unknown> {
  status: number;
  json: T;
}

/**
 * Fire ONE provider completion. `body` is the full request body minus auth
 * (model / messages / tools / max_tokens / system). The proxy adds the auth
 * header from the keyring and returns the upstream status + parsed JSON.
 */
export async function llmComplete<T = unknown>(
  body: unknown,
): Promise<LlmResponse<T>> {
  if (!hasTauri()) throw new AssistantUnavailableError();
  return await invoke<LlmResponse<T>>("llm_complete", { body });
}
