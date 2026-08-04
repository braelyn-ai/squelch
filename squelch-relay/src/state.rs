//! Shared handler state.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, SemaphorePermit};

use crate::config::Config;
use crate::jwt::JwtSigner;
use crate::opens::OpenStore;
use crate::ratelimit::{PIXEL_REQUESTS_PER_MINUTE, REQUESTS_PER_MINUTE, RateLimiter};

/// State threaded through the router, the auth layer, and the rate limiter.
/// Cheap to clone (one `Arc`): the config, HTTP client, token cache, and limiter
/// are process-wide singletons, and cloning must never fork the cache or the
/// buckets.
#[derive(Clone)]
pub struct RelayState {
    inner: Arc<Inner>,
}

/// Process-wide ceiling on concurrent APNs requests. `FANOUT` bounds one batch
/// only; without this, C concurrent pushes open C*FANOUT sockets.
const MAX_INFLIGHT_APNS: usize = 64;

struct Inner {
    config: Config,
    http: reqwest::Client,
    signer: JwtSigner,
    limiter: Mutex<RateLimiter>,
    /// The pixel route gets its OWN buckets. Both routes see the proxy's address
    /// and so share one bucket per limiter; on a single limiter, one popular
    /// newsletter's opens would spend the daemon's push budget.
    pixel_limiter: Mutex<RateLimiter>,
    apns_inflight: Semaphore,
    opens: OpenStore,
}

impl RelayState {
    /// Build state from validated config. Fails if the `.p8` is not a usable
    /// ES256 key, so the relay refuses to start rather than discovering it on
    /// the first push.
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let signer = JwtSigner::new(
            &config.apns_key_pem,
            &config.apns_key_id,
            &config.apns_team_id,
        )?;
        // APNs expects providers to hold a long-lived HTTP/2 connection rather
        // than reconnecting per push; ALPN negotiates it, so a plain-HTTP test
        // override still works over 1.1.
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(600))
            .timeout(Duration::from_secs(10))
            .build()?;
        let opens = OpenStore::open(config.db_path.as_deref())?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                http,
                signer,
                limiter: Mutex::new(RateLimiter::per_minute(REQUESTS_PER_MINUTE)),
                pixel_limiter: Mutex::new(RateLimiter::per_minute(PIXEL_REQUESTS_PER_MINUTE)),
                apns_inflight: Semaphore::new(MAX_INFLIGHT_APNS),
                opens,
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
    /// Poisoning is RECOVERED, not propagated: the guarded value is a token
    /// bucket with no invariant a panic could corrupt, and `.expect()` would
    /// brick every future push while `/healthz` kept answering 200.
    pub(crate) fn check_rate(&self, ip: IpAddr) -> bool {
        self.inner
            .limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(ip, Instant::now())
    }

    /// Charge one pixel fetch against `ip`'s separate bucket.
    pub(crate) fn check_pixel_rate(&self, ip: IpAddr) -> bool {
        self.inner
            .pixel_limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(ip, Instant::now())
    }

    /// The open buffer.
    pub(crate) fn opens(&self) -> &OpenStore {
        &self.inner.opens
    }

    /// Wait for a slot in the process-wide APNs concurrency budget; the permit
    /// releases it when dropped.
    pub(crate) async fn apns_slot(&self) -> Option<SemaphorePermit<'_>> {
        // The semaphore is never closed; proceeding without a permit beats
        // dropping a push if that ever changes.
        self.inner.apns_inflight.acquire().await.ok()
    }
}
