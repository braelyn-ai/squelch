//! The routes: the signup form, the form post, the public waitlist post, the
//! console login hop, Google's callback, and liveness. The operator's half of
//! the waitlist (the dashboard and its two buttons) lives next door in
//! [`crate::admin`].
//!
//! THE CONSOLE HOP is the second thing that walks through Google here, and it is
//! deliberately the smaller half. Google forbids wildcard redirect URIs, so a
//! tenant console at `https://<label>.<base>` cannot run OAuth itself; it links
//! to `GET /console/auth?tenant=<label>` here, this service proves who is signed
//! in, and the "ticket" handed back is an ordinary PAIRING CODE minted through
//! the warden. The daemon claims it into a device token like any other device.
//! No new crypto, no new trust relationship, and revocation, audit, one-shot and
//! TTL are all the ones already shipped.
//!
//! THE REDIRECT IS CONSTRUCTED, NEVER ECHOED. There is no return-URL parameter
//! on `/console/auth`, so there is no open redirect to find: the destination is
//! this deployment's own base domain plus a label this crate validated. See
//! [`crate::pages::console_callback_url`].
//!
//! THE CONSOLE HOP ANSWERS EVERY WELL-FORMED LABEL THE SAME WAY: a 302 to
//! Google. It does NOT look the tenant up first, and that is a deliberate
//! reversal of this file's other rule (check everything before Google). Looking
//! it up meant a real address got a redirect and an unprovisioned one got a
//! page, which is a directory of which hosted addresses exist, answerable by
//! anybody, one label at a time. The only thing that shapes the answer now is
//! whether the label could BE a label: a malformed one is a `400` and tells a
//! stranger nothing they did not type themselves.
//!
//! WHAT PAYS FOR THAT is the callback: the mailbox check there refuses "the
//! wrong Google account" and "no such tenant" with one page and one status, so
//! walking the label space costs a full consent per guess and still learns
//! nothing. A person who mistypes their own address spends one Google screen,
//! which is the price of not running an address oracle.
//!
//! REFUSALS ON THE CONSOLE PATH ARE UNIFORM. A tenant that does not exist, a
//! tenant that is not active, and a Google account that is not the one that owns
//! the mailbox all produce one page, because anything that told them apart would
//! answer "which addresses are real" and "who owns this one" to anybody who
//! asked. They are also CONSOLE pages: a person who was signing in to a mailbox
//! they already own is never handed the signup form (see
//! [`crate::pages::console_problem`]).
//!
//! THE SHAPE OF THE FLOW, and why it is split where it is:
//!
//! 1. `POST /signup` validates everything that can be validated BEFORE a human
//!    is sent to Google: the invite, the label, and whether the label is free
//!    in the cluster. A user who has already approved a Google consent screen
//!    and is then told their address was taken has spent something they cannot
//!    get back.
//! 2. Nothing is spent at that point either. The invite is RESERVED, not
//!    consumed; the tenant is not created. A signup abandoned at Google leaves
//!    no trace but an expired session and a reservation that lapses with it.
//! 3. `GET /oauth/callback` is where the irreversible things happen, in this
//!    order: exchange, CREATE the tenant (learning its recipient), seal to that
//!    recipient, INSTALL the credential, record, consume the invite. The invite
//!    is spent LAST so that a failure anywhere above leaves the user able to try
//!    again with the code they were given, and every one of those failures hands
//!    the reservation back so the retry does not have to wait it out.
//!
//! THE RESERVATION IS WHAT MAKES "ONE CODE, ONE TENANT" TRUE. Checking the code
//! at step 1 and spending it at step 3 leaves minutes in between, and one code
//! posted from N tabs used to pass N checks and provision N tenants, with only
//! the last consume losing. The hold is taken in the same statement that checks
//! availability, so the second tab is refused at the door; it names the session
//! holding it, so only that session can spend or release it; and it expires with
//! the session, so an abandoned signup costs the code nothing.
//!
//! PROVISIONING IS TWO CALLS under wire v2, and the gap between them is a state
//! a user can land in: the warden has minted this tenant's age identity and is
//! holding it `pending`, and the credential never arrived. That is retriable
//! rather than broken, so the page says so, the invite stays unspent, and a
//! retry with the SAME address and the SAME Google account walks back through
//! call 1 to the SAME recipient. `POST /signup` therefore lets a pending label
//! through its availability check; the warden is the thing that decides whether
//! the mailbox coming back is the one that reserved it.
//!
//! PRIVACY, again because this is the file where it would slip: the invite
//! code, its hash, the authorization code, `state`, the PKCE verifier, the
//! session id, the pairing code, and both tokens never reach a log line. The
//! label does; the mailbox address does not (it is the user's identity, and
//! this service's logs are not the place for a list of customers' addresses).
//! A WAITLIST ADDRESS DOES NOT EITHER, and it is the stricter case: whoever
//! submitted it is not a customer and has consented to nothing, so the route
//! that takes it logs whether a row was created and nothing more.

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
use crate::cookie::{self, SessionClaim};
use crate::invites;
use crate::labels;
use crate::oauth::{self, Flow, GoogleEndpoints};
use crate::pages;
use crate::seal;
use crate::sessions::{self, InsertError, SessionKind};
use crate::state::ControlState;
use crate::store::StoreError;
use crate::warden::WardenError;

/// Ceiling on the signup form body. The largest legitimate one is two short
/// fields.
pub const MAX_BODY: usize = 8 * 1024;

/// Ceiling on either submitted field before it is even normalized. A label is
/// at most 30 characters and a code is 19; this is slack, and it exists so that
/// a megabyte of "label" is not run through Unicode case folding.
const MAX_FIELD: usize = 128;

