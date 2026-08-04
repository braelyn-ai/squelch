//! `POST /v1/sessions`, `GET /link`, `GET /callback`, `POST /v1/claim`, and
//! `GET /healthz`.
//!
//! Two of these routes answer daemons in JSON and two answer humans in HTML,
//! and the split matters: the JSON routes describe exactly why they refused
//! something, while the HTML pages say only what a person can act on. Neither
//! ever renders a code.
//!
//! PRIVACY: session ids, claim tokens and hashes, auth codes, and auth URLs
//! never reach a log line at any level — counts, statuses, and timings only. No
//! error message echoes the value it rejected, because these strings travel
//! back over the wire into somebody else's log.

use std::time::Duration;

use axum::{
    Json,
    body::Bytes,
    extract::{RawQuery, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::sessions::{ClaimOutcome, ParkOutcome, Parked, RegisterError, SESSION_TTL, SessionKind};
use crate::state::BrokerState;
use crate::validate;

/// Ceiling on a registration or claim body. The largest legitimate one is an
/// auth URL plus two short strings, and an unauthenticated POST route should
/// not be a place to spend somebody else's memory.
pub const MAX_BODY: usize = 16 * 1024;

/// Google's authorization codes run around 250 characters today. The cap is
/// loose enough to survive a format change and tight enough that a parked
/// session cannot be used as storage.
const MAX_CODE: usize = 512;

/// OAuth error codes are short identifiers (`access_denied`).
const MAX_ERROR: usize = 64;

/// The claim token is a pre-image the broker only ever hashes: it is never
/// rendered, never logged, and never compared as a string, so its shape is the
/// daemon's business. Bounding its length is all this route needs.
const MAX_CLAIM_TOKEN: usize = 512;

/// What a callback reports when Google's `error` is missing, empty, or shaped
/// like something other than an OAuth error code.
const GENERIC_ERROR: &str = "invalid_request";

/// A handler error carrying an HTTP status and a client-safe message.
#[derive(Debug)]
pub struct BrokerError {
    status: StatusCode,
    message: String,
}

impl BrokerError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl IntoResponse for BrokerError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

pub async fn healthz() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    session_id: String,
    claim_token_hash: String,
    auth_url: String,
}

/// `POST /v1/sessions` — a daemon parking the consent URL it just built.
///
/// Unauthenticated by design: every stranger's self-hosted daemon is a
/// legitimate client. What stops this from being useful to anyone else is that
/// the URL must be Google's own authorization endpoint pointed back at this
/// broker's callback, and the code it eventually parks is worthless without the
/// PKCE verifier that never leaves the daemon.
pub async fn register(
    State(state): State<BrokerState>,
    body: Bytes,
) -> Result<Response, BrokerError> {
    let req: RegisterRequest = parse_json(&body)?;

    if !validate::is_session_id(&req.session_id) {
        return Err(BrokerError::bad_request(format!(
            "session_id must be {} base64url characters (32 bytes)",
            validate::SESSION_ID_LEN
        )));
    }
    let claim_token_hash = validate::claim_token_hash(&req.claim_token_hash).ok_or_else(|| {
        BrokerError::bad_request(format!(
            "claim_token_hash must be {} lowercase hex characters (a SHA-256)",
            validate::CLAIM_TOKEN_HASH_LEN
        ))
    })?;
    // The one field that could turn this service into an open redirect wearing
    // our domain.
    validate::auth_url(&req.auth_url, &req.session_id, &state.config().callback_url)
        .map_err(|e| BrokerError::bad_request(e.to_string()))?;

    // `SelfHost` is the only kind today; the hosted flow's consent URL is built
    // by the confidential web client and claimed by the control plane, and
    // arrives here as a second kind rather than a second table.
    let kind = SessionKind::SelfHost;
    match state.register(req.session_id, kind, claim_token_hash, req.auth_url) {
        Ok(()) => {
            // PRIVACY: a kind and a count. Never the id, the hash, or the URL.
            tracing::info!(
                kind = kind.as_str(),
                sessions = state.live_sessions(),
                "session registered"
            );
            Ok((
                StatusCode::CREATED,
                Json(json!({ "expires_in": SESSION_TTL.as_secs() })),
            )
                .into_response())
        }
        // Never an overwrite: a stranger who guessed a live id must not be able
        // to retarget a consent already in flight.
        Err(RegisterError::Duplicate) => Err(BrokerError::new(
            StatusCode::CONFLICT,
            "session_id is already registered",
        )),
        Err(RegisterError::Full) => {
            tracing::warn!(sessions = state.live_sessions(), "session table full");
            Err(BrokerError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the session table is full; retry shortly",
            ))
        }
    }
}

