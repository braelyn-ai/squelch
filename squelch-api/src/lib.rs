//! squelch-api: the HUMAN DOOR — the `/client/*` API for the user's own client.
//! Bearer auth on every route but the pairing claim, which is how a device with
//! no credential gets one. Carries sealed METADATA and exactly one sealed body
//! (audited before it is served, `no-store`), and is the only surface with write
//! capability. No token, secret, or message body is ever logged. See
//! docs/SECURITY.md §4.

mod assistant;
mod auth;
mod console;
mod devices;
mod error;
mod events;
pub mod gmail_write;
pub mod guard;
mod handlers;
mod invite_mail;
mod markdown;
mod pair;
mod sharing;
mod state;
pub mod tracking;
pub mod unsubscribe;

pub use assistant::AssistantRelay;
pub use auth::require_bearer;
pub use error::ApiError;
/// The auth-mail retention pass. Exported for the daemon's timer: it uses the
/// WRITE credential, which the readonly-bound sync loop must never touch.
pub use handlers::run_shred_pass;
/// The invite minter. Exported for the daemon binaries that build state by
/// hand and for the integration suite, which points it at a mock control plane.
pub use sharing::Sharing;
pub use state::{ApiState, StateError, attach_event_channel};

use axum::{
    Router, middleware,
    routing::{delete, get, post, put},
};

/// Build this crate's router: the bearer-authed `/client/*` tree, plus the TWO
/// unauthenticated routes, each in its own single-route router so the auth
/// boundary is a line you can see rather than a layer you have to trace:
///
/// - `GET /t/{token}` ([`tracking::pixel_router`]) — the read-tracking pixel,
///   fetched by a recipient's mail client, which has no token and never will;
/// - `POST /client/pair` ([`pair::pair_router`]) — the pairing claim, which is
///   how a device with no credential gets its first one.
///
/// Nothing else is ever added to either. Everything in [`client_router`] is
/// behind the bearer.
///
/// `/console` ([`console::console_router`]) is the third tree, and the only one
/// authenticated by a COOKIE rather than a header. It sits outside the bearer
/// layer (signing in has to be possible without a credential) and outside the
/// `/client` CORS layer (a browser attaches a cookie by itself, so the
/// permissive CORS the JSON tree can afford would be a gift to any page on the
/// web). What it authenticates with is not a new credential: a console session
/// IS a device token, verified by the same store call.
pub fn router(state: ApiState) -> Router {
    client_router(state.clone())
        .merge(pair::pair_router(state.clone()))
        .merge(console::console_router(state.clone()))
        .merge(tracking::pixel_router(state))
        // The bare root: a browser typing the tenant hostname lands on the
        // console's login page instead of a 404. A redirect carries nothing,
        // so it needs no auth; on hosted the Ingress publishes "/" as an
        // EXACT match so this stays the only extra path a vhost serves.
        .route(
            "/",
            axum::routing::get(|| async { axum::response::Redirect::temporary("/console") }),
        )
}

/// The `/client/*` router. Bearer auth is layered over every route in it: the
/// `SQUELCH_API_TOKEN` master token, or any device token the store has issued
/// and not revoked. A daemon with neither still serves this tree; it just 401s
/// everything until `squelchd token issue` or a pairing claim mints one.
fn client_router(state: ApiState) -> Router {
    Router::new()
        .route("/client/updates", get(handlers::get_updates))
        .route(
            "/client/updates/{message_id}/status",
            post(handlers::set_update_status),
        )
        // "Remind me about this later." Human door only, like every write: it is
        // the user scheduling their own attention, and the agent door gets no
        // say in when the user is shown something. POST sets (and re-sets, one
        // reminder per message), DELETE un-schedules.
        .route(
            "/client/updates/{message_id}/reminder",
            post(handlers::set_update_reminder).delete(handlers::clear_update_reminder),
        )
        .route("/client/refresh", post(handlers::refresh_now))
        .route("/client/thread/{thread_id}", get(handlers::get_thread))
        .route("/client/attachments/{id}", get(handlers::get_attachment))
        .route("/client/shipments", get(handlers::get_shipments))
        // Force a carrier pass. HUMAN DOOR ONLY: the agent door reads the
        // shipments table and never gets to spend the operator's carrier quota.
        .route("/client/shipments/poll", post(handlers::poll_shipments_now))
        // "Stop showing me this package." HUMAN DOOR ONLY, like every write: it
        // is a statement about what the user wants to see. A read-side hide, not
        // a delete — the row keeps polling and returns on its own when an update
        // lands, so there is deliberately no un-clear route to pair with it.
        .route(
            "/client/shipments/{id}/clear",
            post(handlers::clear_shipment),
        )
        .route("/client/receipts", get(handlers::get_receipts))
        .route("/client/banking", get(handlers::get_banking))
        .route("/client/calendar", get(handlers::get_calendar))
        .route("/client/search", get(handlers::search))
        // The user's own sent mail. Human door only — this is the one listing
        // that reads `is_sent = 1`, and the agent door gets no such route: what
        // the user writes is not the agent's to page through.
        .route("/client/sent", get(handlers::get_sent))
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
        // The composer/signature preview: the send path's own markdown render,
        // exposed so the client never grows a second, drifting renderer.
        .route("/client/markdown/preview", post(handlers::markdown_preview))
        // Recipient autocomplete over Sent-derived contacts. Human door only —
        // the agent door must never see who the user writes to.
        .route("/client/contacts", get(handlers::get_contacts))
        .route("/client/sealed", get(handlers::list_sealed))
        .route(
            "/client/sealed/{message_id}/reveal",
            post(handlers::reveal_sealed),
        )
        .route(
            "/client/shredder",
            get(handlers::get_shredder).post(handlers::set_shredder),
        )
        // Read tracking, human door only: the opens of the user's OWN sent mail,
        // and whether the client should default new sends to tracked. The pixel
        // that produces these rows is the unauthenticated route merged in
        // `router`; the agent door has neither.
        .route(
            "/client/messages/{message_id}/opens",
            get(tracking::get_message_opens),
        )
        // Reply-recipient preview: who a reply (or `?all=true`, a reply-all)
        // would be addressed to, derived server-side from the parent's headers
        // by the same code the send path uses. A READ — it reaches Gmail for
        // metadata only and sends nothing — so it needs no confirm gate, but it
        // does need the write credential the fetch rides on.
        .route(
            "/client/messages/{message_id}/reply_recipients",
            get(handlers::reply_recipients),
        )
        .route(
            "/client/tracking-config",
            get(tracking::get_tracking_config).post(tracking::set_tracking_config),
        )
        .route("/client/shredder/run", post(handlers::run_shredder))
        .route("/client/marketing", get(handlers::get_marketing))
        .route("/client/audit", get(handlers::get_audit))
        .route("/client/stats", get(handlers::get_stats))
        // SHARING. The GET is a read (may this daemon share, and what could the
        // mail say); the POST spends a quota and sends mail as the user, so it
        // sits with the write routes below rather than here... except it does
        // not, and that is deliberate: the action routes are gated on the WRITE
        // credential being configured at all, and this route's own refusal says
        // something more useful than a blanket 403. See `sharing::post_invites`.
        .route("/client/invites", get(sharing::get_invites))
        .route("/client/invites", post(sharing::post_invites))
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
        // Assistant relay: human door ONLY. The body is the user's own
        // conversation, sent by their paired client, and it is spent against
        // the tenant's assistant budget at the gateway — the agent door never
        // gains a route that burns tenant money or fronts a daemon-held
        // credential.
        .route(
            "/client/assistant/messages",
            post(assistant::assistant_messages),
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
