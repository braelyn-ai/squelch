//! Shared handler state: the config, the control store, the pending-signup
//! table, the warden client, the age recipient, and one rate limiter per route.
//!
//! Cheap to clone (one `Arc`), because axum clones state per request and
//! cloning must never fork the session table or the buckets.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use age::x25519::Recipient;

use crate::config::Config;
use crate::ratelimit::{
    CALLBACK_REQUESTS_PER_MINUTE, PAGE_REQUESTS_PER_MINUTE, RateLimiter,
    SIGNUP_REQUESTS_PER_MINUTE,
};
use crate::sessions::SessionStore;
use crate::store::ControlStore;
use crate::warden::Warden;

#[derive(Clone)]
pub struct ControlState {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    store: ControlStore,
    /// The box's age recipient, parsed once at startup. A public key: this
    /// process can seal and cannot open.
    recipient: Recipient,
    warden: Arc<dyn Warden>,
    sessions: Mutex<SessionStore>,
    page_limiter: Mutex<RateLimiter>,
    signup_limiter: Mutex<RateLimiter>,
    callback_limiter: Mutex<RateLimiter>,
}

impl ControlState {
    /// Build state from validated config, an open store, and a warden client.
    /// The recipient is parsed here so a bad one is a startup failure and never
    /// a failure discovered after a user has granted consent.
    pub fn new(
        config: Config,
        store: ControlStore,
        warden: Arc<dyn Warden>,
    ) -> Result<Self, crate::seal::SealError> {
        let recipient = crate::seal::parse_recipient(&config.age_recipient)?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                store,
                recipient,
                warden,
                sessions: Mutex::new(SessionStore::new()),
                page_limiter: Mutex::new(RateLimiter::per_minute(PAGE_REQUESTS_PER_MINUTE)),
                signup_limiter: Mutex::new(RateLimiter::per_minute(SIGNUP_REQUESTS_PER_MINUTE)),
                callback_limiter: Mutex::new(RateLimiter::per_minute(CALLBACK_REQUESTS_PER_MINUTE)),
            }),
        })
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn store(&self) -> &ControlStore {
        &self.inner.store
    }

    pub fn recipient(&self) -> &Recipient {
        &self.inner.recipient
    }

    pub fn warden(&self) -> &dyn Warden {
        self.inner.warden.as_ref()
    }

    /// The pending-signup table. Poisoning is RECOVERED rather than propagated:
    /// the guarded value is a map with no invariant a panic could corrupt, and
    /// an `.expect()` would brick every later signup while `/healthz` kept
    /// answering 200.
    pub fn sessions(&self) -> MutexGuard<'_, SessionStore> {
        self.inner.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop every expired session. Called by the background sweep.
    pub fn sweep_sessions(&self) -> usize {
        self.sessions().sweep(Instant::now())
    }

    /// Live pending signups. A COUNT is the only thing about that table that
    /// may be logged.
    pub fn live_sessions(&self) -> usize {
        self.sessions().len()
    }

    pub(crate) fn check_page_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.page_limiter, ip)
    }

    pub(crate) fn check_signup_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.signup_limiter, ip)
    }

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
