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

/// The longest lifetime we will honor from a carrier, mirroring what
/// `MAX_RETRY_AFTER` does to the other carrier-supplied duration in this module.
/// A day is far past any real carrier token (UPS's is four hours, FedEx's one)
/// so the clamp costs nothing, and it keeps a garbage `expires_in` — `u64::MAX`,
/// or UPS's string spelling of it — from overflowing the `Instant` arithmetic
/// below and PANICKING the poller task out of existence.
const MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A carrier's token endpoint response. Every field beyond these two is ignored;
/// carriers add their own (`token_type`, `issued_at`, `scope`) and change them.
#[derive(Clone, Deserialize)]
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

/// Hand-written so a bearer token can never ride out through a stray `{:?}`,
/// the way every other secret-bearing struct in the workspace does it (see the
/// carrier cred structs in `crate::config`). The module header promises no
/// credential leaves here in an error or a log; a derived `Debug` over a field
/// named `access_token` is exactly how that promise gets broken later.
impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Where a carrier wants the client id and secret on the token request.
#[derive(Clone, Copy)]
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

/// Hand-written for the same reason as [`TokenResponse`]'s: the client id is
/// safe to show, the secret half never is.
impl std::fmt::Debug for ClientAuth<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (variant, client_id) = match self {
            ClientAuth::Basic { client_id, .. } => ("Basic", client_id),
            ClientAuth::Form { client_id, .. } => ("Form", client_id),
        };
        f.debug_struct(variant)
            .field("client_id", client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
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
        // `expires_in` is carrier input like any other field here, so it is
        // CLAMPED rather than trusted: `Instant + Duration` panics on overflow,
        // and that panic would unwind out of the poll and take the whole poller
        // task with it — silently, until shutdown. Degrade, never panic.
        let ttl = resp
            .expires_in
            .map_or(DEFAULT_TTL, Duration::from_secs)
            .min(MAX_TTL);
        // A token whose whole life is shorter than the skew is used immediately
        // and re-minted next call, rather than treated as born-expired. The
        // `checked_add` is belt-and-braces behind the clamp: an unrepresentable
        // instant yields "already expired", which costs one extra token call.
        let now = Instant::now();
        let good_until = now
            .checked_add(ttl.saturating_sub(EXPIRY_SKEW))
            .unwrap_or(now);
        let token = resp.access_token;
        *slot = Some(Cached {
            token: token.clone(),
            good_until,
        });
        Ok(token)
    }

    /// Drop the cached token, so the next call mints a fresh one.
    ///
    /// Called when the carrier REJECTED the token we hold. Local expiry is only
    /// our belief about the carrier's; a token can die server-side (key rotation
    /// at their gateway, an app re-provisioned) while `good_until` still says it
    /// is live, and without this the same dead token is replayed until its full
    /// stated lifetime elapses, which for USPS is eight hours.
    pub async fn invalidate(&self) {
        *self.inner.lock().await = None;
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

    /// A carrier `expires_in` of `u64::MAX` (UPS spells it as a STRING, which
    /// parses to `u64::MAX` happily) once reached `Instant::now() + ttl` and
    /// PANICKED — unwinding out of the poll and killing the spawned poller task
    /// for the life of the process. It is clamped now, so the token caches
    /// normally instead.
    #[tokio::test]
    async fn an_absurd_expiry_is_clamped_not_panicked() {
        for absurd in [
            u64::MAX,
            i64::MAX as u64,
            u64::MAX / 2,
            MAX_TTL.as_secs() + 1,
        ] {
            let cache = TokenCache::new();
            let calls = Arc::new(AtomicUsize::new(0));
            for _ in 0..2 {
                let calls = calls.clone();
                let token = cache
                    .get_or_refresh(|| async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(response("tok", absurd))
                    })
                    .await
                    .unwrap();
                assert_eq!(token, "tok");
            }
            // Clamped to MAX_TTL, so it is a normal live token: minted once,
            // reused on the second call.
            assert_eq!(calls.load(Ordering::SeqCst), 1, "expires_in {absurd}");
        }
    }

    /// The whole point of the string spelling: it is the wire path an absurd
    /// TTL actually arrives on, and it must not panic either.
    #[test]
    fn a_string_expiry_of_u64_max_parses_and_survives_the_clamp() {
        let parsed: TokenResponse =
            serde_json::from_str(r#"{"access_token":"t","expires_in":"18446744073709551615"}"#)
                .unwrap();
        assert_eq!(parsed.expires_in, Some(u64::MAX));
        let ttl = parsed
            .expires_in
            .map_or(DEFAULT_TTL, Duration::from_secs)
            .min(MAX_TTL);
        assert_eq!(ttl, MAX_TTL);
        assert!(Instant::now().checked_add(ttl).is_some());
    }

    /// Mirrors config.rs's `carrier_debug_redacts_the_secrets`: nothing in this
    /// module may put credential material in a log through a stray `{:?}`.
    #[test]
    fn debug_redacts_token_and_client_secret() {
        let rendered = format!("{:?}", response("super-secret-token", 3600));
        assert!(
            !rendered.contains("super-secret-token"),
            "access token leaked: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
        // The non-secret field stays visible — that is what makes it useful.
        assert!(rendered.contains("3600"));

        for auth in [
            ClientAuth::Basic {
                client_id: "the-id",
                client_secret: "ups-sekret",
            },
            ClientAuth::Form {
                client_id: "the-id",
                client_secret: "fedex-sekret",
            },
        ] {
            let rendered = format!("{auth:?}");
            assert!(
                !rendered.contains("sekret"),
                "client secret leaked: {rendered}"
            );
            assert!(rendered.contains("<redacted>"));
            assert!(rendered.contains("the-id"));
        }
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
