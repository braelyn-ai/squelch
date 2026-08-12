//! The flow cookie: a signed, HttpOnly, SameSite=Lax claim that ties the
//! browser that started an OAuth hop to the browser that comes back from Google.
//!
//! WHAT THE COOKIE CARRIES AND WHAT IT DOES NOT is the whole design. It carries
//! a session id, the label, and (on a signup) the invite row being spent. It
//! does NOT carry the PKCE verifier or the CSRF `state` — those live server-side
//! in [`crate::sessions`], keyed by the id, one-shot, ten minutes. A cookie is a
//! value the client holds; the two secrets that make an authorization code
//! redeemable must never be among them.
//!
//! The MAC is HMAC-SHA256 over the exact payload bytes, compared with
//! [`squelch_httpauth::ct_eq`]. A tampered label, a swapped invite id, or a
//! replayed payload from another deployment all fail the same way: the session
//! is refused with no detail about which field was wrong.
//!
//! ONE COOKIE FOR BOTH FLOWS, under the name a browser has always seen. A
//! console login is the same ten-minute hop with a different errand, and a
//! second name would mean a second value to clear on every terminal answer for
//! no property gained: the server-side session is what decides which flow this
//! is, and the claim is only ever held against it.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Cookie name. Not `__Host-` prefixed: that prefix requires `Secure`, which a
/// local http development run cannot set, and a name that only works in
/// production is a name whose absence nobody notices until production. The
/// properties `__Host-` would buy (no `Domain`, `Path=/`) are set explicitly
/// below instead.
pub const COOKIE_NAME: &str = "passband_signup";

/// How long a signup may sit between the form post and the return from Google.
/// The same ten minutes the server-side session gets: both must expire, because
/// either one outliving the other is a session that cannot complete but can
/// still be replayed at.
pub const COOKIE_TTL_SECS: i64 = 10 * 60;

/// Ceiling on a presented cookie. The real one is a couple of hundred bytes;
/// this stops an attacker spending our CPU on base64 and HMAC over a megabyte
/// of their choosing.
const MAX_COOKIE_LEN: usize = 1024;

/// What the cookie asserts. Serialized compactly because it rides on every
/// request in the flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClaim {
    /// Server-side session key. High-entropy and opaque; the sensitive half of
    /// the session (state, PKCE verifier) is stored under it, never here.
    pub sid: String,
    /// The validated tenant label this hop is for.
    pub label: String,
    /// The invite row being spent, on a signup. An id, never the code or its
    /// hash: a hash in a cookie is an offline check for whoever steals the
    /// cookie.
    ///
    /// `None` on a console login, which spends nothing. The field is SKIPPED
    /// when absent, so a signup's payload bytes are exactly what they were
    /// before console sessions existed; and because the callback holds this
    /// against the server-side session's own kind, a cookie claiming an invite
    /// for a console session (or the reverse) is refused like any other
    /// mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<i64>,
    /// Issued-at, unix seconds. The TTL is enforced on the server, so this is
    /// signed rather than trusted.
    pub iat: i64,
}

/// Sign a claim into a cookie value: `base64url(json).base64url(mac)`.
pub fn sign(key: &[u8], claim: &SessionClaim) -> String {
    // The claim is our own struct of primitives; serialization cannot fail.
    let payload = serde_json::to_vec(claim).unwrap_or_default();
    let mac = mac(key, &payload);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(mac)
    )
}

/// Verify and decode a cookie value.
///
/// Every failure is `None`: a bad shape, a bad MAC, an expired claim, and a
/// payload that is not our JSON are one answer, because the page above shows
/// one message for all of them and an attacker learns nothing from which.
pub fn verify(key: &[u8], value: &str, now_unix: i64) -> Option<SessionClaim> {
    if value.len() > MAX_COOKIE_LEN {
        return None;
    }
    let (payload_b64, mac_b64) = value.split_once('.')?;
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let presented = URL_SAFE_NO_PAD.decode(mac_b64).ok()?;
    // Constant-time, and over the DECODED bytes, so two spellings of the same
    // MAC cannot be told apart by timing or by encoding.
    if !squelch_httpauth::ct_eq(&presented, &mac(key, &payload)) {
        return None;
    }
    let claim: SessionClaim = serde_json::from_slice(&payload).ok()?;
    // Both ends of the window: a future `iat` is a clock problem or a forgery
    // attempt, and either way it is not a session this process opened.
    let age = now_unix.checked_sub(claim.iat)?;
    if !(0..=COOKIE_TTL_SECS).contains(&age) {
        return None;
    }
    Some(claim)
}

fn mac(key: &[u8], payload: &[u8]) -> Vec<u8> {
    // HMAC accepts any key length; the config floor is what makes it a real key.
    let mut m = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    m.update(payload);
    m.finalize().into_bytes().to_vec()
}

/// The `Set-Cookie` value that installs a signup session.
///
/// - `HttpOnly`: no script on this origin (there is none) or any injected into
///   it can read the session.
/// - `SameSite=Lax`: the cookie must survive Google's top-level redirect back
///   to `/oauth/callback`, which `Strict` would drop, while still not riding on
///   a cross-site POST.
/// - `Secure` whenever this deployment is served over https. A local http dev
///   run cannot set it (browsers refuse the cookie), so it follows the origin
///   rather than being unconditional.
/// - `Path=/` and no `Domain`, so the cookie is bound to exactly this host.
pub fn set_cookie(value: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{COOKIE_NAME}={value}; Path=/; Max-Age={COOKIE_TTL_SECS}; HttpOnly; SameSite=Lax{secure}"
    )
}

