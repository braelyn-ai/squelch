//! The tenant CONSOLE: server-rendered HTML under `/console`, for the person who
//! owns this daemon.
//!
//! THE SPINE OF THE DESIGN: A CONSOLE SESSION IS A DEVICE TOKEN. There is no new
//! credential type here, no session table, no signing key. Signing in means
//! claiming a pairing code the same way the app does; the `sqd_` token that comes
//! back is set as an HttpOnly cookie, and every later request verifies it through
//! the SAME [`SqliteStore::verify_device_token`] the bearer path uses. A browser
//! is just another paired device, so revocation, the audit trail, the one-shot
//! claim and the ten-minute TTL are all inherited rather than reinvented.
//!
//! [`SqliteStore::verify_device_token`]: squelch_core::store::SqliteStore::verify_device_token
//!
//! TWO WAYS IN, one mechanism:
//!
//! - PASTE A CODE (`POST /console/login-code`). The universal path, and the only
//!   one a self-host has: `squelchd pair` prints a code, the user types it here;
//! - GOOGLE, VIA THE CONTROL PLANE (`GET /console/callback?code=`). Google forbids
//!   wildcard redirect URIs, so a per-tenant hostname cannot run OAuth itself. The
//!   login page LINKS to the control plane, which authenticates the mailbox, mints
//!   a pairing code through the warden, and sends the browser back here with it.
//!   The callback claims that code exactly like a pasted one. The link renders
//!   only when `SQUELCH_CONSOLE_SSO_URL` is configured, which on self-host it is
//!   not, so the button simply is not there.
//!
//! WHAT THIS FILE WILL NOT DO:
//!
//! - no JavaScript, no external asset, no font to fetch, and a `default-src
//!   'none'` CSP that says so to the browser and not only to the tests. The page
//!   discipline is `squelch-control/src/pages.rs`'s, MIRRORED here rather than
//!   imported: the daemon does not depend on the control plane, and a console
//!   that could only be built alongside the hosted signup service would not ship
//!   on self-host at all;
//! - no token in any page, ever. A pairing code renders exactly once, on the page
//!   that mints it, which is the entire point of that page. A device token
//!   appears only in a `Set-Cookie`;
//! - no bare 401. A human lives here, so a dead, absent or forged cookie renders
//!   the login page. Uniformly: a burned code, a wrong code, an expired one and a
//!   claim against a store that could not answer produce the same bytes.
//!
//! CSRF: `SameSite=Lax` on the cookie, plus an `Origin`/`Sec-Fetch-Site` check
//! in front of EVERY mutating POST (see [`csrf_guard`]) so the console does not
//! rest on one browser control alone.

use axum::{
    Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Extension, Form, FromRequestParts, Path, Query, State,
        rejection::{FormRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, Request, StatusCode, Uri, header, request::Parts},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use squelch_core::store::sqlite::device_tokens::{
    PAIRING_CODE_LEN, PAIRING_TTL_SECS, normalize_pairing_code,
};
use squelch_core::store::{DeviceToken, Store};

use crate::state::ApiState;

/// The session cookie's name.
///
/// NOT `__Host-` prefixed, for the reason `squelch-control/src/cookie.rs` gives:
/// the prefix requires `Secure`, a plain-http loopback run cannot set it, and a
/// cookie name that only works in production is a name whose absence nobody
/// notices until production. The two properties the prefix would buy (`Path=/`,
/// no `Domain`) are set explicitly in [`set_cookie`] instead.
const COOKIE_NAME: &str = "passband_console";

/// How long a browser keeps the session cookie. The TOKEN has no expiry of its
/// own (it lives until revoked), so this only bounds how long a browser will keep
/// presenting one nobody signed out of. Sign out revokes; this is the backstop.
const COOKIE_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// The `device_tokens.name` every console session carries, so `squelchd token
/// list` and the devices table below both show a browser for what it is.
const CONSOLE_DEVICE_NAME: &str = "console";

/// Ceiling on a console POST body. The largest form here is one pairing code.
const MAX_BODY_BYTES: usize = 4096;

/// Sustained sign-in attempts per client per minute, across BOTH ways in (the
/// pasted-code form and the control plane's callback). Sized to a human retyping
/// a code they are reading off a terminal, not to a guesser: ten survives every
/// legitimate fumble and caps a stranger at a rate the 40-bit space laughs at.
/// The limiter itself lives on [`ApiState`]; who a "client" is depends on
/// whether the listener attached `ConnectInfo` (see the limiter's doc).
pub(crate) const CONSOLE_SIGNIN_REQUESTS_PER_MINUTE: f64 = 10.0;

/// The eight symbols a pairing code is made of, after [`normalize_pairing_code`]
/// folds case and strips dashes: Crockford base32, no I/L/O/U, exactly as the
/// store mints them (`squelch_core::store::sqlite::device_tokens`).
const PAIRING_CODE_ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Does this even LOOK like a code the store could have minted?
///
/// The console gates shape BEFORE the store on purpose, and `/client/pair`
/// deliberately does not — different doors, different trades. Pairing's wire
/// contract wants every guess to cost the guesser budget, so it refuses to give
/// a wrong-length probe a free pass; but a garbage POST here would spend the
/// TENANT'S active code's attempt budget (five misses burn it), handing any
/// stranger a five-request denial of the sign-in flow. Shape-gating means only
/// plausible codes reach the store: a real guess still burns, line noise does
/// not.
fn code_shaped(normalized: &str) -> bool {
    normalized.len() == PAIRING_CODE_LEN
        && normalized.chars().all(|c| PAIRING_CODE_ALPHABET.contains(c))
}

/// Ceiling on a presented cookie, before it costs a hash and a store lookup. An
/// issued token is 47 characters; this is headroom, not a limit anyone meets.
const MAX_COOKIE_LEN: usize = 512;

/// The window `/console` counts its band figures over, matching the human door's
/// default so the console and the app do not disagree about "this month".
const STATS_WINDOW_DAYS: i64 = 30;

/// Tenant-label bounds, the control plane's rule mirrored (see
/// `squelch_control::labels`): the label is only used to build the SSO link, and
/// building one control would refuse is a worse failure than no link at all.
const MIN_LABEL_LEN: usize = 3;
const MAX_LABEL_LEN: usize = 30;

/// `Sec-Fetch-Site`, which `http` has no constant for. Set by the browser and
/// unsettable by page script, which is what makes it worth reading.
const SEC_FETCH_SITE: &str = "sec-fetch-site";

// --- page shell -------------------------------------------------------------

/// The Content-Security-Policy every console page carries.
///
/// The single allowance is the inline `<style>`. `form-action 'self'` is exact
/// here, unlike the signup service's: every form on these pages POSTs to this
/// origin and answers itself or a same-origin redirect. The hop to Google is a
/// plain `<a href>`, which `form-action` does not govern.
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// Escape text for HTML. EVERYTHING interpolated into a page goes through this,
/// including strings that were validated elsewhere: "this one was checked
/// already" is how the exception becomes the rule.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encode a query-parameter VALUE. Only the unreserved set survives, so
/// a label, a code or a URL can never break out of the parameter it sits in.
fn percent_encode(s: &str) -> String {
    use std::fmt::Write as _;
    s.bytes()
        .fold(String::with_capacity(s.len()), |mut out, b| {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                // Writing to a String cannot fail.
                _ => {
                    let _ = write!(out, "%{b:02X}");
                }
            }
            out
        })
}

/// The `passband://pair` deep link the native client claims. Built HERE from
/// this daemon's own origin and the code it just minted, never from anything a
/// caller supplied as a URL.
fn deep_link(daemon_url: &str, pair_code: &str) -> String {
    format!(
        "passband://pair?url={}&code={}",
        percent_encode(daemon_url),
        percent_encode(pair_code)
    )
}

/// The shell every console page shares. `no-store` and `no-referrer` are not
/// decoration: these pages name a mailbox and one of them carries a live pairing
/// code.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let title = escape_html(title);
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex, nofollow">
<title>{title}</title>
<style>
:root {{ color-scheme: light dark; }}
body {{ margin: 0; padding: 3rem 1.25rem; background: #fbfaf8; color: #1a1a1a;
  font: 1rem/1.55 ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; }}
