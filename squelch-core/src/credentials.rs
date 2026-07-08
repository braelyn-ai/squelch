//! Credential storage trait. Kept transport-agnostic; keyring impl comes later.

use crate::error::Result;
use crate::types::AccountId;
use async_trait::async_trait;

/// An OAuth token for a Gmail account. Fields are stubs for v0.
#[derive(Debug, Clone)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Abstracts where OAuth tokens live (keyring, env, etc).
#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn token(&self, account: AccountId) -> Result<OAuthToken>;
}

/// Env-var backed stub for the v0 skeleton. Real keyring impl lands later.
pub struct EnvCredentialStore;

#[async_trait]
impl CredentialStore for EnvCredentialStore {
    async fn token(&self, _account: AccountId) -> Result<OAuthToken> {
        // TODO: read from keyring in a real build. For the skeleton we pull an
        // access token from the environment if present.
        let access_token = std::env::var("SQUELCH_ACCESS_TOKEN").unwrap_or_default();
        Ok(OAuthToken {
            access_token,
            refresh_token: None,
            expires_at: None,
        })
    }
}
