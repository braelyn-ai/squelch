//! Bearer-token auth for the human door.
//!
//! Every `/client/*` route is wrapped by [`require_bearer`]. The expected token
//! is the static shared secret held in [`ApiState`] (sourced from
//! `SQUELCH_API_TOKEN`), which is guaranteed non-empty by construction — so the
//! "serve open" case cannot happen here; it is refused earlier, when the state
//! is built.
//!
//! SECURITY:
//! - The `Authorization: Bearer <token>` value is compared to the expected
//!   token in CONSTANT TIME via `subtle::ConstantTimeEq`, so a timing side
//!   channel cannot leak the secret prefix-by-prefix.
//! - A missing/malformed header, or a mismatched token, both return a bare 401
//!   with no detail. The token is never logged or echoed.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::Response,
};
use subtle::ConstantTimeEq;

use crate::state::ApiState;

/// Constant-time equality over two byte slices. `ConstantTimeEq` is only defined
/// for equal-length slices, so we branch on length first (length is not secret)
/// and then compare in constant time.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Extract a `Bearer` token from an `Authorization` header value.
fn parse_bearer(value: &str) -> Option<&str> {
    // Case-insensitive scheme, single space, then the token.
    let rest = value.strip_prefix("Bearer ").or_else(|| {
        // Tolerate lowercase / mixed-case scheme.
        let (scheme, rest) = value.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then_some(rest)
    })?;
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

/// Middleware: require a valid bearer token or return 401.
pub async fn require_bearer(
    State(state): State<ApiState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer);

    match presented {
        // Constant-time compare against the configured token.
        Some(tok) if ct_eq(tok.as_bytes(), state.token.as_bytes()) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer_variants() {
        assert_eq!(parse_bearer("Bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("BEARER abc"), Some("abc"));
        assert_eq!(parse_bearer("Bearer  spaced "), Some("spaced"));
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer("Bearer "), None);
        assert_eq!(parse_bearer(""), None);
    }

    #[test]
    fn ct_eq_matches_only_equal() {
        assert!(ct_eq(b"secret", b"secret"));
        assert!(!ct_eq(b"secret", b"secreu"));
        assert!(!ct_eq(b"secret", b"secre"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }
}
