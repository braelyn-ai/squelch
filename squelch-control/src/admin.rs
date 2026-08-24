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
//! It is never rendered into a page and never logged, so "resend the invite" is
//! not a thing this service can do. What the buttons do instead is revoke the
//! old code and mint a new one, which is the same outcome for the person
//! waiting and a much smaller promise for this crate to keep.
//!
//! It IS put in a URL, in one place: the emailed link carries `?invite=` so the
//! signup form arrives filled in. That reverses this crate's original rule and
//! the reasons for the rule did not go away, so they are written down where the
//! link is built ([`crate::resend`]) and where it is read
//! ([`crate::handlers::signup_form`]) rather than only here.
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
const ALREADY_APPROVED: &str = "That row is not waiting any more. It was already approved, or there is no row with that id, \
     and nothing was minted or sent.";

const NO_SUCH_ROW: &str = "There is no waitlist row with that id.";

/// A fresh invite REPLACES one, so there has to be one.
const NOT_APPROVED: &str = "Approve that row first.";

/// The one thing a re-send will not do. A spent code means somebody set a
/// mailbox up with it, and quietly minting a second is an extra tenant nobody
/// approved.
const INVITE_SPENT: &str = "That invite has already been used, so nothing was sent. If they need another mailbox, \
     issue a code with the CLI.";

const STORE_TROUBLE: &str = "The store did not answer. Nothing changed, so try again.";

/// A direct invite typed as something that is not an address. Says what is
/// wrong, unlike the refusals above it: this one is the operator's own typo and
/// there is nobody to keep it from.
const INVALID_ADDRESS: &str = "That is not an email address. Nothing was sent.";

/// A direct invite for somebody already approved. The row is on the page below
/// the banner, with the button that replaces a lost code.
const ALREADY_INVITED: &str = "That address has already been invited, so nothing extra was sent. Its row is below, and \
     \"Send fresh invite\" replaces a code that never arrived.";

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
    origin_matches(&state.config().public_url, headers)
}

/// [`same_origin`] against a stated origin, so it can be tested without a
/// deployment's whole config behind it.
///
/// TWO SOURCES, AND THE STATED ORIGIN IS NOT ALWAYS THE BETTER ONE. `Origin` is
/// the specific answer when it names something, and `Sec-Fetch-Site` is the
/// unforgeable one: it is a `Sec-` prefixed forbidden header name, so no page,
/// no script, and no `fetch` option can write it, and only the browser that
/// built the request decides its value. That is why the fallbacks below lean on
/// it rather than on failing closed.
///
/// The trailing slash is trimmed off BOTH sides. A serialized origin has no
/// path, so no browser sends one, but something that rewrites the header in
/// flight is not a browser and does not always agree; matching on that is a
/// lockout with no error to read.
fn origin_matches(expected: &str, headers: &HeaderMap) -> bool {
    let site = headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        // "none" is a typed URL or a bookmark, which no other page can forge.
        .map(|site| site == "same-origin" || site == "none");

    match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        // `null` is an origin the browser declined to NAME: a sandboxed frame,
        // a redirect chain, or a header rewritten by a proxy between the form
        // and this service. It is the absence of a stated origin rather than a
        // statement of a different one, so matching it against ours refuses a
        // request the browser itself calls same-origin. Deciding it on fetch
        // metadata is not a loosening: a cross-site page's POST arrives with
        // `cross-site` written by the browser and is refused here, and an
        // attacker cannot write that header at all. Absent metadata IS refused,
        // which is stricter than the no-Origin case below: a client that sends
        // `null` and no metadata is claiming an opaque origin with nothing to
        // corroborate it. Found live 2026-08-14, on an operator's own browser.
        Some("null") => site.unwrap_or(false),
        Some(origin) => origin.trim_end_matches('/') == expected.trim_end_matches('/'),
        // Nothing stated at all. A request with neither header is not a browser
        // making it, and the cookie is still required.
        None => site.unwrap_or(true),
    }
}

