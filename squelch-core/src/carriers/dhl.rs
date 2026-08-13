//! DHL Unified Tracking API client. No OAuth — a bare `DHL-API-Key` header —
//! which also makes DHL the strictest metered carrier here: the free tier is
//! one call per five seconds, 250 per day.

use super::{CarrierClient, TrackError};
use crate::config::DhlCarrierConfig;
use crate::triage::CarrierTrack;
use async_trait::async_trait;
use std::time::Duration;

const BASE_URL: &str = "https://api-eu.dhl.com";

/// DHL's free tier allows 1 call per 5 seconds; violating it burns the daily
/// budget on 429s.
const MIN_INTERVAL: Duration = Duration::from_secs(5);

// TODO(wave3): every field is written here and read once `track` issues the real
// request.
#[allow(dead_code)]
pub struct DhlClient {
    http: reqwest::Client,
    /// Secret material, NEVER logged or included in an error.
    api_key: String,
    base_url: String,
}

impl DhlClient {
    /// `None` when the key is absent or blank — a blank header is a guaranteed
    /// 401 at DHL, better spent as "not configured".
    pub fn from_config(cfg: &DhlCarrierConfig, http: reqwest::Client) -> Option<Self> {
        let api_key = cfg.api_key()?;
        Some(Self {
            http,
            api_key: api_key.to_string(),
            base_url: BASE_URL.to_string(),
        })
    }

    /// Point the client at a mock server. Test hook only.
    #[doc(hidden)]
    pub fn for_test(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            http,
            api_key: "test-api-key".to_string(),
            base_url: base_url.into(),
        }
    }
}

#[async_trait]
impl CarrierClient for DhlClient {
    fn carrier(&self) -> &'static str {
        "dhl"
    }

    fn min_interval(&self) -> Duration {
        MIN_INTERVAL
    }

    async fn track(&self, _tracking_number: &str) -> Result<CarrierTrack, TrackError> {
        // TODO(wave3): GET {base_url}/track/shipments?trackingNumber={n} with
        // the DHL-API-Key header.
        Err(TrackError::Transient)
    }
}
