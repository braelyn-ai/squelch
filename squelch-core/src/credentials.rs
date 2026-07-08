//! Credential storage.
//!
//! [`OAuthToken`] is the in-memory token the rest of the system consumes.
//! [`StoredToken`] is the JSON-serialized shape persisted in the OS keyring
//! (access token + optional refresh token + absolute expiry).
//! [`KeyringCredentialStore`] implements [`CredentialStore`] against the OS
//! keyring and transparently refreshes an expired access token using the stored
//! refresh token.
//!
//! SECURITY: tokens are never logged. The keyring entry is keyed by
//! `(service = "squelch", username = <account email>)`.

use crate::config::OAuthClientConfig;
use crate::error::{CoreError, Result};
use crate::types::AccountId;
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The keyring service name. One entry per account email under this service.
pub const KEYRING_SERVICE: &str = "squelch";

/// An OAuth token for a Gmail account, as consumed in-memory.
#[derive(Debug, Clone)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Abstracts where OAuth tokens live (keyring, env, etc).
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// Return a *currently valid* access token, refreshing if necessary.
    async fn token(&self, account: AccountId) -> Result<OAuthToken>;
}

/// JSON-serialized token as persisted in the keyring. Expiry is stored as an
/// absolute UTC instant so validity survives process restarts.
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

    /// Serialize to the JSON blob stored in the keyring.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| CoreError::Credential(format!("serializing token: {e}")))
    }

    /// Parse from the JSON blob stored in the keyring.
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

/// Persist a token for an account into the OS keyring. Used by the auth flow.
pub fn store_token(account_email: &str, token: &StoredToken) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account_email)
        .map_err(|e| CoreError::Credential(format!("opening keyring entry: {e}")))?;
    entry
        .set_password(&token.to_json()?)
        .map_err(|e| CoreError::Credential(format!("writing keyring entry: {e}")))?;
    Ok(())
}

/// Read the raw stored token for an account from the keyring, if present.
pub fn load_token(account_email: &str) -> Result<StoredToken> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account_email)
        .map_err(|e| CoreError::Credential(format!("opening keyring entry: {e}")))?;
    let json = entry.get_password().map_err(|e| {
        CoreError::Credential(format!(
            "no stored credentials for {account_email} (run `squelchd auth` first): {e}"
        ))
    })?;
    StoredToken::from_json(&json)
}

/// Keyring-backed credential store with transparent refresh.
///
/// `account_email` is the keyring username. `client` supplies the OAuth client
/// id/secret needed to redeem the refresh token. `resolve_email` maps the
/// numeric [`AccountId`] to the email; in v0 this is a fixed single account.
pub struct KeyringCredentialStore {
    account_id: AccountId,
    account_email: String,
    client: OAuthClientConfig,
}

impl KeyringCredentialStore {
    /// Construct a store bound to a single account (v0: exactly one account).
    pub fn new(account_id: AccountId, account_email: String, client: OAuthClientConfig) -> Self {
        Self {
            account_id,
            account_email,
            client,
        }
    }

    /// The account email this store is bound to.
    pub fn account_email(&self) -> &str {
        &self.account_email
    }

    /// Exchange a refresh token for a fresh access token, persist, and return it.
    fn refresh(&self, refresh_token: &str) -> Result<StoredToken> {
        use oauth2::basic::BasicClient;
        use oauth2::{
            AuthUrl, ClientId, ClientSecret, RefreshToken, TokenResponse, TokenUrl,
        };

        let oauth = BasicClient::new(ClientId::new(self.client.client_id.clone()))
            .set_client_secret(ClientSecret::new(self.client.client_secret.clone()))
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

        // Google typically omits a new refresh token on refresh; keep the old.
        let new_refresh = resp
            .refresh_token()
            .map(|r| r.secret().to_string())
            .or_else(|| Some(refresh_token.to_string()));

        let fresh = StoredToken::from_response(
            resp.access_token().secret().to_string(),
            new_refresh,
            resp.expires_in(),
        );
        store_token(&self.account_email, &fresh)?;
        Ok(fresh)
    }

    /// Synchronous core of [`CredentialStore::token`]: load, refresh if needed.
    fn valid_token_blocking(&self) -> Result<OAuthToken> {
        let stored = load_token(&self.account_email)?;
        if stored.is_expired(ChronoDuration::seconds(REFRESH_SKEW_SECS)) {
            let refresh = stored.refresh_token.clone().ok_or_else(|| {
                CoreError::Credential(
                    "access token expired and no refresh token is stored; re-run `squelchd auth`"
                        .to_string(),
                )
            })?;
            return Ok(self.refresh(&refresh)?.into_oauth());
        }
        Ok(stored.into_oauth())
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
        // Keyring + blocking HTTP; keep it off the async runtime's core threads.
        let store = self.clone_for_blocking();
        tokio::task::spawn_blocking(move || store.valid_token_blocking())
            .await
            .map_err(|e| CoreError::Credential(format!("join error: {e}")))?
    }
}

impl KeyringCredentialStore {
    fn clone_for_blocking(&self) -> KeyringCredentialStore {
        KeyringCredentialStore {
            account_id: self.account_id,
            account_email: self.account_email.clone(),
            client: self.client.clone(),
        }
    }
}

/// Env-var backed stub for the v0 skeleton. Still handy for tests / CI without a
/// keyring. Real deployments use [`KeyringCredentialStore`].
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
        // Optional fields omitted from the blob entirely.
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

        // Expires in 30s but with a 60s skew we consider it expired (refresh early).
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

        // No expiry known -> treated as valid.
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
}
