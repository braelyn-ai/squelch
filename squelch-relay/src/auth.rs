//! Optional bearer auth for `POST /v1/push`.
//!
//! Mechanically identical to `squelch-api/src/auth.rs` — same parser, same
//! constant-time comparison — with one difference: the relay's token is
//! OPTIONAL. Device tokens are unguessable capabilities, so the design doc
//! allows a v1 deployment to run unauthenticated-but-rate-limited. That choice
//! is made once, at router construction: this middleware is only mounted when a
//! token is configured, so it never has to decide whether to serve open.
//!
//! SECURITY:
//! - `Authorization: Bearer <token>` is compared in CONSTANT TIME via
//!   `subtle::ConstantTimeEq`, so a timing side channel cannot leak the secret
//!   prefix-by-prefix.
//! - A missing, malformed, or mismatched token returns a bare 401 with no
//!   detail. The token is never logged or echoed.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::Response,
};
use subtle::ConstantTimeEq;

use crate::state::RelayState;

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

/// Middleware: require a valid bearer token or return 401. Only mounted when
/// `SQUELCH_RELAY_AUTH_TOKEN` is configured.
pub async fn require_bearer(
    State(state): State<RelayState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = state.auth_token() else {
        // Unreachable in practice: the layer is not mounted without a token.
        // Fail CLOSED anyway rather than inventing an open path.
        return Err(StatusCode::UNAUTHORIZED);
    };

    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer);

    match presented {
        Some(tok) if ct_eq(tok.as_bytes(), expected.as_bytes()) => Ok(next.run(req).await),
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
    }
}
