//! Configuration. Everything tunable lives here, loaded from
//! `~/.config/squelch/config.toml` with env-var overrides. Nothing magic is
//! hardcoded: the Stage-1 triage importance ladder, thresholds, and paths are
//! all fields on [`Config`] with sane defaults so a missing config file still
//! yields a working system.

use crate::error::CoreError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The READ scope. This is all the sync daemon + triage ever request; the read
/// credential is `gmail.readonly` and nothing else. Hard invariant, hence a
/// `const`. See [`WRITE_SCOPES`] for the separate, opt-in action credential.
pub const GMAIL_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";

/// The WRITE scopes, requested ONLY by `squelchd auth --write` and loaded ONLY
/// by human-door action endpoints — never by sync/triage. `gmail.modify` covers
/// label/read-state/archive mutations; `gmail.send` covers sending. Kept as a
/// distinct grep-obvious constant from [`GMAIL_READONLY_SCOPE`] so the two
/// credentials can never be conflated.
pub const GMAIL_MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
pub const GMAIL_SEND_SCOPE: &str = "https://www.googleapis.com/auth/gmail.send";

/// Convenience: the full set of scopes for the write credential.
pub const WRITE_SCOPES: &[&str] = &[GMAIL_MODIFY_SCOPE, GMAIL_SEND_SCOPE];

/// Which backend persists OAuth tokens.
///
/// `Keyring` uses the OS secret service (macOS Keychain, Linux Secret Service).
/// `File` writes a mode-0600 JSON file — the only viable option on a headless
/// Linux box with no desktop keyring. Default is [`CredentialBackend::default`]:
/// keyring on macOS, file on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialBackend {
    Keyring,
    File,
}

impl Default for CredentialBackend {
    fn default() -> Self {
        // Headless Linux typically has no Secret Service; default to a file.
        // macOS always has Keychain.
        if cfg!(target_os = "macos") {
            CredentialBackend::Keyring
        } else {
            CredentialBackend::File
        }
    }
}

impl CredentialBackend {
    /// Parse from the `credential_backend` config / `SQUELCH_CRED_BACKEND` env
    /// string. Case-insensitive. Unknown values fall back to the platform
    /// default.
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "keyring" => Some(CredentialBackend::Keyring),
            "file" => Some(CredentialBackend::File),
            _ => None,
        }
    }
}

/// Sync-related tunables. Real config, not constants, so the sync engine can
/// wire them in without a schema change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// How many days of history to backfill on the initial sync.
    pub backfill_days: u32,
    /// How often (seconds) the incremental poll loop wakes to call
    /// `history.list`. A poll batch IS the coalesced batch — polling replaces
    /// the old IDLE wake-coalescing entirely.
    pub poll_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            backfill_days: 30,
            poll_secs: 45,
        }
    }
}

/// Resolved (present) OAuth client credentials.
#[derive(Debug, Clone)]
pub struct OAuthClientConfig {
    pub client_id: String,
    pub client_secret: String,
}

/// Importance scores (0-100) assigned by the Stage-1 rules engine per rung.
/// Tunable so operators can bias what surfaces without recompiling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Stage1Config {
    /// Bills/past-due always surface via their tier; this is the raw score.
    pub bill_importance: u8,
    /// A message matched a `Surface` sender rule.
    pub rule_surface_importance: u8,
    /// A message matched a `Squelch` sender rule.
    pub rule_squelch_importance: u8,
    /// A message matched a `Filtered` rule (deferred to Stage-2).
    pub rule_filtered_importance: u8,
    /// Sender appears in the user's Sent mail (known contact).
    pub known_contact_importance: u8,
    /// Ops/monitoring alert from an automated sender.
    pub alert_importance: u8,
    /// Newsletter / receipt / cold-sales noise.
    pub noise_importance: u8,
    /// Ambiguous fall-through (unknown sender, no pattern) -> Stage-2.
    pub fallthrough_importance: u8,
}

