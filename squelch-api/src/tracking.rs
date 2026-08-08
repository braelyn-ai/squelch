//! Read tracking: the UNAUTHENTICATED pixel route, plus the human door's read
//! and config sides.
//!
//! `GET /t/{token}` is one of the TWO routes in this crate served WITHOUT a
//! bearer (the other is `POST /client/pair`, the pairing claim), and it has to
//! be — it is fetched by a stranger's mail client, which has no token and never
//! will. It is therefore built to leak nothing:
//!
//! - the response is byte-identical for a known token, an unknown token, a
//!   malformed one, and a store that just failed. Always `200`, always the same
//!   43-byte GIF. There is no 404 to probe with and no timing branch worth
//!   measuring;
//! - it READS nothing. It appends one `message_opens` row and returns an image;
//!   no mail, no metadata, no ids reach the response;
//! - an unknown token appends nothing at all (the store's INSERT…SELECT gates on
//!   an existing tracker), so unsolicited traffic cannot grow the table, and a
//!   token that is ours still stops recording past the store's per-token cap;
//! - it touches the store, whose mutex the whole daemon shares, without any
//!   credential in front of it, so a flood is bounded by [`PIXEL_CONCURRENCY`]
//!   and by rejecting non-token shapes before the store is reached. The pairing
//!   claim carries the same bound for the same reason
//!   ([`crate::pair::PAIR_CONCURRENCY`]), differing only in that it WAITS for a
//!   slot instead of bailing out, because its answer must not vary with load.
//!
//! [`pixel_router`] is mounted OUTSIDE the bearer layer deliberately and is the
//! only thing in that router; the `/client/*` tree and `/mcp` are untouched.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use squelch_core::store::{MessageOpen, Store};
use squelch_core::tracking::classify_user_agent;

use crate::error::ApiError;
use crate::handlers::{audit_action, store_call};
use crate::state::ApiState;

/// The `app_settings` key behind `/client/tracking-config`: whether the client
/// should default new sends to tracked. The daemon never reads it — every send
/// states its own `include_tracker` — it is remembered preference, not policy.
pub const TRACKING_DEFAULT_ENABLED_KEY: &str = "tracking.default_enabled";

/// Ceiling on the stored `User-Agent`. It is attacker-supplied text; the
/// classification only needs a substring, and nothing downstream needs the tail.
const MAX_USER_AGENT: usize = 256;

/// Shape of a token we mint: 32 url-safe base64 chars. The range is wider than
/// that so an older or longer token still records, but anything outside it
/// cannot be ours and is rejected before the store is touched.
const TOKEN_MIN_LEN: usize = 16;
const TOKEN_MAX_LEN: usize = 64;

/// Pixel fetches allowed to be in the store at once. This route is
/// unauthenticated and its URL is printed in every tracked email, so it is the
/// one place a stranger can make the daemon work; the store mutex it takes is
/// the same one `/client/*`, `/mcp`, and the sync engine wait on. A flood is
/// therefore bounded here rather than allowed to serialize the whole daemon.
pub(crate) const PIXEL_CONCURRENCY: usize = 4;

fn is_tracking_token(token: &str) -> bool {
    (TOKEN_MIN_LEN..=TOKEN_MAX_LEN).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A 43-byte transparent 1x1 GIF89a. Inlined rather than generated so the
/// response body is a compile-time constant with no allocation and no failure
/// mode of its own.
const PIXEL_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, // "GIF89a"
    0x01, 0x00, 0x01, 0x00, // 1x1
    0x80, 0x00, 0x00, // global color table, 2 entries
    0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, // white, black
    0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, // graphic control: index 0 transparent
    0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, // image descriptor
    0x02, 0x02, 0x44, 0x01, 0x00, // LZW: one pixel
    0x3B, // trailer
];

/// The ONE response this route can produce. `no-store` matters twice over: an
/// intermediary caching the pixel would swallow every later open, and the image
/// is not content anyone should retain.
fn pixel_response() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/gif"),
            (
                header::CACHE_CONTROL,
                "no-store, no-cache, must-revalidate, private",
            ),
            (header::PRAGMA, "no-cache"),
            (header::EXPIRES, "0"),
        ],
        PIXEL_GIF,
    )
        .into_response()
}

