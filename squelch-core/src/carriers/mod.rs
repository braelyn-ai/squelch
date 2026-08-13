//! BYOK carrier tracking-API clients: one [`CarrierClient`] per carrier, keyed
//! by the same lower-case slug [`crate::triage::shipment`] attributes a tracking
//! number to, so a detected shipment and the client that can poll it are matched
//! by string without a second mapping table.
//!
//! CREDENTIALS ARE THE FEATURE FLAG (see [`crate::config::CarriersConfig`]):
//! [`build_registry`] inserts a client only for a carrier whose creds fully
//! resolve, so an empty registry means no carrier API is ever contacted. All
//! clients share ONE [`reqwest::Client`], and therefore one connection pool —
//! four pools for four carriers polled on the same cadence would be four idle
//! keepalive sets.
//!
//! Every failure funnels through [`TrackError`], whose variants are the only
//! distinctions the poller acts on: back off with the carrier's own delay
//! ([`TrackError::RateLimited`]), stop retrying a number that does not exist
//! ([`TrackError::NotFound`]), stop polling a carrier whose creds went bad
//! ([`TrackError::Auth`]), or count a failure and try again
//! ([`TrackError::Transient`]). Errors carry NO url, body, or credential: a
//! carrier's 401 body routinely echoes the client id back.

pub mod dhl;
pub mod fedex;
pub mod oauth;
pub mod ups;
pub mod usps;

use crate::config::CarriersConfig;
use crate::triage::CarrierTrack;
use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Same shape as [`crate::tracking`]'s poller client: a carrier API that has
/// stopped answering must fail the pass, not hold the poll task open.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on an honored `Retry-After`. The header is attacker-adjacent input
/// (carrier, or any proxy in front of it), and an unclamped `Retry-After: 86400`
/// would park a shipment for a day on one bad response.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// Why a track call failed, reduced to the four cases the poller treats
/// differently. Deliberately carries no detail beyond a rate-limit delay:
/// carrier error bodies quote request urls and credentials back at you.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TrackError {
    /// The carrier does not know this tracking number. Retrying will not help
    /// until the shipper hands it over, so the poller ages the row out rather
    /// than counting failures forever.
    #[error("tracking number not found")]
    NotFound,
    /// Rate limited. `retry_after` is the carrier's own delay when it sent a
    /// usable `Retry-After`, clamped to [`MAX_RETRY_AFTER`]; `None` means the
    /// poller picks its own backoff.
    #[error("carrier rate limited")]
    RateLimited { retry_after: Option<Duration> },
    /// Credentials were rejected. A whole-carrier condition, not a per-shipment
    /// one — no other number will fare better with the same key.
    #[error("carrier rejected credentials")]
    Auth,
    /// Anything else: transport failure, 5xx, unparseable body.
    #[error("transient carrier failure")]
    Transient,
}

/// One carrier's tracking API.
///
/// Implementations are shared across poll passes behind an [`Arc`], so they hold
/// their own token cache and must stay `Send + Sync` and interior-mutable
/// (see [`oauth::TokenCache`]).
#[async_trait]
pub trait CarrierClient: Send + Sync {
    /// Lower-case carrier slug, matching `ShipmentInfo::carrier` ("ups", "usps",
    /// "fedex", "dhl"). This is the registry key.
    fn carrier(&self) -> &'static str;

    /// Floor between two calls to THIS carrier's API. The poller paces itself by
    /// carrier, not globally: a carrier's quota is its own.
    fn min_interval(&self) -> Duration;

    /// Poll one tracking number. Never panics on carrier input; an unrecognized
    /// status vocabulary is reported as [`CarrierTrack::status`] `None` with the
    /// raw string preserved, not guessed at.
    async fn track(&self, tracking_number: &str) -> Result<CarrierTrack, TrackError>;
}

/// Enabled carriers by slug. Absence means "not configured", which is exactly
/// what the poller needs: a shipment whose carrier has no entry is skipped.
pub type CarrierRegistry = HashMap<String, Arc<dyn CarrierClient>>;

/// The carrier clients' shared HTTP client. REDIRECTS ARE REFUSED for the same
/// reason [`crate::tracking`] refuses them: requests carry a bearer token or an
/// API key, and the default policy would let a `307` walk that credential to a
/// host of the carrier's (or an interceptor's) choosing.
fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Build the registry: one client per carrier whose credentials fully resolve.
///
/// Returns an EMPTY registry rather than an error when nothing is configured or
/// the HTTP client cannot be built — carrier polling is an optional side
/// feature, and no daemon should fail to start because a carrier could not be
/// reached.
pub fn build_registry(cfg: &CarriersConfig) -> CarrierRegistry {
    let mut registry = CarrierRegistry::new();
    if !cfg.any_enabled() {
        return registry;
    }
    let http = match http_client() {
        Ok(http) => http,
        Err(e) => {
            // No url and no credential reached the builder, so `e` is safe.
            eprintln!("squelch: carrier HTTP client could not be built: {e}");
            return registry;
        }
    };

    let mut insert = |client: Option<Arc<dyn CarrierClient>>| {
        if let Some(client) = client {
            registry.insert(client.carrier().to_string(), client);
        }
    };
    insert(
        cfg.ups
            .as_ref()
            .and_then(|c| ups::UpsClient::from_config(c, http.clone()))
            .map(|c| Arc::new(c) as Arc<dyn CarrierClient>),
    );
    insert(
        cfg.fedex
            .as_ref()
            .and_then(|c| fedex::FedexClient::from_config(c, http.clone()))
            .map(|c| Arc::new(c) as Arc<dyn CarrierClient>),
    );
    insert(
        cfg.usps
            .as_ref()
            .and_then(|c| usps::UspsClient::from_config(c, http.clone()))
            .map(|c| Arc::new(c) as Arc<dyn CarrierClient>),
    );
    insert(
        cfg.dhl
            .as_ref()
            .and_then(|c| dhl::DhlClient::from_config(c, http.clone()))
            .map(|c| Arc::new(c) as Arc<dyn CarrierClient>),
    );
    registry
}

