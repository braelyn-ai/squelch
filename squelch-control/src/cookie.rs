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
//!
//! THE ADMIN COOKIE IS THE EXCEPTION, and it is separated twice over. It is a
//! long-lived credential rather than a hop marker: holding it means being the
//! operator, and the operator can mint invites. So it gets its own name AND
//! [`AdminClaim`] carries a required `aud` marker that [`SessionClaim`] does
//! not have. Both halves are signed with the same key, so either alone would be
//! thin: a signed session claim presented under the admin name has no `aud` and
//! is refused, and an admin claim presented as a session has no `sid` and is
//! refused the same way. `SameSite=Strict` (not `Lax`) keeps that cookie off
//! requests another SITE's page causes, which is half the CSRF defense on the
//! admin POSTs; the sibling subdomains it does not cover are handled by the
//! origin check in [`crate::admin`].

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

/// The admin session cookie. Its own name, not a second value under the signup
/// one: the two are different credentials with different lifetimes, and a
/// browser that holds both must be able to lose one without losing the other.
pub const ADMIN_COOKIE_NAME: &str = "passband_admin";

/// How long one admin sign-in lasts. THIRTY DAYS.
///
/// It was twelve hours, on the reasoning that a working day is long enough to
/// get through a morning's waitlist and short enough that a laptop left open is
/// not an admin session next week. The first half was wrong about how this page
/// is actually used: approving a waitlist is a minute of work a few times a
/// week, not a morning of it, so twelve hours meant the operator fetched the
/// token out of Railway nearly every visit. A credential that has to be looked
/// up that often is a credential that ends up somewhere convenient, which is a
/// worse outcome than a long session.
///
/// The expiry was never the real kill switch and is not being asked to be one
/// now. Rotating `SQUELCH_CONTROL_ADMIN_TOKEN` still ends every session on the
/// next request, because [`AdminClaim::tfp`] pins each cookie to the token that
/// opened it, and that is instant rather than a wait of any length. The cookie
/// is `HttpOnly`, `SameSite=Strict`, and `Secure` off a plain-http origin, so
/// what a longer window widens is one case: a browser somebody else gets their
/// hands on. [`crate::admin::logout`] is the answer to that one, and it exists
/// because this constant grew.
pub const ADMIN_COOKIE_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// The `aud` every admin claim carries and no other claim in this crate does.
/// The marker is what makes the two claim types different DOCUMENTS under one
/// key, rather than two shapes that happen to have different fields.
pub const ADMIN_AUD: &str = "admin";

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
    /// Whether this is an APP login rather than a console one. The two are
    /// otherwise indistinguishable from the cookie alone: both spend no invite,
    /// so `invite.is_none()` names neither on its own.
    ///
    /// ONLY THE REFUSAL COPY TURNS ON IT. Where the server-side session survives
    /// it is the authority for which flow this is, as everywhere else here; this
    /// field is read exactly when the session is GONE (expired, replayed) and
    /// the page still has to be written in the words of the flow the person was
    /// in. A forged `true` therefore buys a stranger a differently worded
    /// refusal and nothing else.
    ///
    /// Skipped when false, so a signup's and a console login's payload bytes are
    /// exactly what they were before app logins existed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub app: bool,
    /// Issued-at, unix seconds. The TTL is enforced on the server, so this is
    /// signed rather than trusted.
    pub iat: i64,
}

/// `skip_serializing_if` for a plain `bool`, so the default rides as an absence.
fn is_false(b: &bool) -> bool {
    !*b
}

/// What the admin cookie asserts: that whoever holds it presented the admin
/// token to this deployment, and when.
///
/// TWO FIELDS, one of which exists only to be checked. `aud` is the domain
/// separator: a [`SessionClaim`] signed with the same key has no such field, so
/// it cannot deserialize into this, and [`verify_admin`] holds the value
/// against [`ADMIN_AUD`] as well.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminClaim {
    /// Always [`ADMIN_AUD`]. No default and no `Option`: a payload without it
    /// must fail to parse rather than fall back to something.
    pub aud: String,
    /// Which admin token opened this session, as [`token_fingerprint`]. It is
    /// what makes rotating `SQUELCH_CONTROL_ADMIN_TOKEN` a kill switch: without
    /// it, a cookie minted under a leaked token keeps working for the whole of
    /// [`ADMIN_COOKIE_TTL_SECS`] and the only remedy is rotating the cookie key,
    /// which signs out every signup in flight too. That mattered when the window
    /// was twelve hours and it matters thirty times more now.
    pub tfp: String,
    /// Issued-at, unix seconds. The TTL is enforced on the server, so this is
    /// signed rather than trusted.
    pub iat: i64,
}