main {{ max-width: 44rem; margin: 0 auto; }}
h1 {{ font-size: 1.35rem; font-weight: 600; letter-spacing: -0.01em; margin: 0 0 1rem; }}
h2 {{ font-size: 1.05rem; font-weight: 600; margin: 2rem 0 0.6rem; }}
p {{ margin: 0 0 1rem; }}
ul {{ margin: 0 0 1.25rem; padding-left: 1.2rem; }}
li {{ margin: 0 0 0.4rem; }}
label {{ display: block; font-weight: 600; margin: 0 0 0.35rem; }}
input[type=text] {{ width: 100%; max-width: 20rem; box-sizing: border-box; padding: 0.6rem 0.7rem;
  margin: 0 0 1rem; border: 1px solid #cdc7bd; border-radius: 6px; background: #fff; color: inherit;
  font: inherit; letter-spacing: 0.06em; }}
button {{ padding: 0.5rem 1rem; border: 0; border-radius: 6px; background: #1a1a1a; color: #fbfaf8;
  font: inherit; font-weight: 500; cursor: pointer; }}
table {{ border-collapse: collapse; width: 100%; margin: 0 0 1.25rem; }}
th, td {{ text-align: left; padding: 0.45rem 0.6rem 0.45rem 0; border-bottom: 1px solid #e3ded6;
  vertical-align: top; font-size: 0.95rem; }}
