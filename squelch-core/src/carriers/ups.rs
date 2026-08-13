//! UPS Tracking API client. OAuth client-credentials with the pair in an
//! `Authorization: Basic` header; the token endpoint and the tracking endpoint
//! share one origin, so a single `base_url` override redirects both in tests.

use super::oauth::TokenCache;
use super::{CarrierClient, TrackError};
use crate::config::UpsCarrierConfig;
use crate::triage::CarrierTrack;
use async_trait::async_trait;
use std::time::Duration;

const BASE_URL: &str = "https://onlinetools.ups.com";

/// Floor between UPS calls. UPS meters per second, not per day.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

// TODO(wave3): every field is written here and read once `track` issues the real
// request.
#[allow(dead_code)]
pub struct UpsClient {
    http: reqwest::Client,
    client_id: String,
    /// Secret material, NEVER logged or included in an error.
    client_secret: String,
    token: TokenCache,
    base_url: String,
}

impl UpsClient {
    /// `None` when credentials do not fully resolve — half a pair leaves UPS
    /// out of the registry rather than sending an empty string at UPS's auth
    /// endpoint.
    pub fn from_config(cfg: &UpsCarrierConfig, http: reqwest::Client) -> Option<Self> {
        let (client_id, client_secret) = cfg.credentials()?;
        Some(Self {
            http,
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            token: TokenCache::new(),
            base_url: BASE_URL.to_string(),
        })
    }

    /// Point the client at a mock server. Test hook only.
    #[doc(hidden)]
    pub fn for_test(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            http,
            client_id: "test-client-id".to_string(),
            client_secret: "test-client-secret".to_string(),
            token: TokenCache::new(),
            base_url: base_url.into(),
        }
    }
}

#[async_trait]
impl CarrierClient for UpsClient {
    fn carrier(&self) -> &'static str {
        "ups"
    }

    fn min_interval(&self) -> Duration {
        MIN_INTERVAL
    }

    async fn track(&self, _tracking_number: &str) -> Result<CarrierTrack, TrackError> {
        // TODO(wave3): POST {base_url}/security/v1/oauth/token via
        // TokenCache::client_credentials, then GET
        // {base_url}/api/track/v1/details/{tracking_number}.
        Err(TrackError::Transient)
    }
}
