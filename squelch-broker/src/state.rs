//! Shared handler state.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::Config;
use crate::ratelimit::{JSON_REQUESTS_PER_MINUTE, PAGE_REQUESTS_PER_MINUTE, RateLimiter};
use crate::sessions::{
    ClaimOutcome, ParkOutcome, Parked, RegisterError, SessionKind, SessionStore,
};

/// State threaded through the router and both rate-limit layers. Cheap to clone
/// (one `Arc`): the config, the session table, and the limiters are
/// process-wide singletons, and cloning must never fork the table or the
/// buckets.
#[derive(Clone)]
pub struct BrokerState {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    sessions: SessionStore,
    limiter: Mutex<RateLimiter>,
    /// The human-facing pages get their OWN buckets: behind the expected proxy
    /// both limiters see one address, and on a single limiter a link scanner
    /// would spend the daemons' claim budget.
    page_limiter: Mutex<RateLimiter>,
}

impl BrokerState {
    /// Build state from validated config. Nothing here can fail: the broker
    /// holds no key to parse and no database to open.
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                sessions: SessionStore::new(),
                limiter: Mutex::new(RateLimiter::per_minute(JSON_REQUESTS_PER_MINUTE)),
                page_limiter: Mutex::new(RateLimiter::per_minute(PAGE_REQUESTS_PER_MINUTE)),
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

    /// Register a session against the current clock.
    pub(crate) fn register(
        &self,
        session_id: String,
        kind: SessionKind,
        claim_token_hash: [u8; 32],
        auth_url: String,
    ) -> Result<(), RegisterError> {
        self.inner
            .sessions
            .register(session_id, kind, claim_token_hash, auth_url, Instant::now())
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

    /// Charge one JSON request against `ip`'s bucket. False means "over the
    /// limit". Poisoning is RECOVERED, not propagated: the guarded value is a
    /// token bucket with no invariant a panic could corrupt, and `.expect()`
    /// would brick every future request while `/healthz` kept answering 200.
    pub(crate) fn check_json_rate(&self, ip: IpAddr) -> bool {
        self.inner
            .limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(ip, Instant::now())
    }

    /// Charge one page view against `ip`'s separate bucket.
    pub(crate) fn check_page_rate(&self, ip: IpAddr) -> bool {
        self.inner
            .page_limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(ip, Instant::now())
    }
}