th {{ font-weight: 600; }}
dl {{ margin: 0 0 1.25rem; }}
dt {{ font-weight: 600; margin: 0.6rem 0 0; }}
dd {{ margin: 0; }}
.muted {{ color: #6b6b6b; font-size: 0.9rem; }}
.stop {{ border-left: 3px solid #b3261e; padding: 0.1rem 0 0.1rem 0.85rem; }}
.code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 1.5rem;
  letter-spacing: 0.08em; background: #efece7; padding: 0.4rem 0.7rem; border-radius: 6px;
  display: inline-block; user-select: all; }}
code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9em;
  background: #efece7; padding: 0.1em 0.35em; border-radius: 3px; word-break: break-all; }}
a.button {{ display: inline-block; margin: 0.25rem 0 1.25rem; padding: 0.5rem 1rem;
  background: #1a1a1a; color: #fbfaf8; text-decoration: none; border-radius: 6px; font-weight: 500; }}
form.inline {{ display: inline; }}
@media (prefers-color-scheme: dark) {{
  body {{ background: #141414; color: #e8e6e3; }}
  .muted {{ color: #9a9a9a; }}
  code, .code {{ background: #262626; }}
  th, td {{ border-bottom-color: #333; }}
  input[type=text] {{ background: #1f1f1f; border-color: #3a3a3a; }}
  button, a.button {{ background: #e8e6e3; color: #141414; }}
  .stop {{ border-left-color: #f2b8b5; }}
}}
</style>
</head>
<body><main>{body}</main></body>
</html>
"#
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store, no-cache"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        html,
    )
        .into_response()
}

/// A dead end a person can read: a heading, a sentence, and the way back.
fn problem(status: StatusCode, heading: &str, detail: &str) -> Response {
    page(
        status,
        heading,
        &format!(
            r#"<h1>{heading}</h1>
<p>{detail}</p>
<p><a class="button" href="/console">Back to the console</a></p>"#,
            heading = escape_html(heading),
            detail = escape_html(detail),
        ),
    )
}

// --- where the request arrived ----------------------------------------------

/// The origin this request believes it reached, derived from the request itself
/// rather than from configuration the daemon does not have.
///
/// It decides three things: whether the session cookie carries `Secure`, what
/// URL the pairing deep link points at, and which tenant label the SSO link
/// names. All three are self-referential (the browser is already here), so the
/// `Host` header being caller-controlled costs nothing: the worst a forged one
/// buys is a link that points somewhere the forger already controls, in a page
/// only they can see.
///
/// All three hang off one answer, [`Site::origin_is_https`], so the escape hatch
/// moves all three together. That is the point of it: see that method.
#[derive(Clone, Debug, Default)]
struct Site {
    /// Lowercased `host[:port]`, or empty when the request carried no usable
    /// authority at all.
    host: String,
    /// The operator's declaration that this console is served over plain http.
    /// From `[console] allow_insecure_cookie`; see [`Site::origin_is_https`].
    allow_insecure_cookie: bool,
}

/// Whether an authority is loopback, and therefore the one origin treated as
/// plain http on its own, without the operator saying so.
///
/// Nothing else gets it automatically, including a plain-http LAN address: a
/// session token crossing a network in the clear is exactly the thing `Secure`
/// exists to prevent, and silently obliging would be worse than the console not
/// working there. Front it with TLS, reach it on loopback, or say out loud that
/// you accept the trade with `[console] allow_insecure_cookie` (see
/// [`Site::origin_is_https`]) and get a banner on the login page for it.
fn is_loopback(authority: &str) -> bool {
    let host = strip_port(authority);
    host == "localhost"
        || host == "::1"
        // The whole 127.0.0.0/8 block, not just the one address people type.
        || host.strip_prefix("127.").is_some_and(|rest| {
            rest.split('.')
                .all(|octet| !octet.is_empty() && octet.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// `host[:port]` -> `host`, IPv6 literals included.
fn strip_port(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        // `[::1]:8848` -> `::1`; a bracket with no close is not an authority we
        // will reason about, so it falls through to the whole string.
        return rest.split_once(']').map_or(authority, |(inner, _)| inner);
    }
    authority.split_once(':').map_or(authority, |(h, _)| h)
}

impl Site {
    /// The authority, from the URI when the protocol put it there (HTTP/2) and
    /// from `Host` otherwise (HTTP/1.1, which is what a proxy hands us).
    ///
    /// Filtered to the characters an authority may contain. Everything below
    /// escapes what it interpolates anyway, but a value that reaches a
    /// `Set-Cookie` or an `href` is worth narrowing at the source.
    fn from_parts(headers: &HeaderMap, uri: &Uri, allow_insecure_cookie: bool) -> Self {
        let raw = uri
            .authority()
            .map(|a| a.as_str().to_string())
            .or_else(|| {
                headers
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            })
            .unwrap_or_default()
            .to_ascii_lowercase();
        let usable = raw.len() <= 255
            && !raw.is_empty()
            && raw.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']')
            });
        Self {
            host: if usable { raw } else { String::new() },
            allow_insecure_cookie,
        }
    }

    /// Whether this console is reached over https, which is ONE answer with
    /// three consumers: the cookie's `Secure` attribute, the scheme
    /// [`Site::url`] builds (the pairing deep link, and the origin
    /// [`same_origin_request`] compares against), and whether
    /// [`Site::tenant_label`] will name a tenant for the SSO link.
    ///
    /// Normally it is derived from the authority alone: loopback is http,
    /// everything else is https (see [`is_loopback`]).
    ///
    /// `allow_insecure_cookie` overrides that to http, and it is NOT only about
    /// the cookie despite the name it is configured under. The operator is
    /// telling us the whole console is served over plain http, so all three
    /// consumers have to agree: a deep link and a CSRF origin comparison that
    /// still said `https://` would leave the console signed in but unable to
    /// POST anything, which is how it behaves on a plain-http LAN today with the
    /// hatch shut. Enabling it on an origin that really is https therefore costs
    /// more than the `Secure` attribute: the SSO button disappears and the
    /// `Origin` fallback in [`same_origin_request`] starts refusing. That is the
    /// documented deal, and the login page says so where the user can see it.
    fn origin_is_https(&self) -> bool {
        !self.allow_insecure_cookie && !self.host.is_empty() && !is_loopback(&self.host)
    }

    /// This daemon's own base URL, for the pairing deep link and for the
    /// same-origin comparison. `None` when the request carried no authority to
    /// build one from, in which case the code still renders and only the one-tap
    /// link is missing.
    ///
    /// The DEFAULT PORT is dropped, because a browser's `Origin` never spells it
    /// and a proxy's `Host` sometimes does. Leaving it in would make a perfectly
    /// same-origin form post look cross-site to [`same_origin_request`].
    fn url(&self) -> Option<String> {
        if self.host.is_empty() {
            return None;
        }
        let secure = self.origin_is_https();
        let scheme = if secure { "https" } else { "http" };
        let default_port = if secure { ":443" } else { ":80" };
        let host = self.host.strip_suffix(default_port).unwrap_or(&self.host);
        Some(format!("{scheme}://{host}"))
    }

    /// The tenant label this daemon answers as: the leftmost DNS label of a
    /// hosted-shaped hostname (`<label>.<base domain>`, three parts or more).
    ///
    /// `None` for loopback, for an IP literal, and for anything that fails the
    /// control plane's label rule. The only consumer is the SSO link, and a link
    /// control would refuse is worse than no link.
    fn tenant_label(&self) -> Option<&str> {
        if !self.origin_is_https() {
            return None;
        }
        let host = strip_port(&self.host);
        let parts: Vec<&str> = host.split('.').collect();
        if parts.len() < 3 || parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())) {
            return None;
        }
        let label = parts[0];
        let shaped = (MIN_LABEL_LEN..=MAX_LABEL_LEN).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        shaped.then_some(label)
    }
}

impl FromRequestParts<ApiState> for Site {
    /// Infallible: a request with no usable authority still renders a page, it
    /// just cannot offer a deep link or an SSO hop.
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Site::from_parts(
            &parts.headers,
            &parts.uri,
            state.console_allow_insecure_cookie,
        ))
    }
}

/// The rate limiter's key for this request, read from the `ConnectInfo` the
/// listener may or may not have attached. When it did not (in-process tests,
/// exotic serve setups) every request shares one bucket: coarse, never a
/// bypass, and no forwarded header is consulted because a believed header is a
/// way to mint unlimited fresh identities.
struct ClientIp(std::net::IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    /// Infallible: an unknowable peer meters under the shared key.
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(ClientIp(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|ci| ci.0.ip())
                .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
        ))
    }
}

// --- the cookie -------------------------------------------------------------

/// The `Set-Cookie` that installs a session. The value is the `sqd_` token
/// verbatim: it is already 256 bits of opaque, revocable capability, and wrapping
/// it in a second signed envelope would only add a key to lose.
///
/// - `HttpOnly`: there is no script on this origin, and nothing injected into one
///   can read the session either;
/// - `SameSite=Lax`, NOT `Strict`, and this was learned live: the SSO landing is
///   a navigation chain that STARTED at accounts.google.com, and Chrome withholds
///   Strict cookies from every request in a cross-site-initiated chain — including
///   the same-site 303 hop to `/console` — so the first thing a freshly signed-in
///   user saw was the login page again. Lax sends the cookie on top-level GET
///   navigations (fixing the landing) while still withholding it from cross-site
///   POSTs; the mutating routes are guarded by [`csrf_guard`]'s Origin checks
///   regardless, so CSRF has two answers and neither is this attribute alone;
/// - `Secure` unless this origin is plain http: loopback ([`is_loopback`]), or an
///   operator who declared it with `[console] allow_insecure_cookie`
///   ([`Site::origin_is_https`]);
/// - `Path=/` and NO `Domain`, so the cookie is bound to this exact host and
///   never offered to a sibling tenant on the same base domain.
fn set_cookie(token: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{COOKIE_NAME}={token}; Path=/; Max-Age={COOKIE_TTL_SECS}; HttpOnly; SameSite=Lax{secure}"
    )
}

/// The `Set-Cookie` that clears a session. Sent on every sign-out and on every
/// refusal of a cookie we could not verify, so a browser stops presenting a
/// credential that will never work again.
fn clear_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}")
}

/// Pull the console cookie out of the request. Hand-parsed rather than pulled in
/// as a dependency: the header is a `;`-separated list of `name=value` and the
/// value is base64url, so there is nothing to unquote.
///
/// Every `Cookie` header is searched, because a client may send several.
fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|header| header.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name.trim() == COOKIE_NAME).then(|| value.trim())
        })
        .filter(|v| !v.is_empty() && v.len() <= MAX_COOKIE_LEN)
        .map(str::to_string)
}

/// Attach a `Set-Cookie` to a response already built.
fn with_cookie(mut resp: Response, cookie: &str) -> Response {
    if let Ok(value) = header::HeaderValue::from_str(cookie) {
        resp.headers_mut().append(header::SET_COOKIE, value);
    }
    resp
}

/// `303 See Other` to a LITERAL path of ours. Every caller passes a constant, so
/// there is no redirect target a request can influence and no open redirect to
/// have an opinion about.
fn see_other(location: &'static str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response()
}

// --- the session ------------------------------------------------------------

/// A verified console session. Carries the token's ROW ID and nothing else: the
/// plaintext is needed once, to verify, and is never held past that.
#[derive(Clone, Copy, Debug)]
struct ConsoleSession {
    token_id: i64,
}

/// Verify the cookie the same way the bearer path verifies a header.
///
/// `None` is the uniform no: absent, malformed, unknown, revoked, and a store
/// that could not answer are one answer, because the page above shows one thing
/// for all of them.
///
/// Bounded by [`crate::auth::DEVICE_AUTH_CONCURRENCY`]'s semaphore, for the same
/// reason the bearer path is: a cookie is a guess anyone can make, and each guess
/// takes the store mutex the whole daemon shares.
async fn console_session(state: &ApiState, headers: &HeaderMap) -> Option<ConsoleSession> {
    let presented = cookie_token(headers)?;
    let _permit = state.device_auth_slots().acquire().await.ok()?;
    let store = state.store.clone();
    let verified = tokio::task::spawn_blocking(move || store.verify_device_token(&presented))
        .await
        .ok()?
        .ok()??;
    // Account scoping, even here: this daemon serves one account, and a token
    // belonging to any other is not a session on this console.
    (verified.account_id == state.account_id).then_some(ConsoleSession {
        token_id: verified.id,
    })
}

// --- CSRF -------------------------------------------------------------------

/// Whether a request came from this origin.
///
/// `Sec-Fetch-Site` is preferred and believed absolutely: the browser sets it,
/// page script cannot, and `same-origin` is the only value a form on one of these
/// pages produces. Where it is absent, `Origin` is compared against the authority
/// the request was routed by. Where BOTH are absent, the answer is no: every
/// browser has sent `Origin` on POST for years, and fail-closed is the only
/// defensible default for a cookie-authenticated write.
fn same_origin_request(headers: &HeaderMap, site: &Site) -> bool {
    if let Some(fetch_site) = headers.get(SEC_FETCH_SITE).and_then(|v| v.to_str().ok()) {
        return fetch_site.eq_ignore_ascii_case("same-origin");
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(expected) = site.url() else {
        return false;
    };
    // Whole-origin compare (scheme included), so `http://tenant.example` posting
    // at `https://tenant.example` is as cross-site as any other stranger.
    origin.eq_ignore_ascii_case(&expected)
}

/// Refuse every mutating request that did not come from this origin, ahead of any
/// handler and ahead of the session check.
///
/// This is the second half of the CSRF answer; `SameSite=Lax` is the first.
/// Two independent controls because the cost is one header comparison and the
/// failure mode of relying on one is somebody else's page spending the user's
/// credential.
///
/// GET and HEAD are untouched: nothing behind them mutates.
async fn csrf_guard(State(state): State<ApiState>, req: Request<Body>, next: Next) -> Response {
    if req.method().is_safe() {
        return next.run(req).await;
    }
    let site = Site::from_parts(
        req.headers(),
        req.uri(),
        state.console_allow_insecure_cookie,
    );
    if !same_origin_request(req.headers(), &site) {
        return problem(
            StatusCode::FORBIDDEN,
            "That request did not come from the console",
            "The console only accepts actions submitted from its own pages. Open the console \
             again and retry from there.",
        );
    }
    next.run(req).await
}

/// Require a verified session for the routes that act on the account.
///
/// A failure renders the LOGIN PAGE, never a bare 401: a person is reading this.
/// The cookie is cleared on the way out, so a browser holding a revoked token
/// stops presenting it.
async fn require_console_session(
    State(state): State<ApiState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let site = Site::from_parts(
        req.headers(),
        req.uri(),
        state.console_allow_insecure_cookie,
    );
    let Some(session) = console_session(&state, req.headers()).await else {
        return with_cookie(
            login_page(
                &state,
                &site,
                Some("That session has ended. Sign in again to continue."),
            ),
            &clear_cookie(site.origin_is_https()),
        );
    };
    req.extensions_mut().insert(session);
    next.run(req).await
}

// --- routes -----------------------------------------------------------------

/// The console router, merged OUTSIDE the bearer layer and outside the `/client`
/// CORS layer in [`crate::router`].
///
/// It is cookie-authenticated HTML, so it must never carry the permissive CORS
/// headers the bearer-authed JSON tree does: those exist because a bearer is
/// carried explicitly by a client that has one, and a cookie is carried
/// automatically by a browser that has been pointed at us.
pub fn console_router(state: ApiState) -> Router {
    let authed = Router::new()
        .route("/console/pair", post(pair))
        .route("/console/revoke/{id}", post(revoke))
        .route("/console/logout", post(logout))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_console_session,
        ));

    Router::new()
        .route("/console", get(home))
        .route("/console/callback", get(callback))
        .route("/console/login-code", post(login_code))
        .merge(authed)
        // CSRF wraps EVERYTHING, including the unauthenticated login POST: a
        // cross-site page must not be able to sign a browser into an account of
        // the attacker's choosing either.
        .layer(middleware::from_fn_with_state(state.clone(), csrf_guard))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// `GET /console` — the home page when signed in, the login page when not.
async fn home(State(state): State<ApiState>, site: Site, headers: HeaderMap) -> Response {
    match console_session(&state, &headers).await {
        Some(session) => home_page(&state, &site, session).await,
        None => login_page(&state, &site, None),
    }
}

/// The pasted-code form.
#[derive(Deserialize)]
struct LoginCodeForm {
    code: String,
}

/// `POST /console/login-code` — sign in by pasting a pairing code.
///
/// THE UNIVERSAL PATH: it is how a self-host signs in, and it is the fallback on
/// hosted when the Google hop is unavailable. The claim is the store's, unchanged
/// and unauthenticated, so a wrong code costs the user's live code exactly as it
/// does at `/client/pair`.
async fn login_code(
    State(state): State<ApiState>,
    site: Site,
    ClientIp(ip): ClientIp,
    form: Result<Form<LoginCodeForm>, FormRejection>,
) -> Response {
    // A malformed body is refused with the same page as a wrong code: what was
    // wrong with the request is not something this surface answers.
    let code = form.map(|Form(f)| f.code).unwrap_or_default();
    claim_and_land(&state, &site, &code, ip).await
}

/// The control plane's hand-back.
#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
}

/// `GET /console/callback?code=` — land a browser the control plane
/// authenticated.
///
/// The `code` is a PAIRING CODE the control plane minted through the warden after
/// checking that the Google-authenticated mailbox is this tenant's own. Nothing
/// about it is trusted here beyond the store's own claim: a code that was burned,
/// replayed, expired or never minted fails exactly like a typo.
///
/// There is no `return`/`next` parameter, here or anywhere in this flow, so there
/// is no open redirect to defend: the control plane CONSTRUCTS this URL from its
/// own base-domain config and the validated label.
async fn callback(
    State(state): State<ApiState>,
    site: Site,
    ClientIp(ip): ClientIp,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Response {
    let code = query.map(|Query(q)| q.code).unwrap_or_default();
    claim_and_land(&state, &site, &code, ip).await
}

/// Trade a pairing code for a session, or refuse uniformly.
///
/// The ONE place a console session is created, so the two ways in cannot drift
/// apart in what they set or what they say when they fail. Three gates stand in
/// front of the store, cheapest first: the per-client rate bucket, the shape
/// check (see [`code_shaped`] for why the console pre-validates and
/// `/client/pair` deliberately does not), and only then the bounded claim.
/// Every gate's refusal is the same page.
async fn claim_and_land(
    state: &ApiState,
    site: &Site,
    code: &str,
    ip: std::net::IpAddr,
) -> Response {
    let allowed = state
        .console_signin_limiter
        .lock()
        .map(|mut l| l.check(ip, std::time::Instant::now()))
        .unwrap_or(false);
    if !allowed {
        return refused_sign_in(state, site);
    }
    let normalized = normalize_pairing_code(code);
    if !code_shaped(&normalized) {
        return refused_sign_in(state, site);
    }
    // The claim is a store write reachable with no credential, so it queues on
    // the same bounded slots `/client/pair` does rather than racing them.
    let Ok(_permit) = state.pair_slots().acquire().await else {
        return refused_sign_in(state, site);
    };
    let store = state.store.clone();
    let code = code.to_string();
    let issued =
        tokio::task::spawn_blocking(move || store.claim_pairing_code(&code, CONSOLE_DEVICE_NAME))
            .await;

    match issued {
        Ok(Ok(issued)) => with_cookie(
            see_other("/console"),
            &set_cookie(&issued.token, site.origin_is_https()),
        ),
        // Wrong, expired, already claimed, burned, and a store that could not
        // answer are one page with one status and one sentence.
        _ => refused_sign_in(state, site),
    }
}

/// The one refusal a sign-in attempt can produce. Rendered as the login page so a
/// person can simply try again, and byte-identical across every reason.
fn refused_sign_in(state: &ApiState, site: &Site) -> Response {
    login_page(
        state,
        site,
        Some(
            "That code did not work. A code is good for ten minutes, works once, and stops \
             working after a few wrong tries. Get a new one and try again.",
        ),
    )
}

/// `POST /console/pair` — mint a pairing code for another device.
///
/// The code renders exactly once, on this page. That is the whole purpose of the
/// page, and it is the only secret material any console page ever contains.
async fn pair(
    State(state): State<ApiState>,
    site: Site,
    Extension(_session): Extension<ConsoleSession>,
) -> Response {
    let minted = crate::handlers::store_call(&state, move |store, account_id| {
        store.mint_pairing_code(account_id, Duration::seconds(PAIRING_TTL_SECS))
    })
    .await;

    match minted {
        Ok(minted) => pair_page(&site, &minted.code),
        Err(_) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The code could not be created",
            "Something went wrong writing to this mailbox's database. Try again, and check the \
             daemon's logs if it keeps happening.",
        ),
    }
}

/// `POST /console/revoke/{id}` — revoke one device token.
///
/// Revoking the token THIS BROWSER is holding is allowed and lands where it
/// should: the cookie is cleared and the next page is the login page. A session
/// that revoked itself but kept its cookie would be a half-dead session, which is
/// worse than either state.
async fn revoke(
    State(state): State<ApiState>,
    site: Site,
    Extension(session): Extension<ConsoleSession>,
    id: Result<Path<i64>, PathRejection>,
) -> Response {
    let Ok(Path(id)) = id else {
        return see_other("/console");
    };

    // Account-scoped in the store: an id belonging to anyone else revokes
    // nothing and reports nothing.
    let revoked = crate::handlers::store_call(&state, move |store, account_id| {
        store.revoke_device_token(account_id, id)
    })
    .await;

    match revoked {
        Ok(_) if id == session.token_id => {
            with_cookie(see_other("/console"), &clear_cookie(site.origin_is_https()))
        }
        // A revoke that matched nothing (unknown id, already revoked) is not an
        // error worth a page: the table it came from is about to be re-rendered
        // and will show the truth.
        Ok(_) => see_other("/console"),
        Err(_) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The device could not be revoked",
            "Something went wrong writing to this mailbox's database. Try again, and check the \
             daemon's logs if it keeps happening.",
        ),
    }
}

/// `POST /console/logout` — revoke this browser's own token and clear the cookie.
///
/// A sign-out that only dropped the cookie would leave a live credential behind
/// in a table nobody is looking at, so this really does revoke.
async fn logout(
    State(state): State<ApiState>,
    site: Site,
    Extension(session): Extension<ConsoleSession>,
) -> Response {
    let token_id = session.token_id;
    // A failed revoke still clears the cookie: the browser must not keep
    // presenting a credential the user has asked to be rid of.
    let _ = crate::handlers::store_call(&state, move |store, account_id| {
        store.revoke_device_token(account_id, token_id)
    })
    .await;
    with_cookie(see_other("/console"), &clear_cookie(site.origin_is_https()))
}

// --- pages ------------------------------------------------------------------

/// The login page. Also the answer to every failed sign-in and every dead cookie,
/// which is why the message is a parameter.
///
/// The Google button renders ONLY when `SQUELCH_CONSOLE_SSO_URL` is configured
/// AND this daemon is answering on a hosted-shaped hostname it can name a tenant
/// label from. On self-host neither holds, and the page is simply the code form.
fn login_page(state: &ApiState, site: &Site, error: Option<&str>) -> Response {
    let error_html = error
        .map(|e| format!(r#"<p class="stop"><strong>{}</strong></p>"#, escape_html(e)))
        .unwrap_or_default();

    let sso_html = match (state.console_sso_url(), site.tenant_label()) {
        (Some(sso), Some(label)) => {
            let href = format!("{sso}/console/auth?tenant={}", percent_encode(label));
            format!(
                r#"<p><a class="button" href="{href}">Continue with Google</a></p>
<p class="muted">Signing in with Google proves you own the mailbox this daemon syncs. Passband
checks the address against this mailbox and sends you straight back here.</p>
<h2>Or paste a pairing code</h2>"#,
                href = escape_html(&href),
            )
        }
        _ => String::new(),
    };

    let insecure_html = if state.console_allow_insecure_cookie {
        r#"<p class="stop"><strong>Insecure cookie mode is enabled.</strong> Your console session
can travel over the network without encryption. Use this only on a trusted LAN.</p>"#
    } else {
        ""
    };

    page(
        StatusCode::OK,
        "Passband console",
        &format!(
            r#"<h1>Passband console</h1>
<p>This is the console for one Passband mailbox. Sign in to see what the daemon is doing, pair a
device, or revoke one.</p>
{error_html}
{insecure_html}
{sso_html}
<form method="post" action="/console/login-code">
<label for="code">Pairing code</label>
<input type="text" id="code" name="code" placeholder="XXXX-XXXX" autocomplete="off"
  autocapitalize="characters" spellcheck="false" required>
<p><button type="submit">Sign in</button></p>
</form>
<p class="muted">Run <code>squelchd pair</code> on the machine that runs this daemon to print a
code. A code is good for ten minutes, works once, and stops working after a few wrong tries.</p>
<p class="muted">Signing in pairs this browser like any other device. It shows up in the device
list as <code>console</code>, and you can revoke it from there or from
<code>squelchd token revoke</code>.</p>"#
        ),
    )
}

/// Render a timestamp the one way this console renders timestamps.
fn stamp(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// One row of the devices table. The current session is marked, because "which
/// of these is the browser I am looking at" is the question a person revoking
/// things actually has.
fn device_row(token: &DeviceToken, session: ConsoleSession) -> String {
    let is_me = token.id == session.token_id;
    let name = escape_html(&token.name);
    let this_one = if is_me {
        r#" <span class="muted">(this browser)</span>"#
    } else {
        ""
    };
    let last_used = token
        .last_used_at
        .map_or_else(|| "never".to_string(), |ts| escape_html(&stamp(ts)));
    let (status, action) = match token.revoked_at {
        Some(at) => (
            format!("revoked {}", escape_html(&stamp(at))),
            String::new(),
        ),
        None => (
            "active".to_string(),
            format!(
                r#"<form class="inline" method="post" action="/console/revoke/{id}">
<button type="submit">{label}</button></form>"#,
                id = token.id,
                label = if is_me {
                    "Revoke and sign out"
                } else {
                    "Revoke"
                },
            ),
        ),
    };
    format!(
        "<tr><td>{name}{this_one}</td><td>{created}</td><td>{last_used}</td><td>{status}</td>\
         <td>{action}</td></tr>",
        created = escape_html(&stamp(token.created_at)),
    )
}

/// The signed-in page: who this mailbox is, what the daemon has done with it,
/// what is paired to it, and the two things you can do from here.
async fn home_page(state: &ApiState, site: &Site, session: ConsoleSession) -> Response {
    let since = Utc::now() - Duration::days(STATS_WINDOW_DAYS);
    let loaded = crate::handlers::store_call(state, move |store, account_id| {
        Ok((
            store.account_email(account_id)?,
            store.list_device_tokens(account_id)?,
            store.stats(account_id, since)?,
        ))
    })
    .await;

    let Ok((email, tokens, stats)) = loaded else {
        return problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "This mailbox could not be read",
            "Something went wrong reading this mailbox's database. Check the daemon's logs.",
        );
    };

    let rows: String = tokens
        .iter()
        .map(|t| device_row(t, session))
        .collect::<Vec<_>>()
        .join("\n");

    let cursor = stats.last_history_id.map_or_else(
        || "not yet set, the first sync has not finished".to_string(),
        |id| id.to_string(),
    );
    let surfaced = stats
        .last_surfaced_at
        .map_or_else(|| "nothing has been surfaced yet".to_string(), stamp);
    let url = site.url().unwrap_or_default();

    page(
        StatusCode::OK,
        "Passband console",
        &format!(
            r#"<h1>Passband console</h1>
<p>Signed in as <code>{email}</code>. This daemon answers at <code>{url}</code>.</p>

<h2>Sync</h2>
<dl>
<dt>Messages triaged</dt><dd>{total}</dd>
<dt>Sealed (metadata only)</dt><dd>{sealed}</dd>
<dt>Needing attention in the last {days} days</dt>
<dd>{standing} standing, {new} new, {open} open</dd>
<dt>Last surfaced</dt><dd>{surfaced}</dd>
<dt>Gmail history cursor</dt><dd>{cursor}</dd>
</dl>

<h2>Devices</h2>
<p>Every device paired with this mailbox. Revoking one takes effect on its next request,
with no restart and nothing to wait out.</p>
<table>
<tr><th>Name</th><th>Added</th><th>Last used</th><th>Status</th><th></th></tr>
{rows}
</table>
<form method="post" action="/console/pair"><button type="submit">Create a pairing code</button></form>
<p class="muted">A code lets one new device in. Creating one retires any code that was still
outstanding, so there is never more than one live at a time.</p>

<h2>Agent door</h2>
<p>The agent door (<code>/mcp</code>) is not served on hosted Passband yet. Tenant addresses
publish the client API and the tracking pixel only, and there is nothing to configure here until
that changes. It is tracked as the <code>/mcp</code> bearer auth open item in
<code>deploy/hosted/PRODUCTION.md</code>. A self-hosted daemon can serve it locally today.</p>

<h2>Sign out</h2>
<p>Signing out revokes this browser's own device token, so the cookie it leaves behind is worth
nothing to anybody who finds it.</p>
<form method="post" action="/console/logout"><button type="submit">Sign out</button></form>"#,
            email = escape_html(&email),
            url = escape_html(&url),
            total = stats.total,
            sealed = stats.sealed,
            days = STATS_WINDOW_DAYS,
            standing = stats.bands.standing,
            new = stats.bands.new,
            open = stats.bands.open,
            surfaced = escape_html(&surfaced),
            cursor = escape_html(&cursor),
            rows = rows,
        ),
    )
}

/// The page a freshly minted pairing code renders on, and the only page in the
/// console that carries a secret. It is shown once, it is not logged, and the
/// store cannot reproduce it.
fn pair_page(site: &Site, code: &str) -> Response {
    let link_html = site
        .url()
        .map(|url| {
            format!(
                r#"<p><a class="button" href="{link}">Open Passband and pair</a></p>"#,
                link = escape_html(&deep_link(&url, code)),
            )
        })
        .unwrap_or_default();

    page(
        StatusCode::OK,
        "Pair a device",
        &format!(
            r#"<h1>Pair a device</h1>
<p>Open Passband on the new device, press <strong>Pair</strong>, and enter this code. On this
device you can use the link instead.</p>
<p><span class="code">{code}</span></p>
{link_html}
<p class="muted">The code is good for ten minutes and works once. If it expires before you get to
it, come back and make another.</p>
<p class="muted">Keep it to yourself while it is live. It is what lets a device in.</p>
<p><a class="button" href="/console">Back to the console</a></p>"#,
            code = escape_html(code),
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use squelch_core::store::SqliteStore;
    use squelch_core::types::AccountId;
    use tower::ServiceExt as _;

    use super::*;

    /// The hosted-shaped host every test drives, so `Secure`, the SSO link and
    /// the deep link are all exercised the way production sees them.
    const HOST: &str = "ada.passband.email";
    const ORIGIN: &str = "https://ada.passband.email";
    const SSO: &str = "https://signup.passband.app";

    /// The FULL router, so these tests exercise the real mounting: the console
    /// outside the bearer layer, the bearer tree beside it.
    fn fixture() -> (Arc<SqliteStore>, AccountId, Router) {
        fixture_with(|s| s)
    }

    fn fixture_with(
        tweak: impl FnOnce(ApiState) -> ApiState,
    ) -> (Arc<SqliteStore>, AccountId, Router) {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("ada@example.com").unwrap();
        let state = tweak(ApiState::new(store.clone(), acct, ""));
        (store, acct, crate::router(state))
    }

    fn ttl() -> Duration {
        Duration::seconds(PAIRING_TTL_SECS)
    }

    /// A GET a browser would send.
    fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
        let mut req = Request::builder().uri(uri).header(header::HOST, HOST);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, format!("{COOKIE_NAME}={c}"));
        }
        req.body(Body::empty()).unwrap()
    }

    /// A same-origin form POST, exactly as a browser submits one from our pages.
    fn post(uri: &str, cookie: Option<&str>, form: &str) -> Request<Body> {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .header(SEC_FETCH_SITE, "same-origin")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, format!("{COOKIE_NAME}={c}"));
        }
        req.body(Body::from(form.to_string())).unwrap()
    }

    async fn body_of(resp: Response) -> String {
        String::from_utf8(to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap()
    }

    /// Typing the bare hostname lands on the console, not a 404. Temporary on
    /// purpose: a cached permanent redirect would outlive any future change of
    /// heart about what the root serves.
    #[tokio::test]
    async fn the_root_redirects_to_the_console() {
        let (_store, _acct, app) = fixture();
        let resp = app.oneshot(get("/", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/console"
        );
    }

    /// Line noise must never reach the store, because a store miss charges the
    /// TENANT'S live code (five misses burn it): the shape gate is what keeps a
    /// stranger's garbage from denying the sign-in flow. The proof is the real
    /// code still claiming after five garbage attempts that would otherwise
    /// have burned it.
    #[tokio::test]
    async fn line_noise_cannot_burn_the_real_code() {
        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();
        for junk in ["", "zap", "not-a-code!", "AAAA-AAAA-AAAA-AAAA", "IIII-LLLL"] {
            let resp = app
                .clone()
                .oneshot(post("/console/login-code", None, &format!("code={junk}")))
                .await
                .unwrap();
            assert!(resp.headers().get(header::SET_COOKIE).is_none(), "{junk:?}");
        }
        let resp = app
            .clone()
            .oneshot(post(
                "/console/login-code",
                None,
                &format!("code={}", minted.code),
            ))
            .await
            .unwrap();
        assert!(
            resp.headers().get(header::SET_COOKIE).is_some(),
            "the real code must still claim after garbage that never reached the store"
        );
    }

    /// The sign-in bucket refuses the eleventh attempt in a minute — even a
    /// REAL code — with the same page as every other refusal. In-process tests
    /// attach no ConnectInfo, so every request shares one bucket, which is
    /// exactly the coarse-but-never-a-bypass fallback the limiter documents.
    #[tokio::test]
    async fn the_sign_in_bucket_empties_after_ten() {
        let (store, acct, app) = fixture();
        for _ in 0..10 {
            let _ = app
                .clone()
                .oneshot(post("/console/login-code", None, "code=0000-0000"))
                .await
                .unwrap();
        }
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();
        let resp = app
            .clone()
            .oneshot(post(
                "/console/login-code",
                None,
                &format!("code={}", minted.code),
            ))
            .await
            .unwrap();
        assert!(
            resp.headers().get(header::SET_COOKIE).is_none(),
            "an empty bucket must refuse even a real code"
        );
    }

    /// The token a `Set-Cookie` installs, or `None` when the response cleared it.
    fn cookie_of(resp: &Response) -> Option<String> {
        let raw = resp
            .headers()
            .get(header::SET_COOKIE)?
            .to_str()
            .ok()?
            .to_string();
        let value = raw
            .split(';')
            .next()?
            .split_once('=')
            .map(|(_, v)| v.to_string())?;
        (!value.is_empty()).then_some(value)
    }

    /// Sign a browser in the way a person does, and hand back its cookie.
    async fn sign_in(app: &Router, store: &SqliteStore, acct: AccountId) -> String {
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();
        let resp = app
            .clone()
            .oneshot(post(
                "/console/login-code",
                None,
                &format!("code={}", minted.code),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        cookie_of(&resp).expect("a successful sign-in sets the session cookie")
    }

    /// The whole loop, in the order a person walks it: mint, post the form, get a
    /// cookie, and have the home page render behind it.
    #[tokio::test]
    async fn the_pasted_code_path_signs_a_browser_in() {
        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();

        // Unauthenticated, the console is the login page and says nothing about
        // the mailbox.
        let html = body_of(app.clone().oneshot(get("/console", None)).await.unwrap()).await;
        assert!(html.contains("Pairing code"), "{html}");
        assert!(!html.contains("ada@example.com"), "{html}");

        let resp = app
            .clone()
            .oneshot(post(
                "/console/login-code",
                None,
                &format!("code={}", minted.code),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers()[header::LOCATION], "/console");
        let set = resp.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(set.contains("HttpOnly"), "{set}");
        assert!(set.contains("SameSite=Lax"), "{set}");
        assert!(set.contains("Secure"), "{set}");
        assert!(set.contains("Path=/"), "{set}");
        assert!(!set.contains("Domain"), "{set}");

        let token = cookie_of(&resp).unwrap();
        assert!(token.starts_with("sqd_"), "the cookie IS the device token");

        let html = body_of(
            app.clone()
                .oneshot(get("/console", Some(&token)))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("ada@example.com"), "{html}");
        assert!(html.contains("console"), "the session shows in the table");
        assert!(!html.contains(&token), "a page must never render the token");

        // And the very same cookie value is a working bearer, because it is one
        // credential and not two.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/client/stats")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// The cookie is verified by the same store call the bearer path uses, so it
    /// passes while the token lives and stops the moment it is revoked. Absent,
    /// garbage and revoked all land on the login page, never a bare 401.
    #[tokio::test]
    async fn the_cookie_gates_the_console() {
        let (store, acct, app) = fixture();
        let token = sign_in(&app, &store, acct).await;

        let resp = app
            .clone()
            .oneshot(get("/console", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_of(resp).await.contains("ada@example.com"));

        let issued = store.list_device_tokens(acct).unwrap();
        let mine = issued.iter().find(|t| t.revoked_at.is_none()).unwrap();
        assert!(store.revoke_device_token(acct, mine.id).unwrap());

        for (name, cookie) in [
            ("revoked", Some(token.as_str())),
            ("absent", None),
            ("garbage", Some("not-a-token")),
            ("token-shaped but never issued", Some("sqd_nope")),
            ("empty", Some("")),
        ] {
            let resp = app.clone().oneshot(get("/console", cookie)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{name} was not a page");
            let html = body_of(resp).await;
            assert!(html.contains("Pairing code"), "{name}: {html}");
            assert!(
                !html.contains("ada@example.com"),
                "{name} leaked the mailbox"
            );
        }

        // The mutating routes answer the same way: a page, never a 401.
        for uri in ["/console/pair", "/console/logout"] {
            let resp = app
                .clone()
                .oneshot(post(uri, Some("sqd_nope"), ""))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let cleared = resp.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .to_string();
            assert!(cleared.contains("Max-Age=0"), "{uri}: {cleared}");
            assert!(body_of(resp).await.contains("Pairing code"), "{uri}");
        }
    }

    /// The pair button mints a REAL code: the page shows it once, and the code on
    /// that page claims a working device token at `/client/pair`.
    #[tokio::test]
    async fn the_pair_button_mints_a_code_that_works() {
        let (store, acct, app) = fixture();
        let token = sign_in(&app, &store, acct).await;

        let html = body_of(
            app.clone()
                .oneshot(post("/console/pair", Some(&token), ""))
                .await
                .unwrap(),
        )
        .await;
        assert!(html.contains("ten minutes"), "the TTL is stated in words");
        assert!(
            html.contains("passband://pair?url=https%3A%2F%2Fada.passband.email"),
            "{html}"
        );

        // Pull the code back out of the page and spend it on the real claim.
        let code = html
            .split(r#"<span class="code">"#)
            .nth(1)
            .and_then(|rest| rest.split('<').next())
            .unwrap()
            .to_string();
        assert_eq!(code.len(), 9, "XXXX-XXXX");

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/client/pair")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"code": code, "device_name": "phone"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "the printed code claims");
    }

    /// Revoking the token your own cookie carries logs you out cleanly: the
    /// cookie is cleared and the next page is the login page. Same for sign out.
    #[tokio::test]
    async fn revoking_your_own_session_logs_you_out() {
        for uri in ["logout", "revoke-self"] {
            let (store, acct, app) = fixture();
            let token = sign_in(&app, &store, acct).await;
            let mine = store
                .list_device_tokens(acct)
                .unwrap()
                .into_iter()
                .find(|t| t.revoked_at.is_none())
                .unwrap();

            let path = if uri == "logout" {
                "/console/logout".to_string()
            } else {
                format!("/console/revoke/{}", mine.id)
            };
            let resp = app
                .clone()
                .oneshot(post(&path, Some(&token), ""))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::SEE_OTHER, "{uri}");
            assert_eq!(resp.headers()[header::LOCATION], "/console");
            let cleared = resp.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .to_string();
            assert!(cleared.contains("Max-Age=0"), "{uri}: {cleared}");

            // The token really is dead, not merely forgotten by the browser.
            assert!(
                store
                    .list_device_tokens(acct)
                    .unwrap()
                    .iter()
                    .all(|t| t.revoked_at.is_some()),
                "{uri} left a live token behind"
            );
            let html = body_of(app.oneshot(get("/console", Some(&token))).await.unwrap()).await;
            assert!(html.contains("Pairing code"), "{uri}: {html}");
        }
    }

    /// Revoking somebody ELSE's device leaves this session alone: no cleared
    /// cookie, and the row comes back revoked.
    #[tokio::test]
    async fn revoking_another_device_does_not_touch_this_session() {
        let (store, acct, app) = fixture();
        let token = sign_in(&app, &store, acct).await;
        let phone = store.issue_device_token(acct, "phone").unwrap();

        let resp = app
            .clone()
            .oneshot(post(
                &format!("/console/revoke/{}", phone.id),
                Some(&token),
                "",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(resp.headers().get(header::SET_COOKIE).is_none());

        let html = body_of(app.oneshot(get("/console", Some(&token))).await.unwrap()).await;
        assert!(html.contains("ada@example.com"), "still signed in");
        assert!(html.contains("revoked "), "the table shows the tombstone");
    }

    /// EVERY mutating POST refuses a cross-site request, whether it carries a
    /// session or not, before it reaches a handler.
    #[tokio::test]
    async fn cross_site_posts_are_refused() {
        let (store, acct, app) = fixture();
        let token = sign_in(&app, &store, acct).await;
        let phone = store.issue_device_token(acct, "phone").unwrap();

        let targets = [
            "/console/login-code".to_string(),
            "/console/pair".to_string(),
            "/console/logout".to_string(),
            format!("/console/revoke/{}", phone.id),
        ];

        for uri in &targets {
            for (name, origin, fetch_site) in [
                ("evil origin", Some("https://evil.example"), None),
                (
                    "cross-site fetch metadata",
                    Some(ORIGIN),
                    Some("cross-site"),
                ),
                ("same-site sibling tenant", Some(ORIGIN), Some("same-site")),
                ("no origin at all", None, None),
            ] {
                let mut req = Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::HOST, HOST)
                    .header(header::COOKIE, format!("{COOKIE_NAME}={token}"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
                if let Some(o) = origin {
                    req = req.header(header::ORIGIN, o);
                }
                if let Some(f) = fetch_site {
                    req = req.header(SEC_FETCH_SITE, f);
                }
                let resp = app
                    .clone()
                    .oneshot(req.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri} / {name}");
                assert!(resp.headers().get(header::SET_COOKIE).is_none());
            }
        }

        // Nothing was spent: the device is still live and the session still works.
        assert!(
            store
                .list_device_tokens(acct)
                .unwrap()
                .iter()
                .filter(|t| t.revoked_at.is_none())
                .count()
                == 2
        );
    }

    /// An `Origin` header alone (no fetch metadata) is enough, and it must match
    /// the origin this request arrived on, scheme included.
    #[tokio::test]
    async fn origin_alone_is_checked_against_this_origin() {
        let (store, acct, app) = fixture();
        let token = sign_in(&app, &store, acct).await;

        for (name, origin, expect) in [
            ("exact match", ORIGIN, StatusCode::OK),
            (
                "plain http twin",
                "http://ada.passband.email",
                StatusCode::FORBIDDEN,
            ),
            (
                "sibling tenant",
                "https://eve.passband.email",
                StatusCode::FORBIDDEN,
            ),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/console/pair")
                        .header(header::HOST, HOST)
                        .header(header::ORIGIN, origin)
                        .header(header::COOKIE, format!("{COOKIE_NAME}={token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), expect, "{name}");
        }
    }

    /// The Google button is a hosted-only affordance: no configured SSO URL means
    /// no button, which is the self-host posture.
    #[tokio::test]
    async fn the_sso_link_renders_only_when_it_is_configured() {
        let (_, _, app) = fixture();
        let html = body_of(app.oneshot(get("/console", None)).await.unwrap()).await;
        assert!(!html.contains("Continue with Google"), "{html}");
        assert!(!html.contains("signup.passband.app"), "{html}");

        let (_, _, app) = fixture_with(|s| s.with_console_sso_url(Some(SSO.to_string())));
        let html = body_of(app.clone().oneshot(get("/console", None)).await.unwrap()).await;
        assert!(html.contains("Continue with Google"), "{html}");
        assert!(
            html.contains("https://signup.passband.app/console/auth?tenant=ada"),
            "{html}"
        );

        // ...and only on a hostname it can name a tenant label from. A loopback
        // self-host that somehow has the variable set still shows no button.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/console")
                    .header(header::HOST, "localhost:8848")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!body_of(resp).await.contains("Continue with Google"));
    }

    /// The callback claims a real, control-plane-minted code, and refuses every
    /// kind of bad one with the SAME bytes: nothing here is an oracle for which
    /// codes exist.
    #[tokio::test]
    async fn the_callback_claims_a_code_and_refuses_uniformly() {
        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();

        let resp = app
            .clone()
            .oneshot(get(
                &format!("/console/callback?code={}", minted.code),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers()[header::LOCATION], "/console");
        let token = cookie_of(&resp).expect("the callback sets the session cookie");
        assert!(token.starts_with("sqd_"));

        // A code the store burned by too many misses, one that was never minted,
        // an expired one, the code just spent, and no code at all.
        let burned = store.mint_pairing_code(acct, ttl()).unwrap();
        for _ in 0..5 {
            let _ = store.claim_pairing_code("ZZZZ-ZZZZ", "attacker");
        }
        let expired = store
            .mint_pairing_code(acct, Duration::seconds(-1))
            .unwrap();

        let mut answers = Vec::new();
        for uri in [
            format!("/console/callback?code={}", burned.code),
            format!("/console/callback?code={}", minted.code),
            format!("/console/callback?code={}", expired.code),
            "/console/callback?code=AAAA-AAAA".to_string(),
            "/console/callback".to_string(),
        ] {
            let resp = app.clone().oneshot(get(&uri, None)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            assert!(resp.headers().get(header::SET_COOKIE).is_none(), "{uri}");
            answers.push(body_of(resp).await);
        }
        assert!(
            answers.windows(2).all(|w| w[0] == w[1]),
            "the refusals differ, which makes them an oracle"
        );
        assert!(answers[0].contains("That code did not work"));

        // The pasted-code form refuses with the very same page, so the two doors
        // cannot be told apart either.
        let resp = app
            .oneshot(post("/console/login-code", None, "code=AAAA-AAAA"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_of(resp).await, answers[0]);
    }

    /// A loopback self-host is the one place a cookie may go out without
    /// `Secure`, because a browser will not store a `Secure` cookie from
    /// `http://` and the console would simply not work.
    #[tokio::test]
    async fn the_loopback_exception_drops_secure_and_nothing_else() {
        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/console/login-code")
                    .header(header::HOST, "127.0.0.1:8849")
                    .header(SEC_FETCH_SITE, "same-origin")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("code={}", minted.code)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let set = resp.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(!set.contains("Secure"), "{set}");
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Lax"),
            "{set}"
        );
    }

    /// The console is NOT behind the bearer layer and NOT inside the `/client`
    /// CORS layer. The first would make signing in impossible; the second would
    /// hand a cookie-authenticated surface to any page on the web.
    #[tokio::test]
    async fn the_console_is_mounted_outside_the_bearer_and_outside_cors() {
        let (_, _, app) = fixture();
        let resp = app.clone().oneshot(get("/console", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "the console must never answer with CORS headers"
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/client/stats")
                    .header(header::HOST, HOST)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "gated sibling");
    }

    /// Every page carries the same headers, states no em dash, and escapes what
    /// it interpolates.
    #[tokio::test]
    async fn every_page_carries_the_headers_and_the_house_copy_rules() {
        let (store, acct, app) = fixture();
        let token = sign_in(&app, &store, acct).await;

        for req in [
            get("/console", None),
            get("/console", Some(&token)),
            post("/console/pair", Some(&token), ""),
            post("/console/pair", None, ""),
        ] {
            let resp = app.clone().oneshot(req).await.unwrap();
            let h = resp.headers().clone();
            assert_eq!(h[header::X_FRAME_OPTIONS], "DENY");
            assert_eq!(h[header::REFERRER_POLICY], "no-referrer");
            assert_eq!(h[header::CACHE_CONTROL], "no-store, no-cache");
            assert!(
                h[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .contains("default-src 'none'")
            );
            let html = body_of(resp).await;
            assert!(!html.contains('\u{2014}'), "em dash: {html}");
            assert!(!html.contains("<script"), "{html}");
            assert!(!html.contains("http://"), "{html}");
        }
    }

    /// A device NAME is user-supplied (it comes from a pairing claim body) and
    /// lands in the devices table, so it is an escape path.
    #[tokio::test]
    async fn the_devices_table_escapes_what_a_device_called_itself() {
        let (store, acct, app) = fixture();
        let token = sign_in(&app, &store, acct).await;
        store
            .issue_device_token(acct, r#"<script>alert(1)</script>"#)
            .unwrap();

        let html = body_of(app.oneshot(get("/console", Some(&token))).await.unwrap()).await;
        assert!(!html.contains("<script>alert(1)"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn the_cookie_is_found_among_others_and_bounded() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("other=1; {COOKIE_NAME}=sqd_abc; another=2")
                .parse()
                .unwrap(),
        );
        assert_eq!(cookie_token(&headers).as_deref(), Some("sqd_abc"));

        // A name that merely ENDS with ours is a different cookie.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("x{COOKIE_NAME}=sqd_abc").parse().unwrap(),
        );
        assert_eq!(cookie_token(&headers), None);

        // Several Cookie headers are all searched.
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, "other=1".parse().unwrap());
        headers.append(
            header::COOKIE,
            format!("{COOKIE_NAME}=sqd_abc").parse().unwrap(),
        );
        assert_eq!(cookie_token(&headers).as_deref(), Some("sqd_abc"));

        // An absurd value never reaches a hash.
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("{COOKIE_NAME}={}", "a".repeat(MAX_COOKIE_LEN + 1))
                .parse()
                .unwrap(),
        );
        assert_eq!(cookie_token(&headers), None);
    }

    #[test]
    fn loopback_is_the_only_automatically_insecure_origin() {
        for host in [
            "localhost",
            "localhost:8849",
            "127.0.0.1:8849",
            "[::1]:8849",
        ] {
            assert!(is_loopback(host), "{host}");
            assert!(
                !(Site {
                    host: host.into(),
                    ..Site::default()
                })
                .origin_is_https(),
                "{host}"
            );
        }
        for host in ["ada.passband.email", "192.168.1.9", "127notlocal.example"] {
            assert!(!is_loopback(host), "{host}");
            assert!(
                (Site {
                    host: host.into(),
                    ..Site::default()
                })
                .origin_is_https(),
                "{host}"
            );
        }
    }

    #[tokio::test]
    async fn configured_insecure_mode_supports_plain_http_lan_and_warns() {
        let (store, acct, app) =
            fixture_with(|state| state.with_console_allow_insecure_cookie(true));

        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/console")
                    .header(header::HOST, "192.168.1.9:8849")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        assert!(
            body_of(login)
                .await
                .contains("Insecure cookie mode is enabled")
        );

        let minted = store.mint_pairing_code(acct, ttl()).unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/console/login-code")
                    .header(header::HOST, "192.168.1.9:8849")
                    .header(header::ORIGIN, "http://192.168.1.9:8849")
                    .header(SEC_FETCH_SITE, "same-origin")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("code={}", minted.code)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let set = resp.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(!set.contains("Secure"), "{set}");
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Lax"),
            "{set}"
        );
    }

    /// The `Secure` attribute is the visible half of the hatch. This is the
    /// other half, and the half a browser without `Sec-Fetch-Site` actually
    /// depends on: [`same_origin_request`] falls back to comparing `Origin`
    /// whole, scheme included, against [`Site::url`]. On a plain-http LAN with
    /// the hatch shut that comparison expects `https://` and refuses every POST
    /// the console can make, so the login it just rendered cannot be completed.
    #[tokio::test]
    async fn the_hatch_is_what_lets_a_plain_http_lan_post_at_all() {
        let lan_post = |code: &str| {
            Request::builder()
                .method("POST")
                .uri("/console/login-code")
                .header(header::HOST, "192.168.1.9:8849")
                .header(header::ORIGIN, "http://192.168.1.9:8849")
                // NO `Sec-Fetch-Site`: it is believed absolutely where it is
                // present, so sending it would return before `Origin` is ever
                // consulted and this test would pass exercising nothing.
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("code={code}")))
                .unwrap()
        };

        let (store, acct, app) =
            fixture_with(|state| state.with_console_allow_insecure_cookie(true));
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();
        let resp = app.oneshot(lan_post(&minted.code)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);

        let (store, acct, app) = fixture();
        let minted = store.mint_pairing_code(acct, ttl()).unwrap();
        let resp = app.oneshot(lan_post(&minted.code)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// The other side of that coin, and the reason the hatch is documented as a
    /// declaration about the whole origin rather than as a cookie flag: turning
    /// it on where the console really is https costs the SSO button and the
    /// `Origin` fallback too, because all three follow the one answer.
    #[tokio::test]
    async fn the_hatch_on_an_https_origin_costs_the_sso_link_and_the_origin_check() {
        let (store, acct, app) = fixture_with(|s| {
            s.with_console_sso_url(Some(SSO.to_string()))
                .with_console_allow_insecure_cookie(true)
        });

        let html = body_of(app.clone().oneshot(get("/console", None)).await.unwrap()).await;
        assert!(!html.contains("Continue with Google"), "{html}");

        let minted = store.mint_pairing_code(acct, ttl()).unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/console/login-code")
                    .header(header::HOST, "ada.passband.email")
                    .header(header::ORIGIN, "https://ada.passband.email")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("code={}", minted.code)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// The label only ever feeds the SSO link, so anything that is not plainly a
    /// hosted tenant hostname yields nothing rather than a guess.
    #[test]
    fn the_tenant_label_is_the_leftmost_hosted_label_or_nothing() {
        let label = |host: &str| {
            (Site {
                host: host.into(),
                ..Site::default()
            })
            .tenant_label()
            .map(str::to_string)
        };
        assert_eq!(label("ada.passband.email").as_deref(), Some("ada"));
        assert_eq!(label("ada-2.passband.email").as_deref(), Some("ada-2"));
        assert_eq!(label("ada.passband.email:8443").as_deref(), Some("ada"));
        for host in [
            "passband.email",     // no tenant part
            "localhost:8849",     // loopback
            "127.0.0.1",          // an address, not a name
            "-ad.passband.email", // hyphen edge
            "ad.passband.email",  // too short
            "",                   // no host at all
        ] {
            assert_eq!(label(host), None, "{host}");
        }
        assert_eq!(label(&format!("{}.passband.email", "a".repeat(31))), None);
    }

    #[test]
    fn the_deep_link_is_the_shape_the_app_parses() {
        assert_eq!(
            deep_link("https://ada.passband.email", "ABCD-EFGH"),
            "passband://pair?url=https%3A%2F%2Fada.passband.email&code=ABCD-EFGH"
        );
    }

    #[test]
    fn escaping_covers_the_characters_that_end_a_page() {
        assert_eq!(
            escape_html(r#"<script>alert("x&y")</script>"#),
            "&lt;script&gt;alert(&quot;x&amp;y&quot;)&lt;/script&gt;"
        );
        assert_eq!(escape_html("it's"), "it&#39;s");
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
    }
}