impl Default for Stage1Config {
    fn default() -> Self {
        Self {
            bill_importance: 95,
            rule_surface_importance: 80,
            rule_squelch_importance: 10,
            rule_filtered_importance: 30,
            known_contact_importance: 70,
            alert_importance: 75,
            noise_importance: 15,
            fallthrough_importance: 40,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Google OAuth client id (from your own GCP "Desktop app" client).
    pub client_id: Option<String>,
    /// Google OAuth client secret.
    pub client_secret: Option<String>,
    /// The single Gmail account this v0 instance manages. Also the keyring key.
    pub account_email: Option<String>,

    /// Path to the SQLite store.
    pub db_path: PathBuf,
    /// Which backend persists OAuth tokens (`keyring` or `file`). Defaults per
    /// platform (keyring on macOS, file on Linux). Override with
    /// `SQUELCH_CRED_BACKEND`.
    pub credential_backend: CredentialBackend,
    /// Path to the JSON credentials file used by the `file` backend. Defaults to
    /// `~/.config/squelch/credentials.json`. Ignored by the keyring backend.
    pub credentials_path: Option<PathBuf>,
    /// Default minimum importance for surfacing updates.
    pub default_min_importance: u8,
    /// How aggressively to squelch. Placeholder; the triage agent owns semantics.
    pub squelch_level: u8,
    /// Stage-1 rules-engine tuning.
    pub stage1: Stage1Config,
    /// Sync tunables (backfill window, poll interval).
    pub sync: SyncConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            account_email: None,
            db_path: PathBuf::from("squelch.db"),
            credential_backend: CredentialBackend::default(),
            credentials_path: None,
            default_min_importance: 0,
            squelch_level: 0,
            stage1: Stage1Config::default(),
            sync: SyncConfig::default(),
        }
    }
}

