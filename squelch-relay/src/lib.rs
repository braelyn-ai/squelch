//! squelch-relay: a blind APNs forwarder with a short-lived open buffer. It
//! holds the `.p8` signing key, a queue of (opaque token, timestamp, user
//! agent) rows waiting for their daemon, and nothing else — no device registry,
//! no mail. The push carries only an opaque `event_id` and a generic alert.
//!
//! PRIVACY: device token values, open tokens, request payloads, the bearer
//! token, and the signed APNs JWT are NEVER logged — counts, statuses, and
//! timing only.

pub mod auth;
pub mod config;
mod handlers;
pub mod jwt;
pub mod opens;
pub mod ratelimit;
mod state;

pub use config::{Cidr, Config, ConfigError, Environment};
pub use jwt::{JwtError, JwtSigner};
pub use opens::Open;
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
///
/// `GET /t/{token}` is the one route that must answer strangers: mail clients
/// fetching the pixel cannot present a bearer. It is rate-limited against its
/// own buckets, for the same reason auth is the outer layer on the daemon's.
///
/// `/v1/opens` is mounted ONLY when a bearer is configured. Anonymous mode is a
/// supported deployment for `/v1/push`, where the worst a stranger can do is
/// spend the bucket, but draining opens both harvests live tracking tokens and
/// destroys the buffer on the way out — that is not a nuisance, so an
/// auth-less relay simply has no drain. The pixel still buffers; nothing can
/// collect it until a bearer exists.
pub fn router(state: RelayState) -> Router {
    let authed = state.auth_token().is_some();

    let mut daemon = Router::new().route("/v1/push", post(handlers::push));
    if authed {
        daemon = daemon.route("/v1/opens", get(handlers::opens));
    }
    daemon = daemon.layer(middleware::from_fn_with_state(
        state.clone(),
        ratelimit::limit,
    ));
    if authed {
        daemon = daemon.layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));
    }
    let daemon = daemon.with_state(state.clone());

    let pixel = Router::new()
        .route("/t/{token}", get(handlers::pixel))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_pixel,
        ))
        .with_state(state);

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(daemon)
        .merge(pixel)
}
