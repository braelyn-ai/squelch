//! Shared handler state.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, SemaphorePermit};

use crate::config::Config;
use crate::jwt::JwtSigner;
use crate::ratelimit::{REQUESTS_PER_MINUTE, RateLimiter};

/// State threaded through the router, the auth layer, and the rate limiter.
///
/// Cheap to clone (one `Arc`), matching how squelch-api clones `ApiState`. The
/// config, the HTTP client, the token cache, and the limiter are all
/// process-wide singletons: cloning must never fork the JWT cache or the
/// buckets.
#[derive(Clone)]
pub struct RelayState {
    inner: Arc<Inner>,
}

/// Process-wide ceiling on concurrent APNs requests. `FANOUT` in [`crate`]'s
/// handler bounds concurrency inside one batch only; without this, C concurrent
/// pushes open C*FANOUT sockets. 64 keeps a healthy pipe to Apple full while
/// staying a fixed, known number of file descriptors.
const MAX_INFLIGHT_APNS: usize = 64;

struct Inner {
    config: Config,
    http: reqwest::Client,
    signer: JwtSigner,
    limiter: Mutex<RateLimiter>,
    apns_inflight: Semaphore,
}

impl RelayState {
    /// Build state from validated config, constructing the APNs HTTP client and
    /// the JWT signer. Fails if the `.p8` is not a usable ES256 key — the relay
    /// refuses to start rather than discovering it on the first push.
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let signer = JwtSigner::new(
            &config.apns_key_pem,
            &config.apns_key_id,
            &config.apns_team_id,
        )?;
        // Connection reuse matters: APNs expects providers to hold a long-lived
        // HTTP/2 connection rather than reconnecting per push. `http2` is
        // negotiated by ALPN, so a plain-HTTP test override still works over 1.1.
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(600))
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                http,
                signer,
                limiter: Mutex::new(RateLimiter::per_minute(REQUESTS_PER_MINUTE)),
                apns_inflight: Semaphore::new(MAX_INFLIGHT_APNS),
            }),
        })
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    pub(crate) fn signer(&self) -> &JwtSigner {
        &self.inner.signer
    }

    /// The configured bearer token, or `None` when the push route is open.
    pub fn auth_token(&self) -> Option<&str> {
        self.inner.config.auth_token.as_deref()
    }

    /// Charge one push against `ip`'s bucket. False means "over the limit".
    ///
    /// Poisoning is RECOVERED, not propagated: the guarded value is a token
    /// bucket with no invariant a panic could corrupt, and `.expect()` here
    /// would let one unrelated panic brick every future push while `/healthz`
    /// kept answering 200, so nothing would ever restart the process.
    pub(crate) fn check_rate(&self, ip: IpAddr) -> bool {
        self.inner
            .limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(ip, Instant::now())
    }

    /// Wait for a slot in the process-wide APNs concurrency budget. The permit
    /// releases the slot when dropped.
    pub(crate) async fn apns_slot(&self) -> Option<SemaphorePermit<'_>> {
        // The semaphore is never closed, so this only fails if that changes;
        // proceeding without a permit is strictly better than dropping a push.
        self.inner.apns_inflight.acquire().await.ok()
    }
}
