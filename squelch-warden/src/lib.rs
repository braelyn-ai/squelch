//! squelch-warden: the VPS-side tenant provisioner for hosted Passband.
//!
//! One box runs one squelchd per tenant under a systemd template unit, each
//! bound to its own loopback port, each fronted by Caddy at
//! `<label>.<base domain>`. The warden is the small privileged agent that makes
//! that happen: the control plane (`squelch-control`, on Railway) calls four
//! JSON routes here and the warden does all the local work — port allocation,
//! directories, credential files, env files, Caddy sites, `systemctl`, and the
//! `squelchd pair` exec that produces the app handoff.
//!
//! ## What the warden is trusted with, and what it is not
//!
//! It IS trusted with root on one box. It creates units and writes files as
//! root, so its bearer token is the box.
//!
//! It is NOT trusted with anyone's mail or anyone's tokens. Credentials arrive
//! as age ciphertext that the control plane encrypted to this box's recipient,
//! and the warden writes them to disk verbatim without ever being able to read
//! them: **the age identity belongs to the daemons, and the warden never holds
//! it** — it only names its path in each tenant's env file. That is why
//! [`validate::validate_ciphertext`] refuses a body that is not armored: an
//! unarmored one would mean the control plane had handed over a plaintext
//! refresh token, and this is the last place on the path that can notice.
//!
//! It also never links `squelch-core`. It has no store, no mail parser, and no
//! OAuth client.
//!
//! ## Layout
//!
//! - [`config`] — env-driven startup config, validated hard, token redacted.
//! - [`validate`] — everything the control plane may send.
//! - [`host`] — the two traits every side effect goes through, and their real
//!   implementations. Nothing else in the crate touches `std::fs` or
//!   `std::process`.
//! - [`state`] — the one flat JSON state file and port allocation.
//! - [`provision`] — the ordered provisioning sequence and its unwind.
//! - [`auth`] / [`handlers`] — the wire.
//!
//! Deployment artifacts (unit files, Caddyfile, the from-zero runbook) live in
//! `deploy/hosted/`.

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

pub mod auth;
pub mod config;
pub mod handlers;
pub mod host;
pub mod provision;
pub mod state;
#[cfg(test)]
pub mod testing;
pub mod validate;

pub use config::{Config, ConfigError};
pub use provision::{Pairing, ProvisionRequest, Provisioned, Warden, WardenError};

/// State threaded through the router. Cheap to clone (one `Arc`): the
/// provisioner holds the config, the host traits, and the mutex that serializes
/// state mutations, and cloning must never fork any of them.
#[derive(Clone)]
pub struct WardenState {
    warden: Arc<Warden>,
}

impl WardenState {
    pub fn new(warden: Arc<Warden>) -> Self {
        Self { warden }
    }

    /// The configured bearer. Reached only by [`auth::require_bearer`], and
    /// only as bytes for a constant-time compare.
    pub(crate) fn token(&self) -> &str {
        &self.warden.config().token
    }

    pub(crate) fn warden(&self) -> Arc<Warden> {
        self.warden.clone()
    }
}

/// Build the warden router.
///
/// Every `/v1` route sits behind the bearer gate and a body limit. `/healthz`
/// sits outside both: it is the liveness probe for systemd and for whatever
/// watches the box, it takes no input, and it says nothing but `ok`. Putting a
/// credential in front of that would only mean the probe has to hold one.
pub fn router(state: WardenState) -> Router {
    let v1 = Router::new()
        .route("/v1/tenants", post(handlers::create_tenant))
        .route(
            "/v1/tenants/{label}",
            get(handlers::get_tenant).delete(handlers::delete_tenant),
        )
        .route("/v1/tenants/{label}/pair", post(handlers::repair_tenant))
        .layer(DefaultBodyLimit::max(handlers::MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ))
        .with_state(state);

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(v1)
}
