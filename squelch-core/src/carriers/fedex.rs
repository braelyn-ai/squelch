//! FedEx Track API client. OAuth client-credentials with the pair in the form
//! body; token and tracking endpoints share one origin, so a single `base_url`
//! override redirects both in tests.

use super::oauth::TokenCache;
use super::{CarrierClient, TrackError};
use crate::config::FedexCarrierConfig;
use crate::triage::CarrierTrack;
use async_trait::async_trait;
use std::time::Duration;

const BASE_URL: &str = "https://apis.fedex.com";

/// Floor between FedEx calls. FedEx meters per second, not per day.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

// TODO(wave3): every field is written here and read once `track` issues the real
// request.
#[allow(dead_code)]
pub struct FedexClient {
    http: reqwest::Client,
    client_id: String,
    /// Secret material, NEVER logged or included in an error.
    client_secret: String,
    token: TokenCache,
    base_url: String,
}

impl FedexClient {
    /// `None` when credentials do not fully resolve — half a pair leaves FedEx
    /// out of the registry rather than sending an empty string at FedEx's auth
    /// endpoint.
    pub fn from_config(cfg: &FedexCarrierConfig, http: reqwest::Client) -> Option<Self> {
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
impl CarrierClient for FedexClient {
    fn carrier(&self) -> &'static str {
        "fedex"
    }

    fn min_interval(&self) -> Duration {
        MIN_INTERVAL
    }

    async fn track(&self, _tracking_number: &str) -> Result<CarrierTrack, TrackError> {
        // TODO(wave3): POST {base_url}/oauth/token via TokenCache (form auth),
        // then POST {base_url}/track/v1/trackingnumbers.
        Err(TrackError::Transient)
    }
}
