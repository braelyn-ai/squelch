//! Sharing: the user hands a friend an invite, and the mail comes from THEIR
//! mailbox.
//!
//! The daemon is the middle of a three-party errand, and it is in the middle
//! for a reason each side cares about:
//!
//! - THE CONTROL PLANE mints the code, because a code is a free tenant and only
//!   the thing that provisions tenants may make one. The daemon presents a
//!   share token ([`squelch_control::share`], on the other side of the wire)
//!   and gets back one code and nothing else.
//! - THE USER'S OWN GMAIL sends the mail, because "Ada invited you" is worth
//!   infinitely more arriving FROM Ada than from a marketing address. It lands
//!   in an existing relationship, it threads, a reply reaches her, and it is in
//!   her Sent folder where she can see exactly what went out under her name.
//! - THE RECIPIENT'S ADDRESS NEVER LEAVES THIS MACHINE. The control plane is
//!   told a code was minted and by whom; it is not told who it was for. An
//!   invitee has consented to nothing at all, and the daemon their friend
//!   already trusts is the right and only place their address should be.
//!
//! HUMAN DOOR ONLY, twice over: it spends a quota and it sends mail as the
//! user. The agent door has no route to any of it.
//!
//! WHAT IS NEVER LOGGED HERE: the invite code (live credential material), the
//! share token, and any recipient address. What may be logged is a count and an
//! outcome word, which is what an operator needs to tell "the control plane is
//! down" from "Gmail refused us".
//!
//! COPY RULES (house style): the product is Passband, the voice is the site's
//! lowercase one, and there are no em dashes in anything a person reads.

use std::time::Duration;

use serde::Deserialize;

use squelch_core::store::Store as _;
use squelch_core::types::OpenRate;

/// Where the daemon asks for a code, and the bearer it asks with. Resolved from
/// the environment the warden renders (`SQUELCH_CONTROL_URL` +
/// `SQUELCH_CONTROL_TOKEN`); a self-hosted daemon has neither, and sharing is
/// simply absent there.
///
/// DELIBERATELY DERIVES NOTHING: `token` is a live bearer and a derived `Debug`
/// is how it reaches a formatted line.
pub struct Sharing {
    url: String,
    /// `Bearer <token>`, precomputed once. As secret as the token in it.
    auth_header: String,
    http: reqwest::Client,
}

/// Budget for the whole mint round trip. Generous by the standards of this
/// crate's outbound calls, because a person is watching a spinner that says
/// "sending" and one slow hop is better than a refusal they have to retry.
const MINT_TIMEOUT: Duration = Duration::from_secs(20);

/// Ceiling on the control plane's answer. It is a small JSON object; anything
/// larger is not the API we know.
const MAX_RESPONSE_BODY: usize = 64 * 1024;

/// How far back [`share_stat`] looks for the open rate.
///
/// Ninety days, and the same number is the floor: see [`MIN_STAT_DAYS`].
pub const STAT_WINDOW_DAYS: i64 = 90;

/// How much history the open rate must actually rest on before it may be
/// quoted at a stranger.
///
/// A mailbox synced last Tuesday can compute a rate. It cannot compute one
/// worth putting in an email under somebody's name, because the first days of a
/// mailbox are all backfill nobody has read. Thirty days of received mail is
/// the floor, checked against the OLDEST row in the window rather than against
/// the window's own length.
pub const MIN_STAT_DAYS: i64 = 30;

/// And how many messages. A quiet mailbox can clear [`MIN_STAT_DAYS`] on
/// eleven messages, where one unopened newsletter moves the number nine points.
pub const MIN_STAT_MESSAGES: u64 = 200;

/// The rate is rounded to a multiple of this before anyone sees it.
///
/// Two reasons, and the second is the one that matters. It stops the copy
/// claiming a precision the measurement does not have (see [`share_stat`] on
/// what `opened` cannot see). And an exact percentage plus a known window is a
/// fingerprint of one person's mailbox volume, which is not a thing to mail to
/// a stranger.
const STAT_ROUNDING: u32 = 5;

/// Above this, the number is left out of the mail entirely.
///
/// The copy exists to say something good. "I only need to manually open 78% of
/// my email" is an anti-testimonial, and the app must never send one under the
/// user's name; without a number the mail falls back to copy that makes no
/// claim at all.
const STAT_CEILING: u32 = 40;

