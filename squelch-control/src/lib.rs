//! squelch-control: the hosted signup control plane (Passband hosted tier).
//!
//! It runs on Railway, holds the confidential web OAuth client, and turns one
//! invite code plus one Google consent into one provisioned tenant daemon on the
//! VPS. The provisioning itself belongs to `squelch-warden`, which this crate
//! only ever talks to over the wire contract in `docs/HOSTED.md`.
//!
//! THE TRUST SPLIT IS THE WHOLE DESIGN, and every module here exists to keep one
//! half of it:
//!
//! - This process holds the OAuth client secret and an age RECIPIENT — a public
//!   key. It can seal a tenant's token; it cannot open one. There is no identity
//!   file on Railway and no code path here that would read one.
//! - A plaintext refresh token exists ONLY in memory, between the token exchange
//!   returning and [`seal`] encrypting it. It is never written to the control
//!   store, never logged at any level, and never rendered into a page.
//! - The warden receives ciphertext (age ASCII armor) and writes it verbatim.
//!   Only the tenant's daemon, handed the identity by systemd, can decrypt.
//!
//! PRIVACY, enforced by review of every `tracing` call in this crate: invite
//! codes, their hashes, pairing codes, OAuth codes, `state`, PKCE verifiers,
//! session ids, cookie MACs, the warden bearer, and access/refresh tokens NEVER
//! reach a log line. Labels, account emails, statuses, and counts may.

pub mod config;
pub mod cookie;
pub mod handlers;
pub mod invites;
pub mod labels;
pub mod oauth;
pub mod pages;
pub mod ratelimit;
pub mod seal;
pub mod sessions;
pub mod state;
pub mod store;
pub mod warden;

pub use config::{Config, ConfigError};
pub use state::ControlState;
pub use store::ControlStore;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

/// Build the control-plane router.
///
/// Three surfaces, three budgets, because a 429 costs a different thing on each
/// (the shape is stolen from `squelch-broker`, which learned it the same way):
///
/// - `GET /` is a page. Browsers prefetch it and link scanners fetch it, and
///   that traffic must not spend the budget signup needs.
/// - `POST /signup` is tight. It validates an invite code, so it is the one
///   route where a stranger can guess at a secret, and a real human posts it
///   once.
/// - `GET /oauth/callback` is the most generous. Refusing it destroys a consent
///   the user has ALREADY granted at Google, which they cannot grant twice
///   without walking the whole flow again.
///
/// `/healthz` sits outside every layer so liveness answers while a client is
/// throttled.
pub fn router(state: ControlState) -> Router {
    let form = Router::new()
        .route("/", get(handlers::signup_form))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_page,
        ))
        .with_state(state.clone());

    let signup = Router::new()
        .route("/signup", post(handlers::signup))
        .layer(DefaultBodyLimit::max(handlers::MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_signup,
        ))
        .with_state(state.clone());

    let callback = Router::new()
        .route("/oauth/callback", get(handlers::oauth_callback))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_callback,
        ))
        .with_state(state);

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(form)
        .merge(signup)
        .merge(callback)
}
