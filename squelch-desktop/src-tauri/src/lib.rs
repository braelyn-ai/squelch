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

/// Per-image ceiling shared by both image commands.
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// The ONE HTTP client every image fetch goes through.
///
/// This used to be a `Client::builder()` per call, which threw away the
/// connection pool on every image: warming a 40-image newsletter paid 40 TLS
/// handshakes to the same CDN host. Now that the frontend warms whole-inbox
/// image sets (hundreds of fetches, mostly to a handful of hosts), keep-alive
/// reuse is the difference between "instant" and "why is this so slow".
/// Built once, lazily; a build failure is cached as `None` and re-reported.
fn image_client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .ok()
        })
        .as_ref()
        .ok_or_else(|| "client build failed".to_string())
}

/// A validated `image/<subtype>` content type, lowercased with any parameters
/// (`; charset=…`) dropped. Rejects anything that isn't a well-formed image
/// MIME so a hostile `Content-Type` can never shape the `data:` URL we build
/// from it.
fn image_mime(resp: &reqwest::Response) -> Result<String, String> {
    let raw = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = raw.split(';').next().unwrap_or("").trim().to_string();
    let Some(subtype) = mime.strip_prefix("image/") else {
        return Err("not an image".into());
    };
    let ok = !subtype.is_empty()
        && subtype
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'));
    if !ok {
        return Err("not an image".into());
    }
    Ok(mime)
}

/// Shared core of both image commands: fetch one remote image in RUST (no
/// webview, no CORS taint) and return its validated MIME plus raw bytes.
/// Guards: http/https only, 10s timeout, 8MB cap, `image/*` content-type, no
/// credentials/cookies (a bare client has none) and no Referer (reqwest sends
/// none, matching the frame's `referrerPolicy="no-referrer"`). Exposure is
/// equivalent to the <img> fetches the webview already makes for the same
/// mail-derived URLs — this just hands the bytes to same-origin JS instead of
/// painting them. Errors are REDACTED: never echo the mail-derived URL back
/// into JS logs.
async fn fetch_image_parts(url: &str) -> Result<(String, Vec<u8>), String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("only http(s) urls".into());
    }
    let resp = image_client()?
        .get(url)
        .send()
        .await
        .map_err(|_| "image fetch failed".to_string())?;
    if !resp.status().is_success() {
        return Err("image fetch failed".into());
    }
    let mime = image_mime(&resp)?;
    // Cheap pre-check on the declared length, then the real check on the body
    // (a lying/absent Content-Length must not get us past the cap).
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_IMAGE_BYTES {
            return Err("image too large".into());
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|_| "image fetch failed".to_string())?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("image too large".into());
    }
    Ok((mime, bytes.to_vec()))
}

/// Raw base64 bytes of one remote image, for pixel reading (the newsletter-thumb
/// fill color decodes these through a blob). Returned base64 so the IPC payload
/// is one string, not a JSON byte array.
#[tauri::command]
async fn fetch_image(url: String) -> Result<String, String> {
    let (_mime, bytes) = fetch_image_parts(&url).await?;
    Ok(b64_encode(&bytes))
}

/// One remote image as a ready-to-render `data:<mime>;base64,<…>` URL.
///
/// This is what lets the email frame render with ZERO network fetches: the
/// frontend rewrites every `<img src>` / CSS `url()` in a message to the data
/// URL returned here BEFORE the srcdoc is built, so nothing can arrive late and
/// reflow the frame. Same guards as [`fetch_image`] — the MIME is validated
/// (see [`image_mime`]) precisely because it is interpolated into that URL.
#[tauri::command]
async fn fetch_image_data_url(url: String) -> Result<String, String> {
    let (mime, bytes) = fetch_image_parts(&url).await?;
    Ok(format!("data:{mime};base64,{}", b64_encode(&bytes)))
}

/// Minimal dependency-free base64 (standard alphabet, padded).
fn b64_encode(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { A[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Install the native macOS backdrop behind the (transparent) webview.
///
/// This is the layer the CSS glass language sits ON: AppKit renders the blurred,
/// refracted desktop; the page paints translucent cards over it. Without it a
/// transparent window is just a hole — every "frosted" surface in the CSS would
/// have nothing to frost.
///
/// Two tiers, best-first:
///   1. `NSGlassEffectView` (macOS 26 Tahoe+) — the real Liquid Glass material,
///      with its specular edge treatment and live refraction.
///   2. `NSVisualEffectView` / `UnderWindowBackground` — the pre-Tahoe frosted
///      material. Visually flatter, still a legible translucent backdrop.
///
/// Both are decoration. If AppKit refuses (unsupported version, or the private
/// transparency path is off), we log and carry on: `--canvas-fallback` in
/// global.css paints an opaque surface so the app is never an unreadable
/// see-through window.
#[cfg(target_os = "macos")]
fn apply_native_backdrop(window: &tauri::WebviewWindow) {
    use window_vibrancy::{
        apply_liquid_glass, apply_vibrancy, LiquidGlassOptions, NSGlassEffectViewStyle,
        NSVisualEffectMaterial, NSVisualEffectState,
    };

    // radius 0 / content_view None => the glass view is inserted BELOW the
    // webview as a full-bleed background subview. It never reparents our
    // content, so a failure here cannot break the app's view hierarchy.
    match apply_liquid_glass(
        window,
        LiquidGlassOptions::new(NSGlassEffectViewStyle::Regular),
    ) {
        Ok(()) => return,
        Err(e) => eprintln!("liquid glass unavailable ({e}); falling back to vibrancy"),
    }

    if let Err(e) = apply_vibrancy(
        window,
        NSVisualEffectMaterial::UnderWindowBackground,
        // Stay frosted when the window loses focus — the sitrep is a glanceable
        // surface, so it has to stay legible while it sits in the background.
        Some(NSVisualEffectState::Active),
        None,
    ) {
        // Nothing is rendering behind the transparent webview, so the window
        // would be a literal hole punched through to the desktop. Repaint it
        // opaque: no glass, but a usable app. The CSS needs no fallback branch
        // because it only ever paints translucent tints ON TOP of this.
        eprintln!("window vibrancy unavailable ({e}); falling back to an opaque window");
        let _ = window.set_background_color(Some(tauri::window::Color(238, 244, 251, 255)));
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        // Attachment download: save dialog + fs write to the user-chosen path.
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|_app| {
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;
                if let Some(window) = _app.get_webview_window("main") {
                    apply_native_backdrop(&window);
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            assistant_key_status,
            set_assistant_key,
            clear_assistant_key,
            llm_complete,
            fetch_image,
            fetch_image_data_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running squelch-desktop");
}
