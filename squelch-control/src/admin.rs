//! The operator's door: the waitlist dashboard and the two buttons on it.
//!
//! ONE CREDENTIAL, AND IT IS NOT MOUNTED WITHOUT ONE. These routes exist only
//! when the waitlist trio is configured (see [`crate::config::WaitlistConfig`]),
//! so a deployment with no admin token answers 404 here rather than 403: there
//! is nothing at this URL to guess at.
//!
//! WHAT GUARDS IT, in the order an attacker meets it: a 32-character minimum on
//! the token, a constant-time compare, one uniform refusal for every way a login
//! can fail, its own tight rate bucket (`limit_admin_login`), and a
//! `SameSite=Strict` cookie.
//!
//! CSRF TAKES BOTH HALVES, and the cookie is only the first. `SameSite=Strict`
//! narrows to the registrable domain, so `passband.app`, `warden.passband.app`,
//! and the gateway are all "same site" as this service and a page on any of them
//! can post these forms with the cookie attached. [`same_origin`] is what
//! narrows it to this origin, and it is why these POSTs still carry no token of
//! their own.
//!
//! THE PLAINTEXT INVITE CODE EXISTS BETWEEN TWO LINES OF [`mint_and_send`]:
//! [`crate::invites::mint`] produces it and the Resend request body consumes it.
//! It is never rendered into a page, never put in a URL, and never logged, so
//! "resend the invite" is not a thing this service can do. What the buttons do
//! instead is revoke the old code and mint a new one, which is the same outcome
//! for the person waiting and a much smaller promise for this crate to keep.
//!
//! PRIVACY: a waitlist address is shown on the dashboard and nowhere else. Every
//! log line here names the ROW ID.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;

use crate::cookie::{self, AdminClaim};
use crate::handlers::{field, field_capped};
use crate::invites;
use crate::pages;
use crate::state::ControlState;
use crate::store::{WAITLIST_APPROVED, WAITLIST_PENDING, WaitlistRow};

/// Ceiling on the presented admin token. Well above any token an operator
/// would generate, because a value truncated on the way in fails the compare
/// and is indistinguishable from a wrong one: the lockout would look like a
/// forgotten password.
const MAX_TOKEN: usize = 512;

/// What every refused login says. ONE sentence for "no token", "wrong token",
/// and "not a token at all", for the same reason the invite refusal is one
/// sentence: anything that tells them apart is a signal to whoever is guessing.
const LOGIN_REFUSED: &str = "That token was not accepted.";

/// What an action taken without a live session says. Deliberately not
/// [`LOGIN_REFUSED`]: nothing was guessed here, a session aged out, and the fix
/// is to sign in again.
const SIGNED_OUT: &str = "Your admin session has ended. Sign in again.";

/// The approval guard, reported. Covers a double-clicked button, a refreshed
/// POST, and an id that names no row, because the one statement that decides
/// this cannot tell them apart and none of the three minted anything.
const ALREADY_APPROVED: &str =
    "That row is not waiting any more. It was already approved, or there is no row with that id, \
     and nothing was minted or sent.";

const NO_SUCH_ROW: &str = "There is no waitlist row with that id.";

/// A fresh invite REPLACES one, so there has to be one.
const NOT_APPROVED: &str = "Approve that row first.";

/// The one thing a re-send will not do. A spent code means somebody set a
/// mailbox up with it, and quietly minting a second is an extra tenant nobody
/// approved.
const INVITE_SPENT: &str =
    "That invite has already been used, so nothing was sent. If they need another mailbox, \
     issue a code with the CLI.";

const STORE_TROUBLE: &str = "The store did not answer. Nothing changed, so try again.";

/// The compare-and-swap in [`mint_and_send`], reported. Two presses of the same
/// button raced and the other one won: its code is the one on the row and the
/// one in the mail, and this call took its own mint back.
const ALREADY_HANDLED: &str =
    "That row was already handled by another click, so nothing extra was sent.";

/// A re-send that would tear up a signup already in progress.
const INVITE_HELD: &str =
    "Somebody is redeeming that code right now, so it was left alone. Try again in a few minutes.";

