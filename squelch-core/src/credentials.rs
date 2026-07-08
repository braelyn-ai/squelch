//! Credential storage.
//!
//! [`OAuthToken`] is the in-memory token the rest of the system consumes.
//! [`StoredToken`] is the JSON-serialized shape persisted to a backend (access
//! token + optional refresh token + absolute expiry).
//!
//! TWO-DOOR MODEL: each account has up to two *separate* credentials keyed by
//! [`CredentialKind`]:
//!   - [`CredentialKind::Read`]  — `gmail.readonly`; the sync daemon + triage
//!     use ONLY this. Stored in the plain-email slot for back-compat.
//!   - [`CredentialKind::Write`] — `gmail.modify` + `gmail.send`; loaded ONLY by
//!     human-door action endpoints. Stored in a `#write`-suffixed slot.
//!
//! A store is constructed *bound to one kind*; a Read-bound store can never
//! return the write token and vice versa.
//!
//! Both backends ([`KeyringCredentialStore`], [`FileCredentialStore`]) share the
//! same refresh logic via [`refresh_stored_token`] — no duplication.
//!
//! SECURITY: tokens are never logged. On unix the file backend is mode 0600.

use crate::config::OAuthClientConfig;
use crate::error::{CoreError, Result};
use crate::types::AccountId;
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The keyring service name. One entry per (account email, kind) slot.
pub const KEYRING_SERVICE: &str = "squelch";

/// Which credential a store is bound to. Determines both the OAuth scopes that
/// minted the token and the storage slot it lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialKind {
    /// Read-only (`gmail.readonly`). Used by sync + triage.
    Read,
    /// Read/write (`gmail.modify` + `gmail.send`). Human-door actions only.
    Write,
}

impl CredentialKind {
    /// Storage-slot suffix. Read is empty for back-compat with pre-two-door
    /// tokens already sitting in the plain-email keyring slot; Write is
    /// disambiguated by suffix.
    pub fn slot_suffix(self) -> &'static str {
        match self {
            CredentialKind::Read => "",
            CredentialKind::Write => "#write",
        }
    }

    /// The full storage-slot key for an account email under this kind.
    pub fn slot_key(self, account_email: &str) -> String {
        format!("{account_email}{}", self.slot_suffix())
    }
}

/// An OAuth token for a Gmail account, as consumed in-memory.
#[derive(Debug, Clone)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Abstracts where OAuth tokens live (keyring, file, env). A store is bound to a
/// single account and a single [`CredentialKind`]; `token` returns only that
/// kind's token.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// Return a *currently valid* access token, refreshing if necessary.
    async fn token(&self, account: AccountId) -> Result<OAuthToken>;
}

/// JSON-serialized token as persisted. Expiry is an absolute UTC instant so
/// validity survives process restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Absolute expiry instant, if the provider supplied `expires_in`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl StoredToken {
    /// Build from a fresh token exchange/refresh response.
    pub fn from_response(
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<Duration>,
    ) -> Self {
        let expires_at = expires_in
            .and_then(|d| ChronoDuration::from_std(d).ok())
            .map(|d| Utc::now() + d);
        Self {
            access_token,
            refresh_token,
            expires_at,
        }
    }

    /// Serialize to the JSON blob stored in a backend.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| CoreError::Credential(format!("serializing token: {e}")))
    }

    /// Parse from the JSON blob stored in a backend.
    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s)
            .map_err(|e| CoreError::Credential(format!("parsing stored token: {e}")))
    }

    /// True if the access token is expired or within `skew` of expiring. Tokens
    /// with no known expiry are treated as *not* expired (best effort).
    pub fn is_expired(&self, skew: ChronoDuration) -> bool {
        match self.expires_at {
            Some(exp) => Utc::now() + skew >= exp,
            None => false,
        }
    }

    fn into_oauth(self) -> OAuthToken {
        OAuthToken {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: self.expires_at,
        }
    }
}