/// Map a non-success HTTP status onto [`TrackError`]. Shared by every carrier so
/// the poller's backoff does not depend on which one answered.
///
/// 403 is [`TrackError::Auth`] alongside 401: carriers spend it on both an
/// invalid token and an unsubscribed API product, and both are operator
/// problems that no retry fixes.
pub fn classify_status(status: StatusCode, headers: &HeaderMap) -> TrackError {
    match status {
        StatusCode::NOT_FOUND => TrackError::NotFound,
        StatusCode::TOO_MANY_REQUESTS => TrackError::RateLimited {
            retry_after: retry_after(headers),
        },
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => TrackError::Auth,
        _ => TrackError::Transient,
    }
}

/// The `Retry-After` delay, in either RFC 9110 form (delta-seconds or an
/// HTTP-date), clamped to [`MAX_RETRY_AFTER`]. A date already in the past yields
/// `Some(0)` — the carrier said "now", not "never".
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim().to_string();
    let delay = match raw.parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs),
        Err(_) => {
            let at = chrono::DateTime::parse_from_rfc2822(&raw).ok()?;
            let secs = (at.timestamp() - chrono::Utc::now().timestamp()).max(0);
            Duration::from_secs(secs as u64)
        }
    };
    Some(delay.min(MAX_RETRY_AFTER))
}

/// Reject a non-2xx response, handing back the response itself on success so the
/// caller can read the body it wanted.
pub fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, TrackError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    Err(classify_status(resp.status(), resp.headers()))
}

/// Send a prepared request and decode its JSON body. The whole path — transport
/// failure, HTTP status, malformed body — collapses into [`TrackError`] here, so
/// no client has to decide for itself whether a 429 is transient.
pub async fn send_json<T: DeserializeOwned>(req: reqwest::RequestBuilder) -> Result<T, TrackError> {
    let resp = req.send().await.map_err(|_| TrackError::Transient)?;
    check_status(resp)?
        .json::<T>()
        .await
        .map_err(|_| TrackError::Transient)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DhlCarrierConfig, UpsCarrierConfig};

    fn headers(retry_after: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, retry_after.parse().unwrap());
        h
    }

    #[test]
    fn registry_is_empty_without_credentials() {
        assert!(build_registry(&CarriersConfig::default()).is_empty());
    }

    #[test]
    fn registry_holds_only_configured_carriers() {
        let cfg = CarriersConfig {
            ups: Some(UpsCarrierConfig {
                client_id: Some("id".into()),
                client_secret: Some("secret".into()),
            }),
            // Half a pair is not a carrier, and neither is a blank key.
            dhl: Some(DhlCarrierConfig {
                api_key: Some("  ".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let registry = build_registry(&cfg);
        assert_eq!(registry.keys().collect::<Vec<_>>(), vec!["ups"]);
        assert_eq!(registry["ups"].carrier(), "ups");
    }

    #[test]
    fn statuses_map_to_their_error_class() {
        let empty = HeaderMap::new();
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND, &empty),
            TrackError::NotFound
        );
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED, &empty),
            TrackError::Auth
        );
        assert_eq!(
            classify_status(StatusCode::FORBIDDEN, &empty),
            TrackError::Auth
        );
        assert_eq!(
            classify_status(StatusCode::BAD_GATEWAY, &empty),
            TrackError::Transient
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, &headers("30")),
            TrackError::RateLimited {
                retry_after: Some(Duration::from_secs(30))
            }
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, &empty),
            TrackError::RateLimited { retry_after: None }
        );
    }

    #[test]
    fn retry_after_accepts_seconds_and_dates_and_clamps() {
        assert_eq!(retry_after(&headers("12")), Some(Duration::from_secs(12)));
        assert_eq!(retry_after(&headers("86400")), Some(MAX_RETRY_AFTER));
        // A past HTTP-date means "now", not a negative wait.
        assert_eq!(
            retry_after(&headers("Wed, 21 Oct 2015 07:28:00 GMT")),
            Some(Duration::ZERO)
        );
        let soon = chrono::Utc::now() + chrono::Duration::seconds(120);
        let parsed = retry_after(&headers(&soon.to_rfc2822())).unwrap();
        assert!(parsed <= Duration::from_secs(120) && parsed >= Duration::from_secs(110));
        assert_eq!(retry_after(&headers("not-a-delay")), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }
}