/// What a browser must say about where an admin POST came from.
///
/// `SameSite=Strict` narrows the cookie to this SITE, which is not this ORIGIN:
/// every `passband.app` name (the marketing site, the warden, the gateway) is
/// same-site with this one, so a page on any of them can post these forms and
/// the browser attaches the cookie. This check is what narrows it the rest of
/// the way. A request with neither header is not a browser making it, and the
/// cookie is still required.
fn same_origin(state: &ControlState, headers: &HeaderMap) -> bool {
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        return origin == state.config().public_url.trim_end_matches('/');
    }
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        // "none" is a typed URL or a bookmark, which no other page can forge.
        Some(site) => site == "same-origin" || site == "none",
        None => true,
    }
}

/// `GET /admin` — the dashboard, or the door.
pub async fn page(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if !is_admin(&state, &headers) {
        // No message: this is the front door, not a refusal. A stranger who
        // asked for `/admin` learns only that the form exists.
        return pages::admin_login(None);
    }
    dashboard(&state, None)
}

/// `POST /admin/login` — present the token, get a twelve-hour session.
pub async fn login(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let config = state.config();
    let Some(waitlist) = config.waitlist.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !same_origin(&state, &headers) {
        return cross_origin();
    }

    let presented = field_capped(&body, "token", MAX_TOKEN);
    if !squelch_httpauth::ct_eq(presented.as_bytes(), waitlist.admin_token.as_bytes()) {
        // PRIVACY: that a login was refused. Never what was presented, and
        // never how close it was.
        tracing::warn!("admin login refused");
        return pages::admin_login(Some(LOGIN_REFUSED));
    }

    let value = cookie::sign_admin(
        &config.cookie_key,
        &AdminClaim::new(&waitlist.admin_token, Utc::now().timestamp()),
    );
    tracing::info!("admin signed in");
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/admin".to_string()),
            (
                header::SET_COOKIE,
                cookie::set_admin_cookie(&value, !config.is_insecure()),
            ),
        ],
    )
        .into_response()
}

/// `POST /admin/approve` — let one person in.
///
/// The store's own guard decides whether this call is the one that approves:
/// [`crate::store::ControlStore::approve_waitlist`] moves the row out of
/// `pending` in a single statement, so a double click, a refreshed POST, and
/// two operators on the same row produce exactly one minted invite and one
/// email between them.
pub async fn approve(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_admin(&state, &headers) {
        return signed_out(&state);
    }
    if !same_origin(&state, &headers) {
        return cross_origin();
    }
    let Some(id) = row_id(&body) else {
        return dashboard(&state, Some(NO_SUCH_ROW));
    };

    match state.store().approve_waitlist(id, Utc::now()) {
        Ok(true) => {}
        Ok(false) => return dashboard(&state, Some(ALREADY_APPROVED)),
        Err(e) => {
            tracing::error!(id, error = %e, "approving a waitlist row failed");
            return dashboard(&state, Some(STORE_TROUBLE));
        }
    }

    // A row that just left `pending` names no invite yet, so that NULL is what
    // this call expects to replace.
    match mint_and_send(&state, id, None).await {
        Some(problem) => dashboard(&state, Some(problem)),
        None => back_to_dashboard(),
    }
}