/// Ceiling on an authorization code, matching what the broker accepts for the
/// same value. Google's run around 250 characters today.
const MAX_CODE: usize = 512;

/// The longest address anybody may submit, RFC 5321's limit. It is read one
/// character OVER this so that too long arrives too long and is REFUSED: a
/// truncated address is a well-formed address belonging to somebody else, and
/// silently mailing an invite there is the one failure mode worth spending a
/// constant on.
const MAX_EMAIL: usize = 254;

/// Entropy behind a session id and behind the CSRF `state`. 32 bytes is 43
/// unpadded base64url characters.
const RANDOM_BYTES: usize = 32;

/// What every invite failure says. ONE message for "no such code", "already
/// used", "expired", "held by another signup", "revoked", and "not even shaped
/// like a code", because anything that tells those apart is an oracle for the
/// code space.
const INVITE_REFUSED: &str = "That invite code is not usable. Check it and try again.";

/// What a claimed label says, whichever authority reported it. Both this
/// control plane's record and the warden's answer produce it, so a person
/// cannot tell which of the two knows about an address.
const LABEL_TAKEN: &str = "That address is already taken. Pick another one.";

/// What every failure of a pending session says on the callback. Expired,
/// missing, tampered, and state-mismatched are one answer for the same reason.
const SESSION_REFUSED: &str =
    "That signup could not be verified, or it took too long. Please start again.";

/// THE ONE ANSWER the console login gives to every identity-shaped refusal: a
/// label that is not a label, a tenant nobody has provisioned, a tenant that is
/// not running, and a Google account that does not own the mailbox.
///
/// One sentence for all four because they are the same question asked four
/// ways. Distinguishing them would hand a stranger a directory of which
/// addresses exist and, worse, a check for whether a given Google account owns
/// one. The copy is written to be actionable ANYWAY: whichever of the four it
/// was, the fix is the same two things.
const CONSOLE_REFUSED_HEADING: &str = "That sign in could not be completed";
const CONSOLE_REFUSED: &str = "Check that you opened this from your own mailbox address, and sign \
     in with the Google account that mailbox belongs to.";

/// [`SESSION_REFUSED`] in the console flow's own words. Same four causes
/// (expired, missing, tampered, state-mismatched) and the same single answer;
/// what changes is that it says SIGN IN rather than sign up, and the page it
/// renders on links back to the console rather than to the signup form.
const CONSOLE_SESSION_REFUSED: &str =
    "That sign in could not be verified, or it took too long. Open your console and sign in again.";