/// What the control plane answers a mint with.
///
/// NO `Debug`: `code` is live credential material until it is in the mail.
#[derive(Deserialize)]
pub struct MintedInvite {
    pub code: String,
    pub signup_url: String,
    /// RFC3339. Read back as a DAY COUNT for the copy, so the mail never
    /// disagrees with the control plane about how long the code lasts.
    pub expires_at: String,
    /// Codes left in this tenant's window after this one.
    pub remaining: i64,
}

/// Why a mint did not happen. Each maps to copy the app shows; nothing here
/// carries a code, a token, or an address.
#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    /// The control plane could not be reached, or answered something
    /// unreadable.
    #[error("the invite service could not be reached")]
    Unreachable,
    /// Our share token is not accepted. An operator problem, not a user one:
    /// nothing the person at the keyboard does will fix it.
    #[error("this mailbox is not set up to send invites")]
    Unauthorized,
    /// This tenant has used its invites for the window.
    #[error("you have used all your invites for now")]
    QuotaExhausted,
    /// The control plane has no invite feature configured.
    #[error("invites are not available on this deployment")]
    Unavailable,
}

impl Sharing {
    /// Build the client, or `None` when this daemon has no control plane to ask
    /// (every self-host, and any tenant nobody has run `share mint` for).
    ///
    /// EMPTY COUNTS AS ABSENT, the same rule every other resolver in this
    /// codebase keeps: the kubelet leaves an optional `secretKeyRef` unset
    /// rather than empty, but a hand-edited Deployment can leave `""`, and an
    /// empty bearer must not become `Bearer `.
    pub fn new(url: String, token: String) -> Option<Self> {
        let url = url.trim().trim_end_matches('/').to_string();
        let token = token.trim().to_string();
        if url.is_empty() || token.is_empty() {
            return None;
        }
        let http = reqwest::Client::builder()
            // Redirects refused: this request carries the share token, and a
            // redirect is how a bearer ends up at a host nobody chose. The
            // answer carries a live invite code back, which is the same
            // argument in the other direction.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(MINT_TIMEOUT)
            .build()
            .ok()?;
        Some(Self {
            auth_header: format!("Bearer {token}"),
            url,
            http,
        })
    }

