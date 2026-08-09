//! The four routes: the signup form, the form post, Google's callback, and
//! liveness.
//!
//! THE SHAPE OF THE FLOW, and why it is split where it is:
//!
//! 1. `POST /signup` validates everything that can be validated BEFORE a human
//!    is sent to Google: the invite, the label, and whether the label is free
//!    on the box. A user who has already approved a Google consent screen and
//!    is then told their address was taken has spent something they cannot get
//!    back.
//! 2. Nothing is spent at that point either. The invite is checked, not
//!    consumed; the tenant is not created. A signup abandoned at Google leaves
//!    no trace but an expired session.
//! 3. `GET /oauth/callback` is where the irreversible things happen, in this
//!    order: exchange, seal, provision, record, consume the invite. The invite
//!    is spent LAST so that a provisioning failure leaves the user able to try
//!    again with the code they were given.
//!
//! PRIVACY, again because this is the file where it would slip: the invite
//! code, its hash, the authorization code, `state`, the PKCE verifier, the
//! session id, the pairing code, and both tokens never reach a log line. The
//! label does; the mailbox address does not (it is the user's identity, and
//! this service's logs are not the place for a list of customers' addresses).

use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::config::OUTBOUND_TIMEOUT;
use crate::cookie::{self, SignupClaim};
use crate::invites;
use crate::labels;
use crate::oauth::{self, GoogleEndpoints};
use crate::pages;
use crate::seal;
use crate::sessions::InsertError;
use crate::state::ControlState;
use crate::store::StoreError;
use crate::warden::WardenError;

/// Ceiling on the signup form body. The largest legitimate one is two short
/// fields.
pub const MAX_BODY: usize = 8 * 1024;

/// Ceiling on either submitted field before it is even normalized. A label is
/// at most 30 characters and a code is 9; this is slack, and it exists so that
/// a megabyte of "label" is not run through Unicode case folding.
const MAX_FIELD: usize = 128;

/// Ceiling on an authorization code, matching what the broker accepts for the
/// same value. Google's run around 250 characters today.
const MAX_CODE: usize = 512;

/// Entropy behind a session id and behind the CSRF `state`. 32 bytes is 43
/// unpadded base64url characters.
const RANDOM_BYTES: usize = 32;

/// What every invite failure says. ONE message for "no such code", "already
/// used", "revoked", and "not even shaped like a code", because anything that
/// tells those apart is an oracle for a 40-bit secret.
const INVITE_REFUSED: &str = "That invite code is not usable. Check it and try again.";

/// What a claimed label says, whichever authority reported it. Both this
/// control plane's record and the warden's answer produce it, so a person
/// cannot tell which of the two knows about an address.
const LABEL_TAKEN: &str = "That address is already taken. Pick another one.";

/// What every failure of the signup session says on the callback. Expired,
/// missing, tampered, and state-mismatched are one answer for the same reason.
const SESSION_REFUSED: &str =
    "That signup could not be verified, or it took too long. Please start again.";

pub async fn healthz() -> &'static str {
    "ok"
}

/// `GET /` — the form.
pub async fn signup_form(State(state): State<ControlState>) -> Response {
    pages::signup_form(&state.config().base_domain, "", "", None)
}