/// Refresh a token grace window: refresh if within 60s of expiry.
const REFRESH_SKEW_SECS: i64 = 60;

// ---------------------------------------------------------------------------
// Shared refresh logic (used by both keyring and file backends).
// ---------------------------------------------------------------------------

/// Exchange a refresh token for a fresh access token via Google's token
/// endpoint. Pure network op — persistence is the caller's job. Shared by every
/// backend so refresh behavior can't drift between them.
///
/// Google typically omits a new refresh token on refresh, so we preserve the
/// caller's `refresh_token` when the response doesn't carry one.
pub fn refresh_stored_token(
    client: &OAuthClientConfig,
    refresh_token: &str,
) -> Result<StoredToken> {
    use oauth2::basic::BasicClient;
    use oauth2::{AuthUrl, ClientId, ClientSecret, RefreshToken, TokenResponse, TokenUrl};

    let oauth = BasicClient::new(ClientId::new(client.client_id.clone()))
        .set_client_secret(ClientSecret::new(client.client_secret.clone()))
        .set_auth_uri(
            AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string())
                .map_err(|e| CoreError::Credential(format!("bad auth url: {e}")))?,
        )
        .set_token_uri(
            TokenUrl::new(crate::auth::GOOGLE_TOKEN_URL.to_string())
                .map_err(|e| CoreError::Credential(format!("bad token url: {e}")))?,
        );

    let http = oauth2::reqwest::blocking::ClientBuilder::new()
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CoreError::Credential(format!("building http client: {e}")))?;

    let resp = oauth
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request(&http)
        .map_err(|e| CoreError::Credential(format!("refresh failed: {e}")))?;

    let new_refresh = resp
        .refresh_token()
        .map(|r| r.secret().to_string())
        .or_else(|| Some(refresh_token.to_string()));

    Ok(StoredToken::from_response(
        resp.access_token().secret().to_string(),
        new_refresh,
        resp.expires_in(),
    ))
}

/// Given a stored token, return a currently-valid [`OAuthToken`], refreshing via
/// `refresh_stored_token` if within the skew window. `persist` re-saves the
/// refreshed blob into whatever backend the caller owns. Shared by all backends.
fn validate_or_refresh(
    stored: StoredToken,
    client: &OAuthClientConfig,
    persist: impl FnOnce(&StoredToken) -> Result<()>,
) -> Result<OAuthToken> {
    if stored.is_expired(ChronoDuration::seconds(REFRESH_SKEW_SECS)) {
        let refresh = stored.refresh_token.clone().ok_or_else(|| {
            CoreError::Credential(
                "access token expired and no refresh token is stored; re-run `squelchd auth`"
                    .to_string(),
            )
        })?;
        let fresh = refresh_stored_token(client, &refresh)?;
        persist(&fresh)?;
        return Ok(fresh.into_oauth());
    }
    Ok(stored.into_oauth())
}

// ---------------------------------------------------------------------------
// Keyring backend.
// ---------------------------------------------------------------------------

/// Persist a token into the OS keyring at `(service = "squelch", slot)` where
/// `slot` = email + kind suffix. Used by the auth flow.
pub fn store_token(account_email: &str, kind: CredentialKind, token: &StoredToken) -> Result<()> {
    let slot = kind.slot_key(account_email);
    let entry = keyring::Entry::new(KEYRING_SERVICE, &slot)
        .map_err(|e| CoreError::Credential(format!("opening keyring entry: {e}")))?;
    entry
        .set_password(&token.to_json()?)
        .map_err(|e| CoreError::Credential(format!("writing keyring entry: {e}")))?;
    Ok(())
}

