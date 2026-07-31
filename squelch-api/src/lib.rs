//! squelch-api: the HUMAN DOOR — the authenticated `/client/*` API for the
//! user's own client. Bearer auth on every route; the router refuses to serve
//! without a token. Carries sealed METADATA and exactly one sealed body (audited
//! before it is served, `no-store`), and is the only surface with write
//! capability. No token, secret, or message body is ever logged. See
//! docs/SECURITY.md §4.

mod auth;
mod devices;
mod error;
mod events;
pub mod guard;
pub mod gmail_write;
mod handlers;
mod state;
pub mod unsubscribe;

pub use auth::require_bearer;
pub use error::ApiError;
/// The auth-mail retention pass. Exported for the daemon's timer: it uses the
/// WRITE credential, which the readonly-bound sync loop must never touch.
pub use handlers::run_shred_pass;
pub use state::{ApiState, StateError, attach_event_channel};

use axum::{
    Router,
    middleware,
    routing::{delete, get, post, put},
};

/// Build the `/client/*` router. Bearer auth is already layered over every
/// route; [`ApiState::from_env`] / [`ApiState::new`] refuse an empty token.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/client/updates", get(handlers::get_updates))
        .route(
            "/client/updates/{message_id}/status",
            post(handlers::set_update_status),
        )
        .route("/client/refresh", post(handlers::refresh_now))
        .route("/client/thread/{thread_id}", get(handlers::get_thread))
        .route(
            "/client/attachments/{id}",
            get(handlers::get_attachment),
        )
        .route("/client/shipments", get(handlers::get_shipments))
        .route("/client/receipts", get(handlers::get_receipts))
        .route("/client/banking", get(handlers::get_banking))
        .route("/client/calendar", get(handlers::get_calendar))
        .route("/client/search", get(handlers::search))
        .route("/client/rules", get(handlers::list_rules))
        .route("/client/rules", post(handlers::create_rule))
        .route("/client/rules/{id}", put(handlers::update_rule))
        .route("/client/rules/{id}", delete(handlers::delete_rule))
        // Local drafts: served EXCLUSIVELY here. Unsent compositions stay on
        // this machine — the agent door has no drafts route and never learns the
        // table exists. PUT is keyed (one draft per reply target, one for new
        // mail), so it edits in place instead of piling up rows.
        .route(
            "/client/drafts",
            get(handlers::list_drafts).put(handlers::put_draft),
        )
        .route("/client/drafts/{id}", delete(handlers::delete_draft))
        .route("/client/sealed", get(handlers::list_sealed))
        .route(
            "/client/sealed/{message_id}/reveal",
            post(handlers::reveal_sealed),
        )
        .route(
            "/client/shredder",
            get(handlers::get_shredder).post(handlers::set_shredder),
        )
        .route("/client/shredder/run", post(handlers::run_shredder))
        .route("/client/marketing", get(handlers::get_marketing))
        .route("/client/audit", get(handlers::get_audit))
        .route("/client/stats", get(handlers::get_stats))
        .route("/client/usage", get(handlers::get_usage))
        .route(
            "/client/triage-config",
            get(handlers::get_triage_config).post(handlers::set_triage_config),
        )
        .route(
            "/client/triage-feedback",
            get(handlers::get_triage_feedback).post(handlers::post_triage_feedback),
        )
        // Dev re-triage + inspector: human-door only.
        .route("/client/retriage", post(handlers::retriage))
        .route(
            "/client/triage-debug/{message_id}",
            get(handlers::triage_debug),
        )
        // Unsubscribe: human-door only (never exposed on the agent door).
        .route("/client/unsubscribe", post(handlers::unsubscribe))
        .route("/client/unsubscribes", get(handlers::list_unsubscribes))
        .route(
            "/client/unsubscribes/resolution",
            post(handlers::unsubscribe_resolution),
        )
        // Notification delivery: the SSE feed plus the iOS NSE's by-id fetch.
        // Human door only — the agent door gains no access to the event log.
        .route("/client/events", get(events::events_stream))
        .route("/client/events/{id}", get(events::get_event))
        // Push-device registration. Human door only — the agent door never
        // learns a device exists.
        .route("/client/devices", post(devices::register_device))
        // Unregister carries the token in the BODY, not a path segment: a device
        // token is capability material, and a URL path is the most-logged part
        // of a request.
        .route(
            "/client/devices/unregister",
            post(devices::unregister_device),
        )
        // Actions: the only write capability. Require the opt-in write
        // credential; 403 without one.
        .route("/client/actions/archive", post(handlers::action_archive))
        .route("/client/actions/label", post(handlers::action_label))
        .route("/client/actions/send", post(handlers::action_send))
        // Bearer auth wraps EVERY route above.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ))
        // CORS wraps auth (outermost) so credential-less OPTIONS preflights are
        // answered instead of 401ing. Permissive by design: bearer auth is the
        // security boundary and no cookies are involved.
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(state)
}
