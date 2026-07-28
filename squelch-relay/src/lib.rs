//! squelch-relay: the BLIND APNs RELAY.
//!
//! A stateless forwarder that turns `POST /v1/push` into APNs pushes. It holds
//! the `.p8` signing key that cannot ship inside every user's daemon, and
//! nothing else: no database, no device registry, no mail. See `README.md` for
//! the design rationale.
//!
//! PRIVACY (hard invariant from the design doc):
//! - The APNs payload carries an opaque `event_id` and a generic alert. No
//!   subject, sender, or body ever reaches this process.
//! - Device token VALUES, request payloads, the bearer token, and the signed
//!   APNs JWT are NEVER logged. Log lines carry token COUNTS, HTTP statuses,
//!   and timing only.
//!
//! SECURITY:
//! - `POST /v1/push` is bearer-authed: `SQUELCH_RELAY_AUTH_TOKEN` is REQUIRED
//!   unless `SQUELCH_RELAY_ALLOW_ANONYMOUS=1` says otherwise, and comparison is
//!   constant-time (see [`auth`]). Running open is a v1 allowance the design
//!   doc makes, but it has to be asked for, never defaulted into.
//! - A per-client-IP token bucket sits INSIDE that auth layer (see
//!   [`ratelimit`]), so unauthenticated junk cannot spend a real client's
//!   budget.
//! - The APNs payload is bounded (opaque id ≤ 256 bytes, encoded body ≤ 4 KB)
//!   and the fan-out has a wall-clock budget, so one request can never become
//!   an amplifier or hold a connection open indefinitely.
//! - Device tokens are validated as hex before being interpolated into the APNs
//!   path, so a request body can never reshape the outgoing URL.
//! - The bind default is loopback; a TLS proxy is expected to front it.

pub mod auth;
pub mod config;
mod handlers;
pub mod jwt;
pub mod ratelimit;
mod state;

pub use config::{Config, ConfigError, Environment};
pub use jwt::{JwtError, JwtSigner};
pub use state::RelayState;

use axum::{
    Router, middleware,
    routing::{get, post},
};

/// Build the relay router.
///
/// `/healthz` is deliberately outside both the auth and the rate-limit layers:
/// it is the process liveness probe and must answer even when a client is being
/// throttled. Everything else lives under `/v1/*`.
///
/// Layer order on the push route is auth OUTSIDE the rate limit, so an
/// unauthenticated request never spends the bucket. The reverse order saves a
/// constant-time comparison per junk request and pays for it with availability:
/// behind the expected TLS proxy the whole deployment shares one bucket, so any
/// internet client could 429 the legitimate daemon with a flood of 401s.
pub fn router(state: RelayState) -> Router {
    let mut push = Router::new().route("/v1/push", post(handlers::push)).layer(
        middleware::from_fn_with_state(state.clone(), ratelimit::limit),
    );
    if state.auth_token().is_some() {
        push = push.layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));
    }
    let push = push.with_state(state);

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(push)
}