impl Config {
    /// Default config path: `~/.config/squelch/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| h.join(".config").join("squelch").join("config.toml"))
    }

    /// Load config from the default path (if present), applying env-var
    /// overrides. A missing file is not an error — defaults are used.
    pub fn load() -> Self {
        let mut cfg = match Self::default_path() {
            Some(p) => Self::from_path(&p).unwrap_or_default(),
            None => Self::default(),
        };
        cfg.apply_env_overrides();
        cfg
    }

    /// Parse a config from a specific TOML file. Returns `None` if the file is
    /// absent or unparseable (callers fall back to defaults).
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
    }

    /// Env-var overrides (highest precedence). Env always wins over the file so
    /// operators can override without editing config.
    fn apply_env_overrides(&mut self) {
        if let Some(p) = std::env::var_os("SQUELCH_DB_PATH") {
            self.db_path = PathBuf::from(p);
        }
        if let Ok(v) = std::env::var("SQUELCH_MIN_IMPORTANCE")
            && let Ok(n) = v.parse::<u8>()
        {
            self.default_min_importance = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_CLIENT_ID")
            && !v.is_empty()
        {
            self.client_id = Some(v);
        }
        if let Ok(v) = std::env::var("SQUELCH_CLIENT_SECRET")
            && !v.is_empty()
        {
            self.client_secret = Some(v);
        }
        if let Ok(v) = std::env::var("SQUELCH_ACCOUNT_EMAIL")
            && !v.is_empty()
        {
            self.account_email = Some(v);
        }
        if let Ok(v) = std::env::var("SQUELCH_BACKFILL_DAYS")
            && let Ok(n) = v.parse::<u32>()
        {
            self.sync.backfill_days = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_POLL_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            self.sync.poll_secs = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_SQUELCH_LEVEL")
            && let Ok(n) = v.parse::<u8>()
        {
            self.squelch_level = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_CRED_BACKEND")
            && let Some(b) = CredentialBackend::from_str_lenient(&v)
        {
            self.credential_backend = b;
        }
        if let Some(p) = std::env::var_os("SQUELCH_CREDENTIALS_PATH") {
            self.credentials_path = Some(PathBuf::from(p));
        }
    }

    /// Resolve the credentials-file path for the `file` backend: the configured
    /// path if set, else `~/.config/squelch/credentials.json`.
    pub fn resolve_credentials_path(&self) -> PathBuf {
        if let Some(p) = &self.credentials_path {
            return p.clone();
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| {
                h.join(".config")
                    .join("squelch")
                    .join("credentials.json")
            })
            .unwrap_or_else(|| PathBuf::from("credentials.json"))
    }

    /// Load config from an explicit path (if present), then apply env overrides.
    /// A missing file is fine — you can drive everything from the environment.
    pub fn load_from(path: &std::path::Path) -> Self {
        let mut cfg = Self::from_path(path).unwrap_or_default();
        cfg.apply_env_overrides();
        cfg
    }

    /// Fetch OAuth client credentials, erroring with a helpful message if the
    /// user hasn't set up their GCP client yet.
    pub fn oauth_client(&self) -> Result<OAuthClientConfig, CoreError> {
        let client_id = self.client_id.clone().filter(|s| !s.is_empty());
        let client_secret = self.client_secret.clone().filter(|s| !s.is_empty());
        match (client_id, client_secret) {
            (Some(client_id), Some(client_secret)) => Ok(OAuthClientConfig {
                client_id,
                client_secret,
            }),
            _ => Err(CoreError::Credential(format!(
                "missing OAuth client credentials. Create a Google Cloud \"Desktop app\" \
                 OAuth client (with the Gmail API enabled) and set client_id/client_secret in {} \
                 or via SQUELCH_CLIENT_ID / SQUELCH_CLIENT_SECRET.",
                Self::default_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.config/squelch/config.toml".to_string())
            ))),
        }
    }

    /// The configured account email, erroring helpfully if unset.
    pub fn require_account_email(&self) -> Result<String, CoreError> {
        self.account_email
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CoreError::Credential(
                    "no account_email configured (set account_email in config or \
                     SQUELCH_ACCOUNT_EMAIL)"
                        .to_string(),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that touch process-wide env must not run concurrently.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn sync_defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.sync.backfill_days, 30);
        assert_eq!(c.sync.poll_secs, 45);
        assert!(c.client_id.is_none());
    }

    #[test]
    fn load_from_toml() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("squelch-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
client_id = "abc.apps.googleusercontent.com"
client_secret = "sekret"
account_email = "you@gmail.com"
db_path = "/tmp/squelch.db"

[sync]
backfill_days = 90
"#,
        )
        .unwrap();
        let c = Config::load_from(&path);
        assert_eq!(
            c.client_id.as_deref(),
            Some("abc.apps.googleusercontent.com")
        );
        assert_eq!(c.account_email.as_deref(), Some("you@gmail.com"));
        assert_eq!(c.db_path, PathBuf::from("/tmp/squelch.db"));
        assert_eq!(c.sync.backfill_days, 90);
        // unspecified sync field falls back to default
        assert_eq!(c.sync.poll_secs, 45);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_default() {
        let c = Config::load_from(std::path::Path::new("/nonexistent/squelch/config.toml"));
        assert_eq!(c.db_path, PathBuf::from("squelch.db"));
    }

    #[test]
    fn env_overrides_file() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK so no other test reads env concurrently.
        unsafe {
            std::env::set_var("SQUELCH_CLIENT_ID", "env-id");
            std::env::set_var("SQUELCH_BACKFILL_DAYS", "7");
        }
        let mut c = Config {
            client_id: Some("file-id".to_string()),
            ..Config::default()
        };
        c.apply_env_overrides();
        assert_eq!(c.client_id.as_deref(), Some("env-id"));
        assert_eq!(c.sync.backfill_days, 7);
        unsafe {
            std::env::remove_var("SQUELCH_CLIENT_ID");
            std::env::remove_var("SQUELCH_BACKFILL_DAYS");
        }
    }

    #[test]
    fn oauth_client_errors_when_missing() {
        let c = Config::default();
        assert!(c.oauth_client().is_err());
    }

    #[test]
    fn credential_backend_default_is_platform_appropriate() {
        let b = CredentialBackend::default();
        if cfg!(target_os = "macos") {
            assert_eq!(b, CredentialBackend::Keyring);
        } else {
            assert_eq!(b, CredentialBackend::File);
        }
    }

    #[test]
    fn credential_backend_parse() {
        assert_eq!(
            CredentialBackend::from_str_lenient("keyring"),
            Some(CredentialBackend::Keyring)
        );
        assert_eq!(
            CredentialBackend::from_str_lenient("  FILE "),
            Some(CredentialBackend::File)
        );
        assert_eq!(CredentialBackend::from_str_lenient("nonsense"), None);
    }

    #[test]
    fn env_selects_credential_backend() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_CRED_BACKEND", "file");
            std::env::set_var("SQUELCH_CREDENTIALS_PATH", "/tmp/squelch-test-creds.json");
        }
        let mut c = Config {
            credential_backend: CredentialBackend::Keyring,
            ..Config::default()
        };
        c.apply_env_overrides();
        assert_eq!(c.credential_backend, CredentialBackend::File);
        assert_eq!(
            c.resolve_credentials_path(),
            PathBuf::from("/tmp/squelch-test-creds.json")
        );
        unsafe {
            std::env::remove_var("SQUELCH_CRED_BACKEND");
            std::env::remove_var("SQUELCH_CREDENTIALS_PATH");
        }
    }

    #[test]
    fn credential_backend_from_toml() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("squelch-cfg-be-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
credential_backend = "file"
credentials_path = "/var/lib/squelch/creds.json"
"#,
        )
        .unwrap();
        // Ensure env doesn't clobber the file value under test.
        unsafe {
            std::env::remove_var("SQUELCH_CRED_BACKEND");
            std::env::remove_var("SQUELCH_CREDENTIALS_PATH");
        }
        let c = Config::load_from(&path);
        assert_eq!(c.credential_backend, CredentialBackend::File);
        assert_eq!(
            c.resolve_credentials_path(),
            PathBuf::from("/var/lib/squelch/creds.json")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
