//! Push-device registration on the human door: `POST /client/devices` and
//! `POST /client/devices/unregister`.
//!
//! This is where a phone tells ITS OWN daemon "here is my APNs token". The
//! daemon's pusher task ([`squelch_core::push`]) later fans event ids out to
//! every registered token through the blind relay. Nothing about this surface
//! exists on the agent door, and no message content is involved at any point.
//!
//! REGISTRATION IS IDEMPOTENT because iOS hands an app its device token on every
//! single launch. A re-register refreshes `last_registered_at` on the same row;
//! it never forks a second one. See `Store::upsert_device`.
//!
//! The token bounds here (16-200 characters, hex only) deliberately MIRROR the
//! relay's own validation. Storing a token the relay would reject just moves the
//! failure from registration time — where a client can see it — to push time,
//! where it is a silent per-token error in a log.
//!
//! PRIVACY: a device token is user-owned capability material. It is never logged
//! here, and the success response deliberately does NOT echo it back — the
//! caller already has it, and a body that carries it is one more place it can
//! end up in a proxy log for no benefit at all.
//!
//! The same rule is why unregistering is a POST carrying the token in a BODY
//! rather than `DELETE /client/devices/{token}`: a path segment is the most
//! copied part of a request — access logs, proxy logs, error reports, metrics
//! labels all keep it by default — and it would be incoherent to refuse the
//! token in a response body while putting it in the request line.

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use squelch_core::store::{Device, Store};

use crate::error::ApiError;
use crate::handlers::blocking;
use crate::state::ApiState;

/// Mirrors `squelch-relay`'s own `TOKEN_MIN_LEN`/`TOKEN_MAX_LEN`. Loose around
/// today's 64-hex-character APNs token so a format change needs no release.
const TOKEN_MIN_LEN: usize = 16;
const TOKEN_MAX_LEN: usize = 200;

/// A platform tag is a label, not free text; keep it short and boring so it can
/// never become a smuggling channel into the `devices` table.
const MAX_PLATFORM_LEN: usize = 32;
const DEFAULT_PLATFORM: &str = "ios";

#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    pub token: String,
    /// `ios` when omitted. Kept open (rather than an enum) so a macOS-over-APNs
    /// experiment needs no schema or API change.
    #[serde(default)]
    pub platform: Option<String>,
}

/// The unregister body. One field, and the reason it is a body at all is in the
/// module header: capability material does not belong in a URL path.
#[derive(Debug, Deserialize)]
pub struct UnregisterBody {
    pub token: String,
}

/// The registration result. NOTE THE ABSENT `token` FIELD — see the module
/// header.
#[derive(Debug, Serialize)]
pub struct DeviceView {
    pub id: i64,
    pub platform: String,
    pub created_at: DateTime<Utc>,
    pub last_registered_at: DateTime<Utc>,
}

impl From<Device> for DeviceView {
    fn from(d: Device) -> Self {
        Self {
            id: d.id,
            platform: d.platform,
            created_at: d.created_at,
            last_registered_at: d.last_registered_at,
        }
    }
}

/// Bounds-check a device token exactly as the relay does. The error message
/// states the RULE, never the offending value: reflecting a rejected token into
/// a response body is how it ends up in someone's proxy log.
fn validate_token(token: &str) -> Result<&str, ApiError> {
    let token = token.trim();
    if token.len() < TOKEN_MIN_LEN || token.len() > TOKEN_MAX_LEN {
        return Err(ApiError::bad_request(format!(
            "token must be {TOKEN_MIN_LEN}-{TOKEN_MAX_LEN} characters"
        )));
    }
    if !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request("token must be hexadecimal"));
    }
    Ok(token)
}

/// `ios` by default; otherwise a short lowercase-ish label.
fn validate_platform(platform: Option<&str>) -> Result<String, ApiError> {
    let raw = platform.unwrap_or(DEFAULT_PLATFORM).trim();
    if raw.is_empty() {
        return Ok(DEFAULT_PLATFORM.to_string());
    }
    if raw.len() > MAX_PLATFORM_LEN
        || !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(ApiError::bad_request(
            "platform must be a short alphanumeric label",
        ));
    }
    Ok(raw.to_ascii_lowercase())
}

/// `POST /client/devices` — register (or refresh) one APNs device token.
///
/// Idempotent: registering an already-known token returns the same row with a
/// refreshed `last_registered_at`, which is what makes it safe for the app to
/// call on every launch.
pub async fn register_device(
    State(state): State<ApiState>,
    Json(body): Json<RegisterBody>,
) -> Result<impl IntoResponse, ApiError> {
    let token = validate_token(&body.token)?.to_string();
    let platform = validate_platform(body.platform.as_deref())?;

    let store = state.store.clone();
    let account_id = state.account_id;
    let device = blocking(move || store.upsert_device(account_id, &token, &platform)).await?;

    // Audited like every other human-door state change, with the device ROW ID
    // as the target. The token itself never reaches the audit log.
    crate::handlers::audit_action(
        &state,
        "device.register",
        Some(device.id.to_string()),
        &device.platform,
    )
    .await;

    Ok(Json(DeviceView::from(device)))
}

/// `POST /client/devices/unregister` — unregister a device.
///
/// A POST-with-a-body, NOT `DELETE /client/devices/{token}`: the token is a
/// capability, and a URL path is the one part of a request every intermediary
/// records by default. See the module header.
///
/// 204 whether or not a row was there. The caller's intent ("this token must not
/// receive pushes") is satisfied either way, and a 404 would turn the endpoint
/// into an oracle for which tokens this account has registered.
pub async fn unregister_device(
    State(state): State<ApiState>,
    Json(body): Json<UnregisterBody>,
) -> Result<impl IntoResponse, ApiError> {
    let token = validate_token(&body.token)?.to_string();

    let store = state.store.clone();
    let account_id = state.account_id;
    let removed = blocking(move || store.delete_device_by_token(account_id, &token)).await?;

    if removed {
        crate::handlers::audit_action(&state, "device.unregister", None, "removed").await;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bounds_mirror_the_relays() {
        let ok = "a".repeat(64);
        assert_eq!(validate_token(&ok).unwrap(), ok);
        // Whitespace is trimmed, not rejected — a client that pastes a token
        // with a stray newline meant the token.
        assert_eq!(validate_token(&format!(" {ok}\n")).unwrap(), ok);

        assert!(validate_token(&"a".repeat(15)).is_err(), "too short");
        assert!(validate_token(&"a".repeat(201)).is_err(), "too long");
        assert!(validate_token(&"g".repeat(64)).is_err(), "not hex");
        assert!(validate_token("").is_err());
    }

    #[test]
    fn platform_defaults_to_ios_and_stays_a_label() {
        assert_eq!(validate_platform(None).unwrap(), "ios");
        assert_eq!(validate_platform(Some("")).unwrap(), "ios");
        assert_eq!(validate_platform(Some("  ")).unwrap(), "ios");
        assert_eq!(validate_platform(Some("macOS")).unwrap(), "macos");
        assert!(validate_platform(Some(&"x".repeat(33))).is_err());
        assert!(validate_platform(Some("ios; drop table")).is_err());
    }
}