/// Ceiling on an echoed header value. An origin is a scheme, a host, and maybe
/// a port; anything past this is not one, and the operator reading it does not
/// need the rest to recognize what their client is doing.
const MAX_ECHOED: usize = 128;

/// The two headers the refusal turned on, as a line a log and a page can both
/// carry.
///
/// These are the CALLER'S bytes, so they are filtered to the characters an
/// origin is made of before they go anywhere. Nothing here is a secret: it is
/// what the client itself sent, and the deployment's own origin is the URL in
/// the address bar and in every invite email.
fn origin_report(headers: &HeaderMap) -> String {
    let field = |name: &str| -> String {
        let Some(raw) = headers.get(name).and_then(|v| v.to_str().ok()) else {
            return "absent".to_string();
        };
        let mut out: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "-_.:/[]".contains(*c))
            .take(MAX_ECHOED)
            .collect();
        if out.is_empty() {
            out.push_str("unreadable");
        }
        out
    };
    format!(
        "Origin: {} / Sec-Fetch-Site: {}",
        field("origin"),
        field("sec-fetch-site")
    )
}

/// `GET /admin` — the dashboard, or the door.
pub async fn page(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if !is_admin(&state, &headers) {
        // No message: this is the front door, not a refusal. A stranger who
        // asked for `/admin` learns only that the form exists.
        return pages::admin_login(None);
    }
    dashboard(&state, None).await
}

/// `POST /admin/logout` — end this session deliberately.
///
/// IT EXISTS BECAUSE THE SESSION GOT LONG. At twelve hours the browser signed
/// itself out by lunchtime and a button would have been decoration; at thirty
/// days ([`cookie::ADMIN_COOKIE_TTL_SECS`]) the only way off a machine that is
/// not yours would have been rotating the admin token in Railway, which signs
/// out every other browser too. This is the small exit, and the token rotation
/// stays the big one.
///
/// NO ADMIN CHECK, deliberately: it clears the cookie and renders the door
/// either way. Refusing to sign out a session that is already dead would be a
/// refusal with nothing behind it, and answering the same way for a live
/// session and an expired one says nothing about which was presented.
///
/// The origin check stays, because a POST that clears an operator's session
/// from another site's page is a nuisance somebody else can cause.
pub async fn logout(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if state.config().waitlist.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !same_origin(&state, &headers) {
        return cross_origin(&state, &headers);
    }
    clearing(&state, pages::admin_signed_out())
}

