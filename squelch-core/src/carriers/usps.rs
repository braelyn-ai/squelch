//! USPS Tracking API (v3) client. OAuth client-credentials with the consumer
//! pair in an `Authorization: Basic` header; token and tracking endpoints share
//! one origin, so a single `base_url` override redirects both in tests.

use super::oauth::TokenCache;
use super::{CarrierClient, TrackError};
use crate::config::UspsCarrierConfig;
use crate::triage::CarrierTrack;
use async_trait::async_trait;
use std::time::Duration;

const BASE_URL: &str = "https://apis.usps.com";

/// Floor between USPS calls. The default API product allows 60/hour, which the
/// poller's per-carrier budget enforces; this floor only keeps bursts polite.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

// TODO(wave3): every field is written here and read once `track` issues the real
// request.
#[allow(dead_code)]
pub struct UspsClient {
    http: reqwest::Client,
    consumer_key: String,
    /// Secret material, NEVER logged or included in an error.
    consumer_secret: String,
    token: TokenCache,
    base_url: String,
}

impl UspsClient {
    /// `None` when credentials do not fully resolve — half a pair leaves USPS
    /// out of the registry rather than sending an empty string at USPS's auth
    /// endpoint.
    pub fn from_config(cfg: &UspsCarrierConfig, http: reqwest::Client) -> Option<Self> {
        let (consumer_key, consumer_secret) = cfg.credentials()?;
        Some(Self {
            http,
            consumer_key: consumer_key.to_string(),
            consumer_secret: consumer_secret.to_string(),
            token: TokenCache::new(),
            base_url: BASE_URL.to_string(),
        })
    }

    /// Point the client at a mock server. Test hook only.
    #[doc(hidden)]
    pub fn for_test(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            http,
            consumer_key: "test-consumer-key".to_string(),
            consumer_secret: "test-consumer-secret".to_string(),
            token: TokenCache::new(),
            base_url: base_url.into(),
        }
    }
}

#[async_trait]
impl CarrierClient for UspsClient {
    fn carrier(&self) -> &'static str {
        "usps"
    }

    fn min_interval(&self) -> Duration {
        MIN_INTERVAL
    }

    async fn track(&self, _tracking_number: &str) -> Result<CarrierTrack, TrackError> {
        // TODO(wave3): POST {base_url}/oauth2/v3/token via
        // TokenCache::client_credentials, then GET
        // {base_url}/tracking/v3/tracking/{tracking_number}.
        Err(TrackError::Transient)
    }
}