/// `GET /link?s=<session_id>` — the human-facing interstitial.
///
/// Unknown, malformed, and expired all render the same page: the session id is
/// the only thing a stranger could probe here, and telling them apart would
/// answer "does this session exist?" for anyone who asks.
pub async fn link(State(state): State<BrokerState>, RawQuery(query): RawQuery) -> Response {
    let session_id = param(query.as_deref(), "s").unwrap_or_default();
    if !validate::is_session_id(&session_id) {
        return expired_page();
    }
    let Some(auth_url) = state.auth_url(&session_id) else {
        return expired_page();
    };
    tracing::info!("link shown");
    interstitial_page(&auth_url)
}

/// `GET /callback?code=…&state=…` (or `?error=…&state=…`) — Google's redirect
/// target, opened by the user's browser.
///
/// The page never displays the code, and the code never appears anywhere but
/// the session table and the eventual claim response.
pub async fn callback(State(state): State<BrokerState>, RawQuery(query): RawQuery) -> Response {
    let query = query.as_deref();
    // First value wins on a repeat: Google sends one of each, and a crafted URL
    // is fully attacker-controlled anyway, so there is nothing to gain by
    // refusing rather than picking deterministically.
    let session_id = param(query, "state").unwrap_or_default();
    let code = param(query, "code");
    let error = param(query, "error");

    if !validate::is_session_id(&session_id) {
        return expired_page();
    }

    let (outcome, denied) = match (error, code) {
        // A refusal is an outcome, not a failure: the daemon needs to hear it
        // so it can stop polling and say why.
        (Some(e), _) => (Parked::Denied(sanitize_error(&e)), true),
        (None, Some(c)) if is_code(&c) => (Parked::Code(c), false),
        // Neither, or a code shaped like nothing Google issues.
        _ => return invalid_callback_page(),
    };

    match state.park(&session_id, outcome) {
        ParkOutcome::Parked => {
            tracing::info!(denied, "callback parked");
            if denied {
                declined_page()
            } else {
                authorized_page()
            }
        }
        // First write wins, so a replayed redirect cannot swap the code a
        // daemon is about to claim for one an attacker holds.
        ParkOutcome::AlreadyUsed => {
            tracing::info!("callback replayed");
            already_used_page()
        }
        ParkOutcome::Unknown => expired_page(),
    }
}

#[derive(Debug, Deserialize)]
struct ClaimRequest {
    session_id: String,
    claim_token: String,
}

