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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![get_settings, set_settings])
        .run(tauri::generate_context!())
        .expect("error while running squelch-desktop");
}