/// Read the raw stored token for an account's kind slot from the keyring.
pub fn load_token(account_email: &str, kind: CredentialKind) -> Result<StoredToken> {
    let slot = kind.slot_key(account_email);
    let entry = keyring::Entry::new(KEYRING_SERVICE, &slot)
        .map_err(|e| CoreError::Credential(format!("opening keyring entry: {e}")))?;
    let json = entry.get_password().map_err(|e| {
        CoreError::Credential(format!(
            "no stored credentials for {account_email} ({kind:?} slot) \
             (run `squelchd auth` first): {e}"
        ))
    })?;
    StoredToken::from_json(&json)
}

/// Keyring-backed credential store with transparent refresh, bound to one
/// account and one [`CredentialKind`].
pub struct KeyringCredentialStore {
    account_id: AccountId,
    account_email: String,
    kind: CredentialKind,
    client: OAuthClientConfig,
}

impl KeyringCredentialStore {
    /// Construct a Read-bound store (the sync engine's back-compat entry point).
    pub fn new(account_id: AccountId, account_email: String, client: OAuthClientConfig) -> Self {
        Self::new_with_kind(account_id, account_email, CredentialKind::Read, client)
    }

    /// Construct a store bound to an explicit kind.
    pub fn new_with_kind(
        account_id: AccountId,
        account_email: String,
        kind: CredentialKind,
        client: OAuthClientConfig,
    ) -> Self {
        Self {
            account_id,
            account_email,
            kind,
            client,
        }
    }

    /// The account email this store is bound to.
    pub fn account_email(&self) -> &str {
        &self.account_email
    }

    /// The credential kind this store is bound to.
    pub fn kind(&self) -> CredentialKind {
        self.kind
    }

    /// Synchronous core: load this kind's slot, refresh if needed, re-persist.
    fn valid_token_blocking(&self) -> Result<OAuthToken> {
        let stored = load_token(&self.account_email, self.kind)?;
        let email = self.account_email.clone();
        let kind = self.kind;
        validate_or_refresh(stored, &self.client, |fresh| store_token(&email, kind, fresh))
    }

    fn clone_for_blocking(&self) -> KeyringCredentialStore {
        KeyringCredentialStore {
            account_id: self.account_id,
            account_email: self.account_email.clone(),
            kind: self.kind,
            client: self.client.clone(),
        }
    }
}

#[async_trait]
impl CredentialStore for KeyringCredentialStore {
    async fn token(&self, account: AccountId) -> Result<OAuthToken> {
        if account != self.account_id {
            return Err(CoreError::Credential(format!(
                "account {account} not managed by this store (bound to {})",
                self.account_id
            )));
        }
        let store = self.clone_for_blocking();
        tokio::task::spawn_blocking(move || store.valid_token_blocking())
            .await
            .map_err(|e| CoreError::Credential(format!("join error: {e}")))?
    }
}

// ---------------------------------------------------------------------------
// File backend (headless Linux: no Secret Service).
// ---------------------------------------------------------------------------

/// On-disk shape of the credentials file: a map from slot key
/// (`email` or `email#write`) to its stored token.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    slots: BTreeMap<String, StoredToken>,
}

impl CredentialsFile {
    fn read(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| CoreError::Credential(format!("parsing credentials file: {e}"))),
            // A missing file is an empty set of slots, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(CoreError::Credential(format!(
                "reading credentials file: {e}"
            ))),
        }
    }

    /// Write atomically-ish with mode 0600 on unix (temp file + rename so a
    /// crash mid-write can't truncate the existing creds).
    fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                CoreError::Credential(format!("creating credentials dir: {e}"))
            })?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| CoreError::Credential(format!("serializing credentials: {e}")))?;

        let tmp = path.with_extension("json.tmp");
        write_private(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)
            .map_err(|e| CoreError::Credential(format!("finalizing credentials file: {e}")))?;
        // Ensure the final path also carries 0600 (rename preserves tmp's mode,
        // but be explicit in case the dest pre-existed with looser bits).
        set_private_mode(path)?;
        Ok(())
    }
}

