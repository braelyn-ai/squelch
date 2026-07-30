//! Error -> HTTP response mapping for the human door: errors become
//! `{"error": "..."}`. Internal detail never leaks — every `CoreError` other
//! than `NotFound`/`InvalidInput` collapses to an opaque 500, and no token or
//! body ever reaches an error message.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use squelch_core::CoreError;

/// A handler error carrying an HTTP status and a client-safe message.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// 400 Bad Request with a client-facing message.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// 404 Not Found (opaque; used for both missing and sealed-hidden).
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not found")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::NotFound => ApiError::not_found(),
            CoreError::InvalidInput(m) => ApiError::bad_request(m),
            // Opaque: never leak internal/store/credential detail across the wire.
            _ => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        }
    }
}