/// The three answers `POST /waitlist` gives. JSON rather than a page: the only
/// client is the site's own form, which shows its own copy in its own voice, so
/// what crosses the wire is a machine reason and never a sentence.
///
/// `{"ok":true}` is the answer to a NEW address and to one already on the list.
/// See [`waitlist`].
const WAITLIST_JOINED: &str = r#"{"ok":true}"#;
const INVALID_EMAIL: &str = r#"{"ok":false,"error":"invalid_email"}"#;
const WAITLIST_UNAVAILABLE: &str = r#"{"ok":false,"error":"unavailable"}"#;

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

    // The invite is GATED here and held below, once the address is known to be
    // free. Nothing is spent either way, and every failure produces the
    // identical message.
    //
    // Gate first, hold later, so a signup refused for its ADDRESS does not leave
    // a hold on a perfectly good code; and gate at all, so a nonsense code costs
    // a point lookup rather than a round trip to the warden.
    if !invites::is_plausible(&raw_invite) {
        return reject(INVITE_REFUSED, &label);
    }
    let code_hash = invites::hash(&raw_invite);
    match state
        .store()
        .find_available_invite(&code_hash, chrono::Utc::now())
    {
        Ok(Some(_)) => {}
        Ok(None) => return reject(INVITE_REFUSED, &label),
        Err(e) => {
            tracing::error!(error = %e, "invite lookup failed");
            return reject("Something went wrong on our side. Please try again.", &label);
        }
    }

    // Two authorities on whether a label is free, and both are asked: this
    // control plane's own record, and the warden, which is the one that knows
    // what actually exists in the cluster.
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
        // A PENDING tenant is a half-finished signup, and this is how its owner
        // gets to finish it: the label is let through here and the warden
        // decides at call 1 whether the Google account coming back is the one
        // that reserved it. A stranger who guesses a pending label spends a
        // consent and gets "already taken"; they cannot take it, and their
        // invite is not spent either.
        Ok(Some(s)) if s.is_pending() => {}
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

    let consent = match oauth::consent_url(&endpoints(&state), csrf_state, Flow::Signup) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "building the consent url failed");
            return reject("Something went wrong on our side. Please try again.", &label);
        }
    };

    // THE HOLD. Atomic with the availability check it repeats, and the last
    // thing that can refuse this signup for its invite code: from here the user
    // is going to Google.
    let holder = sessions::fingerprint(&sid);
    let now = chrono::Utc::now();
    let reserved_until = now + reservation_window();
    let invite_id = match state
        .store()
        .reserve_invite(&code_hash, &holder, now, reserved_until)
    {
        Ok(Some(id)) => id,
        // Lost the race with another tab, or the code went away between the
        // gate above and here. Same message as every other invite failure.
        Ok(None) => return reject(INVITE_REFUSED, &label),
        Err(e) => {
            tracing::error!(error = %e, "reserving the invite failed");
            return reject("Something went wrong on our side. Please try again.", &label);
        }
    };

    // The guard is taken and dropped by this ONE statement. Holding it across
    // the `if` below would deadlock the moment the arm asked for the session
    // count, because a std `Mutex` is not reentrant.
    let inserted = state.sessions().insert(
        sid.clone(),
        SessionKind::Signup { invite_id },
        consent.state,
        consent.pkce_verifier,
        label.clone(),
        Instant::now(),
    );
    if let Err(InsertError::Full) = inserted {
        tracing::warn!(sessions = state.live_sessions(), "signup session table full");
        // The session that would have held this code does not exist, so the
        // hold is handed back rather than left to lapse on its own.
        release_invite(&state, invite_id, &holder);
        return reject(
            "Too many signups are in flight right now. Please try again in a few minutes.",
            &label,
        );
    }

    let claim = SessionClaim {
        sid,
        label: label.clone(),
        invite: Some(invite_id),
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

/// `POST /waitlist` — an address asking to be told when there is room.
///
/// ONE ANSWER FOR A NEW ADDRESS AND FOR ONE ALREADY ON THE LIST. A route that
/// said "you are already on it" is a membership oracle: it answers, to anybody
/// who asks, whether a given person wants hosted Passband. So a fresh row and a
/// swallowed duplicate are the same `200 {"ok":true}`, and the only thing that
/// answers differently is a string that is not an address at all, which tells a
/// stranger nothing they did not type themselves.
///
/// CORS ON EVERY ANSWER, including the refusals. The form is served from the
/// marketing site and posted to this one, so the browser only shows the answer
/// if the header is on it; a 400 without one is a form whose error state is
/// "network failure". `Cache-Control: no-store` because nothing about a
/// submission is cacheable, and `Vary: Origin` because the header depends on
/// who asked. The answers this handler never writes (a 429, a 413) get the
/// same headers from [`waitlist_cors`].
pub async fn waitlist(State(state): State<ControlState>, body: Bytes) -> Response {
    // The route is mounted only when the feature is configured, so this is
    // belt and braces: an answer with no allowed origin would be a public
    // write with no browser telling anybody where it may be posted from.
    let Some((_, waitlist)) = state.waitlist() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let origin = &waitlist.allowed_origin;

    let email = field_capped(&body, "email", MAX_EMAIL + 1);
    if !is_email(&email) {
        return waitlist_answer(origin, StatusCode::BAD_REQUEST, INVALID_EMAIL);
    }

    match state.store().add_to_waitlist(&email) {
        // PRIVACY: whether this submission created a row, and nothing else.
        // Never the address, on either branch.
        Ok(created) => {
            tracing::info!(created, "waitlist submission");
            waitlist_answer(origin, StatusCode::OK, WAITLIST_JOINED)
        }
        Err(e) => {
            tracing::error!(error = %e, "recording a waitlist submission failed");
            waitlist_answer(
                origin,
                StatusCode::INTERNAL_SERVER_ERROR,
                WAITLIST_UNAVAILABLE,
            )
        }
    }
}

/// Middleware: put the CORS headers on EVERYTHING the waitlist route answers,
/// including the answers the handler never gets to write.
///
/// The handler's own three headers cover the answers it produces. They do not
/// cover the 429 from the rate limiter or the 413 from the body limit, and
/// those are exactly the refusals a browser meets: without the header the fetch
/// rejects as a network error and the form cannot tell "slow down" from "we are
/// down". Outermost layer on the sub-router, so it wraps both.
///
/// `insert`, not `append`: the handler sets the same headers on its own path
/// and two copies of `Access-Control-Allow-Origin` are treated as none.
pub async fn waitlist_cors(
    State(state): State<ControlState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut resp = next.run(req).await;
    if let Some((_, waitlist)) = state.waitlist()
        && let Ok(origin) = header::HeaderValue::from_str(&waitlist.allowed_origin)
    {
        let headers = resp.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(header::VARY, header::HeaderValue::from_static("origin"));
        headers.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
    }
    resp
}

/// `OPTIONS /waitlist` — the preflight.
///
/// The site posts `application/x-www-form-urlencoded`, which is a CORS SIMPLE
/// request and never preflighted. This exists so that stays a fact about today's
/// form rather than a load-bearing one: a content type or a header added on the
/// site later turns the post into a preflighted request, and without this route
/// that change would be a 405 nobody could see from the Rust side.
pub async fn waitlist_preflight(State(state): State<ControlState>) -> Response {
    let Some((_, waitlist)) = state.waitlist() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        StatusCode::NO_CONTENT,
        [
            (
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                waitlist.allowed_origin.clone(),
            ),
            (header::ACCESS_CONTROL_ALLOW_METHODS, "POST".to_string()),
            (
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                "content-type".to_string(),
            ),
            (header::VARY, "origin".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
    )
        .into_response()
}

/// `GET /console/auth?tenant=<label>` — the console login hop.
///
/// THE ONE THING CHECKED HERE IS SHAPE. This route deliberately does NOT ask
/// whether the tenant exists, because the answer would be visible from outside:
/// a real address would get a 302 and an unprovisioned one a page, and that
/// difference is a directory of every hosted mailbox, one guess at a time. A
/// well-formed label goes to Google whether or not anybody owns it, and the
/// callback refuses "not your mailbox" and "no such mailbox" with one page.
///
/// The cost is a consent screen spent by somebody who mistyped their own
/// address. The alternative was answering "does ada exist" to anybody who asked,
/// so this is the trade taken.
///
/// NO RETURN URL, and no parameter that could become one. The only thing this
/// route accepts is a label, and the only place it can send anybody is that
/// label's own console under this deployment's own base domain.
pub async fn console_auth(
    State(state): State<ControlState>,
    RawQuery(query): RawQuery,
) -> Response {
    let config = state.config();
    let raw_label = param(query.as_deref(), "tenant").unwrap_or_default();
    // Bounded before it is case-folded, exactly as the form field is: a
    // megabyte of "tenant" is not run through Unicode.
    let raw_label: String = raw_label.chars().take(MAX_FIELD).collect();

    // SHAPE ONLY, and the validator's own messages are dropped: they are useful
    // on the signup form, where the person is choosing the label, and here they
    // would say which rule a guess broke. A label nobody has provisioned is NOT
    // refused here at all; it goes to Google like any other and is refused on
    // the way back, so this route cannot be asked which addresses exist.
    let Ok(label) = labels::parse(&raw_label) else {
        return console_refused();
    };

    let (sid, csrf_state) = match (random_token(), random_token()) {
        (Ok(a), Ok(b)) => (a, b),
        _ => {
            tracing::error!("the system random source failed");
            return console_unavailable();
        }
    };

    let consent = match oauth::consent_url(&endpoints(&state), csrf_state, Flow::Console) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "building the console consent url failed");
            return console_unavailable();
        }
    };

    let inserted = state.sessions().insert(
        sid.clone(),
        SessionKind::Console,
        consent.state,
        consent.pkce_verifier,
        label.clone(),
        Instant::now(),
    );
    if let Err(InsertError::Full) = inserted {
        tracing::warn!(sessions = state.live_sessions(), "session table full");
        return console_unavailable();
    }

    let claim = SessionClaim {
        sid,
        label: label.clone(),
        // A console login spends nothing, and the callback holds this against
        // the session's own kind: a cookie that claimed an invite here would be
        // refused as a mismatch.
        invite: None,
        iat: chrono::Utc::now().timestamp(),
    };
    let cookie_value = cookie::sign(&config.cookie_key, &claim);

    // PRIVACY: the label and a count. Never the session id, the state, or which
    // mailbox owns the label.
    tracing::info!(label = %label, sessions = state.live_sessions(), "console login started");

    (
        StatusCode::FOUND,
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
        // No cookie, so no way to know which flow this was. The signup wording
        // is the fallback because a callback with no cookie at all is very
        // nearly always a stale bookmark of the flow that has one.
        return done(refused_session());
    };
    // WHICH FLOW THIS IS, for the refusals below. Taken from the cookie only
    // while the cookie is all there is; once the session is in hand it is the
    // authority, here as everywhere else.
    let claimed_console = claim.invite.is_none();
    // Bound in its own statement so the session lock is released before
    // anything below can want it again.
    let session = state.sessions().take(&claim.sid, Instant::now());
    let Some(session) = session else {
        // Nothing to hand back: an expired session's hold expired with it, and a
        // replay is finding a session that already spent or released its code.
        let console_label = claimed_console.then(|| claim.label.clone());
        return done(refused_session_for(&state, console_label.as_deref()));
    };
    let console_label =
        matches!(session.kind, SessionKind::Console).then(|| session.label.clone());
    let console_label = console_label.as_deref();

    // From here the session is gone but a signup's invite is still held, so
    // every exit hands the code back. The holder is recomputed from the session
    // id the cookie carried, which `take` has just proved names a live session.
    // A console login holds nothing, so `release` is a no-op on that branch and
    // the exits below do not have to know which flow they are on.
    let holder = sessions::fingerprint(&claim.sid);
    let held_invite = session.kind.invite_id();
    let release = || {
        if let Some(id) = held_invite {
            release_invite(&state, id, &holder);
        }
    };

    if !squelch_httpauth::ct_eq(returned_state.as_bytes(), session.state.as_bytes()) {
        tracing::warn!("callback state mismatch");
        release();
        return done(refused_session_for(&state, console_label));
    }
    // The cookie and the server-side session must agree, on the label AND on
    // which flow this is. The session is the authority; this catches a cookie
    // signed by this key for a DIFFERENT session, which is the one forgery a
    // valid MAC cannot rule out on its own.
    if claim.label != session.label || claim.invite != held_invite {
        tracing::warn!("callback cookie does not match its session");
        release();
        return done(refused_session_for(&state, console_label));
    }
    let Some(code) = code.filter(|c| is_code(c)) else {
        release();
        return done(refused_session_for(&state, console_label));
    };

    let label = session.label;

    // THE FORK. A console login shares everything above (state, cookie, one-shot
    // session, code shape) and nothing below: it provisions nothing, seals
    // nothing, and spends no invite. Destructured rather than tested, so the
    // signup half below holds an invite id the type system produced instead of
    // one it was told to assume.
    let SessionKind::Signup { invite_id } = session.kind else {
        return done(console_login(&state, &label, code, session.pkce_verifier).await);
    };

    // ---- from here on, the irreversible half ----

    let grant = match oauth::exchange_code(&endpoints(&state), code, session.pkce_verifier).await {
        Ok(g) => g,
        // A PARTIAL CONSENT is its own answer, and the only exchange failure a
        // user can act on: Google's screen lets the boxes be unchecked one by
        // one, and the person who unchecked one is the person reading this page.
        // ONE page for whichever box it was: naming the missing scope would say
        // nothing they cannot see on Google's screen and would put a third
        // wording of the same instruction in front of them.
        //
        // Nothing has been provisioned at this point, so the retry is clean.
        Err(oauth::OAuthError::Scope) => {
            tracing::info!(label = %label, "consent granted only part of the scope set");
            release();
            return done(partial_consent_problem());
        }
        Err(e) => {
            // The error type only. Its `Display` is written to carry no code,
            // no secret, and no provider body.
            tracing::warn!(error = %e, label = %label, "token exchange failed");
            release();
            return done(pages::problem(
                StatusCode::BAD_GATEWAY,
                "Google did not complete the sign in",
                "Nothing was set up. Please start again, and approve all three Gmail permissions when Google asks.",
            ));
        }
    };

    // One mailbox, one daemon. Checked before anything is provisioned, and
    // enforced again by a unique index when the tenant row is written.
    match state.store().active_tenant_for_email(&grant.account_email) {
        Ok(Some(existing)) => {
            tracing::info!(label = %existing, "signup refused: mailbox already has a tenant");
            release();
            return done(pages::problem(
                StatusCode::CONFLICT,
                "That Google account already has a mailbox",
                "This Google account is already set up with Passband. Open the app and pair it with the mailbox you already have.",
            ));
        }
        Ok(None) => {}
        Err(e) => {
            tracing::error!(error = %e, "tenant lookup failed");
            release();
            return done(internal_problem());
        }
    }

    // CALL 1. The warden mints this tenant's age identity, keeps it, and hands
    // back only the public half. Nothing is encrypted yet and no credential
    // exists on the far side: what this creates is a reservation, tied to this
    // mailbox, that only this mailbox can complete.
    let created = match state.warden().create_tenant(&label, &grant.account_email).await {
        Ok(c) => c,
        Err(WardenError::LabelTaken) => {
            release();
            return done(pages::problem(
                StatusCode::CONFLICT,
                "That address was just taken",
                "Someone claimed that address while you were signing in. Start again and pick another one. Your invite code has not been used.",
            ));
        }
        Err(e) => {
            // PRIVACY: the error type and the label. Never the recipient, the
            // bearer, or anything the warden said verbatim.
            tracing::error!(error = %e, label = %label, "creating the tenant failed");
            release();
            return done(pages::problem(
                StatusCode::BAD_GATEWAY,
                "Your mailbox could not be set up",
                "Nothing was set up and your invite code has not been used. Please try again in a few minutes.",
            ));
        }
    };

    // The ONE moment a plaintext refresh token exists on this machine ends
    // here. From this line on there is only ciphertext, and it is readable by
    // exactly one identity: the one the warden just minted for THIS tenant.
    let ciphertext = match seal::seal_credentials(
        &created.recipient,
        &grant.account_email,
        &grant.token,
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, label = %label, "sealing the credential failed");
            release();
            // NOT `internal_problem`: call 1 has reserved the address, so
            // "nothing was set up" would be false. It is not the retriable page
            // either, because a warden that answered with an unusable key will
            // answer with the same one on the retry (the recipient for a pending
            // label is stable by contract), and promising "we will finish the
            // job" would send someone round a loop.
            return done(pages::problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong on our side",
                "Your mailbox was not set up and your invite code has not been used. Please start again, and if it happens twice, get in touch.",
            ));
        }
    };
    drop(grant.token);

    // THE LLM KEYS — triage and assistant — when this deployment fronts a
    // Bifrost gateway: minted here, between call 1 and call 2, so the keys are
    // installed before the workload is applied and the pod is born with them
    // instead of being rolled onto them.
    //
    // FAIL-SOFT, deliberately, and unlike everything around it: triage is not
    // mail custody. A Bifrost outage must cost a tenant its LLM keys — which
    // `squelch-control llm mint` backfills — and never the signup itself, so
    // every failure in this block is one loud line and a shrug. The two mints
    // fail independently: a gateway that refuses one key still gets the other
    // minted and installed, and the single warden PUT carries whichever
    // succeeded. The key VALUES exist only inside this block; the ids are
    // recorded once the tenant row exists below, and until then they ride in
    // `vk_id` / `assistant_vk_id`.
    let mut vk_id: Option<String> = None;
    let mut assistant_vk_id: Option<String> = None;
    if let Some((bifrost, llm)) = state.bifrost() {
        // The ids are kept EVEN IF the install below fails, so the record can
        // name the keys a revoke or a re-mint must find.
        let triage_value = match bifrost.mint_virtual_key(&label, llm.budget_usd).await {
            Ok(vk) => {
                vk_id = Some(vk.id);
                Some(vk.value)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    label = %label,
                    "LLM KEY NOT MINTED: this tenant will run without a triage key until `squelch-control llm mint` backfills it"
                );
                None
            }
        };
        let assistant_value = match bifrost
            .mint_assistant_key(&label, &llm.assistant_models, llm.assistant_budget_usd)
            .await
        {
            Ok(vk) => {
                assistant_vk_id = Some(vk.id);
                Some(vk.value)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    label = %label,
                    "ASSISTANT KEY NOT MINTED: this tenant will run without an assistant key until `squelch-control llm mint` backfills it"
                );
                None
            }
        };
        if (triage_value.is_some() || assistant_value.is_some())
            && let Err(e) = state
                .warden()
                .put_llm_key(&label, triage_value.as_deref(), assistant_value.as_deref())
                .await
        {
            tracing::error!(
                error = %e,
                label = %label,
                vk_id = vk_id.as_deref().unwrap_or_default(),
                assistant_vk_id = assistant_vk_id.as_deref().unwrap_or_default(),
                "LLM KEYS NOT INSTALLED: minted but the warden did not take them; run `squelch-control llm mint` to replace them (which prints the old ids to revoke)"
            );
        }
    }

    // CALL 2. The credential is installed and the workload applied. A failure
    // here leaves the pending tenant standing, which is the retriable state the
    // page below describes: no credential was written, no invite was spent, and
    // the same address is still reserved for this mailbox.
    let pairing = match state.warden().put_credentials(&label, &ciphertext).await {
        Ok(p) => p,
        Err(WardenError::AlreadyProvisioned) => {
            release();
            log_orphaned_vks(&label, vk_id.as_deref(), assistant_vk_id.as_deref());
            return done(pages::problem(
                StatusCode::CONFLICT,
                "That mailbox is already set up",
                "That address finished setting up a moment ago, so nothing was changed here and your invite code has not been used. Open Passband, point it at your mailbox, and ask for a new pairing code.",
            ));
        }
        Err(e) => {
            tracing::warn!(error = %e, label = %label, "tenant created but the credential was not installed; the signup is retriable");
            // The page tells this user to start again with the SAME code, so the
            // hold has to go back now rather than in ten minutes.
            release();
            log_orphaned_vks(&label, vk_id.as_deref(), assistant_vk_id.as_deref());
            return done(incomplete_problem());
        }
    };

    // The record, and the last enforcement of both unique constraints. NO
    // RELEASE on this path: the mailbox is running in the cluster, so the code
    // must not go back on the shelf for somebody to spend on a second one. It
    // stays held, unspent, until an operator sorts the row out.
    if let Err(e) = state.store().insert_tenant(&label, &grant.account_email) {
        // The daemon is running in the cluster but this control plane could not
        // record it, so it will not be visible to `tenants list` and the label
        // will look free here. Loud, with the label, because a human has to go
        // clean it up.
        tracing::error!(
            error = %e,
            label = %label,
            "PROVISIONED BUT NOT RECORDED: the tenant is running in the cluster and has no control-plane row"
        );
        log_orphaned_vks(&label, vk_id.as_deref(), assistant_vk_id.as_deref());
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

    // The vk ids join the row they were minted for, now that the row exists.
    // Fail-soft like the rest of the LLM block: a record that did not land
    // costs `llm revoke` its pointer, not the user their mailbox.
    if let Some(id) = &vk_id {
        match state.store().set_tenant_vk(&label, id) {
            Ok(true) => {}
            Ok(false) => tracing::error!(label = %label, vk_id = %id, "VK NOT RECORDED: the tenant row vanished under it"),
            Err(e) => tracing::error!(error = %e, label = %label, vk_id = %id, "VK NOT RECORDED: revoke or re-mint by this id by hand"),
        }
    }
    if let Some(id) = &assistant_vk_id {
        match state.store().set_tenant_assistant_vk(&label, id) {
            Ok(true) => {}
            Ok(false) => tracing::error!(label = %label, assistant_vk_id = %id, "ASSISTANT VK NOT RECORDED: the tenant row vanished under it"),
            Err(e) => tracing::error!(error = %e, label = %label, assistant_vk_id = %id, "ASSISTANT VK NOT RECORDED: revoke or re-mint by this id by hand"),
        }
    }

    // LAST: spend the invite, which this session has held since the form was
    // posted. That hold is what makes this consume unfailable: the code cannot
    // have been spent by anyone else, and only a hold that lapsed AND was taken
    // by another session could refuse it. A failure is therefore a broken
    // invariant, logged at error and never shown: the tenant exists, and telling
    // the user their mailbox did not get made would be a lie.
    if let Err(e) = state.store().consume_invite(invite_id, &label, &holder) {
        tracing::error!(error = %e, label = %label, "tenant provisioned but the invite was not consumed");
    }

    tracing::info!(label = %label, "tenant provisioned");

    done(pages::success(
        &config.tenant_url(&label),
        &pairing.pair_code,
        PAIRING_MINUTES,
    ))
}

