//! Shared handler state.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::Config;
use crate::ratelimit::{
    CALLBACK_REQUESTS_PER_MINUTE, CLAIM_REQUESTS_PER_MINUTE, PAGE_REQUESTS_PER_MINUTE,
    REGISTER_REQUESTS_PER_MINUTE, RateLimiter,
};
use crate::sessions::{
    ClaimOutcome, NewSession, ParkOutcome, Parked, RegisterError, SessionKind, SessionStore,
};

/// State threaded through the router and both rate-limit layers. Cheap to clone
/// (one `Arc`): the config, the session table, and the limiters are
/// process-wide singletons, and cloning must never fork the table or the
/// buckets.
#[derive(Clone)]
pub struct BrokerState {
    inner: Arc<Inner>,
}

/// One limiter per route, never one shared: a 429 costs a different thing on
/// each of them, so they cannot share a budget. See [`crate::ratelimit`].
struct Inner {
    config: Config,
    sessions: SessionStore,
    register_limiter: Mutex<RateLimiter>,
    claim_limiter: Mutex<RateLimiter>,
    page_limiter: Mutex<RateLimiter>,
    callback_limiter: Mutex<RateLimiter>,
}

impl BrokerState {
    /// Build state from validated config. Nothing here can fail: the broker
    /// holds no key to parse and no database to open.
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                sessions: SessionStore::new(),
                register_limiter: Mutex::new(RateLimiter::per_minute(REGISTER_REQUESTS_PER_MINUTE)),
                claim_limiter: Mutex::new(RateLimiter::per_minute(CLAIM_REQUESTS_PER_MINUTE)),
                page_limiter: Mutex::new(RateLimiter::per_minute(PAGE_REQUESTS_PER_MINUTE)),
                callback_limiter: Mutex::new(RateLimiter::per_minute(CALLBACK_REQUESTS_PER_MINUTE)),
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// The session table itself. The router only ever reaches it through the
    /// wrappers below, which pass `Instant::now()`; this accessor is for a
    /// caller that needs to name the clock, which in practice means a test
    /// aging a session past its TTL.
    pub fn sessions(&self) -> &SessionStore {
        &self.inner.sessions
    }

    /// Register a session against the current clock, under this deployment's
    /// per-client ceiling.
    pub(crate) fn register(&self, new: NewSession) -> Result<(), RegisterError> {
        self.inner.sessions.register(
            new,
            self.inner.config.max_sessions_per_client(),
            Instant::now(),
        )
    }

    pub(crate) fn auth_url(&self, session_id: &str) -> Option<String> {
        self.inner.sessions.auth_url(session_id, Instant::now())
    }

    pub(crate) fn park(&self, session_id: &str, outcome: Parked) -> ParkOutcome {
        self.inner
            .sessions
            .park(session_id, outcome, Instant::now())
    }

    pub(crate) fn claim(&self, session_id: &str, presented: &[u8; 32]) -> ClaimOutcome {
        self.inner
            .sessions
            .claim(session_id, presented, Instant::now())
    }

    /// Drop every expired session; the periodic sweep the contract asks for on
    /// top of lazy purging. Returns how many went.
    pub fn sweep_expired(&self) -> usize {
        self.inner.sessions.sweep(Instant::now())
    }

    /// Live sessions. A count is the only thing about the table that may be
    /// logged.
    pub fn live_sessions(&self) -> usize {
        self.inner.sessions.len()
    }

    /// The session kind, for a handler deciding whether it serves that kind.
    pub(crate) fn session_kind(&self, session_id: &str) -> Option<SessionKind> {
        self.inner.sessions.kind(session_id, Instant::now())
    }

    /// Charge one registration against `ip`'s bucket. False means "over the
    /// limit". Poisoning is RECOVERED, not propagated: the guarded value is a
    /// token bucket with no invariant a panic could corrupt, and `.expect()`
    /// would brick every future request while `/healthz` kept answering 200.
    pub(crate) fn check_register_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.register_limiter, ip)
    }

    /// Charge one claim poll against `ip`'s separate bucket.
    pub(crate) fn check_claim_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.claim_limiter, ip)
    }

    /// Charge one page view against `ip`'s separate bucket.
    pub(crate) fn check_page_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.page_limiter, ip)
    }

    /// Charge one callback against `ip`'s separate bucket.
    pub(crate) fn check_callback_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.callback_limiter, ip)
    }

    fn charge(&self, limiter: &Mutex<RateLimiter>, ip: IpAddr) -> bool {
        limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(ip, Instant::now())
    }
}
