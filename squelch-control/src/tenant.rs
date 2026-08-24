//! The tenant door: `POST /tenant/invite`.
//!
//! One route, one caller, one answer. A provisioned daemon presents its share
//! token (see [`crate::share`]) and gets back ONE invite code to put in a mail
//! its user is sending to a friend. That is the entire surface, and everything
//! about it is shaped by the fact that the credential presented here lives in a
//! Secret on a pod rather than in an operator's head.
//!
//! THIS IS THE FOURTH DOOR, and it is worth naming what makes it different from
//! the three that came before it:
//!
//! - `/signup` and `/waitlist` face STRANGERS. Anybody can post to them, so
//!   they are tight, and their refusals are uniform enough not to answer
//!   questions.
//! - `/admin/*` faces ONE OPERATOR holding a token that can approve, mint, and
//!   mail. It is generous, because there is one of them.
//! - `/tenant/invite` faces EVERY DAEMON WE RUN. It is authenticated like the
//!   admin door and metered like neither, because its callers share egress
//!   addresses (see [`crate::ratelimit::TENANT_REQUESTS_PER_MINUTE`]) and the
//!   limit that matters is per tenant and lives in the database.
//!
//! WHAT DOES NOT CROSS THIS WIRE, and this is the deliberate part: THE
//! RECIPIENT. The daemon knows who its user is mailing, because the daemon is
//! the thing sending the mail. This process does not, does not ask, and has
//! nowhere to put it. An invitee is a stranger who has consented to nothing at
//! all, so their address stays on the machine their friend already trusts, and
//! `invite_codes.invited_by` records only which TENANT a code came from. The
//! same rule this crate already keeps for a waitlist address, one step further
//! out.
//!
//! WHAT COMES BACK IS A LIVE CREDENTIAL. The plaintext code is in this
//! response, which makes this the only route on the service that hands one to a
//! machine. It is bounded the same way the mailed one is: single use, expiring,
//! and counted against a quota its holder cannot raise. Nothing here logs it,
//! and [`InviteMinted`] derives no `Debug`.
//!
//! REFUSALS ARE JSON AND THEY ARE COARSE. The caller is a daemon with a UI
//! behind it, so unlike a stranger-facing page this route may say WHICH of two
//! things went wrong, but only across the line that matters to the user: "your
//! credential is no good" (401, and the daemon shows nothing) versus "you are
//! out of invites this month" (429, and the daemon says so). Every way a token
//! can be bad is one answer, for the reason [`crate::share`] rule 3 gives.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse, response::Response};
use chrono::Utc;
use serde::Serialize;

use crate::{invites, share, state::ControlState, store::Sharer};

/// The error code in a refusal body. Machine-readable, because the reader is a
/// machine; the daemon maps these onto its own copy rather than quoting them.
#[derive(Serialize)]
struct Refusal {
    error: &'static str,
}

/// Wrong, revoked, or belonging to a tenant that is no longer active. ONE
/// answer for all three.
const UNAUTHORIZED: &str = "unauthorized";
/// The tenant is out of invites for this window.
const QUOTA_EXHAUSTED: &str = "quota_exhausted";
/// This deployment has no invite feature at all (no waitlist config, so no
/// expiry policy and no operator behind it).
const UNAVAILABLE: &str = "unavailable";

/// A minted code on its way to one daemon.
///
/// NO `Debug`, for the reason the module header gives: `code` is live
/// credential material for the length of this response.
#[derive(Serialize)]
pub struct InviteMinted {
    /// The plaintext, formatted `XXXX-XXXX-XXXX-XXXX`. Shown once, here.
    code: String,
    /// Where it is redeemed, so the daemon never has to hardcode a URL for a
    /// deployment it might not be part of.
    signup_url: String,
    /// RFC3339, so the mail the daemon writes can say the true number of days
    /// rather than a constant compiled into it months ago.
    expires_at: String,
    /// How many more codes this tenant may mint inside the current window,
    /// AFTER this one. The client turns it into "3 invites left"; it is a
    /// courtesy, and the quota is enforced here regardless of what anyone does
    /// with it.
    remaining: i64,
}