/// `POST /v1/claim` — the daemon polling for its code.
///
/// One successful claim empties the session; a second gets a 404. A wrong token
/// is a 403 that leaves the session intact, so guessing cannot be used to
/// destroy someone else's pending consent.
pub async fn claim(State(state): State<BrokerState>, body: Bytes) -> Result<Response, BrokerError> {
    let req: ClaimRequest = parse_json(&body)?;

    if !validate::is_session_id(&req.session_id) {
        return Err(BrokerError::bad_request(format!(
            "session_id must be {} base64url characters (32 bytes)",
            validate::SESSION_ID_LEN
        )));
    }
    if req.claim_token.is_empty() || req.claim_token.len() > MAX_CLAIM_TOKEN {
        return Err(BrokerError::bad_request(format!(
            "claim_token must be 1..={MAX_CLAIM_TOKEN} bytes"
        )));
    }

    let presented: [u8; 32] = Sha256::digest(req.claim_token.as_bytes()).into();
    let (status, body) = match state.claim(&req.session_id, &presented) {
        ClaimOutcome::Pending => (StatusCode::OK, json!({ "status": "pending" })),
        ClaimOutcome::Complete(code) => (
            StatusCode::OK,
            json!({ "status": "complete", "code": code }),
        ),
        ClaimOutcome::Denied(error) => (
            StatusCode::OK,
            json!({ "status": "denied", "error": error }),
        ),
        ClaimOutcome::Forbidden => (
            StatusCode::FORBIDDEN,
            json!({ "error": "claim token mismatch" }),
        ),
        // Expired, already claimed, or never existed: one answer for all three,
        // and the daemon's recovery is the same in every case.
        ClaimOutcome::Unknown => (StatusCode::NOT_FOUND, json!({ "status": "unknown" })),
    };
    // PRIVACY: the status word only. Never the id, the token, or the code.
    tracing::info!(status = %status.as_u16(), "claim");
    Ok((status, Json(body)).into_response())
}