/// The console login's half of the callback: prove who is signed in, hold it
/// against who owns the mailbox, and hand the browser a pairing code for that
/// tenant's own console.
///
/// THE ORDER MATTERS. The mailbox is DISCOVERED from Google (never taken from
/// anything the browser sent) and then compared, constant time, against what
/// this control plane's store says owns the label. Only after that does anything
/// reach the warden, so a stranger who guesses a real label cannot make a
/// pairing code exist, let alone see one.
///
/// The tenant is looked up again HERE rather than carried in the session: a
/// tenant torn down while somebody was at Google must not be signed in to on the
/// strength of a ten-minute-old row.
async fn console_login(
    state: &ControlState,
    label: &str,
    code: String,
    pkce_verifier: String,
) -> Response {
    // Identity only, and the function that does it hands back a mailbox rather
    // than a token: there is no credential on this path for anything downstream
    // to hold.
    let account_email = match oauth::verify_identity(&endpoints(state), code, pkce_verifier).await {
        Ok(email) => email,
        Err(e) => {
            // PRIVACY: the error type, which is written to carry no code, no
            // token, and no provider body.
            tracing::info!(error = %e, label = %label, "console login did not complete at Google");
            return console_refused();
        }
    };

    let owner = match state.store().active_tenant_email(label) {
        Ok(Some(email)) => email,
        // Provisioned and then torn down while the user was at Google, or a
        // label that never existed and only reached here because nothing before
        // this point could tell the difference either.
        Ok(None) => return console_refused(),
        Err(e) => {
            tracing::error!(error = %e, "console tenant lookup failed");
            return console_unavailable();
        }
    };

    // BOTH SIDES normalized the way the store normalizes on insert, so a
    // capitalized Google answer is the same mailbox rather than a stranger. The
    // stored side is folded too even though every row is written folded: this is
    // the comparison that decides whether somebody gets into a mailbox, and it
    // must not turn on a column's history. `ct_eq` for the compare itself.
    let presented = account_email.trim().to_lowercase();
    let owner = owner.trim().to_lowercase();
    if !squelch_httpauth::ct_eq(presented.as_bytes(), owner.as_bytes()) {
        // PRIVACY: the label, never either address. That somebody signed in with
        // the wrong Google account is not a reason to write a list of mailboxes
        // into a log.
        tracing::info!(label = %label, "console login refused: not the mailbox owner");
        return console_refused();
    }

    // THE TICKET. An ordinary pairing code for an ordinary device, one-shot and
    // ten minutes, which the tenant's own daemon will claim into a device token.
    let pairing = match state.warden().pair(label).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, label = %label, "minting the console pairing code failed");
            return console_unavailable();
        }
    };

    // PRIVACY: never the code, which is live for ten minutes and is the whole
    // credential.
    tracing::info!(label = %label, "console login complete");

    // CONSTRUCTED, not echoed: this deployment's base domain, the validated
    // label, and the validated code, percent-encoded.
    let destination = pages::console_callback_url(
        &state.config().tenant_url(label),
        &pairing.pair_code,
    );
    let Ok(location) = header::HeaderValue::from_str(&destination) else {
        // Unreachable with a validated label and an encoded code; a refusal
        // rather than a panic, because the alternative is a 500 on the one route
        // whose failure locks somebody out of their own console.
        tracing::error!(label = %label, "the console redirect would not fit in a header");
        return console_unavailable();
    };

    (
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            // The URL in that header carries a live pairing code. Nothing may
            // cache it, and it must not ride out as a referer.
            (
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store, no-cache"),
            ),
            (
                header::REFERRER_POLICY,
                header::HeaderValue::from_static("no-referrer"),
            ),
        ],
    )
        .into_response()
}