/// `POST /tenant/invite` - mint one invite code for the presenting tenant.
///
/// Every refusal leaves NO ROW BEHIND, so a retry is free. The one thing that
/// happens before the last refusal is the mint itself, and a code that is only
/// a string in this process is not a row; see the comment at that line.
pub async fn invite(state: State<ControlState>, headers: axum::http::HeaderMap) -> Response {
    let State(state) = state;

    // The expiry policy lives in the waitlist config, which is also what says
    // there is an operator behind this deployment at all. Without it there is
    // no invite feature to lend a tenant a piece of.
    if state.config().waitlist.is_none() {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
    }

    let Some(sharer) = authenticate(&state, &headers).await else {
        // PRIVACY: that a mint was refused. Never the token, never a prefix of
        // it, and never which of the three ways it was bad.
        tracing::warn!("share token refused");
        return refuse(StatusCode::UNAUTHORIZED, UNAUTHORIZED);
    };

    // ---- from here on, the half that writes ----
    //
    // THE CODE IS MINTED BEFORE THE QUOTA IS CHECKED, which looks backwards and
    // is not. The quota check and the insert have to be ONE transaction (see
    // [`crate::store::ControlStore::mint_shared_invite`]), so the hash it
    // inserts has to exist before it is called. A code minted and then refused
    // by the quota is a string this process drops on the floor: nothing was
    // written, nobody was told, and the entropy cost is a few dozen bytes.
    let minted = match invites::mint() {
        Ok(m) => m,
        Err(_) => {
            tracing::error!("the system random source failed");
            return refuse(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
        }
    };
    let expires_at = Utc::now() + chrono::Duration::days(invites::DEFAULT_TTL_DAYS);
    let window_start = Utc::now() - chrono::Duration::days(share::QUOTA_WINDOW_DAYS);
    let remaining = match state
        .store()
        .mint_shared_invite(
            sharer.id,
            &minted.code_hash,
            expires_at,
            window_start,
            share::QUOTA_PER_WINDOW,
        )
        .await
    {
        Ok(Some(remaining)) => remaining,
        Ok(None) => {
            // The label may be logged; the mailbox behind it may not.
            tracing::info!(label = %sharer.label, "share quota exhausted");
            return refuse(StatusCode::TOO_MANY_REQUESTS, QUOTA_EXHAUSTED);
        }
        Err(e) => {
            tracing::error!(error = %e, label = %sharer.label, "recording a shared invite failed");
            return refuse(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
        }
    };

    // PRIVACY: the label and a count. Never the code, and never the mailbox
    // this tenant is about to send it to, which this process was never told.
    tracing::info!(label = %sharer.label, remaining, "invite shared");

    (
        StatusCode::OK,
        Json(InviteMinted {
            code: minted.code,
            signup_url: state.config().public_url.clone(),
            expires_at: expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            remaining,
        }),
    )
        .into_response()
}

/// The tenant behind the `Authorization: Bearer` header, or `None`.
///
/// A HASH AND A POINT LOOKUP, never a comparison against a list: the store's
/// unique index does the matching, so this does the same work for a token that
/// is one byte wrong as for one that is pure noise. A missing or malformed
/// header takes the same path as a wrong token, which is why nothing here
/// distinguishes them.
async fn authenticate(state: &ControlState, headers: &axum::http::HeaderMap) -> Option<Sharer> {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))?;
    // Bounded before it is hashed: this is an unauthenticated public route
    // until the line below returns, and hashing a megabyte of header is work a
    // stranger should not be able to ask for. A real token is 47 bytes.
    if presented.is_empty() || presented.len() > MAX_TOKEN_LEN {
        return None;
    }
    match state
        .store()
        .tenant_by_share_token(&share::hash(presented))
        .await
    {
        Ok(found) => found,
        Err(e) => {
            tracing::error!(error = %e, "looking up a share token failed");
            None
        }
    }
}

/// Ceiling on a presented bearer, well above the 47 bytes a real one is. A
/// bound on work done before authentication, not a validity check: a token of
/// a legal length is still refused by the lookup.
const MAX_TOKEN_LEN: usize = 256;

fn refuse(status: StatusCode, error: &'static str) -> Response {
    (status, Json(Refusal { error })).into_response()
}