/// `POST /admin/login` — present the token, open a session.
pub async fn login(State(state): State<ControlState>, headers: HeaderMap, body: Bytes) -> Response {
    let config = state.config();
    let Some(waitlist) = config.waitlist.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !same_origin(&state, &headers) {
        return cross_origin(&state, &headers);
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

/// `POST /admin/invite` — invite an address that never asked.
///
/// The waitlist is a queue of people who found the site first. This is the
/// other direction: someone the operator already knows, invited by typing their
/// address. It lands on the SAME ledger as an approved waitlist row, so the
/// history, the "email not sent" badge, and the re-send button all work on it
/// without knowing which door it came in by.
pub async fn invite(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_admin(&state, &headers) {
        return signed_out(&state);
    }
    if !same_origin(&state, &headers) {
        return cross_origin(&state, &headers);
    }

    // Capped one over the limit so a value AT the limit is still whole and an
    // address longer than we accept is refused rather than truncated into a
    // different, deliverable one.
    let email = field_capped(&body, "email", crate::handlers::MAX_EMAIL + 1);
    if !crate::handlers::is_email(&email) {
        return dashboard(&state, Some(INVALID_ADDRESS)).await;
    }

    let id = match state.store().invite_directly(&email, Utc::now()).await {
        Ok(Some(id)) => id,
        // Already on the approved half. Not an error worth a red banner, but
        // not silence either: the row is on the page with its own button, and
        // saying so is what stops the operator from typing it again.
        Ok(None) => return dashboard(&state, Some(ALREADY_INVITED)).await,
        Err(e) => {
            tracing::error!(error = %e, "recording a direct invite failed");
            return dashboard(&state, Some(STORE_TROUBLE)).await;
        }
    };

    // PRIVACY: the row id, like every other line in this module. The address is
    // in scope right here and does not go in the log.
    tracing::info!(id, "invited an address directly");
    // A row that was just created or just promoted names no invite yet, which
    // is the NULL this mint expects to replace.
    match mint_and_send(&state, id, None).await {
        Some(problem) => dashboard(&state, Some(problem)).await,
        None => back_to_dashboard(),
    }
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
        return cross_origin(&state, &headers);
    }
    let Some(id) = row_id(&body) else {
        return dashboard(&state, Some(NO_SUCH_ROW)).await;
    };

    match state.store().approve_waitlist(id, Utc::now()).await {
        Ok(true) => {}
        Ok(false) => return dashboard(&state, Some(ALREADY_APPROVED)).await,
        Err(e) => {
            tracing::error!(id, error = %e, "approving a waitlist row failed");
            return dashboard(&state, Some(STORE_TROUBLE)).await;
        }
    }

    // A row that just left `pending` names no invite yet, so that NULL is what
    // this call expects to replace.
    match mint_and_send(&state, id, None).await {
        Some(problem) => dashboard(&state, Some(problem)).await,
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
        return cross_origin(&state, &headers);
    }
    let Some(id) = row_id(&body) else {
        return dashboard(&state, Some(NO_SUCH_ROW)).await;
    };

    let row = match state.store().waitlist_entry(id).await {
        Ok(Some(row)) => row,
        Ok(None) => return dashboard(&state, Some(NO_SUCH_ROW)).await,
        Err(e) => {
            tracing::error!(id, error = %e, "reading a waitlist row failed");
            return dashboard(&state, Some(STORE_TROUBLE)).await;
        }
    };
    if row.status != WAITLIST_APPROVED {
        return dashboard(&state, Some(NOT_APPROVED)).await;
    }

    if let Some(old) = row.invite_id {
        // The reservation check travels WITH the delete. Asking first and
        // deleting second releases the lock in between, and that gap is long
        // enough for a signup to take the hold the check just said was absent:
        // the code would then be deleted out from under somebody who has
        // already granted Google consent they cannot grant twice.
        match state.store().revoke_unheld_invite(old, Utc::now()).await {
            Ok(true) => {}
            // Why it declined, asked only now that nothing destructive is left
            // to do: a race here changes the sentence, not the outcome.
            Ok(false) => {
                // A store that will not answer counts as held, which is the
                // closed direction: refusing costs a click, and guessing wrong
                // the other way costs somebody their signup.
                if state
                    .store()
                    .invite_is_held(old, Utc::now())
                    .await
                    .unwrap_or(true)
                {
                    return dashboard(&state, Some(INVITE_HELD)).await;
                }
                // SPENT is a refusal: somebody set a mailbox up with that code,
                // and minting a second is a tenant nobody approved.
                if invite_is_spent(&state, old).await {
                    return dashboard(&state, Some(INVITE_SPENT)).await;
                }
                // GONE is not. An operator who revoked the code from the CLI
                // leaves a row pointing at nothing, and refusing that too would
                // strand the person waiting behind a message that is not true.
            }
            Err(e) => {
                tracing::error!(id, error = %e, "revoking the replaced invite failed");
                return dashboard(&state, Some(STORE_TROUBLE)).await;
            }
        }
    }

    match mint_and_send(&state, id, row.invite_id).await {
        Some(problem) => dashboard(&state, Some(problem)).await,
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
    let row = match state.store().waitlist_entry(id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!(
                id,
                "the waitlist row went away before its invite was minted"
            );
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
    let invite_id = match state
        .store()
        .insert_invite(&minted.code_hash, expires_at, None)
        .await
    {
        Ok(invite_id) => invite_id,
        Err(e) => {
            tracing::error!(id, error = %e, "recording the minted invite failed");
            return Some(STORE_TROUBLE);
        }
    };
    match state
        .store()
        .set_waitlist_invite(id, invite_id, expected_prior)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            discard(state, id, invite_id).await;
            return Some(ALREADY_HANDLED);
        }
        Err(e) => {
            tracing::error!(id, invite_id, error = %e, "pointing the waitlist row at its invite failed");
            discard(state, id, invite_id).await;
            return Some(STORE_TROUBLE);
        }
    }

    match resend
        .send_invite(&row.email, &minted.code, &state.config().public_url)
        .await
    {
        Ok(()) => {
            match state
                .store()
                .mark_waitlist_notified(id, invite_id, Utc::now())
                .await
            {
                // The row moved to a newer code while this send was in flight,
                // so the delivery this stamp would claim is not the one the row
                // is waiting on. The newer send stamps its own.
                Ok(false) => {
                    tracing::info!(id, "an invite was delivered after its row moved on");
                    return None;
                }
                Ok(true) => tracing::info!(id, "waitlist invite sent"),
                Err(e) => {
                    tracing::error!(id, error = %e, "stamping the waitlist row as notified failed");
                    return None;
                }
            }
        }
        // The row stays approved with no stamp, which is the badge and the
        // button. The provider's own words are not in this error type.
        Err(e) => tracing::warn!(id, error = %e, "sending the waitlist invite failed"),
    }
    None
}

/// Take back a code this call minted and then lost the row for. Best effort: a
/// code nothing points at expires on its own, and there is nobody to tell.
async fn discard(state: &ControlState, id: i64, invite_id: i64) {
    if let Err(e) = state.store().revoke_invite(invite_id).await {
        tracing::error!(id, invite_id, error = %e, "revoking an unclaimed invite failed");
    }
}

/// Whether an invite row is there and has been SPENT.
///
/// Asked only when [`crate::store::ControlStore::revoke_unheld_invite`]
/// answered false, which it does for three causes that must not be treated
/// alike: a code somebody redeemed, a code somebody is holding, and a row that
/// is not there at all. The listing is a scan, and it is affordable here
/// because this runs once per press of a button a human presses.
///
/// A store that will not answer counts as spent. That is the closed direction:
/// the worst case is an operator sending themselves a CLI code, rather than this
/// page minting an invite it could not rule out.
async fn invite_is_spent(state: &ControlState, invite_id: i64) -> bool {
    match state.store().list_invites().await {
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
async fn dashboard(state: &ControlState, error: Option<&str>) -> Response {
    let rows = match state.store().list_waitlist().await {
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
    clearing(state, pages::admin_login(Some(SIGNED_OUT)))
}

/// The same cookie clear on a page that is NOT a refusal. Both doors have to
/// drop the cookie, and only one of them is bad news.
fn clearing(state: &ControlState, mut resp: Response) -> Response {
    let cleared = cookie::clear_admin_cookie(!state.config().is_insecure());
    if let Ok(value) = header::HeaderValue::from_str(&cleared) {
        resp.headers_mut().append(header::SET_COOKIE, value);
    }
    resp
}

/// A POST that did not come from this origin's own page.
///
/// This used to be a bare 403 on the theory that the operator never sees it and
/// the page that caused it is not owed a reason. The first half was wrong: an
/// extension, a proxy, or a sandboxed frame rewrites these headers on a page
/// whose address bar looks correct, and then the only person reading this is
/// the operator, locked out with nothing to act on. The second half still
/// holds, which is why the page says what was expected and what arrived and
/// nothing else: an attacker learns their own request back.
fn cross_origin(state: &ControlState, headers: &HeaderMap) -> Response {
    let report = origin_report(headers);
    tracing::warn!(%report, "admin action refused: cross-origin");
    pages::admin_cross_origin(&state.config().public_url, &report)
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

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                header::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    const HERE: &str = "https://signup.passband.app";

    #[test]
    fn takes_this_origin_however_it_is_spelled() {
        assert!(origin_matches(HERE, &headers(&[("origin", HERE)])));
        // A rewritten header with a path separator on the end is still this
        // origin, and refusing it locks the operator out over a slash.
        assert!(origin_matches(
            HERE,
            &headers(&[("origin", "https://signup.passband.app/")])
        ));
        assert!(origin_matches(
            &format!("{HERE}/"),
            &headers(&[("origin", HERE)])
        ));
    }

    #[test]
    fn an_unnamed_origin_is_decided_by_the_header_no_page_can_write() {
        // The browser calls it same-origin; the Origin header did not survive
        // whatever sat between the form and here.
        assert!(origin_matches(
            HERE,
            &headers(&[("origin", "null"), ("sec-fetch-site", "same-origin")])
        ));
        assert!(origin_matches(
            HERE,
            &headers(&[("origin", "null"), ("sec-fetch-site", "none")])
        ));
        // A sandboxed frame on somebody else's page is still somebody else's
        // page, and the browser says so.
        assert!(!origin_matches(
            HERE,
            &headers(&[("origin", "null"), ("sec-fetch-site", "cross-site")])
        ));
        assert!(!origin_matches(
            HERE,
            &headers(&[("origin", "null"), ("sec-fetch-site", "same-site")])
        ));
        // Opaque and uncorroborated: refused, where a request stating no origin
        // at all is not. Nothing that reaches here is a browser.
        assert!(!origin_matches(HERE, &headers(&[("origin", "null")])));
    }

    #[test]
    fn refuses_every_other_origin() {
        for bad in [
            "https://passband.app",
            "https://warden.passband.app",
            "http://signup.passband.app",
            "null",
            "https://signup.passband.app.evil.test",
        ] {
            assert!(
                !origin_matches(HERE, &headers(&[("origin", bad)])),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn falls_back_to_the_fetch_metadata_only_with_no_origin() {
        assert!(origin_matches(
            HERE,
            &headers(&[("sec-fetch-site", "same-origin")])
        ));
        assert!(origin_matches(
            HERE,
            &headers(&[("sec-fetch-site", "none")])
        ));
        assert!(!origin_matches(
            HERE,
            &headers(&[("sec-fetch-site", "cross-site")])
        ));
        // Not a browser at all. The cookie is still required.
        assert!(origin_matches(HERE, &headers(&[])));
        // The stated origin decides on its own: a browser that names another
        // origin is refused however friendly its fetch metadata reads.
        assert!(!origin_matches(
            HERE,
            &headers(&[
                ("origin", "https://passband.app"),
                ("sec-fetch-site", "same-origin")
            ])
        ));
    }

    #[test]
    fn reports_both_headers_and_keeps_nothing_that_could_end_a_page() {
        assert_eq!(
            origin_report(&headers(&[
                ("origin", HERE),
                ("sec-fetch-site", "cross-site")
            ])),
            "Origin: https://signup.passband.app / Sec-Fetch-Site: cross-site"
        );
        assert_eq!(
            origin_report(&headers(&[])),
            "Origin: absent / Sec-Fetch-Site: absent"
        );
        let nasty = origin_report(&headers(&[("origin", "<script>alert(1)</script>")]));
        assert!(!nasty.contains('<'), "{nasty}");
        assert!(!nasty.contains('>'), "{nasty}");
        let long = "https://".to_string() + &"a".repeat(400);
        let capped = origin_report(&headers(&[("origin", &long)]));
        assert!(
            capped.contains(&"a".repeat(MAX_ECHOED - "https://".len())),
            "kept what fits"
        );
        assert!(!capped.contains(&"a".repeat(MAX_ECHOED)), "and no more");
    }

    #[test]
    fn reads_the_row_a_button_was_on() {
        assert_eq!(row_id(&Bytes::from("id=42")), Some(42));
        assert_eq!(row_id(&Bytes::from("id=42&id=7")), Some(42));
        for bad in ["", "id=", "id=ada", "id=1.5", "id=9999999999999999999999"] {
            assert_eq!(row_id(&Bytes::from(bad)), None, "{bad:?}");
        }
    }
}