/// The one page every identity-shaped console refusal gets. See
/// [`CONSOLE_REFUSED`].
fn console_refused() -> Response {
    pages::console_problem(
        StatusCode::BAD_REQUEST,
        CONSOLE_REFUSED_HEADING,
        CONSOLE_REFUSED,
    )
}

/// What a console login gets when THIS service could not do its job: a store
/// that would not answer, a warden that would not mint. Distinct from
/// [`console_refused`] and not an oracle, because none of these depend on the
/// label or the mailbox: they say "try again", which is true, where the refusal
/// says "check who you are", which would not be.
fn console_unavailable() -> Response {
    pages::console_problem(
        StatusCode::BAD_GATEWAY,
        "Sign in is unavailable right now",
        "Nothing changed. Please try again in a few minutes.",
    )
}

/// The pairing window, in minutes, for the success page's copy. Derived from
/// the DAEMON's constant rather than typed here, because the warden mints that
/// code by running `squelchd pair` on the box: if the daemon's TTL ever moves,
/// this sentence moves with it instead of quietly lying to the user.
const PAIRING_MINUTES: i64 =
    squelch_core::store::sqlite::device_tokens::PAIRING_TTL_SECS / 60;

/// How long a signup holds its invite code: the session's own lifetime.
///
/// The two are one clock on purpose. A hold that died first would let a second
/// tab in while the first is still at Google; a hold that outlived the session
/// would strand the code for a user whose signup is already dead.
fn reservation_window() -> chrono::Duration {
    chrono::Duration::seconds(sessions::SESSION_TTL.as_secs() as i64)
}