impl AdminClaim {
    /// The claim a successful login signs. The only constructor callers need,
    /// so `aud` cannot be spelled wrong at a call site.
    pub fn new(token: &str, now_unix: i64) -> Self {
        Self {
            aud: ADMIN_AUD.to_string(),
            tfp: token_fingerprint(token),
            iat: now_unix,
        }
    }
}

/// A short, one-way name for an admin token.
///
/// Truncated to 16 hex characters (64 bits) because this is an equality check
/// against ONE configured token, not a lookup: it only has to change when the
/// token does. The token itself never rides in the cookie, and this digest is
/// not a secret, but it is still never logged, like everything else here.
pub fn token_fingerprint(token: &str) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    Sha256::digest(token.as_bytes())
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut out, b| {
            // Writing to a String cannot fail.
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Sign a claim into a cookie value: `base64url(json).base64url(mac)`.
pub fn sign(key: &[u8], claim: &SessionClaim) -> String {
    sign_claim(key, claim)
}

/// The same, for an admin claim. A separate entry point rather than one generic
/// public function, so a caller cannot sign the wrong claim type into the
/// wrong cookie by inference.
pub fn sign_admin(key: &[u8], claim: &AdminClaim) -> String {
    sign_claim(key, claim)
}

fn sign_claim<T: Serialize>(key: &[u8], claim: &T) -> String {
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
    let payload = authentic_payload(key, value)?;
    let claim: SessionClaim = serde_json::from_slice(&payload).ok()?;
    fresh(claim.iat, now_unix, COOKIE_TTL_SECS).then_some(claim)
}

/// Verify and decode an ADMIN cookie value.
///
/// Four things must hold, and every failure is the same `None`: the MAC is
/// ours, the payload is an admin claim (`aud` present and [`ADMIN_AUD`]), the
/// session was opened with the token this deployment is configured with NOW,
/// and it is inside [`ADMIN_COOKIE_TTL_SECS`]. The `aud` check is what makes a
/// signup cookie's payload, signed with this very key, useless here; the
/// fingerprint check is what makes rotating the token sign everybody out.
pub fn verify_admin(key: &[u8], token: &str, value: &str, now_unix: i64) -> Option<AdminClaim> {
    let payload = authentic_payload(key, value)?;
    let claim: AdminClaim = serde_json::from_slice(&payload).ok()?;
    if claim.aud != ADMIN_AUD || claim.tfp != token_fingerprint(token) {
        return None;
    }
    fresh(claim.iat, now_unix, ADMIN_COOKIE_TTL_SECS).then_some(claim)
}

/// The bytes of a cookie whose MAC is ours, or `None`. Everything before the
/// claim type is the same work for both cookies, and doing it in one place is
/// what keeps the constant-time compare from being reimplemented per claim.
fn authentic_payload(key: &[u8], value: &str) -> Option<Vec<u8>> {
    if value.len() > MAX_COOKIE_LEN {
        return None;
    }
    let (payload_b64, mac_b64) = value.split_once('.')?;
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let presented = URL_SAFE_NO_PAD.decode(mac_b64).ok()?;
    // Constant-time, and over the DECODED bytes, so two spellings of the same
    // MAC cannot be told apart by timing or by encoding.
    squelch_httpauth::ct_eq(&presented, &mac(key, &payload)).then_some(payload)
}

/// Both ends of the window: a future `iat` is a clock problem or a forgery
/// attempt, and either way it is not a claim this process issued.
fn fresh(iat: i64, now_unix: i64, ttl: i64) -> bool {
    now_unix
        .checked_sub(iat)
        .is_some_and(|age| (0..=ttl).contains(&age))
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

/// The `Set-Cookie` value that installs an admin session.
///
/// - `SameSite=Strict`, not `Lax`: this cookie authorizes minting invites and
///   sending mail, and Strict keeps it off requests another SITE's page caused.
///   It costs nothing here because nothing ever redirects INTO the admin page
///   from somewhere else. It is not the whole CSRF story: "site" is the
///   registrable domain, so a sibling `passband.app` name is inside it and
///   [`crate::admin`] checks the origin as well.
/// - `HttpOnly`, `Path=/`, no `Domain`, and `Secure` whenever the origin is
///   https, for the same reasons the signup cookie has them.
pub fn set_admin_cookie(value: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{ADMIN_COOKIE_NAME}={value}; Path=/; Max-Age={ADMIN_COOKIE_TTL_SECS}; HttpOnly; SameSite=Strict{secure}"
    )
}

/// The `Set-Cookie` value that clears the admin session: the sign-out, and what
/// a refused admin request sends so a stale cookie does not sit in the browser
/// being presented forever.
pub fn clear_admin_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{ADMIN_COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{secure}")
}

/// Pull our cookie out of a `Cookie` header. Hand-parsed rather than pulled in
/// as a dependency: the header is a `;`-separated list of `name=value`, and the
/// value we care about is base64url and a dot, so there is nothing to unquote.
pub fn from_header(header: &str) -> Option<&str> {
    named_cookie(header, COOKIE_NAME)
}

/// The same, for the admin cookie. Both cookies can be present at once (one
/// browser, both errands), so each is looked up by its own name.
pub fn admin_from_header(header: &str) -> Option<&str> {
    named_cookie(header, ADMIN_COOKIE_NAME)
}

fn named_cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (n, value) = pair.split_once('=')?;
        (n.trim() == name).then(|| value.trim())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
    /// The admin token these claims are opened under, long enough to be one.
    const TOKEN: &str = "an admin token of entirely sufficient length";

    fn claim() -> SessionClaim {
        SessionClaim {
            sid: "s".repeat(43),
            label: "ada".into(),
            invite: Some(7),
            app: false,
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

    /// The app marker rides only on an app login, and the two flows that predate
    /// it serialize byte for byte what they did before: `app` is absent, not
    /// `false`, on a signup and on a console claim.
    #[test]
    fn only_an_app_claim_carries_the_app_marker() {
        let payload = |c: &SessionClaim| {
            String::from_utf8(
                URL_SAFE_NO_PAD
                    .decode(sign(KEY, c).split_once('.').unwrap().0)
                    .unwrap(),
            )
            .unwrap()
        };

        assert!(!payload(&claim()).contains("app"), "signup");
        let console = SessionClaim {
            invite: None,
            ..claim()
        };
        assert!(!payload(&console).contains("app"), "console");

        // An app login names no tenant, so its label is empty going out and
        // comes back the same.
        let app = SessionClaim {
            label: String::new(),
            invite: None,
            app: true,
            ..claim()
        };
        let json = payload(&app);
        assert!(json.contains(r#""app":true"#), "{json}");
        let v = sign(KEY, &app);
        assert_eq!(verify(KEY, &v, app.iat).unwrap(), app);
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
    fn round_trips_an_admin_claim() {
        let c = AdminClaim::new(TOKEN, 1_000_000);
        assert_eq!(c.aud, ADMIN_AUD);
        let v = sign_admin(KEY, &c);
        assert_eq!(verify_admin(KEY, TOKEN, &v, c.iat).unwrap(), c);
        assert_eq!(
            verify_admin(KEY, TOKEN, &v, c.iat + ADMIN_COOKIE_TTL_SECS).unwrap(),
            c
        );
    }

    /// ROTATING THE TOKEN IS THE KILL SWITCH. An operator who thinks the admin
    /// token leaked changes it, and every cookie minted under the old one stops
    /// working on the next request rather than lasting out its month.
    /// The alternative kill switch, rotating the cookie key, would also sign out
    /// every signup in flight.
    #[test]
    fn a_rotated_token_ends_every_admin_session() {
        let v = sign_admin(KEY, &AdminClaim::new(TOKEN, 1_000_000));
        assert!(verify_admin(KEY, TOKEN, &v, 1_000_000).is_some());
        assert_eq!(
            verify_admin(
                KEY,
                "a different token, just as long as the other",
                &v,
                1_000_000
            ),
            None
        );
    }

    /// THE DOMAIN SEPARATION, in both directions. Both claims are signed with
    /// the same key, so nothing here fails on the MAC: a signup claim is
    /// refused as admin because it carries no `aud`, and an admin claim is
    /// refused as a signup session because it carries no `sid`. Whoever holds a
    /// perfectly valid cookie for one errand holds nothing for the other.
    #[test]
    fn a_signup_claim_is_not_an_admin_claim() {
        let signup = sign(KEY, &claim());
        assert!(verify(KEY, &signup, 1_000_000).is_some(), "still a session");
        assert_eq!(verify_admin(KEY, TOKEN, &signup, 1_000_000), None);

        let admin = sign_admin(KEY, &AdminClaim::new(TOKEN, 1_000_000));
        assert_eq!(verify(KEY, &admin, 1_000_000), None);

        // Nor can a payload be talked into the right shape: an `aud` that is
        // not ours is refused even though the MAC over it is.
        let forged = sign_admin(
            KEY,
            &AdminClaim {
                aud: "signup".into(),
                tfp: token_fingerprint(TOKEN),
                iat: 1_000_000,
            },
        );
        assert_eq!(verify_admin(KEY, TOKEN, &forged, 1_000_000), None);
    }

    #[test]
    fn refuses_a_tampered_or_expired_admin_claim() {
        let c = AdminClaim::new(TOKEN, 1_000_000);
        let v = sign_admin(KEY, &c);
        let (payload_b64, mac_b64) = v.split_once('.').unwrap();

        // A stretched `iat` under the original MAC buys nothing.
        let forged = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&AdminClaim::new(TOKEN, c.iat + ADMIN_COOKIE_TTL_SECS)).unwrap()
            ),
            mac_b64
        );
        assert_eq!(verify_admin(KEY, TOKEN, &forged, c.iat + 1), None);
        assert_eq!(
            verify_admin(b"another key that is long enough", TOKEN, &v, c.iat),
            None
        );
        assert_eq!(verify_admin(KEY, TOKEN, payload_b64, c.iat), None);
        assert_eq!(
            verify_admin(KEY, TOKEN, &format!("{payload_b64}.AAAA"), c.iat),
            None
        );

        // One second past the window is a sign-in that has to happen again.
        assert_eq!(
            verify_admin(KEY, TOKEN, &v, c.iat + ADMIN_COOKIE_TTL_SECS + 1),
            None
        );
        assert_eq!(verify_admin(KEY, TOKEN, &v, c.iat - 1), None);
        assert_eq!(verify_admin(KEY, TOKEN, "", 0), None);
        assert_eq!(
            verify_admin(KEY, TOKEN, &"a".repeat(MAX_COOKIE_LEN + 1), 0),
            None
        );
    }

    /// One browser can hold both cookies, and each is found by its own name.
    #[test]
    fn finds_each_cookie_by_its_own_name() {
        let signup = sign(KEY, &claim());
        let admin = sign_admin(KEY, &AdminClaim::new(TOKEN, 1_000_000));
        let header = format!("{COOKIE_NAME}={signup}; {ADMIN_COOKIE_NAME}={admin}");
        assert_eq!(from_header(&header), Some(signup.as_str()));
        assert_eq!(admin_from_header(&header), Some(admin.as_str()));
        assert_eq!(admin_from_header(&format!("{COOKIE_NAME}={signup}")), None);
        assert_eq!(
            admin_from_header(&format!("x{ADMIN_COOKIE_NAME}={admin}")),
            None
        );
    }

    /// `SameSite=Strict` is the CSRF defense on every admin POST, so it is
    /// asserted rather than trusted to survive an edit.
    #[test]
    fn the_admin_cookie_is_strict() {
        let v = set_admin_cookie("abc", true);
        assert!(v.contains("HttpOnly"), "{v}");
        assert!(v.contains("SameSite=Strict"), "{v}");
        assert!(v.contains("Secure"), "{v}");
        assert!(v.contains("Path=/"), "{v}");
        assert!(!v.contains("Domain"), "{v}");
        assert!(
            v.contains(&format!("Max-Age={ADMIN_COOKIE_TTL_SECS}")),
            "{v}"
        );
        assert!(!set_admin_cookie("abc", false).contains("Secure"));
        let cleared = clear_admin_cookie(true);
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
        assert!(cleared.contains("SameSite=Strict"), "{cleared}");
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
