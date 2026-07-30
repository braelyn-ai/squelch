//! Push-device registration on the human door: `POST /client/devices` and
//! `POST /client/devices/unregister`. Human door only, no message content.
//!
//! A device token is capability material: never logged, never echoed in a
//! response, never in a URL path (hence unregister POSTs it in a body).
//! Registration is IDEMPOTENT — iOS hands the app its token on every launch.

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

/// Must stay in step with the relay's own bounds: a token the relay would
/// reject is refused here, where a client sees it, not silently at push time.
const TOKEN_MIN_LEN: usize = 16;
const TOKEN_MAX_LEN: usize = 200;

/// A platform tag is a label, not free text — short and boring so it can never
/// become a smuggling channel into the `devices` table.
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

/// The unregister body — the token rides here, never in the path.
#[derive(Debug, Deserialize)]
pub struct UnregisterBody {
    pub token: String,
}

/// The registration result. The `token` field is deliberately absent.
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

/// Bounds-check a device token exactly as the relay does. The error states the
/// RULE, never the offending value.
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
/// Idempotent: a known token returns the same row with a refreshed
/// `last_registered_at`, so the app can call it on every launch.
pub async fn register_device(
    State(state): State<ApiState>,
    Json(body): Json<RegisterBody>,
) -> Result<impl IntoResponse, ApiError> {
    let token = validate_token(&body.token)?.to_string();
    let platform = validate_platform(body.platform.as_deref())?;

    let store = state.store.clone();
    let account_id = state.account_id;
    let device = blocking(move || store.upsert_device(account_id, &token, &platform)).await?;

    // Audited with the device ROW ID; the token never reaches the audit log.
    crate::handlers::audit_action(
        &state,
        "device.register",
        Some(device.id.to_string()),
        &device.platform,
    )
    .await;

    Ok(Json(DeviceView::from(device)))
}

/// `POST /client/devices/unregister` — a POST-with-a-body, never
/// `DELETE /client/devices/{token}`: the token is capability material.
///
/// 204 whether or not a row was there; a 404 would turn the endpoint into an
/// oracle for which tokens this account has registered.
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
        // Whitespace is trimmed, not rejected.
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