/// Parse a JSON body by hand so every malformed body gets the same 400 +
/// `{"error": …}` shape as a failed bound check. serde's `Display` embeds the
/// offending value verbatim, which would reflect a claim token into a body an
/// upstream proxy might log — only the error class and position go back.
fn parse_json<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, BrokerError> {
    serde_json::from_slice(body).map_err(|e| {
        BrokerError::bad_request(format!(
            "invalid request body: {:?} error at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ))
    })
}

/// First value for `name` in a raw query string. `form_urlencoded` never fails,
/// so a garbled query is a missing parameter rather than a rejection shape the
/// human-facing routes would have to render.
fn param(query: Option<&str>, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// The shape of an authorization code. Printable ASCII with no spaces, because
/// this string is handed to a daemon that will put it in an HTTP request and
/// possibly a terminal: a code carrying a control character or a newline is not
/// a code, it is an injection into someone else's log.
fn is_code(code: &str) -> bool {
    (1..=MAX_CODE).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_graphic())
}

/// OAuth error codes are lowercase identifiers. Anything else is reported as a
/// generic error rather than passed through: this string reaches a daemon's
/// terminal, and Google is not the only party who can put it in the URL.
fn sanitize_error(error: &str) -> String {
    let ok = (1..=MAX_ERROR).contains(&error.len())
        && error
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        error.to_string()
    } else {
        GENERIC_ERROR.to_string()
    }
}

/// Minutes, for the pages. Derived so the copy cannot drift from the TTL.
fn ttl_minutes() -> u64 {
    SESSION_TTL.as_secs() / Duration::from_secs(60).as_secs()
}

/// Escape text for HTML. Everything interpolated into a page goes through here,
/// including strings that have already been validated: "this one was checked
/// elsewhere" is how the exception becomes the rule.
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

/// The shell every page shares: no scripts, no external assets, no fonts to
/// fetch. A consent interstitial that pulls in a third party is a consent
/// interstitial a third party can watch.
///
/// `body` is markup by necessity and is built above from escaped values; the
/// title is a value, so it is escaped here rather than trusted for being a
/// literal today.
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
main {{ max-width: 32rem; margin: 0 auto; }}
h1 {{ font-size: 1.35rem; font-weight: 600; letter-spacing: -0.01em; margin: 0 0 1rem; }}
p {{ margin: 0 0 1rem; }}
.muted {{ color: #6b6b6b; font-size: 0.9rem; }}
code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9em;
  background: #efece7; padding: 0.1em 0.35em; border-radius: 3px; }}
a.button {{ display: inline-block; margin: 0.25rem 0 1.25rem; padding: 0.65rem 1.15rem;
  background: #1a1a1a; color: #fbfaf8; text-decoration: none; border-radius: 6px; font-weight: 500; }}
@media (prefers-color-scheme: dark) {{
  body {{ background: #141414; color: #e8e6e3; }}
  .muted {{ color: #9a9a9a; }}
  code {{ background: #262626; }}
  a.button {{ background: #e8e6e3; color: #141414; }}
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
            // These URLs name a session. A shared cache must not keep them, and
            // the click through to Google must not carry this URL as a referer.
            (header::CACHE_CONTROL, "no-store, no-cache"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        html,
    )
        .into_response()
}

/// The interstitial: what is being authorized, what this service cannot do, and
/// one link onward to Google.
fn interstitial_page(auth_url: &str) -> Response {
    let minutes = ttl_minutes();
    page(
        StatusCode::OK,
        "Authorize Passband",
        &format!(
            r#"<h1>Authorize Passband</h1>
<p>Your own copy of <code>squelchd</code>, the Passband mail daemon, is asking for
access to your Google account. Continue to Google to see exactly which
permissions it wants, and to approve or refuse them.</p>
<p>This broker never sees your password or your tokens, and it is
cryptographically incapable of minting them: all it does is hold Google's
one-time code for a few minutes so that your daemon, which alone holds the
secret that makes the code usable, can collect it.</p>
<p><a class="button" href="{href}" rel="noreferrer">Continue to Google</a></p>
<p class="muted">This link is good for {minutes} minutes and works once.</p>"#,
            href = escape_html(auth_url)
        ),
    )
}

/// Google came back with a code; the daemon has not collected it yet.
fn authorized_page() -> Response {
    page(
        StatusCode::OK,
        "Authorized",
        r#"<h1>Authorized</h1>
<p>Google sent the authorization back to your own machine. You can close this
tab and return to your terminal.</p>
<p class="muted">Nothing about your mail passed through this page.</p>"#,
    )
}

/// The user said no. Nothing was authorized, and the daemon will say so.
fn declined_page() -> Response {
    page(
        StatusCode::OK,
        "Consent declined",
        r#"<h1>Consent declined</h1>
<p>Nothing was authorized. You can close this tab and return to your terminal,
where <code>squelchd</code> will report that consent was refused.</p>"#,
    )
}

/// Unknown, malformed, or expired: one page for all three.
fn expired_page() -> Response {
    let minutes = ttl_minutes();
    page(
        StatusCode::NOT_FOUND,
        "Link expired",
        &format!(
            r#"<h1>That link has expired</h1>
<p>Authorization links are good for {minutes} minutes and can be used once.</p>
<p>Run <code>squelchd auth</code> again on your own machine to get a new one.</p>"#
        ),
    )
}

/// A second callback for a session that already has an outcome parked.
fn already_used_page() -> Response {
    page(
        StatusCode::CONFLICT,
        "Link already used",
        r#"<h1>That link has already been used</h1>
<p>Each authorization is collected once. If your daemon is still waiting, run
<code>squelchd auth</code> again to start over.</p>"#,
    )
}

/// A request to `/callback` carrying neither a usable code nor an error.
fn invalid_callback_page() -> Response {
    page(
        StatusCode::BAD_REQUEST,
        "Nothing to authorize",
        r#"<h1>That was not a valid response from Google</h1>
<p>Nothing was authorized and nothing was stored. Run <code>squelchd auth</code>
again on your own machine to start over.</p>"#,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_first_value_of_a_query_parameter() {
        assert_eq!(param(Some("s=abc"), "s").as_deref(), Some("abc"));
        assert_eq!(param(Some("a=1&s=abc&b=2"), "s").as_deref(), Some("abc"));
        assert_eq!(
            param(Some("s=first&s=second"), "s").as_deref(),
            Some("first")
        );
        assert_eq!(param(Some("s=a%20b"), "s").as_deref(), Some("a b"));
        assert_eq!(param(Some("s="), "s").as_deref(), Some(""));
        assert_eq!(param(Some("x=1"), "s"), None);
        assert_eq!(param(None, "s"), None);
        // A garbled query is a missing parameter, never a rejection.
        assert_eq!(param(Some("%%%&&&"), "s"), None);
    }

    #[test]
    fn accepts_only_code_shaped_codes() {
        assert!(is_code("4/0AeanS0b-abcDEF_123"));
        assert!(is_code(&"a".repeat(MAX_CODE)));

        assert!(!is_code(""));
        assert!(!is_code(&"a".repeat(MAX_CODE + 1)));
        // Nothing that could break a line in a daemon's log or terminal.
        for bad in [
            "has space",
            "line\nbreak",
            "tab\there",
            "nul\0byte",
            "bell\x07",
        ] {
            assert!(!is_code(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn passes_through_oauth_error_codes_and_flattens_everything_else() {
        assert_eq!(sanitize_error("access_denied"), "access_denied");
        assert_eq!(
            sanitize_error("admin-policy_enforced2"),
            "admin-policy_enforced2"
        );

        for bad in [
            "",
            "Access Denied",
            "ACCESS_DENIED",
            "<script>alert(1)</script>",
            "access_denied\nrm -rf /",
            &"a".repeat(MAX_ERROR + 1),
        ] {
            assert_eq!(sanitize_error(bad), GENERIC_ERROR, "{bad:?}");
        }
    }

    #[test]
    fn escapes_every_html_metacharacter() {
        assert_eq!(
            escape_html(r#"<a href="x" onclick='y'>&</a>"#),
            "&lt;a href=&quot;x&quot; onclick=&#39;y&#39;&gt;&amp;&lt;/a&gt;"
        );
        assert_eq!(escape_html("plain text"), "plain text");
    }

    /// The interstitial is the page a user reads before handing Google their
    /// consent, so the promise it makes is load-bearing copy, not decoration.
    #[tokio::test]
    async fn the_interstitial_names_the_product_and_states_what_we_cannot_do() {
        let url = "https://accounts.google.com/o/oauth2/v2/auth?client_id=c&state=s";
        let html = render(interstitial_page(url)).await;

        assert!(html.contains("Passband"));
        assert!(html.contains("squelchd"));
        assert!(html.contains("cryptographically incapable of minting them"));
        assert!(html.contains("never sees your password or your tokens"));
        assert!(html.contains("10 minutes"));

        // The link goes to the registered URL, with its separators escaped for
        // the attribute rather than left raw.
        assert!(html.contains(
            r#"href="https://accounts.google.com/o/oauth2/v2/auth?client_id=c&amp;state=s""#
        ));

        // Self-contained: nothing to fetch, nothing to run.
        assert!(!html.contains("<script"));
        assert!(!html.contains("http://"));
        assert!(!html.to_lowercase().contains("src="));
        assert!(!html.contains("stylesheet"));
    }

    /// No page may render a code, and the terminal ones link nowhere.
    #[tokio::test]
    async fn the_outcome_pages_carry_no_session_material() {
        for resp in [
            authorized_page(),
            declined_page(),
            already_used_page(),
            expired_page(),
            invalid_callback_page(),
        ] {
            let html = render(resp).await;
            assert!(!html.contains("<a "), "an outcome page must link nowhere");
            assert!(!html.contains("<script"));
            assert!(html.contains("squelchd") || html.contains("terminal"));
        }
    }

    #[test]
    fn pages_are_uncacheable_and_leak_no_referer() {
        let resp = interstitial_page("https://accounts.google.com/o/oauth2/v2/auth?x=1");
        let h = resp.headers();
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(
            h[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("no-store")
        );
        assert_eq!(h[header::REFERRER_POLICY], "no-referrer");
    }

    /// The copy quotes the TTL; deriving it is what keeps the two in step.
    #[test]
    fn the_page_copy_tracks_the_ttl() {
        assert_eq!(ttl_minutes(), SESSION_TTL.as_secs() / 60);
    }

    async fn render(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), MAX_BODY)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }
}