/// `POST /admin/send` — replace an approved row's invite with a fresh one.
///
/// Both the repair path (the first send failed) and the lost-email path. The
/// old code is revoked FIRST, because the alternative is two live invites for
/// one approval, which is one more mailbox than anybody approved.
pub async fn send(State(state): State<ControlState>, headers: HeaderMap, body: Bytes) -> Response {
    if !is_admin(&state, &headers) {
        return signed_out(&state);
    }
    if !same_origin(&state, &headers) {
        return cross_origin();
    }
    let Some(id) = row_id(&body) else {
        return dashboard(&state, Some(NO_SUCH_ROW));
    };

    let row = match state.store().waitlist_entry(id) {
        Ok(Some(row)) => row,
        Ok(None) => return dashboard(&state, Some(NO_SUCH_ROW)),
        Err(e) => {
            tracing::error!(id, error = %e, "reading a waitlist row failed");
            return dashboard(&state, Some(STORE_TROUBLE));
        }
    };
    if row.status != WAITLIST_APPROVED {
        return dashboard(&state, Some(NOT_APPROVED));
    }

    if let Some(old) = row.invite_id {
        // Asked BEFORE the revoke, because a revoke cannot be taken back: the
        // hold means somebody is at Google's consent screen with that code, and
        // deleting it fails their signup after they have already granted it.
        match state.store().invite_is_held(old, Utc::now()) {
            Ok(true) => return dashboard(&state, Some(INVITE_HELD)),
            Ok(false) => {}
            Err(e) => {
                tracing::error!(id, error = %e, "reading the replaced invite failed");
                return dashboard(&state, Some(STORE_TROUBLE));
            }
        }
        match state.store().revoke_invite(old) {
            Ok(true) => {}
            // SPENT is a refusal: somebody set a mailbox up with that code, and
            // minting a second is a tenant nobody approved.
            Ok(false) if invite_is_spent(&state, old) => {
                return dashboard(&state, Some(INVITE_SPENT));
            }
            // GONE is not. An operator who revoked the code from the CLI leaves
            // a row pointing at nothing, and refusing that too would strand the
            // person waiting behind a message that is not even true.
            Ok(false) => {}
            Err(e) => {
                tracing::error!(id, error = %e, "revoking the replaced invite failed");
                return dashboard(&state, Some(STORE_TROUBLE));
            }
        }
    }

    match mint_and_send(&state, id, row.invite_id).await {
        Some(problem) => dashboard(&state, Some(problem)),
        None => back_to_dashboard(),
    }
}

/// Mint one invite for an approved row and mail it. `Some(message)` is a banner
/// the caller must render; `None` means the row itself now tells the story.
///
/// THE POINTER WRITE IS THE GATE ON THE SEND. Two presses of one button both
/// mint, and [`crate::store::ControlStore::set_waitlist_invite`] compares the
/// pointer each of them read before it writes, so exactly one wins. The loser
/// revokes what it just minted and mails NOTHING: a second live code in the
/// applicant's inbox is a second mailbox they can provision, and one no row
/// names is one no button can ever take back.
///
/// A send that reaches nobody leaves `notified_at` NULL, which is exactly the
/// "email not sent" badge the operator is looking at, so that failure is a log
/// line and a redirect rather than a banner: the row keeps saying it after a
/// refresh, which a banner does not.
///
/// PRIVACY: the id and the error's `Display`. The address is in the row that is
/// in scope here and the code is in a local one, and neither may be formatted
/// into a line.
async fn mint_and_send(
    state: &ControlState,
    id: i64,
    expected_prior: Option<i64>,
) -> Option<&'static str> {
    let (resend, _) = state.waitlist()?;
    let row = match state.store().waitlist_entry(id) {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!(id, "the waitlist row went away before its invite was minted");
            return Some(NO_SUCH_ROW);
        }
        Err(e) => {
            tracing::error!(id, error = %e, "reading the waitlist row failed");
            return Some(STORE_TROUBLE);
        }
    };

    let minted = match invites::mint() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(id, error = %e, "the system random source failed");
            return Some(STORE_TROUBLE);
        }
    };
    let expires_at = Utc::now() + chrono::Duration::days(invites::DEFAULT_TTL_DAYS);
    let invite_id = match state.store().insert_invite(&minted.code_hash, expires_at) {
        Ok(invite_id) => invite_id,
        Err(e) => {
            tracing::error!(id, error = %e, "recording the minted invite failed");
            return Some(STORE_TROUBLE);
        }
    };
    match state
        .store()
        .set_waitlist_invite(id, invite_id, expected_prior)
    {
        Ok(true) => {}
        Ok(false) => {
            discard(state, id, invite_id);
            return Some(ALREADY_HANDLED);
        }
        Err(e) => {
            tracing::error!(id, invite_id, error = %e, "pointing the waitlist row at its invite failed");
            discard(state, id, invite_id);
            return Some(STORE_TROUBLE);
        }
    }

    match resend
        .send_invite(&row.email, &minted.code, &state.config().public_url)
        .await
    {
        Ok(()) => {
            if let Err(e) = state.store().mark_waitlist_notified(id, Utc::now()) {
                tracing::error!(id, error = %e, "stamping the waitlist row as notified failed");
                return None;
            }
            tracing::info!(id, "waitlist invite sent");
        }
        // The row stays approved with no stamp, which is the badge and the
        // button. The provider's own words are not in this error type.
        Err(e) => tracing::warn!(id, error = %e, "sending the waitlist invite failed"),
    }
    None
}

