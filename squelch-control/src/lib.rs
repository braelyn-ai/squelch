//! squelch-control: the hosted signup control plane (Passband hosted tier).
//!
//! It runs on Railway, holds the confidential web OAuth client, and turns one
//! invite code plus one Google consent into one provisioned tenant daemon, which
//! runs as its own pod. The provisioning itself belongs to `squelch-warden`, and
//! this crate only ever talks to it over the two-call wire contract spelled out
//! in [`warden`].
//!
//! THE TRUST SPLIT IS THE WHOLE DESIGN, and every module here exists to keep one
//! half of it:
//!
//! - This process holds the OAuth client secret. It holds no age identity, and
//!   as of wire v2 no long-lived recipient either: the warden mints an identity
//!   PER TENANT and answers the first provisioning call with the public half, so
//!   what this process can do is seal one tenant's token to one key it cannot
//!   open, for the length of one request.
//! - A plaintext refresh token exists ONLY in memory, between the token exchange
//!   returning and [`seal`] encrypting it. It is never written to the control
//!   store, never logged at any level, and never rendered into a page.
//! - The warden receives ciphertext (age ASCII armor) and writes it verbatim
//!   into that tenant's Secret. Only that tenant's daemon, whose pod mounts the
//!   matching identity, can decrypt, and it opens nothing else.
//!
//! PRIVACY, enforced by review of every `tracing` call in this crate: invite
//! codes, their hashes, pairing codes, OAuth codes, `state`, PKCE verifiers,
//! session ids, cookie MACs, the warden bearer, the admin token, the Resend
//! key, and access/refresh tokens NEVER reach a log line. Labels, account
//! emails, statuses, and counts may.
//!
//! A WAITLIST ADDRESS MAY NOT. It belongs to somebody who is not a tenant and
//! has consented to nothing but being told when there is room, so the waitlist
//! and admin paths log the ROW ID and never the address they hold.

pub mod admin;
pub mod bifrost;
pub mod config;
pub mod cookie;
pub mod handlers;
pub mod invites;
pub mod labels;
pub mod oauth;
pub mod pages;
pub mod ratelimit;
pub mod resend;
pub mod seal;
pub mod sessions;
pub mod state;
pub mod store;
pub mod warden;

pub use config::{Config, ConfigError};
pub use state::{ControlState, StateError};
pub use store::ControlStore;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

/// Build the control-plane router.
///
/// Seven surfaces, seven budgets, because a 429 costs a different thing on each
/// (the shape is stolen from `squelch-broker`, which learned it the same way):
///
/// - `GET /` is a page. Browsers prefetch it and link scanners fetch it, and
///   that traffic must not spend the budget signup needs.
/// - `POST /signup` is tight. It validates an invite code, so it is the one
///   route where a stranger can guess at a secret, and a real human posts it
///   once.
/// - `GET /console/auth` and `GET /app/auth` are TIGHTER, and share ONE bucket.
///   They are the only routes that open a server-side session with nothing
///   presented at all, so they are the cheapest things here to loop; a real
///   human hits one of them once per sign in, and anything sustained above the
///   budget is somebody walking the label space. They share a bucket because
///   they are the same errand through the same consent: a budget each would let
///   one client spend twice by alternating them. The pair used to share
///   signup's budget, which meant login traffic could spend it.
/// - `GET /oauth/callback` is the most generous. Refusing it destroys a consent
///   the user has ALREADY granted at Google, which they cannot grant twice
///   without walking the whole flow again. All three flows come back through it.
/// - `POST /waitlist` is small. It is a stranger-facing WRITE, a person joins
///   once, and everything above the budget is rows of junk for an operator to
///   read past; a real submitter who is refused loses one retry, because the
///   answer is the same 200 whether the address was new or not.
/// - `POST /admin/login` is tight and in its OWN bucket, for the reason
///   `/console/auth` has one: it is the second route where a stranger guesses at
///   a secret, and it must not be able to spend the budget the dashboard needs.
/// - The admin dashboard and its two buttons are generous. One operator with a
///   cookie, clicking through a list that re-renders after every approval.
///
/// THE LAST THREE ARE MOUNTED ONLY WHEN THE WAITLIST IS CONFIGURED. With the
/// feature off they do not exist, so an unconfigured deployment answers 404 at
/// an admin URL rather than 403: there is no door to knock on, and no admin
/// token for a stranger to spend their bucket guessing at.
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

    // BOTH LOGIN HOPS, on ONE bucket. They are the same errand through the same
    // consent, and a limiter each would let the same client spend twice by
    // alternating them.
    let console = Router::new()
        .route("/console/auth", get(handlers::console_auth))
        .route("/app/auth", get(handlers::app_auth))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_console_auth,
        ))
        .with_state(state.clone());

    let callback = Router::new()
        .route("/oauth/callback", get(handlers::oauth_callback))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_callback,
        ))
        .with_state(state.clone());

    let app = Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(form)
        .merge(signup)
        .merge(console)
        .merge(callback);

    if state.config().waitlist.is_none() {
        return app;
    }

    let waitlist = Router::new()
        .route(
            "/waitlist",
            post(handlers::waitlist).options(handlers::waitlist_preflight),
        )
        .layer(DefaultBodyLimit::max(handlers::MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_waitlist,
        ))
        // OUTERMOST, so the throttle's 429 and the body limit's 413 carry the
        // CORS headers too. A refusal a browser cannot read is a refusal the
        // form has to report as a network failure.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            handlers::waitlist_cors,
        ))
        .with_state(state.clone());

    let admin_login = Router::new()
        .route("/admin/login", post(admin::login))
        .layer(DefaultBodyLimit::max(handlers::MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_admin_login,
        ))
        .with_state(state.clone());

    let admin = Router::new()
        .route("/admin", get(admin::page))
        .route("/admin/invite", post(admin::invite))
        .route("/admin/approve", post(admin::approve))
        .route("/admin/send", post(admin::send))
        .route("/admin/logout", post(admin::logout))
        .layer(DefaultBodyLimit::max(handlers::MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_admin,
        ))
        .with_state(state);

    app.merge(waitlist).merge(admin_login).merge(admin)
}