/// `POST /signup` — validate, open a session, and send the user to Google.
pub async fn signup(State(state): State<ControlState>, body: Bytes) -> Response {
    let config = state.config();
    let raw_label = field(&body, "label");
    let raw_invite = field(&body, "invite");

    // The form is re-rendered on every refusal with both fields echoed, so a
    // person fixes one thing rather than retyping everything.
    let reject = |error: &str, label: &str| {
        pages::signup_form(&config.base_domain, label, &raw_invite, Some(error))
    };

    let label = match labels::parse(&raw_label) {
        Ok(l) => l,
        Err(e) => return reject(&e.message(), &raw_label),
    };

    // The invite is CHECKED here and consumed only after provisioning. Both
    // failures below produce the identical message.
    if !invites::is_plausible(&raw_invite) {
        return reject(INVITE_REFUSED, &label);
    }
    let invite_id = match state.store().find_unused_invite(&invites::hash(&raw_invite)) {
        Ok(Some(id)) => id,
        Ok(None) => return reject(INVITE_REFUSED, &label),
        Err(e) => {
            tracing::error!(error = %e, "invite lookup failed");
            return reject("Something went wrong on our side. Please try again.", &label);
        }
    };

    // Two authorities on whether a label is free, and both are asked: this
    // control plane's own record, and the warden, which is the one that knows
    // what actually exists on the box.
    match state.store().label_exists(&label) {
        Ok(true) => return reject(LABEL_TAKEN, ""),
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "label lookup failed");
            return reject("Something went wrong on our side. Please try again.", &label);
        }
    }
    match state.warden().status(&label).await {
        Ok(None) => {}
        Ok(Some(_)) => return reject(LABEL_TAKEN, ""),
        Err(e) => {
            // PRIVACY: the warden error type, never the bearer or the URL.
            tracing::error!(error = %e, "warden availability check failed");
            return reject(
                "Signup is temporarily unavailable. Please try again in a few minutes.",
                &label,
            );
        }
    }

    let (sid, csrf_state) = match (random_token(), random_token()) {
        (Ok(a), Ok(b)) => (a, b),
        _ => {
            tracing::error!("the system random source failed");
            return reject("Something went wrong on our side. Please try again.", &label);
        }
    };

    let consent = match oauth::consent_url(&endpoints(&state), csrf_state) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "building the consent url failed");
            return reject("Something went wrong on our side. Please try again.", &label);
        }
    };

    // The guard is taken and dropped by this ONE statement. Holding it across
    // the `if` below would deadlock the moment the arm asked for the session
    // count, because a std `Mutex` is not reentrant.
    let inserted = state.sessions().insert(
        sid.clone(),
        consent.state,
        consent.pkce_verifier,
        label.clone(),
        invite_id,
        Instant::now(),
    );
    if let Err(InsertError::Full) = inserted {
        tracing::warn!(sessions = state.live_sessions(), "signup session table full");
        return reject(
            "Too many signups are in flight right now. Please try again in a few minutes.",
            &label,
        );
    }

    let claim = SignupClaim {
        sid,
        label: label.clone(),
        invite: invite_id,
        iat: chrono::Utc::now().timestamp(),
    };
    let cookie_value = cookie::sign(&config.cookie_key, &claim);

    // PRIVACY: the label and a count. Never the session id or the state.
    tracing::info!(label = %label, sessions = state.live_sessions(), "signup started");

    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, consent.url),
            (
                header::SET_COOKIE,
                cookie::set_cookie(&cookie_value, !config.is_insecure()),
            ),
        ],
    )
        .into_response()
}

