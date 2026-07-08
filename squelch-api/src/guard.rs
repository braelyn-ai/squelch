//! Outbound secret guard for `POST /client/actions/send`.
//!
//! Before any message leaves the process we scan the outgoing body for
//! secret-looking patterns: API-key shapes, long hex/base64 runs, OTP-looking
//! codes near auth words, and PEM headers. A non-empty match set blocks the send
//! (HTTP 422) UNLESS the caller passes `override_guard: true`.
//!
//! SECURITY: the guard NEVER returns or logs the matched text — only the KIND of
//! match (e.g. `"api_key"`). The whole point is to avoid a second copy of the
//! secret anywhere, including our own error responses.

use std::sync::LazyLock;

use regex::Regex;

/// A category of secret-looking match. Rendered as a stable snake_case string in
/// the 422 response so the client can explain what tripped the guard without
/// ever seeing the offending substring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMatch {
    /// Vendor API-key prefixes: `sk-`, `AKIA` (AWS), `ghp_`/`gho_` (GitHub), etc.
    ApiKey,
    /// A long hex run (>= 32 hex chars) — hashes, tokens, private-key material.
    LongHex,
    /// A long base64/base64url run (>= 40 chars) — opaque tokens/blobs.
    LongBase64,
    /// A 6-8 digit code appearing near an auth word (code/otp/verification/…).
    OtpCode,
    /// A PEM block header (`-----BEGIN ... -----`).
    PemBlock,
}

impl GuardMatch {
    /// Stable, REDACTED label — the only thing that ever leaves the process.
    pub fn kind(self) -> &'static str {
        match self {
            GuardMatch::ApiKey => "api_key",
            GuardMatch::LongHex => "long_hex",
            GuardMatch::LongBase64 => "long_base64",
            GuardMatch::OtpCode => "otp_code",
            GuardMatch::PemBlock => "pem_block",
        }
    }
}

// Compiled once. Each is deliberately conservative-but-broad; false positives
// are acceptable because the guard is overridable.

/// Vendor key prefixes. `sk-` (OpenAI/Stripe-ish), `AKIA` (AWS access key id),
/// `ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_` (GitHub tokens), `xox[baprs]-` (Slack),
/// `AIza` (Google), `ya29.` (Google OAuth).
static RE_API_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        (?: sk-[A-Za-z0-9_\-]{16,}
          | AKIA[0-9A-Z]{16}
          | gh[porus]_[A-Za-z0-9]{20,}
          | xox[baprs]-[A-Za-z0-9\-]{10,}
          | AIza[0-9A-Za-z_\-]{20,}
          | ya29\.[0-9A-Za-z_\-]{20,}
        )",
    )
    .expect("api-key regex")
});

/// >= 32 contiguous hex characters (word-bounded).
static RE_LONG_HEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-fA-F]{32,}\b").expect("hex regex"));

/// >= 40 contiguous base64/base64url characters (word-bounded, allows `-_+/`).
static RE_LONG_B64: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Za-z0-9+/_\-]{40,}={0,2}\b").expect("b64 regex"));

/// A 6-8 digit code near an auth word within a small window on the same-ish
/// context. Two orders: word-then-code and code-then-word.
static RE_OTP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?xi)
        (?: (?:code|otp|passcode|password|verification|verify|one[\s-]?time|2fa|auth\w*|pin)
            [^0-9]{0,40}\b\d{6,8}\b
          | \b\d{6,8}\b[^0-9]{0,40}
            (?:code|otp|passcode|password|verification|verify|one[\s-]?time|2fa|auth\w*|pin)
        )",
    )
    .expect("otp regex")
});

/// A PEM block header.
static RE_PEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z0-9 ]+-----").expect("pem regex"));

/// Scan `body` and return the set of matched KINDS (deduped, stable order).
/// Never returns matched text. An empty vec means the body looks clean.
pub fn scan(body: &str) -> Vec<GuardMatch> {
    let mut hits = Vec::new();
    // Order matters only for output stability, not correctness.
    if RE_PEM.is_match(body) {
        hits.push(GuardMatch::PemBlock);
    }
    if RE_API_KEY.is_match(body) {
        hits.push(GuardMatch::ApiKey);
    }
    if RE_OTP.is_match(body) {
        hits.push(GuardMatch::OtpCode);
    }
    // Long-run checks last: an API key often also trips base64/hex, but the more
    // specific kind is more useful. We still include them for defense in depth.
    if RE_LONG_HEX.is_match(body) {
        hits.push(GuardMatch::LongHex);
    }
    if RE_LONG_B64.is_match(body) {
        hits.push(GuardMatch::LongBase64);
    }
    hits
}

/// Convenience: the REDACTED kind labels for a scan result.
pub fn scan_kinds(body: &str) -> Vec<&'static str> {
    scan(body).into_iter().map(GuardMatch::kind).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(body: &str) -> Vec<&'static str> {
        scan_kinds(body)
    }

    #[test]
    fn clean_text_passes() {
        let body = "Hi Alice,\n\nSounds great — let's meet Tuesday at 3pm to review the \
                    Q3 numbers. Talk soon,\nBob";
        assert!(scan(body).is_empty(), "clean prose must not trip the guard");
    }

    #[test]
    fn openai_style_key_caught() {
        let body = "here is the key sk-abcDEF0123456789ghijKLMNopq you asked for";
        assert!(kinds(body).contains(&"api_key"));
    }

    #[test]
    fn aws_access_key_caught() {
        let body = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE and more";
        assert!(kinds(body).contains(&"api_key"));
    }

    #[test]
    fn github_token_caught() {
        let body = "token ghp_16C7e42F292c6912E7710c838347Ae178B4a";
        assert!(kinds(body).contains(&"api_key"));
    }

    #[test]
    fn otp_code_near_auth_word_caught() {
        assert!(kinds("Your verification code is 483920").contains(&"otp_code"));
        assert!(kinds("483920 is your one-time passcode").contains(&"otp_code"));
        assert!(kinds("enter PIN 12345678 to continue").contains(&"otp_code"));
    }

    #[test]
    fn bare_number_without_auth_word_is_not_otp() {
        // A plain 6-digit number with no auth context should not trip OTP.
        assert!(!kinds("The invoice total was 128456 dollars last year").contains(&"otp_code"));
    }

    #[test]
    fn pem_header_caught() {
        let body = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow...\n-----END RSA PRIVATE KEY-----";
        assert!(kinds(body).contains(&"pem_block"));
    }

    #[test]
    fn long_hex_caught() {
        let body = "checksum: 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert!(kinds(body).contains(&"long_hex"));
    }

    #[test]
    fn long_base64_caught() {
        let body = "token=QWxhZGRpbjpvcGVuIHNlc2FtZQ1234567890abcdefGHIJKLMNOP";
        assert!(kinds(body).contains(&"long_base64"));
    }

    #[test]
    fn never_returns_matched_text() {
        // The API contract: only kinds, never substrings. This test documents it.
        let secret = "sk-abcDEF0123456789ghijKLMNopq";
        let out = scan_kinds(&format!("key {secret}"));
        assert!(out.iter().all(|k| !k.contains(secret)));
    }
}