/// Hand a held invite back, so whoever was refused can start again immediately
/// rather than waiting out their own reservation.
///
/// Best effort, always on a path that is already reporting something else: a
/// hold that will not release lapses on its own within the session window, and
/// the cost of that is one person waiting, not a lost code.
fn release_invite(state: &ControlState, invite_id: i64, holder: &str) {
    match state.store().release_invite(invite_id, holder) {
        Ok(true) => {}
        // The hold lapsed and somebody else took the code, or an operator
        // revoked it mid-signup. Nothing to undo, but worth a line.
        Ok(false) => tracing::warn!("the invite hold was already gone"),
        Err(e) => tracing::error!(error = %e, "releasing the invite hold failed"),
    }
}

/// Shout about virtual keys minted for a signup that then failed before its
/// tenant row existed: nothing in the store points at them, so nothing will
/// ever revoke them unless a human sees these lines. The ids only — the
/// values are either installed in the cluster or already dropped. One line
/// per key, so grepping for either id finds its own verdict.
fn log_orphaned_vks(label: &str, vk_id: Option<&str>, assistant_vk_id: Option<&str>) {
    if let Some(id) = vk_id {
        tracing::error!(
            label = %label,
            vk_id = %id,
            "VK ORPHANED: minted for a signup that did not finish; revoke it in Bifrost, or a retry plus `llm mint` replaces it"
        );
    }
    if let Some(id) = assistant_vk_id {
        tracing::error!(
            label = %label,
            assistant_vk_id = %id,
            "ASSISTANT VK ORPHANED: minted for a signup that did not finish; revoke it in Bifrost, or a retry plus `llm mint` replaces it"
        );
    }
}

