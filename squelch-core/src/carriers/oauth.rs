//! The client-credentials grant, shared by every carrier that speaks OAuth
//! (UPS, FedEx, USPS — DHL is a bare API key and needs none of this).
//!
//! The three differ in WHERE the client id and secret ride: UPS puts them in an
//! `Authorization: Basic` header, FedEx in the form body — [`ClientAuth`] is
//! that difference. USPS wants a JSON body neither arm encodes, so its client
//! builds the request itself and shares only [`TokenCache`] and
//! [`TokenResponse`].
//!
//! Tokens are cached in memory only, never persisted: a carrier access token is
//! minutes-to-an-hour lived and re-mintable from creds we already hold, so
//! writing one to disk would add a secret at rest for nothing.

use super::TrackError;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Refresh this long BEFORE the carrier's stated expiry, so a token cannot go
/// stale in flight between our check and the carrier's.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// Assumed lifetime when a token response omits `expires_in`. Short on purpose:
/// the cost of guessing low is one extra token call.
const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// A carrier's token endpoint response. Every field beyond these two is ignored;
/// carriers add their own (`token_type`, `issued_at`, `scope`) and change them.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    /// Seconds of validity, as the carrier reports it. Trusted as given.
    /// UPS types this as a JSON STRING (`"14399"`), so both spellings parse;
    /// an unparseable value means "no stated expiry", answered by
    /// [`DEFAULT_TTL`] rather than a failed mint.
    #[serde(default, deserialize_with = "seconds")]
    pub expires_in: Option<u64>,
}

/// Accept `14399`, `"14399"`, or absence. Anything else is `None`, never an
/// error: a token that arrived is worth using for [`DEFAULT_TTL`] even when its
/// stated lifetime does not parse.
fn seconds<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(u64),
        Text(String),
    }
    Ok(match Option::<Raw>::deserialize(de).unwrap_or(None) {
        Some(Raw::Number(n)) => Some(n),
        Some(Raw::Text(s)) => s.trim().parse().ok(),
        None => None,
    })
}

/// Where a carrier wants the client id and secret on the token request.
#[derive(Debug, Clone, Copy)]
pub enum ClientAuth<'a> {
    /// `Authorization: Basic base64(id:secret)`, body carries only the grant
    /// (UPS, USPS).
    Basic {
        client_id: &'a str,
        client_secret: &'a str,
    },
    /// `client_id` / `client_secret` as form fields (FedEx).
    Form {
        client_id: &'a str,
        client_secret: &'a str,
    },
}

struct Cached {
    token: String,
    /// Absolute instant the carrier's expiry lands, SKEW already subtracted.
    good_until: Instant,
}

/// One carrier's access token, refreshed on demand.
///
/// The lock is held ACROSS the refresh, which single-flights it: N shipments
/// polled concurrently against an expired token produce one token call, not N.
/// Refreshes are seconds apart at worst, so the contention is not worth a
/// double-checked dance.
#[derive(Default)]
pub struct TokenCache {
    inner: Mutex<Option<Cached>>,
}

impl TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached token, or one minted by `fetch`. `fetch` runs only when there
    /// is no live token; a failed fetch caches nothing, so the next caller
    /// retries rather than inheriting the error.
    pub async fn get_or_refresh<F, Fut>(&self, fetch: F) -> Result<String, TrackError>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<TokenResponse, TrackError>> + Send,
    {
        let mut slot = self.inner.lock().await;
        if let Some(cached) = slot.as_ref()
            && Instant::now() < cached.good_until
        {
            return Ok(cached.token.clone());
        }
        let resp = fetch().await?;
        let ttl = resp.expires_in.map_or(DEFAULT_TTL, Duration::from_secs);
        // A token whose whole life is shorter than the skew is used immediately
        // and re-minted next call, rather than treated as born-expired.
        let good_until = Instant::now() + ttl.saturating_sub(EXPIRY_SKEW);
        let token = resp.access_token;
        *slot = Some(Cached {
            token: token.clone(),
            good_until,
        });
        Ok(token)
    }

    /// The whole client-credentials flow, cached: what UPS, FedEx and USPS each
    /// call once per track. `extra_form` carries any carrier-specific fields
    /// beyond `grant_type=client_credentials` (USPS wants `scope`).
    pub async fn client_credentials(
        &self,
        http: &reqwest::Client,
        token_url: &str,
        auth: ClientAuth<'_>,
        extra_form: &[(&str, &str)],
    ) -> Result<String, TrackError> {
        self.get_or_refresh(|| fetch_client_credentials(http, token_url, auth, extra_form))
            .await
    }
}

/// One uncached token call. Public so a carrier that needs a non-standard body
/// can still reuse the response shape and the error mapping.
pub async fn fetch_client_credentials(
    http: &reqwest::Client,
    token_url: &str,
    auth: ClientAuth<'_>,
    extra_form: &[(&str, &str)],
) -> Result<TokenResponse, TrackError> {
    let mut form: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
    form.extend_from_slice(extra_form);
    let mut req = http.post(token_url);
    match auth {
        ClientAuth::Basic {
            client_id,
            client_secret,
        } => {
            // Built by hand rather than via `basic_auth`, so the exact
            // `base64(id:secret)` UPS and USPS expect is what goes out.
            let encoded = STANDARD.encode(format!("{client_id}:{client_secret}"));
            req = req.header(reqwest::header::AUTHORIZATION, format!("Basic {encoded}"));
        }
        ClientAuth::Form {
            client_id,
            client_secret,
        } => {
            form.push(("client_id", client_id));
            form.push(("client_secret", client_secret));
        }
    }
    super::send_json(req.form(&form)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn response(token: &str, expires_in: u64) -> TokenResponse {
        TokenResponse {
            access_token: token.into(),
            expires_in: Some(expires_in),
        }
    }

    #[tokio::test]
    async fn live_token_is_reused() {
        let cache = TokenCache::new();
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let calls = calls.clone();
            let token = cache
                .get_or_refresh(|| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(response("tok", 3600))
                })
                .await
                .unwrap();
            assert_eq!(token, "tok");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn token_inside_the_skew_window_is_refetched() {
        let cache = TokenCache::new();
        let calls = Arc::new(AtomicUsize::new(0));
        // 30s < EXPIRY_SKEW: usable now, never cached for the next call.
        for _ in 0..2 {
            let calls = calls.clone();
            cache
                .get_or_refresh(|| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(response("tok", 30))
                })
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn missing_expiry_still_caches() {
        let cache = TokenCache::new();
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let calls = calls.clone();
            cache
                .get_or_refresh(|| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(TokenResponse {
                        access_token: "tok".into(),
                        expires_in: None,
                    })
                })
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_fetch_caches_nothing() {
        let cache = TokenCache::new();
        let err = cache
            .get_or_refresh(|| async { Err(TrackError::Auth) })
            .await
            .unwrap_err();
        assert_eq!(err, TrackError::Auth);
        let token = cache
            .get_or_refresh(|| async { Ok(response("tok", 3600)) })
            .await
            .unwrap();
        assert_eq!(token, "tok");
    }
}
