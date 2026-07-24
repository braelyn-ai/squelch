//! Thin Tauri shell for squelch-desktop.
//!
//! This crate holds NO product intelligence. Its entire job is:
//!   1. open the window (config in `tauri.conf.json`), and
//!   2. store the human-door connection settings (server URL + API token)
//!      securely in the OS keyring, exposed as exactly two commands.
//!
//! SECURITY: the API token is written ONLY to the OS keyring. It is never
//! written to disk by us, never placed in a log line, and never returned in an
//! error message. `Settings` derives `Serialize` but we deliberately do not
//! `Debug`-print or log it anywhere.

use keyring::Entry;
use serde::{Deserialize, Serialize};

/// Keyring service name shared by both stored fields (per task spec).
const KEYRING_SERVICE: &str = "squelch-desktop";
/// Keyring "account" (username) slots within the service.
const ACCOUNT_URL: &str = "server_url";
const ACCOUNT_TOKEN: &str = "api_token";
/// BYOK assistant key slot. This is the user's OWN Anthropic/OpenAI key for the
/// embedded "ask your inbox" assistant — entirely separate from the human-door
/// `api_token` above. It is written here once, read ONLY by `llm_complete`, and
/// NEVER returned to JS: the frontend can ask whether one is present (and which
/// provider) via `assistant_key_status`, but the secret itself stays Rust-side.
const ACCOUNT_ASSISTANT_KEY: &str = "assistant_api_key";

/// The connection settings the frontend needs to talk to the human door.
/// `api_token` is sensitive and lives only in the keyring at rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub server_url: String,
    pub api_token: String,
}

fn entry(account: &str) -> Result<Entry, String> {
    Entry::new(KEYRING_SERVICE, account).map_err(|e| format!("keyring init failed: {e}"))
}

/// Read a single keyring slot. A missing entry is `Ok(None)` (first run), any
/// other keyring failure is an error. Never logs the value.
fn read_slot(account: &str) -> Result<Option<String>, String> {
    match entry(account)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keyring read failed: {e}")),
    }
}

/// Load stored settings. Returns `None` until BOTH fields have been saved
/// (first-run Connect screen relies on this to decide whether to prompt).
#[tauri::command]
fn get_settings() -> Result<Option<Settings>, String> {
    let server_url = read_slot(ACCOUNT_URL)?;
    let api_token = read_slot(ACCOUNT_TOKEN)?;
    match (server_url, api_token) {
        (Some(server_url), Some(api_token)) => Ok(Some(Settings {
            server_url,
            api_token,
        })),
        _ => Ok(None),
    }
}

/// Persist settings into the OS keyring. The token never touches disk or logs.
#[tauri::command]
fn set_settings(settings: Settings) -> Result<(), String> {
    entry(ACCOUNT_URL)?
        .set_password(&settings.server_url)
        .map_err(|e| format!("keyring write failed: {e}"))?;
    entry(ACCOUNT_TOKEN)?
        .set_password(&settings.api_token)
        .map_err(|e| format!("keyring write failed: {e}"))?;
    Ok(())
}

// --- BYOK assistant proxy ---------------------------------------------------
//
// The embedded assistant is strictly bring-your-own-key. The key is stored in
// the keyring (`set_assistant_key`) and read back ONLY inside `llm_complete`,
// which makes the actual Anthropic/OpenAI call. This exists for two reasons:
//   1. the webview can't call the LLM APIs directly (CORS), and
//   2. the key must never live in JS — not in a fetch header, not in a log.
// Provider is inferred from the key prefix (`sk-ant-` => Anthropic, else
// OpenAI), matching the server-side Stage-2 routing.

/// Provider inferred from an assistant key prefix. Never exposes the key value.
fn provider_for_key(key: &str) -> &'static str {
    if key.starts_with("sk-ant-") {
        "anthropic"
    } else {
        "openai"
    }
}

/// Whether an assistant key is stored, and (if so) which provider it routes to.
/// Lets the Settings UI show "key set — Anthropic" without ever handling the
/// secret. A stored-but-empty slot (our "cleared" sentinel) reads as absent.
#[derive(Debug, Clone, Serialize)]
pub struct AssistantKeyStatus {
    present: bool,
    provider: Option<String>,
}

#[tauri::command]
fn assistant_key_status() -> Result<AssistantKeyStatus, String> {
    match read_slot(ACCOUNT_ASSISTANT_KEY)? {
        Some(k) if !k.is_empty() => Ok(AssistantKeyStatus {
            present: true,
            provider: Some(provider_for_key(&k).to_string()),
        }),
        _ => Ok(AssistantKeyStatus {
            present: false,
            provider: None,
        }),
    }
}

/// Store the user's assistant key in the keyring. Never logged, never echoed.
#[tauri::command]
fn set_assistant_key(key: String) -> Result<(), String> {
    entry(ACCOUNT_ASSISTANT_KEY)?
        .set_password(&key)
        .map_err(|e| format!("keyring write failed: {e}"))
}

/// Forget the stored assistant key. There is no keyring delete in this shell, so
/// we overwrite with empty (treated as absent by `assistant_key_status`).
#[tauri::command]
fn clear_assistant_key() -> Result<(), String> {
    entry(ACCOUNT_ASSISTANT_KEY)?
        .set_password("")
        .map_err(|e| format!("keyring write failed: {e}"))
}

/// Result of one LLM round-trip: the upstream HTTP status plus the parsed JSON
/// body (an Anthropic message, an OpenAI completion, or a provider error object
/// — the caller inspects `status` and shapes accordingly).
#[derive(Debug, Serialize)]
pub struct LlmResponse {
    status: u16,
    json: serde_json::Value,
}

/// Make ONE assistant completion call. `body` is a fully-formed provider request
/// body MINUS auth (model, messages, tools, max_tokens, …) shaped by the JS
/// agent loop. We read the key from the keyring, route by its real prefix (never
/// trusting a caller-supplied provider), inject the auth header, POST, and hand
/// back the raw response. The key never leaves this function.
#[tauri::command]
async fn llm_complete(body: serde_json::Value) -> Result<LlmResponse, String> {
    let key = read_slot(ACCOUNT_ASSISTANT_KEY)?
        .filter(|k| !k.is_empty())
        .ok_or_else(|| "no assistant key set".to_string())?;

    let client = reqwest::Client::new();
    let req = match provider_for_key(&key) {
        "anthropic" => client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01")
            .json(&body),
        // OpenAI (and any non-Anthropic key): chat completions endpoint.
        _ => client
            .post("https://api.openai.com/v1/chat/completions")
            .header("authorization", format!("Bearer {key}"))
            .json(&body),
    };

    // Deliberately generic error strings: an upstream/transport failure must
    // never surface anything that could include the key (it lives in a header,
    // not the URL, but we stay conservative and never format the raw error).
    let resp = req
        .send()
        .await
        .map_err(|_| "assistant request failed (network/tls)".to_string())?;
    let status = resp.status().as_u16();
    let json = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|_| "assistant returned a non-JSON body".to_string())?;
    Ok(LlmResponse { status, json })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        // Attachment download: save dialog + fs write to the user-chosen path.
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            assistant_key_status,
            set_assistant_key,
            clear_assistant_key,
            llm_complete
        ])
        .run(tauri::generate_context!())
        .expect("error while running squelch-desktop");
}