    /// Mint ONE invite code.
    ///
    /// One call, one code, one recipient. Not a batch: the quota is counted per
    /// row on the other side, and a batch that half-failed would have to
    /// explain which halves of it are now live codes nobody received.
    pub async fn mint(&self) -> Result<MintedInvite, ShareError> {
        let resp = self
            .http
            .post(format!("{}/tenant/invite", self.url))
            .header(reqwest::header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .map_err(|_| ShareError::Unreachable)?;

        match resp.status().as_u16() {
            200 => {
                let body = read_capped(resp).await?;
                serde_json::from_slice(&body).map_err(|_| ShareError::Unreachable)
            }
            401 | 403 => Err(ShareError::Unauthorized),
            429 => Err(ShareError::QuotaExhausted),
            // 404 included: a control plane too old to have the route is one
            // that cannot mint for us, which is the same fact.
            404 | 503 => Err(ShareError::Unavailable),
            _ => Err(ShareError::Unreachable),
        }
    }
}

async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>, ShareError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| ShareError::Unreachable)? {
        if out.len() + chunk.len() > MAX_RESPONSE_BODY {
            return Err(ShareError::Unreachable);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The share-worthy open rate as a whole percent, or `None` when this mailbox
/// cannot honestly support one.
///
/// THE THREE REFUSALS, all of which produce copy with no number in it rather
/// than a number with a caveat nobody reads:
///
/// 1. Not enough history ([`MIN_STAT_DAYS`] of received mail).
/// 2. Not enough mail ([`MIN_STAT_MESSAGES`]).
/// 3. A rate that is not worth quoting ([`STAT_CEILING`]).
///
/// AND THE CAVEAT THAT SURVIVES ALL THREE: `opened` counts what THIS DAEMON
/// served the body of, so mail the user read in Gmail on their phone is missing
/// from it and the number is biased LOW. It is the user's own number about the
/// user's own mailbox, sent by the user, which is what makes that acceptable;
/// it would not be acceptable in a claim Passband made itself.
pub fn share_stat(rate: &OpenRate, now: chrono::DateTime<chrono::Utc>) -> Option<u32> {
    if rate.received < MIN_STAT_MESSAGES {
        return None;
    }
    let oldest = rate.oldest_received_at?;
    if (now - oldest).num_days() < MIN_STAT_DAYS {
        return None;
    }
    // Integer arithmetic on the way to a rounded multiple: the percentage is
    // never shown at full precision, so computing it in floating point would
    // only add a way for two runs to disagree at the boundary.
    let percent = (rate.opened * 100).div_ceil(rate.received.max(1)) as u32;
    let rounded = percent.div_ceil(STAT_ROUNDING) * STAT_ROUNDING;
    // ROUNDED UP, then compared: a 41% that rounds to 45 is over the ceiling on
    // the number the reader would actually see, which is the one that has to
    // clear the bar.
    if rounded > STAT_CEILING {
        return None;
    }
    // A floor of one rounding step. A real mailbox never has a true 0%, and
    // "I open 0% of my email" reads as a lie or as a joke.
    Some(rounded.max(STAT_ROUNDING))
}

// --- the human door ---------------------------------------------------------

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use serde_json::json;

use crate::error::ApiError;
use crate::handlers::{audit_action, store_call};
use crate::invite_mail::{self, InviteCopy};
use crate::state::ApiState;

/// How many friends one press may reach.
///
/// Five is a person thinking of people, not a list being worked through. It is
/// well under the control plane's own window quota, so a full press never
/// exhausts an account in one go, and it bounds how many serial Gmail sends one
/// request can sit on.
const MAX_RECIPIENTS: usize = 5;

/// Ceiling on the user's own line. Long enough for a sentence or three, short
/// enough that this is a note and not a newsletter.
const MAX_NOTE: usize = 500;

/// `POST /client/invites` request body.
#[derive(Debug, serde::Deserialize)]
pub struct ShareBody {
    /// Who to invite. One mail each, and none of them can see the others: this
    /// sends N separate messages rather than one with N recipients, which is
    /// the difference between an invitation and a mailing list.
    recipients: Vec<String>,
    /// The user's own line, optional.
    note: Option<String>,
}

/// One recipient's outcome. The address is echoed back because the client sent
/// it and has to show the row it belongs to.
#[derive(Debug, Serialize)]
struct ShareResult {
    email: String,
    sent: bool,
    /// Present only on a failure, and it is copy for a human: never an upstream
    /// status, an API message, or anything about the code.
    error: Option<String>,
}

/// `GET /client/invites` - whether this daemon can share, and what the mail
/// would be able to say.
///
/// The app asks before it shows anything, so a self-hosted daemon and a tenant
/// nobody has run `share mint` for never render a button that could only fail.
pub async fn get_invites(State(state): State<ApiState>) -> Result<impl IntoResponse, ApiError> {
    if state.sharing().is_none() {
        return Ok(Json(json!({ "can_share": false })));
    }
    // The rate is read even when it will not clear the floor: `share_stat` is
    // the only thing that decides whether there is a number, and it decides it
    // in one place for both this preview and the mail itself.
    let since = chrono::Utc::now() - chrono::Duration::days(STAT_WINDOW_DAYS);
    let rate = store_call(&state, move |store, account_id| {
        store.share_open_rate(account_id, since)
    })
    .await?;
    let open_percent = share_stat(&rate, chrono::Utc::now());
    // THE MAIL ITSELF, so the app can show it before anyone presses send.
    // Rendered by the same function that renders the real one, from a sample
    // code, because a preview the client wrote in its own words would drift
    // from what actually goes out - and this mail goes out over the user's own
    // name, from their own address, which is exactly the case where they are
    // owed a look at it first.
    //
    // NO NOTE: theirs is not typed yet. It is inserted verbatim at the top of
    // this text (see `invite_mail::text_body`), which is where the sheet shows
    // it, so the two agree without the note having to make a round trip per
    // keystroke.
    let preview = invite_mail::text_body(&InviteCopy {
        code: PREVIEW_CODE,
        signup_url: PREVIEW_SIGNUP_URL,
        expires_in_days: PREVIEW_EXPIRY_DAYS,
        open_percent,
        note: None,
    });
    Ok(Json(json!({
        "can_share": true,
        "open_percent": open_percent,
        "preview": preview,
    })))
}

/// What the preview stands in with. Shaped like a real code so the block is the
/// right size on screen, and obviously not one: `XXXX` is not in the Crockford
/// alphabet, so this can never be mistaken for something to type in.
const PREVIEW_CODE: &str = "XXXX-XXXX-XXXX-XXXX";

/// And where it would be spent. The real one comes from the control plane per
/// mint; this is the deployment everybody who can see this screen is on.
const PREVIEW_SIGNUP_URL: &str = "https://signup.passband.app";

/// The expiry the copy quotes in a preview. The real one is whatever the
/// control plane stamps on the code it mints; this matches its default, and a
/// preview being a day out is not a promise anybody acts on.
const PREVIEW_EXPIRY_DAYS: i64 = 30;

/// `POST /client/invites` - mint a code per friend and mail each of them from
/// the user's own mailbox.
///
/// SERIAL, AND EACH ONE IS INDEPENDENT. A mint or a send that fails stops that
/// recipient and nobody else, and the response says which. The alternative -
/// all or nothing - would either burn codes nobody received or refuse the whole
/// press because one address was wrong.
///
/// EVERY REFUSAL THAT COSTS NOTHING HAPPENS FIRST: the capability, the
/// recipient list, the note, and the write credential are all checked before a
/// single code is minted, because a minted code that never reaches anybody is
/// spent quota the user cannot get back.
pub async fn post_invites(
    State(state): State<ApiState>,
    Json(body): Json<ShareBody>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(sharing) = state.sharing().cloned() else {
        audit_action(&state, "invite", None, "rejected:not_configured").await;
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "this daemon is not set up to send invites",
        ));
    };

    let recipients: Vec<String> = body
        .recipients
        .iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();
    if recipients.is_empty() {
        return Err(ApiError::bad_request(
            "invite requires at least one address",
        ));
    }
    if recipients.len() > MAX_RECIPIENTS {
        return Err(ApiError::bad_request(format!(
            "invite reaches at most {MAX_RECIPIENTS} people at a time"
        )));
    }
    for r in &recipients {
        if !is_address(r) {
            // The address is NOT echoed into the error: it goes in an audit
            // line as a count and nowhere else.
            return Err(ApiError::bad_request("that is not an email address"));
        }
    }
    let note = body
        .note
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    if note.as_ref().is_some_and(|n| n.chars().count() > MAX_NOTE) {
        return Err(ApiError::bad_request(format!(
            "a note is at most {MAX_NOTE} characters"
        )));
    }
    // THE SAME OUTBOUND GUARD EVERY OTHER SEND PASSES. The note is free text
    // the user typed, and this send is no less real than the ones they compose
    // by hand; a pasted API key on its way out is exactly what the guard is
    // for. Kinds only, never the matched text.
    if let Some(note) = note.as_deref() {
        let matches = crate::guard::scan_kinds(note);
        if !matches.is_empty() {
            audit_action(&state, "invite", None, "rejected:guard").await;
            return Err(ApiError::bad_request(
                "that note looks like it has a secret in it",
            ));
        }
    }

    // Before any mint: a code minted with no way to mail it is spent quota.
    let client = crate::handlers::write_client(&state)?;

    let since = chrono::Utc::now() - chrono::Duration::days(STAT_WINDOW_DAYS);
    let rate = store_call(&state, move |store, account_id| {
        store.share_open_rate(account_id, since)
    })
    .await?;
    let open_percent = share_stat(&rate, chrono::Utc::now());

    let mut results = Vec::with_capacity(recipients.len());
    let mut remaining: Option<i64> = None;
    let mut queue = recipients.into_iter();
    while let Some(email) = queue.next() {
        let minted = match sharing.mint().await {
            Ok(m) => m,
            Err(e) => {
                let reason = e.to_string();
                // A quota refusal ends the press: every remaining recipient
                // would get the same answer, and asking again per address is
                // work nobody benefits from.
                //
                // THEY STILL GET A ROW EACH. Stopping the loop without one
                // would hand back three results for a press of five, and the
                // two missing names would read as "sent" to anybody skimming.
                let fatal = matches!(e, ShareError::QuotaExhausted);
                audit_action(&state, "invite", None, "failed:mint").await;
                results.push(ShareResult {
                    email,
                    sent: false,
                    error: Some(reason.clone()),
                });
                if fatal {
                    results.extend(queue.map(|email| ShareResult {
                        email,
                        sent: false,
                        error: Some(reason.clone()),
                    }));
                    break;
                }
                continue;
            }
        };
        remaining = Some(minted.remaining);

        let copy = InviteCopy {
            code: &minted.code,
            signup_url: &minted.signup_url,
            expires_in_days: days_until(&minted.expires_at),
            open_percent,
            note: note.as_deref(),
        };
        let parts = invite_mail::compose(&email, &copy);
        let raw = match crate::gmail_write::build_reply_rfc822(&parts) {
            Ok(raw) => raw,
            Err(_) => {
                audit_action(&state, "invite", None, "failed:compose").await;
                results.push(ShareResult {
                    email,
                    sent: false,
                    error: Some("that invite could not be composed".into()),
                });
                continue;
            }
        };
        // A COLD SEND: no thread to join. See `invite_mail::compose`.
        match client.send(&raw, None).await {
            Ok(_) => {
                audit_action(&state, "invite", None, "ok").await;
                results.push(ShareResult {
                    email,
                    sent: true,
                    error: None,
                });
            }
            Err(_) => {
                // The code is minted and unspendable by anyone: nobody has it.
                // It lapses on its own, and the quota is what it cost.
                audit_action(&state, "invite", None, "failed:gmail").await;
                results.push(ShareResult {
                    email,
                    sent: false,
                    error: Some("that invite could not be sent".into()),
                });
            }
        }
    }

    Ok(Json(json!({
        "results": results,
        "remaining": remaining,
    })))
}