fn refused_session() -> Response {
    pages::problem(
        StatusCode::BAD_REQUEST,
        "That signup could not be verified",
        SESSION_REFUSED,
    )
}

/// The unverifiable-session refusal, in the words of whichever flow it was.
///
/// `console_label` is `Some` only when this service knows the request was a
/// console login, and it is that login's own label. A console refusal must not
/// render signup copy or a link to the signup form: the person on the other end
/// already has a mailbox and was trying to get into it, and "start again" on the
/// signup page is an instruction to do the one thing they must not.
fn refused_session_for(state: &ControlState, console_label: Option<&str>) -> Response {
    match console_label {
        Some(label) => pages::console_problem_with_link(
            StatusCode::BAD_REQUEST,
            CONSOLE_REFUSED_HEADING,
            CONSOLE_SESSION_REFUSED,
            &state.config().tenant_url(label),
        ),
        None => refused_session(),
    }
}

fn internal_problem() -> Response {
    pages::problem(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong on our side",
        "Nothing was set up and your invite code has not been used. Please try again.",
    )
}

/// What a consent that left something out gets: all three, or none.
///
/// Honest about why rather than vague about what happened. Passband cannot run
/// on two of the three: the daemon reads with one grant, archives and labels
/// with the second, and sends with the third, and a tenant provisioned on a
/// partial grant would look fine until the first button that needed the missing
/// one, with no way back to Google's screen from inside the app.
///
/// `200`, like the "changed my mind" page above and for the same reason: this is
/// a choice the person made at Google, not a failure of this service.
fn partial_consent_problem() -> Response {
    pages::problem(
        StatusCode::OK,
        "Passband needs all three Gmail permissions",
        "Nothing was set up and your invite code has not been used. Passband needs all three: \
         reading your mail to triage it, changing it to archive and label, and sending so you \
         can reply from the app. Start again and leave every box checked on Google's screen.",
    )
}

