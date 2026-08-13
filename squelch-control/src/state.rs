//! Shared handler state: the config, the control store, the pending-signup
//! table, the warden client, and one rate limiter per route.
//!
//! NO AGE RECIPIENT LIVES HERE. Under wire v2 each tenant has its own identity,
//! minted by the warden, and the recipient to seal to arrives per signup in the
//! answer to the first provisioning call. A recipient held in process state
//! would be a key shared by every mailbox.
//!
//! Cheap to clone (one `Arc`), because axum clones state per request and
//! cloning must never fork the session table or the buckets.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::bifrost::{BifrostClient, BifrostError};
use crate::config::{BifrostConfig, Config, OUTBOUND_TIMEOUT, WaitlistConfig};
use crate::ratelimit::{
    ADMIN_LOGIN_REQUESTS_PER_MINUTE, ADMIN_REQUESTS_PER_MINUTE, CALLBACK_REQUESTS_PER_MINUTE,
    CONSOLE_AUTH_REQUESTS_PER_MINUTE, PAGE_REQUESTS_PER_MINUTE, RateLimiter,
    SIGNUP_REQUESTS_PER_MINUTE, WAITLIST_REQUESTS_PER_MINUTE,
};
use crate::resend::{ResendClient, ResendError};
use crate::sessions::SessionStore;
use crate::store::ControlStore;
use crate::warden::Warden;

/// Why state could not be built. Both variants are a client that would not
/// construct, which is the only failure derivation has: everything else that
/// could be refused was refused when the config was validated.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(transparent)]
    Bifrost(#[from] BifrostError),
    #[error(transparent)]
    Resend(#[from] ResendError),
}

#[derive(Clone)]
pub struct ControlState {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    store: ControlStore,
    warden: Arc<dyn Warden>,
    /// The LLM gateway's governance client, DERIVED in [`ControlState::new`]
    /// from `config.bifrost` so it is present exactly when the config says the
    /// gateway is and can never disagree with it. `None` means signup
    /// provisions keyless tenants and the `llm mint` operator command
    /// backfills them later.
    bifrost: Option<Arc<BifrostClient>>,
    /// The invite mailer, DERIVED in [`ControlState::new`] from
    /// `config.waitlist` for the same reason the Bifrost client is: present
    /// exactly when the config says the feature is, never in disagreement with
    /// it. `None` means the waitlist and admin routes are not mounted at all.
    resend: Option<Arc<ResendClient>>,
    sessions: Mutex<SessionStore>,
    page_limiter: Mutex<RateLimiter>,
    signup_limiter: Mutex<RateLimiter>,
    /// `GET /console/auth`'s own bucket. Separate from signup's so console
    /// traffic cannot spend the budget a signup needs, and tighter, because
    /// opening a console session costs a stranger nothing to ask for.
    console_auth_limiter: Mutex<RateLimiter>,
    callback_limiter: Mutex<RateLimiter>,
    waitlist_limiter: Mutex<RateLimiter>,
    admin_limiter: Mutex<RateLimiter>,
    /// `POST /admin/login`'s own bucket, for the reason `console_auth_limiter`
    /// has one: it is the route where a stranger guesses at a secret, and it
    /// must not be able to spend the budget the operator's own page needs.
    admin_login_limiter: Mutex<RateLimiter>,
}

impl ControlState {
    /// Build state from validated config, an open store, and a warden client.
    ///
    /// The Bifrost client and the invite mailer are both DERIVED here from the
    /// config, so each feature has exactly one switch: the config. A caller
    /// cannot hand in a client the config does not describe, or forget one it
    /// does. Fallible only in the one way that derivation is (an HTTP client
    /// failing to build); everything else that could be refused was refused
    /// when the config was validated, and the only key material in the flow
    /// arrives per signup from the warden.
    pub fn new(
        config: Config,
        store: ControlStore,
        warden: Arc<dyn Warden>,
    ) -> Result<Self, StateError> {
        let bifrost = config
            .bifrost
            .as_ref()
            .map(|b| {
                BifrostClient::new(
                    b.url.clone(),
                    b.admin_token.clone(),
                    b.models.clone(),
                    OUTBOUND_TIMEOUT,
                )
                .map(Arc::new)
            })
            .transpose()?;
        let resend = config
            .waitlist
            .as_ref()
            .map(|w| {
                ResendClient::new(
                    w.resend_url.clone(),
                    w.resend_api_key.clone(),
                    w.invite_from.clone(),
                    OUTBOUND_TIMEOUT,
                )
                .map(Arc::new)
            })
            .transpose()?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                store,
                warden,
                bifrost,
                resend,
                sessions: Mutex::new(SessionStore::new()),
                page_limiter: Mutex::new(RateLimiter::per_minute(PAGE_REQUESTS_PER_MINUTE)),
                signup_limiter: Mutex::new(RateLimiter::per_minute(SIGNUP_REQUESTS_PER_MINUTE)),
                console_auth_limiter: Mutex::new(RateLimiter::per_minute(
                    CONSOLE_AUTH_REQUESTS_PER_MINUTE,
                )),
                callback_limiter: Mutex::new(RateLimiter::per_minute(CALLBACK_REQUESTS_PER_MINUTE)),
                waitlist_limiter: Mutex::new(RateLimiter::per_minute(WAITLIST_REQUESTS_PER_MINUTE)),
                admin_limiter: Mutex::new(RateLimiter::per_minute(ADMIN_REQUESTS_PER_MINUTE)),
                admin_login_limiter: Mutex::new(RateLimiter::per_minute(
                    ADMIN_LOGIN_REQUESTS_PER_MINUTE,
                )),
            }),
        })
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn store(&self) -> &ControlStore {
        &self.inner.store
    }

    pub fn warden(&self) -> &dyn Warden {
        self.inner.warden.as_ref()
    }

    /// The Bifrost governance client and its config, together, when this
    /// deployment has the gateway. One `Option` for the pair because both
    /// derive from `config.bifrost` in [`Self::new`]: no caller can see a
    /// client without its budget, or a budget without its client.
    pub fn bifrost(&self) -> Option<(&BifrostClient, &BifrostConfig)> {
        self.inner
            .bifrost
            .as_deref()
            .zip(self.inner.config.bifrost.as_ref())
    }

    /// The invite mailer and its settings, together, when this deployment has
    /// the waitlist. One `Option` for the pair for the same reason
    /// [`Self::bifrost`] is one: both derive from `config.waitlist` in
    /// [`Self::new`], so no caller can see a mailer without the admin token
    /// that gates it, or the token without a way to send.
    pub fn waitlist(&self) -> Option<(&ResendClient, &WaitlistConfig)> {
        self.inner
            .resend
            .as_deref()
            .zip(self.inner.config.waitlist.as_ref())
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

    pub(crate) fn check_console_auth_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.console_auth_limiter, ip)
    }

    pub(crate) fn check_callback_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.callback_limiter, ip)
    }

    pub(crate) fn check_waitlist_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.waitlist_limiter, ip)
    }

    pub(crate) fn check_admin_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.admin_limiter, ip)
    }

    pub(crate) fn check_admin_login_rate(&self, ip: IpAddr) -> bool {
        self.charge(&self.inner.admin_login_limiter, ip)
    }

    fn charge(&self, limiter: &Mutex<RateLimiter>, ip: IpAddr) -> bool {
        limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(ip, Instant::now())
    }
}