/// Take back a code this call minted and then lost the row for. Best effort: a
/// code nothing points at expires on its own, and there is nobody to tell.
fn discard(state: &ControlState, id: i64, invite_id: i64) {
    if let Err(e) = state.store().revoke_invite(invite_id) {
        tracing::error!(id, invite_id, error = %e, "revoking an unclaimed invite failed");
    }
}

/// Whether an invite row is there and has been SPENT.
///
/// Asked only when [`crate::store::ControlStore::revoke_invite`] answered
/// false, which it does for two causes that must not be treated alike: a code
/// somebody redeemed, and a row that is not there at all. The listing is a
/// scan, and it is affordable here because this runs once per press of a button
/// a human presses.
///
/// A store that will not answer counts as spent. That is the closed direction:
/// the worst case is an operator sending themselves a CLI code, rather than this
/// page minting an invite it could not rule out.
fn invite_is_spent(state: &ControlState, invite_id: i64) -> bool {
    match state.store().list_invites() {
        Ok(rows) => rows
            .iter()
            .any(|r| r.id == invite_id && r.used_at.is_some()),
        Err(e) => {
            tracing::error!(invite_id, error = %e, "listing invites failed");
            true
        }
    }
}

/// Whether this request carries a live admin session.
///
/// The cookie is the whole answer: it is signed with this deployment's key,
/// carries the `aud` marker no signup claim has, names the token it was opened
/// with, and expires on its own. See [`crate::cookie::verify_admin`].
fn is_admin(state: &ControlState, headers: &HeaderMap) -> bool {
    let config = state.config();
    let Some(waitlist) = config.waitlist.as_ref() else {
        return false;
    };
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(cookie::admin_from_header)
        .and_then(|v| {
            cookie::verify_admin(
                &config.cookie_key,
                &waitlist.admin_token,
                v,
                Utc::now().timestamp(),
            )
        })
        .is_some()
}

/// The dashboard, listed fresh. `error` is a banner over a correct list, never
/// a page in place of one.
fn dashboard(state: &ControlState, error: Option<&str>) -> Response {
    let rows = match state.store().list_waitlist() {
        Ok(rows) => rows,
        Err(e) => {
            // An empty page under a banner that says the store did not answer.
            // Rendering "nobody is waiting" with no explanation would be the
            // one wrong thing to show an operator.
            tracing::error!(error = %e, "listing the waitlist failed");
            return pages::admin_page(&[], &[], Some(STORE_TROUBLE));
        }
    };
    let (pending, approved): (Vec<WaitlistRow>, Vec<WaitlistRow>) =
        rows.into_iter().partition(|r| r.status == WAITLIST_PENDING);
    pages::admin_page(&pending, &approved, error)
}

/// What an action with no live session gets: the door, a 401, and the stale
/// cookie cleared so the browser stops presenting it.
fn signed_out(state: &ControlState) -> Response {
    let mut resp = pages::admin_login(Some(SIGNED_OUT));
    let cleared = cookie::clear_admin_cookie(!state.config().is_insecure());
    if let Ok(value) = header::HeaderValue::from_str(&cleared) {
        resp.headers_mut().append(header::SET_COOKIE, value);
    }
    resp
}

/// A POST that some other origin's page made this browser send. Bare, because
/// there is nobody to explain it to: the operator never sees this, and the page
/// that caused it is not owed a reason.
fn cross_origin() -> Response {
    tracing::warn!("admin action refused: cross-origin");
    StatusCode::FORBIDDEN.into_response()
}

/// Every action that did something ends here rather than rendering, so a
/// refresh re-reads the list instead of pressing the button again.
fn back_to_dashboard() -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, "/admin")]).into_response()
}

/// The row a button was on. An integer or nothing: an id that is not an id is a
/// refusal, not a lookup.
fn row_id(body: &Bytes) -> Option<i64> {
    field(body, "id").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_row_a_button_was_on() {
        assert_eq!(row_id(&Bytes::from("id=42")), Some(42));
        assert_eq!(row_id(&Bytes::from("id=42&id=7")), Some(42));
        for bad in ["", "id=", "id=ada", "id=1.5", "id=9999999999999999999999"] {
            assert_eq!(row_id(&Bytes::from(bad)), None, "{bad:?}");
        }
    }
}