/// `GET /oauth/callback` — Google's redirect target.
pub async fn oauth_callback(
    State(state): State<ControlState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let config = state.config();
    let secure = !config.is_insecure();
    // Every terminal answer from here clears the cookie: a finished or refused
    // signup must not leave a session riding on the browser.
    let done = |resp: Response| with_cleared_cookie(resp, secure);

    let query = query.as_deref();
    let returned_state = param(query, "state").unwrap_or_default();
    let code = param(query, "code");

    // A refusal at Google is an outcome, not a failure. It is reported before
    // the session is touched so that "I changed my mind" does not also read as
    // a broken signup.
    if let Some(error) = param(query, "error") {
        tracing::info!(oauth_error = %sanitize_error(&error), "consent not granted");
        return done(pages::problem(
            StatusCode::OK,
            "Google access was not granted",
            "Nothing was set up. If you changed your mind, you can close this page. If that was a mistake, start again.",
        ));
    }

    let claim = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(cookie::from_header)
        .and_then(|v| cookie::verify(&config.cookie_key, v, chrono::Utc::now().timestamp()));

    // Cookie missing, cookie forged, session expired, session already spent,
    // and `state` mismatched all end here with one message. Nothing about which
    // it was reaches the page or the log.
    let Some(claim) = claim else {
        return done(refused_session());
    };
    // Bound in its own statement so the session lock is released before
    // anything below can want it again.
    let session = state.sessions().take(&claim.sid, Instant::now());
    let Some(session) = session else {
        return done(refused_session());
    };
    if !squelch_httpauth::ct_eq(returned_state.as_bytes(), session.state.as_bytes()) {
        tracing::warn!("callback state mismatch");
        return done(refused_session());
    }
    // The cookie and the server-side session must agree. The session is the
    // authority; this catches a cookie signed by this key for a DIFFERENT
    // session, which is the one forgery a valid MAC cannot rule out on its own.
    if claim.label != session.label || claim.invite != session.invite_id {
        tracing::warn!("callback cookie does not match its session");
        return done(refused_session());
    }
    let Some(code) = code.filter(|c| is_code(c)) else {
        return done(refused_session());
    };

    let label = session.label;
    let invite_id = session.invite_id;

    // ---- from here on, the irreversible half ----

    let grant = match oauth::exchange_code(&endpoints(&state), code, session.pkce_verifier).await {
        Ok(g) => g,
        Err(e) => {
            // The error type only. Its `Display` is written to carry no code,
            // no secret, and no provider body.
            tracing::warn!(error = %e, label = %label, "token exchange failed");
            return done(pages::problem(
                StatusCode::BAD_GATEWAY,
                "Google did not complete the sign in",
                "Nothing was set up. Please start again, and approve the Gmail read permission when Google asks.",
            ));
        }
    };

    // One mailbox, one daemon. Checked before anything is provisioned, and
    // enforced again by a unique index when the tenant row is written.
    match state.store().active_tenant_for_email(&grant.account_email) {
        Ok(Some(existing)) => {
            tracing::info!(label = %existing, "signup refused: mailbox already has a tenant");
            return done(pages::problem(
                StatusCode::CONFLICT,
                "That Google account already has a mailbox",
                "This Google account is already set up with Passband. Open the app and pair it with the mailbox you already have.",
            ));
        }
        Ok(None) => {}
        Err(e) => {
            tracing::error!(error = %e, "tenant lookup failed");
            return done(internal_problem());
        }
    }

    // The ONE moment a plaintext refresh token exists on this machine ends
    // here. From this line on there is only ciphertext.
    let ciphertext = match seal::seal_token(state.recipient(), &grant.token) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "sealing the credential failed");
            return done(internal_problem());
        }
    };
    drop(grant.token);

    let provisioned = match state
        .warden()
        .provision(&label, &grant.account_email, &ciphertext)
        .await
    {
        Ok(p) => p,
        Err(WardenError::LabelTaken) => {
            return done(pages::problem(
                StatusCode::CONFLICT,
                "That address was just taken",
                "Someone claimed that address while you were signing in. Start again and pick another one. Your invite code has not been used.",
            ));
        }
        Err(e) => {
            tracing::error!(error = %e, label = %label, "provisioning failed");
            return done(pages::problem(
                StatusCode::BAD_GATEWAY,
                "Your mailbox could not be set up",
                "Nothing was set up and your invite code has not been used. Please try again in a few minutes.",
            ));
        }
    };

    // The record, and the last enforcement of both unique constraints.
    if let Err(e) = state.store().insert_tenant(&label, &grant.account_email) {
        // The daemon exists on the box but this control plane could not record
        // it, so it will not be visible to `tenants list` and the label will
        // look free here. Loud, with the label, because a human has to go clean
        // it up.
        tracing::error!(
            error = %e,
            label = %label,
            "PROVISIONED BUT NOT RECORDED: the tenant is running on the box and has no control-plane row"
        );
        let detail = match e {
            StoreError::LabelTaken | StoreError::AccountTaken => {
                "That address or account was claimed while you were signing in. Get in touch and we will sort it out."
            }
            _ => "Your mailbox was set up but we could not finish recording it. Get in touch and we will sort it out.",
        };
        return done(pages::problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong at the last step",
            detail,
        ));
    }

    // LAST: spend the invite. A failure here is logged and does not fail the
    // signup, because the tenant exists and telling the user their mailbox did
    // not get made would be a lie.
    if let Err(e) = state.store().consume_invite(invite_id, &label) {
        tracing::error!(error = %e, label = %label, "tenant provisioned but the invite was not consumed");
    }

    tracing::info!(label = %label, port = provisioned.port, "tenant provisioned");

    done(pages::success(
        &config.tenant_url(&label),
        &provisioned.pair_code,
        PAIRING_MINUTES,
    ))
}