/// Write bytes to `path` creating it 0600 on unix.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| CoreError::Credential(format!("opening credentials file: {e}")))?;
    f.write_all(bytes)
        .map_err(|e| CoreError::Credential(format!("writing credentials file: {e}")))?;
    f.flush()
        .map_err(|e| CoreError::Credential(format!("flushing credentials file: {e}")))?;
    set_private_mode(path)?;
    Ok(())
}

/// Force mode 0600 on unix; no-op elsewhere.
fn set_private_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| CoreError::Credential(format!("setting credentials file mode: {e}")))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Persist a token into the JSON credentials file at `path`, in the slot for
/// `(email, kind)`. Merges with any existing slots. File ends up mode 0600.
pub fn store_token_file(
    path: &Path,
    account_email: &str,
    kind: CredentialKind,
    token: &StoredToken,
) -> Result<()> {
    let mut file = CredentialsFile::read(path)?;
    file.slots.insert(kind.slot_key(account_email), token.clone());
    file.write(path)
}

/// Read the raw stored token for an account's kind slot from the file backend.
pub fn load_token_file(
    path: &Path,
    account_email: &str,
    kind: CredentialKind,
) -> Result<StoredToken> {
    let file = CredentialsFile::read(path)?;
    file.slots
        .get(&kind.slot_key(account_email))
        .cloned()
        .ok_or_else(|| {
            CoreError::Credential(format!(
                "no stored credentials for {account_email} ({kind:?} slot) in {} \
                 (run `squelchd auth` first)",
                path.display()
            ))
        })
}

/// File-backed credential store with transparent refresh, bound to one account
/// and one [`CredentialKind`]. For headless hosts with no Secret Service.
pub struct FileCredentialStore {
    account_id: AccountId,
    account_email: String,
    kind: CredentialKind,
    path: PathBuf,
    client: OAuthClientConfig,
}

impl FileCredentialStore {
    /// Construct a Read-bound file store (sync-engine back-compat entry point).
    pub fn new(
        account_id: AccountId,
        account_email: String,
        path: PathBuf,
        client: OAuthClientConfig,
    ) -> Self {
        Self::new_with_kind(
            account_id,
            account_email,
            CredentialKind::Read,
            path,
            client,
        )
    }

    /// Construct a file store bound to an explicit kind.
    pub fn new_with_kind(
        account_id: AccountId,
        account_email: String,
        kind: CredentialKind,
        path: PathBuf,
        client: OAuthClientConfig,
    ) -> Self {
        Self {
            account_id,
            account_email,
            kind,
            path,
            client,
        }
    }

    /// The account email this store is bound to.
    pub fn account_email(&self) -> &str {
        &self.account_email
    }

    /// The credential kind this store is bound to.
    pub fn kind(&self) -> CredentialKind {
        self.kind
    }

    fn valid_token_blocking(&self) -> Result<OAuthToken> {
        let stored = load_token_file(&self.path, &self.account_email, self.kind)?;
        let path = self.path.clone();
        let email = self.account_email.clone();
        let kind = self.kind;
        validate_or_refresh(stored, &self.client, |fresh| {
            store_token_file(&path, &email, kind, fresh)
        })
    }