/// The one page that describes the gap between the two provisioning calls.
///
/// It is deliberately not the generic failure: the address was reserved for this
/// Google account, and the retry is the same three steps with the same two
/// answers. Saying "nothing was set up" here would be a small lie that sends
/// people off to pick a second address they do not need.
///
/// The wording describes WHAT HAPPENED rather than what is currently true on the
/// far side, because one of the failures that lands here is the warden having
/// lost the pending tenant altogether. The retry is right either way; only the
/// reason it works differs.
fn incomplete_problem() -> Response {
    pages::problem(
        StatusCode::BAD_GATEWAY,
        "Your mailbox is not finished yet",
        "We reserved your address and then could not finish setting it up. \
         Your invite code has not been used. Start again with the same invite code, \
         the same address, and the same Google account, and we will finish the job.",
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
        userinfo_url: &c.userinfo_url,
        timeout: OUTBOUND_TIMEOUT,
    }
}

/// A high-entropy opaque token: session ids and CSRF state.
fn random_token() -> Result<String, std::io::Error> {
    let mut bytes = [0u8; RANDOM_BYTES];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// First value for `name` in a form body, capped at [`MAX_FIELD`].
/// `form_urlencoded` never fails, so a garbled body is missing fields rather
/// than a rejection shape the page would have to render.
pub(crate) fn field(body: &Bytes, name: &str) -> String {
    field_capped(body, name, MAX_FIELD)
}

/// The same with the caller's own ceiling, for the two fields whose legitimate
/// length is not a label's: an address and the admin token. The cap is a bound
/// on work done before validation, so it is always set ABOVE what is valid and
/// the value's own check is what refuses it.
pub(crate) fn field_capped(body: &Bytes, name: &str, cap: usize) -> String {
    url::form_urlencoded::parse(body)
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.chars().take(cap).collect())
        .unwrap_or_default()
}

/// Whether a submitted string is shaped like something we could mail.
///
/// NOT an RFC 5322 validator: that grammar accepts things no mail provider will
/// take and rejecting on it would turn a typo into a lecture. The question here
/// is only whether this could be an address, and the real check is whether the
/// invite arrives. What it does refuse is anything that is not one address:
/// two `@`, a domain with no dot, a control character, and the empty halves.
///
/// THE PUNCTUATION LIST IS THE INTERESTING PART. RFC 5322 has a `name-addr`
/// shape, so `ceo<attacker@evil.tld>` is one address by every test above: one
/// `@`, dotted domain, printable throughout. On the dashboard it reads as a
/// name the operator might recognize, and a provider that parses the shape
/// mails the invite to the part after the angle bracket. Separators go with it,
/// because a comma or a semicolon is how a second recipient gets in.
fn is_email(email: &str) -> bool {
    const REFUSED: &[char] = &['<', '>', ',', ';', ':', '"', '(', ')', '[', ']', '\\', '`'];
    if !(3..=MAX_EMAIL).contains(&email.len())
        || !email.bytes().all(|b| b.is_ascii_graphic())
        || email.contains(REFUSED)
    {
        return false;
    }
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

/// Every answer `POST /waitlist` gives, with the three headers that make it
/// readable from the site and cacheable nowhere.
fn waitlist_answer(origin: &str, status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.to_string()),
            // The header above depends on who asked, so a cache that keyed on
            // the URL alone would hand one origin's answer to another.
            (header::VARY, "origin".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::CONTENT_TYPE, "application/json".to_string()),
        ],
        body,
    )
        .into_response()
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

    /// An address over the limit must come out over the limit. Truncating it
    /// to 254 would produce a different, perfectly valid address, and the
    /// invite would go to whoever owns it.
    #[test]
    fn reads_an_address_one_character_past_the_limit() {
        let long = format!("{}@example.com", "a".repeat(MAX_EMAIL));
        let body = Bytes::from(format!("email={long}"));
        let read = field_capped(&body, "email", MAX_EMAIL + 1);
        assert_eq!(read.len(), MAX_EMAIL + 1);
        assert!(!is_email(&read));
    }

    #[test]
    fn holds_a_submitted_address_to_a_shape() {
        for good in [
            "ada@example.com",
            "ada+hosted@mail.example.co.uk",
            "a@b.c",
            "ADA@EXAMPLE.COM",
        ] {
            assert!(is_email(good), "{good:?}");
        }
        for bad in [
            "",
            "ada",
            "ada@",
            "@example.com",
            "ada@example",
            "ada@.com",
            "ada@example.",
            "ada@@example.com",
            "ada@one.com,bob@two.com",
            "ada @example.com",
            "ada@example.com\n",
            "adaexample.com",
        ] {
            assert!(!is_email(bad), "{bad:?}");
        }
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