/// `GET /t/{token}` — the tracking pixel. Unauthenticated by necessity; see the
/// module header for what that costs and how it is bounded.
pub async fn tracking_pixel(
    State(state): State<ApiState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    // Every early return below still answers with the same image: a token that
    // cannot be ours, and one that is, must be indistinguishable from outside.
    if !is_tracking_token(&token) {
        return pixel_response();
    }
    let Ok(_permit) = state.pixel_slots().try_acquire() else {
        return pixel_response();
    };

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| ua.chars().take(MAX_USER_AGENT).collect::<String>());
    let classification = classify_user_agent(user_agent.as_deref());
    let opened_at = Utc::now().timestamp();

    // The result is deliberately dropped: an unknown token, a known one, and a
    // failed write must be indistinguishable from outside.
    let _ = store_call(&state, move |store, account_id| {
        store.record_open(
            account_id,
            &token,
            opened_at,
            user_agent.as_deref(),
            classification,
        )
    })
    .await;

    pixel_response()
}

/// The pixel route on its own router, with NO bearer layer. Merged into the
/// human door's router at the outermost level so it is visibly separate from
/// everything auth wraps.
///
/// The bare `/t/` is routed too, so the whole token space answers with the
/// image and a 404 is never evidence about a token — matchit would otherwise
/// reject the empty segment before the handler ever ran.
pub fn pixel_router(state: ApiState) -> Router {
    Router::new()
        .route("/t/{token}", get(tracking_pixel))
        .route("/t/", get(|| async { pixel_response() }))
        .with_state(state)
}

/// `GET /client/messages/{id}/opens` — every open of one sent message, oldest
/// first. `{id}` is the LOCAL message id, i.e. the `echo_message_id` the send
/// response handed back. An untracked or unknown message is an empty list, not a
/// 404: "nobody opened it" and "it was never tracked" are the same answer to a
/// client that only ever asks about its own sends.
pub async fn get_message_opens(
    State(state): State<ApiState>,
    Path(message_id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let opens: Vec<MessageOpen> = store_call(&state, move |store, account_id| {
        store.message_opens(account_id, message_id)
    })
    .await?;
    Ok(Json(json!({ "opens": opens })))
}

/// `/client/tracking-config` state. `configured` is the daemon's answer to "is
/// there anywhere to point a pixel" — when it is false the client should hide
/// the toggle entirely, because `include_tracker` would be ignored.
#[derive(Debug, Serialize)]
pub struct TrackingConfigView {
    default_enabled: bool,
    configured: bool,
}

async fn tracking_view(state: &ApiState) -> Result<TrackingConfigView, ApiError> {
    let stored = store_call(state, |store, account_id| {
        store.get_app_setting(account_id, TRACKING_DEFAULT_ENABLED_KEY)
    })
    .await?;
    Ok(TrackingConfigView {
        // Unset — and anything unparseable — is OFF: tracking is opt-in.
        default_enabled: stored.as_deref() == Some("1"),
        configured: state.tracking_base_url().is_some(),
    })
}

pub async fn get_tracking_config(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(tracking_view(&state).await?))
}

#[derive(Debug, Deserialize)]
pub struct TrackingConfigBody {
    #[serde(default)]
    default_enabled: Option<bool>,
}

pub async fn set_tracking_config(
    State(state): State<ApiState>,
    Json(body): Json<TrackingConfigBody>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(enabled) = body.default_enabled {
        let v = if enabled { "1" } else { "0" }.to_string();
        store_call(&state, move |store, account_id| {
            store.set_app_setting(account_id, TRACKING_DEFAULT_ENABLED_KEY, &v)
        })
        .await?;
        // Changing whether outbound mail is watched by default is a policy
        // change worth a row.
        audit_action(
            &state,
            "tracking_policy",
            None,
            if enabled { "enabled" } else { "disabled" },
        )
        .await;
    }
    Ok(Json(tracking_view(&state).await?))
}
