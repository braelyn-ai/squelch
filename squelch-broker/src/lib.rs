//! squelch-broker: a consent relay that is never trusted. It parks a Google
//! OAuth authorization code for a few minutes so that the headless daemon which
//! requested it can claim the code and exchange it itself.
//!
//! It holds no OAuth client credentials, no tokens, and no mail. The code it
//! parks is cryptographically useless without the PKCE verifier, which never
//! leaves the daemon: we are not trusted with tokens because we are nice, we
//! are incapable of minting them. The wire contract is `docs/BROKER.md`.
//!
//! PRIVACY: session ids, claim tokens and their hashes, authorization codes,
//! and consent URLs are NEVER logged — not at debug, not on an error path.
//! Counts, statuses, and timings only.

pub mod config;
mod handlers;
pub mod ratelimit;
pub mod sessions;
mod state;
pub mod validate;

pub use config::{Config, ConfigError};
pub use sessions::{MAX_SESSIONS_PER_CLIENT, SESSION_TTL, SessionKind, SessionStore};
pub use state::BrokerState;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

/// Build the broker router.
///
/// Nothing here is authenticated: every stranger's self-hosted daemon is a
/// legitimate client, so there is no credential anyone could be asked for. The
/// defenses are strict validation of the registered consent URL (its scopes
/// included), high-entropy identifiers, a session table capped globally and per
/// client, and per-client rate limiting.
///
/// EVERY route carries its own buckets, because a 429 costs a different thing
/// on each: registration allocates and is rare, claim polling is frequent and
/// cheap, `/link` is opened by browsers and link scanners, and `/callback`
/// carries a consent the user has already granted and cannot grant twice. On
/// one shared limiter the cheapest traffic sets the price for all of them.
///
/// `/healthz` sits outside every layer so liveness answers while a client is
/// throttled.
pub fn router(state: BrokerState) -> Router {
    // The largest legitimate body is a consent URL plus two short strings.
    let register = Router::new()
        .route("/v1/sessions", post(handlers::register))
        .layer(DefaultBodyLimit::max(handlers::MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_register,
        ))
        .with_state(state.clone());

    let claim = Router::new()
        .route("/v1/claim", post(handlers::claim))
        .layer(DefaultBodyLimit::max(handlers::MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_claim,
        ))
        .with_state(state.clone());

    let link = Router::new()
        .route("/link", get(handlers::link))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_page,
        ))
        .with_state(state.clone());

    let callback = Router::new()
        .route("/callback", get(handlers::callback))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_callback,
        ))
        .with_state(state);

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(register)
        .merge(claim)
        .merge(link)
        .merge(callback)
}
