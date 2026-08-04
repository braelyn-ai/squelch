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
pub use sessions::{SESSION_TTL, SessionKind, SessionStore};
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
/// defenses are strict validation, high-entropy identifiers, a capped session
/// table, and per-IP rate limiting.
///
/// The two daemon-facing JSON routes and the two human-facing pages are metered
/// against SEPARATE buckets: browsers, prefetchers, and link scanners arrive on
/// `/link` and `/callback`, and on one limiter that traffic would spend the
/// budget a daemon needs to poll `/v1/claim`.
///
/// `/healthz` sits outside both layers so liveness answers while a client is
/// throttled.
pub fn router(state: BrokerState) -> Router {
    let daemon = Router::new()
        .route("/v1/sessions", post(handlers::register))
        .route("/v1/claim", post(handlers::claim))
        // The largest legitimate body is a consent URL plus two short strings.
        .layer(DefaultBodyLimit::max(handlers::MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_json,
        ))
        .with_state(state.clone());

    let pages = Router::new()
        .route("/link", get(handlers::link))
        .route("/callback", get(handlers::callback))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_page,
        ))
        .with_state(state);

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(daemon)
        .merge(pages)
}
