//! Configuration. Everything tunable lives here, loaded from
//! `~/.config/squelch/config.toml` with env-var overrides. Nothing magic is
//! hardcoded: the Stage-1 triage importance ladder, thresholds, and paths are
//! all fields on [`Config`] with sane defaults so a missing config file still
//! yields a working system.

use crate::error::CoreError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The one and only OAuth scope squelch will ever request. Read-only Gmail.
/// This is a hard invariant of the project, hence a `const`, not config.
pub const GMAIL_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";

/// Sync-related tunables. Placeholders in v0 but real config, not constants, so
/// the sync agent can wire them in without a schema change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// How many days of history to backfill on the initial sync.
    pub backfill_days: u32,
    /// Debounce window (seconds) for coalescing push/poll notifications.
    pub coalesce_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            backfill_days: 30,
            coalesce_secs: 30,
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
    /// Default minimum importance for surfacing updates.
    pub default_min_importance: u8,
    /// How aggressively to squelch. Placeholder; the triage agent owns semantics.
    pub squelch_level: u8,
    /// Stage-1 rules-engine tuning.
    pub stage1: Stage1Config,
    /// Sync tunables (backfill window, coalesce debounce).
    pub sync: SyncConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            account_email: None,
            db_path: PathBuf::from("squelch.db"),
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
        if let Ok(v) = std::env::var("SQUELCH_COALESCE_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            self.sync.coalesce_secs = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_SQUELCH_LEVEL")
            && let Ok(n) = v.parse::<u8>()
        {
            self.squelch_level = n;
        }
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
        assert_eq!(c.sync.coalesce_secs, 30);
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
        assert_eq!(c.sync.coalesce_secs, 30);
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
}
