//! squelch-relay: a blind, stateless APNs forwarder. It holds the `.p8` signing
//! key and nothing else — no database, no device registry, no mail. The push
//! carries only an opaque `event_id` and a generic alert.
//!
//! PRIVACY: device token values, request payloads, the bearer token, and the
//! signed APNs JWT are NEVER logged — counts, statuses, and timing only.

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

/// Build the relay router. `/healthz` sits outside both layers so liveness
/// answers while a client is throttled. Auth is layered OUTSIDE the rate limit
/// so unauthenticated junk never spends the bucket — behind the expected TLS
/// proxy the whole deployment shares one, so the reverse order would let any
/// internet client 429 the real daemon with a flood of 401s.
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
