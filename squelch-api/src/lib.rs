//! squelch-api: the HUMAN DOOR.
//!
//! A rich, authenticated HTTP API for the user's own desktop client, served
//! under `/client/*`. Unlike the MCP surface (the AGENT DOOR at `/mcp`, which is
//! narrow, read-only, and structurally sealed-absent), the human door may carry
//! sealed METADATA and — on an explicit per-message pull only — a single sealed
//! body. It is also the ONLY place write/action capability will live.
//!
//! SECURITY:
//! - Every `/client/*` route sits behind bearer-token auth (see [`auth`]). The
//!   token is a static shared secret from `SQUELCH_API_TOKEN`; comparison is
//!   constant-time. If the token is unset the router REFUSES TO SERVE rather than
//!   serving open.
//! - `/client/search` excludes sealed rows in SQL, exactly like the MCP surface.
//! - `/client/sealed` returns sealed METADATA only (never bodies).
//! - `/client/sealed/{id}/reveal` returns exactly one sealed body and writes an
//!   audit row BEFORE returning; the response is marked `Cache-Control: no-store`.
//! - No secrets, tokens, or message bodies are ever logged.

mod auth;
mod error;
pub mod guard;
pub mod gmail_write;
mod handlers;
mod state;

pub use auth::require_bearer;
pub use error::ApiError;
pub use state::{ApiState, StateError};

use axum::{
    Router,
    middleware,
    routing::{delete, get, post, put},
};

/// Build the `/client/*` router for the human door.
///
/// The returned router already has bearer auth applied to every route via a
/// middleware layer; callers just `.merge`/`.nest` it into their top-level axum
/// app (or serve it directly, as the dev bin does). The `state` carries the
/// store, the active account, and the (already-validated, non-empty) bearer
/// token — construct it with [`ApiState::from_env`] / [`ApiState::new`], which
/// refuse to build without a token.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/client/updates", get(handlers::get_updates))
        .route(
            "/client/updates/{message_id}/status",
            post(handlers::set_update_status),
        )
        .route("/client/thread/{thread_id}", get(handlers::get_thread))
        .route("/client/search", get(handlers::search))
        .route("/client/rules", get(handlers::list_rules))
        .route("/client/rules", post(handlers::create_rule))
        .route("/client/rules/{id}", put(handlers::update_rule))
        .route("/client/rules/{id}", delete(handlers::delete_rule))
        .route("/client/sealed", get(handlers::list_sealed))
        .route(
            "/client/sealed/{message_id}/reveal",
            post(handlers::reveal_sealed),
        )
        .route("/client/audit", get(handlers::get_audit))
        .route("/client/stats", get(handlers::get_stats))
        // Action STUBS. Another agent implements these THIS SESSION on top of
        // this router; they slot in by replacing the stub handlers. Each returns
        // 501 with {"error":"actions not yet wired"} for now.
        .route("/client/actions/archive", post(handlers::action_archive))
        .route("/client/actions/label", post(handlers::action_label))
        .route("/client/actions/send", post(handlers::action_send))
        // Bearer auth wraps EVERY route above.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ))
        // CORS wraps auth (outermost) so OPTIONS preflights — which browsers
        // send WITHOUT the Authorization header — are answered instead of
        // 401ing. Permissive by design: the webview clients live on
        // tauri://localhost / http://localhost:1420 / proxied tailnet hosts,
        // bearer auth is the actual security boundary, and non-browser
        // clients ignore CORS entirely. No cookies are involved.
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(state)
}