    fn clone_for_blocking(&self) -> FileCredentialStore {
        FileCredentialStore {
            account_id: self.account_id,
            account_email: self.account_email.clone(),
            kind: self.kind,
            path: self.path.clone(),
            client: self.client.clone(),
        }
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn token(&self, account: AccountId) -> Result<OAuthToken> {
        if account != self.account_id {
            return Err(CoreError::Credential(format!(
                "account {account} not managed by this store (bound to {})",
                self.account_id
            )));
        }
        let store = self.clone_for_blocking();
        tokio::task::spawn_blocking(move || store.valid_token_blocking())
            .await
            .map_err(|e| CoreError::Credential(format!("join error: {e}")))?
    }
}

// ---------------------------------------------------------------------------
// Backend-agnostic persistence helpers (used by the auth subcommand).
// ---------------------------------------------------------------------------

use crate::config::CredentialBackend;

/// Persist a freshly-minted token into whichever backend is configured. The
/// auth subcommand calls this so it stays backend-agnostic.
pub fn store_token_backend(
    backend: CredentialBackend,
    credentials_path: &Path,
    account_email: &str,
    kind: CredentialKind,
    token: &StoredToken,
) -> Result<()> {
    match backend {
        CredentialBackend::Keyring => store_token(account_email, kind, token),
        CredentialBackend::File => {
            store_token_file(credentials_path, account_email, kind, token)
        }
    }
}

/// Load a raw stored token from whichever backend is configured (used by the
/// auth subcommand to confirm persistence).
pub fn load_token_backend(
    backend: CredentialBackend,
    credentials_path: &Path,
    account_email: &str,
    kind: CredentialKind,
) -> Result<StoredToken> {
    match backend {
        CredentialBackend::Keyring => load_token(account_email, kind),
        CredentialBackend::File => load_token_file(credentials_path, account_email, kind),
    }
}

/// Build a Read-bound [`CredentialStore`] trait object for the configured
/// backend. The sync engine consumes this; its call sites never change.
pub fn read_store_for_backend(
    backend: CredentialBackend,
    account_id: AccountId,
    account_email: String,
    credentials_path: PathBuf,
    client: OAuthClientConfig,
) -> std::sync::Arc<dyn CredentialStore> {
    match backend {
        CredentialBackend::Keyring => std::sync::Arc::new(KeyringCredentialStore::new(
            account_id,
            account_email,
            client,
        )),
        CredentialBackend::File => std::sync::Arc::new(FileCredentialStore::new(
            account_id,
            account_email,
            credentials_path,
            client,
        )),
    }
}

// ---------------------------------------------------------------------------
// Env-var stub (tests / CI without any backend).
// ---------------------------------------------------------------------------

/// Env-var backed stub for the v0 skeleton. Still handy for tests / CI without a
/// keyring. Real deployments use [`KeyringCredentialStore`] / [`FileCredentialStore`].
pub struct EnvCredentialStore;

#[async_trait]
impl CredentialStore for EnvCredentialStore {
    async fn token(&self, _account: AccountId) -> Result<OAuthToken> {
        let access_token = std::env::var("SQUELCH_ACCESS_TOKEN").unwrap_or_default();
        Ok(OAuthToken {
            access_token,
            refresh_token: None,
            expires_at: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> OAuthClientConfig {
        OAuthClientConfig {
            client_id: "id".into(),
            client_secret: "secret".into(),
        }
    }

    #[test]
    fn token_json_round_trip() {
        let t = StoredToken {
            access_token: "aaa".to_string(),
            refresh_token: Some("rrr".to_string()),
            expires_at: Some(Utc::now()),
        };
        let json = t.to_json().unwrap();
        let back = StoredToken::from_json(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn token_json_round_trip_no_optionals() {
        let t = StoredToken {
            access_token: "aaa".to_string(),
            refresh_token: None,
            expires_at: None,
        };
        let json = t.to_json().unwrap();
        assert!(!json.contains("refresh_token"));
        assert!(!json.contains("expires_at"));
        let back = StoredToken::from_json(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn expiry_logic() {
        let skew = ChronoDuration::seconds(60);

        let past = StoredToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Some(Utc::now() - ChronoDuration::seconds(10)),
        };
        assert!(past.is_expired(skew));

        let soon = StoredToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Some(Utc::now() + ChronoDuration::seconds(30)),
        };
        assert!(soon.is_expired(skew));

        let future = StoredToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Some(Utc::now() + ChronoDuration::hours(1)),
        };
        assert!(!future.is_expired(skew));

        let unknown = StoredToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: None,
        };
        assert!(!unknown.is_expired(skew));
    }

    #[test]
    fn from_response_sets_absolute_expiry() {
        let t = StoredToken::from_response(
            "tok".into(),
            Some("ref".into()),
            Some(Duration::from_secs(3600)),
        );
        let exp = t.expires_at.expect("expiry set");
        let delta = (exp - Utc::now()).num_seconds();
        assert!((3500..=3600).contains(&delta), "delta was {delta}");
    }

    #[test]
    fn slot_keys_differ_by_kind() {
        assert_eq!(CredentialKind::Read.slot_key("you@x.com"), "you@x.com");
        assert_eq!(
            CredentialKind::Write.slot_key("you@x.com"),
            "you@x.com#write"
        );
        assert_ne!(
            CredentialKind::Read.slot_key("you@x.com"),
            CredentialKind::Write.slot_key("you@x.com")
        );
    }

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "squelch-cred-{}-{}-{}.json",
            std::process::id(),
            name,
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ))
    }