/// The pairing window, in minutes, for the success page's copy. Derived from
/// the DAEMON's constant rather than typed here, because the warden mints that
/// code by running `squelchd pair` on the box: if the daemon's TTL ever moves,
/// this sentence moves with it instead of quietly lying to the user.
const PAIRING_MINUTES: i64 =
    squelch_core::store::sqlite::device_tokens::PAIRING_TTL_SECS / 60;

fn refused_session() -> Response {
    pages::problem(
        StatusCode::BAD_REQUEST,
        "That signup could not be verified",
        SESSION_REFUSED,
    )
}

fn internal_problem() -> Response {
    pages::problem(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong on our side",
        "Nothing was set up and your invite code has not been used. Please try again.",
    )
}

/// Attach the cookie-clearing header to a finished response.
fn with_cleared_cookie(mut resp: Response, secure: bool) -> Response {
    if let Ok(value) = header::HeaderValue::from_str(&cookie::clear_cookie(secure)) {
        resp.headers_mut().append(header::SET_COOKIE, value);
    }
    resp
}

/// The endpoints one flow needs, borrowed from config.
fn endpoints(state: &ControlState) -> GoogleEndpoints<'_> {
    let c = state.config();
    GoogleEndpoints {
        client_id: &c.client_id,
        client_secret: &c.client_secret,
        redirect_uri: &c.redirect_uri,
        auth_url: &c.auth_url,
        token_url: &c.token_url,
        profile_url: &c.profile_url,
        timeout: OUTBOUND_TIMEOUT,
    }
}

/// A high-entropy opaque token: session ids and CSRF state.
fn random_token() -> Result<String, std::io::Error> {
    let mut bytes = [0u8; RANDOM_BYTES];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// First value for `name` in a form body, capped. `form_urlencoded` never
/// fails, so a garbled body is missing fields rather than a rejection shape the
/// page would have to render.
fn field(body: &Bytes, name: &str) -> String {
    url::form_urlencoded::parse(body)
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.chars().take(MAX_FIELD).collect())
        .unwrap_or_default()
}

/// First value for `name` in a raw query string.
fn param(query: Option<&str>, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

/// The shape of an authorization code: printable ASCII, no spaces, bounded.
/// A code carrying a control character is not a code, it is an injection into
/// whatever reads the next log line.
fn is_code(code: &str) -> bool {
    (1..=MAX_CODE).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_graphic())
}

/// OAuth error codes are short lowercase identifiers. Anything else is reported
/// generically: this string reaches a log line, and Google is not the only
/// party who can put it in the URL.
fn sanitize_error(error: &str) -> String {
    let ok = (1..=64).contains(&error.len())
        && error
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        error.to_string()
    } else {
        "invalid_request".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_capped_form_fields() {
        let body = Bytes::from("label=Ada&invite=ABCD-EFGH");
        assert_eq!(field(&body, "label"), "Ada");
        assert_eq!(field(&body, "invite"), "ABCD-EFGH");
        assert_eq!(field(&body, "missing"), "");

        let long = Bytes::from(format!("label={}", "a".repeat(MAX_FIELD * 4)));
        assert_eq!(field(&long, "label").len(), MAX_FIELD);
    }

    #[test]
    fn reads_query_parameters() {
        assert_eq!(param(Some("code=abc&state=xyz"), "code").as_deref(), Some("abc"));
        assert_eq!(param(Some("a=1"), "code"), None);
        assert_eq!(param(None, "code"), None);
    }

    #[test]
    fn holds_an_authorization_code_to_a_shape() {
        assert!(is_code("4/0Ab_5-abcDEF"));
        assert!(!is_code(""));
        assert!(!is_code("with space"));
        assert!(!is_code("with\nnewline"));
        assert!(!is_code(&"a".repeat(MAX_CODE + 1)));
    }

    #[test]
    fn sanitizes_a_provider_error_before_it_reaches_a_log() {
        assert_eq!(sanitize_error("access_denied"), "access_denied");
        assert_eq!(sanitize_error("<script>"), "invalid_request");
        assert_eq!(sanitize_error(""), "invalid_request");
        assert_eq!(sanitize_error(&"a".repeat(100)), "invalid_request");
    }

    #[test]
    fn random_tokens_are_high_entropy_and_distinct() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_ne!(a, b);
        // 32 bytes is 43 unpadded base64url characters.
        assert_eq!(a.len(), 43);
    }
}