/// The `Set-Cookie` value that clears the session. Sent on every terminal
/// outcome, success or refusal, so a finished signup leaves nothing behind.
pub fn clear_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}")
}

/// Pull our cookie out of a `Cookie` header. Hand-parsed rather than pulled in
/// as a dependency: the header is a `;`-separated list of `name=value`, and the
/// value we care about is base64url and a dot, so there is nothing to unquote.
pub fn from_header(header: &str) -> Option<&str> {
    header.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == COOKIE_NAME).then(|| value.trim())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn claim() -> SessionClaim {
        SessionClaim {
            sid: "s".repeat(43),
            label: "ada".into(),
            invite: Some(7),
            iat: 1_000_000,
        }
    }

    #[test]
    fn round_trips_a_claim() {
        let c = claim();
        let v = sign(KEY, &c);
        assert_eq!(verify(KEY, &v, c.iat).unwrap(), c);
        assert_eq!(verify(KEY, &v, c.iat + COOKIE_TTL_SECS).unwrap(), c);
    }

    /// A console login carries no invite, and the field is absent rather than
    /// null: a signup's payload is byte for byte what it was before the console
    /// flow existed, and a console claim cannot be read as a signup for invite
    /// row zero.
    #[test]
    fn a_console_claim_carries_no_invite() {
        let c = SessionClaim {
            invite: None,
            ..claim()
        };
        let v = sign(KEY, &c);
        assert_eq!(verify(KEY, &v, c.iat).unwrap(), c);

        let payload = URL_SAFE_NO_PAD
            .decode(v.split_once('.').unwrap().0)
            .unwrap();
        let json = String::from_utf8(payload).unwrap();
        assert!(!json.contains("invite"), "{json}");

        let signup = String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(sign(KEY, &claim()).split_once('.').unwrap().0)
                .unwrap(),
        )
        .unwrap();
        assert!(signup.contains(r#""invite":7"#), "{signup}");
    }

    /// The label and the invite id are what the callback acts on, so flipping
    /// either must fail. Both are inside the MAC.
    #[test]
    fn refuses_a_tampered_payload() {
        let c = claim();
        let v = sign(KEY, &c);
        let (payload_b64, mac_b64) = v.split_once('.').unwrap();
        let mut payload: SessionClaim =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload_b64).unwrap()).unwrap();
        payload.label = "www".into();
        let forged = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap()),
            mac_b64
        );
        assert_eq!(verify(KEY, &forged, c.iat), None);

        payload.label = "ada".into();
        payload.invite = Some(8);
        let forged = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap()),
            mac_b64
        );
        assert_eq!(verify(KEY, &forged, c.iat), None);
    }

    #[test]
    fn refuses_a_foreign_or_stripped_mac() {
        let c = claim();
        let v = sign(KEY, &c);
        let payload_b64 = v.split_once('.').unwrap().0;
        assert_eq!(verify(b"another key that is long enough", &v, c.iat), None);
        assert_eq!(verify(KEY, payload_b64, c.iat), None);
        assert_eq!(verify(KEY, &format!("{payload_b64}."), c.iat), None);
        assert_eq!(verify(KEY, &format!("{payload_b64}.AAAA"), c.iat), None);
    }

    #[test]
    fn refuses_an_expired_or_future_claim() {
        let c = claim();
        let v = sign(KEY, &c);
        assert_eq!(verify(KEY, &v, c.iat + COOKIE_TTL_SECS + 1), None);
        assert_eq!(verify(KEY, &v, c.iat - 1), None);
    }

    #[test]
    fn refuses_garbage_without_spending_much() {
        assert_eq!(verify(KEY, "", 0), None);
        assert_eq!(verify(KEY, "not-a-cookie", 0), None);
        assert_eq!(verify(KEY, &"a".repeat(MAX_COOKIE_LEN + 1), 0), None);
    }

    #[test]
    fn finds_the_cookie_among_others() {
        let v = sign(KEY, &claim());
        let header = format!("other=1; {COOKIE_NAME}={v}; another=2");
        assert_eq!(from_header(&header), Some(v.as_str()));
        assert_eq!(from_header("other=1; another=2"), None);
        assert_eq!(from_header(""), None);
        // A cookie whose NAME merely ends with ours must not match.
        assert_eq!(from_header(&format!("x{COOKIE_NAME}={v}")), None);
    }

    #[test]
    fn set_cookie_carries_the_attributes_that_matter() {
        let v = set_cookie("abc", true);
        assert!(v.contains("HttpOnly"), "{v}");
        assert!(v.contains("SameSite=Lax"), "{v}");
        assert!(v.contains("Secure"), "{v}");
        assert!(v.contains("Path=/"), "{v}");
        assert!(!v.contains("Domain"), "{v}");
        // A plaintext dev origin cannot set Secure, or the browser drops it.
        assert!(!set_cookie("abc", false).contains("Secure"));
        assert!(clear_cookie(true).contains("Max-Age=0"));
    }
}
