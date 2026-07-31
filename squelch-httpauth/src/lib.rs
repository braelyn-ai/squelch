//! Shared HTTP bearer-auth primitives for BOTH doors: the human door
//! (`squelch-api`) and the relay's push route (`squelch-relay`).
//!
//! These two lived as byte-identical copies in each crate's `auth.rs`, which is
//! exactly the shape a security fix gets applied to once and forgotten in the
//! other. The middleware stays in each crate (the state types differ); the
//! comparison and the header parse live here, tested once.
//!
//! A leaf crate rather than a module of `squelch-core`: the relay is the one
//! binary that runs on shared public infrastructure, and it is blind by design
//! — no store, no mail, no credentials. Reaching these two functions must not
//! link the mail core into it.

use subtle::ConstantTimeEq;

/// Constant-time byte equality. Length is branched on first (it is not secret);
/// `ConstantTimeEq` is only defined for equal-length slices.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Extract a `Bearer` token from an `Authorization` header value.
pub fn parse_bearer(value: &str) -> Option<&str> {
    let rest = value.strip_prefix("Bearer ").or_else(|| {
        // The scheme is case-insensitive.
        let (scheme, rest) = value.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then_some(rest)
    })?;
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
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