/// Whole days from now until an RFC3339 stamp, floored at one.
///
/// The copy says "one use, N days", so N has to be what a person would say
/// looking at a calendar. A code that expires in 29 hours is one day, not zero,
/// and a stamp that will not parse falls back to nothing rather than to a
/// number the mail would state as fact.
fn days_until(expires_at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(expires_at)
        .map(|t| {
            (t.with_timezone(&chrono::Utc) - chrono::Utc::now())
                .num_days()
                .max(1)
        })
        .unwrap_or(1)
}

/// The shape of an address, bounded. Deliberately not a grammar: the daemon
/// does not decide what Gmail will accept, and this only exists to refuse the
/// obviously-not-an-address before spending a code on it.
fn is_address(s: &str) -> bool {
    let bytes = s.as_bytes();
    (3..=254).contains(&bytes.len())
        && s.bytes().all(|b| b.is_ascii_graphic())
        && matches!(s.split('@').collect::<Vec<_>>().as_slice(), [local, domain]
            if !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
                && !domain.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn rate(received: u64, opened: u64, days: i64) -> OpenRate {
        OpenRate {
            received,
            opened,
            oldest_received_at: Some(Utc::now() - Duration::days(days)),
        }
    }

    /// The number a real mailbox produces, rounded to a step and no finer.
    #[test]
    fn rounds_the_rate_to_a_step() {
        let now = Utc::now();
        // 132/1000 is 13.2%, which the reader sees as 15.
        assert_eq!(share_stat(&rate(1000, 132, 90), now), Some(15));
        // Exactly on a step stays there.
        assert_eq!(share_stat(&rate(1000, 100, 90), now), Some(10));
    }

    /// Three refusals, three silences. Every one of them is a mail with no
    /// number in it, never a mail with a bad number.
    #[test]
    fn refuses_a_number_it_cannot_support() {
        let now = Utc::now();
        // Not enough mail, however long the history.
        assert_eq!(share_stat(&rate(MIN_STAT_MESSAGES - 1, 10, 365), now), None);
        // Not enough history, however much mail.
        assert_eq!(share_stat(&rate(5000, 100, MIN_STAT_DAYS - 1), now), None);
        // No history at all: a window with no rows in it.
        assert_eq!(
            share_stat(
                &OpenRate {
                    received: 5000,
                    opened: 10,
                    oldest_received_at: None,
                },
                now
            ),
            None
        );
        // And the anti-testimonial guard: this user opens most of their mail,
        // so the app says nothing rather than saying that.
        assert_eq!(share_stat(&rate(1000, 780, 90), now), None);
    }

    /// The ceiling is applied to the number the READER sees, not to the exact
    /// one: 41% rounds up to 45, which is over the bar.
    #[test]
    fn applies_the_ceiling_after_rounding() {
        let now = Utc::now();
        assert_eq!(share_stat(&rate(1000, 400, 90), now), Some(40));
        assert_eq!(share_stat(&rate(1000, 410, 90), now), None);
    }

    /// Nobody opens literally none of their mail, and a mail claiming it reads
    /// as broken. One step is the floor.
    #[test]
    fn never_claims_zero() {
        let now = Utc::now();
        assert_eq!(share_stat(&rate(1000, 0, 90), now), Some(STAT_ROUNDING));
        assert_eq!(share_stat(&rate(1000, 1, 90), now), Some(STAT_ROUNDING));
    }

    /// The resolver's absent cases, which are every self-hosted daemon.
    #[test]
    fn no_control_plane_means_no_sharing() {
        assert!(Sharing::new(String::new(), "pbs_token".into()).is_none());
        assert!(Sharing::new("https://signup.passband.app".into(), String::new()).is_none());
        assert!(Sharing::new("  ".into(), "  ".into()).is_none());
        assert!(Sharing::new("https://signup.passband.app/".into(), "pbs_token".into()).is_some());
    }
}