    #[test]
    fn file_store_round_trip() {
        let path = tmp_path("roundtrip");
        let tok = StoredToken {
            access_token: "read-access".into(),
            refresh_token: Some("read-refresh".into()),
            expires_at: Some(Utc::now() + ChronoDuration::hours(1)),
        };
        store_token_file(&path, "you@x.com", CredentialKind::Read, &tok).unwrap();
        let back = load_token_file(&path, "you@x.com", CredentialKind::Read).unwrap();
        assert_eq!(tok, back);
        std::fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn file_store_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_path("mode");
        let tok = StoredToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: None,
        };
        store_token_file(&path, "you@x.com", CredentialKind::Read, &tok).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn file_store_kind_slot_separation() {
        // A write token stored under Write must NEVER come back for a Read load.
        let path = tmp_path("kindsep");
        let read_tok = StoredToken {
            access_token: "READ-ONLY-TOKEN".into(),
            refresh_token: None,
            expires_at: None,
        };
        let write_tok = StoredToken {
            access_token: "WRITE-CAPABLE-TOKEN".into(),
            refresh_token: None,
            expires_at: None,
        };
        store_token_file(&path, "you@x.com", CredentialKind::Read, &read_tok).unwrap();
        store_token_file(&path, "you@x.com", CredentialKind::Write, &write_tok).unwrap();

        let got_read = load_token_file(&path, "you@x.com", CredentialKind::Read).unwrap();
        let got_write = load_token_file(&path, "you@x.com", CredentialKind::Write).unwrap();
        assert_eq!(got_read.access_token, "READ-ONLY-TOKEN");
        assert_eq!(got_write.access_token, "WRITE-CAPABLE-TOKEN");
        assert_ne!(got_read.access_token, got_write.access_token);
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn file_store_read_bound_never_returns_write() {
        // A Read-bound FileCredentialStore must fail (NotFound-ish) rather than
        // return the write token, even when only the write slot is populated.
        let path = tmp_path("readbound");
        let write_tok = StoredToken {
            access_token: "WRITE-CAPABLE-TOKEN".into(),
            refresh_token: None,
            expires_at: None,
        };
        store_token_file(&path, "you@x.com", CredentialKind::Write, &write_tok).unwrap();

        let store = FileCredentialStore::new(
            1_i64,
            "you@x.com".into(),
            path.clone(),
            client(),
        );
        // Read slot is empty -> error, and it certainly never yields the write token.
        let err = store.token(1_i64).await;
        assert!(err.is_err(), "read-bound store must not read the write slot");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn file_store_returns_valid_token() {
        let path = tmp_path("valid");
        let tok = StoredToken {
            access_token: "still-good".into(),
            refresh_token: Some("r".into()),
            expires_at: Some(Utc::now() + ChronoDuration::hours(1)),
        };
        store_token_file(&path, "you@x.com", CredentialKind::Read, &tok).unwrap();
        let store =
            FileCredentialStore::new(1_i64, "you@x.com".into(), path.clone(), client());
        let got = store.token(1_i64).await.unwrap();
        assert_eq!(got.access_token, "still-good");
        std::fs::remove_file(&path).ok();
    }
}
