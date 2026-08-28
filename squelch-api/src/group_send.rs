//! Addressing a send group: resolving one into an audience, and the FAN-OUT —
//! one message per member, which is the mode a `to`/`bcc` blast cannot express.
//!
//! WHY A JOB. A twelve-person fan-out is twelve serial Gmail calls plus twelve
//! echo fetches. The composer's POST budget is 15 seconds and the echo alone is
//! allowed 5 of them, so doing this inline would time out the client while the
//! mail was still going. The route therefore records the whole audience as
//! PENDING, returns, and settles recipients from a background task.
//!
//! Which is also why there is no progress channel here. The `group_sends` row
//! IS the progress: `reached` climbs and `pending` falls as the job walks the
//! list, so the Groups page's ordinary history read watches a send happen
//! without a socket, an event kind, or a second endpoint.
//!
//! ONE GUARD VERDICT PER SEND, matching what `action_send` already promises a
//! forward: the outbound guard scans the body once, before the first message
//! goes, because twelve identical 422s for one composition would read as the
//! first override having been ignored.

use std::time::Duration;

use chrono::Utc;
use squelch_core::store::sqlite::groups::{GroupSendRecipient, GroupSendStatus};
use squelch_core::types::GroupMode;

use crate::error::ApiError;
use crate::gmail_write::{GmailWriteClient, ReplyParts, build_reply_rfc822};
use crate::handlers::{audit_action, store_call};
use crate::state::ApiState;

/// Gap between the messages of one fan-out.
///
/// Gmail's per-user rate limiting is generous next to a list this size, so this
/// is not a quota dance — it is back-pressure on a loop that would otherwise
/// open a hundred connections as fast as the runtime allows, from a daemon that
/// is also serving the client that started it.
const FAN_OUT_GAP: Duration = Duration::from_millis(400);

/// What addressing a group resolves to.
pub(crate) struct GroupAudience {
    pub group_id: i64,
    pub mode: GroupMode,
    /// The membership AS IT IS NOW. Snapshotted here so a group edited midway
    /// through a fan-out cannot change who the rest of it reaches.
    pub addrs: Vec<String>,
}

/// Resolve a `group_id` from a request body into an audience.
///
/// The 404 is the same answer for "no such group" and "not yours", exactly like
/// every other id-addressed route on this door.
pub(crate) async fn resolve(state: &ApiState, group_id: i64) -> Result<GroupAudience, ApiError> {
    let group = store_call(state, move |store, account_id| {
        store.get_send_group(account_id, group_id)
    })
    .await?
    .ok_or_else(ApiError::not_found)?;

    let addrs = store_call(state, move |store, account_id| {
        store.send_group_addrs(account_id, group_id)
    })
    .await?;

    if addrs.is_empty() {
        return Err(ApiError::bad_request("that group has nobody in it yet"));
    }
    Ok(GroupAudience {
        group_id,
        mode: group.mode,
        addrs,
    })
}

/// The prefix a composer uses for an UNRESOLVED group token in a recipient
/// field. It is a client-side draft encoding — `#preseed investors` survives a
/// draft round-trip where a bare id would not — and it must never reach the
/// wire: no emittable address can start with it.
pub(crate) const GROUP_TOKEN_PREFIX: char = '#';

/// Refuse a recipient list still carrying an unresolved group token.
///
/// The failure this exists for is a client that restored a draft naming a group
/// it could not resolve (deleted, or a different account) and sent anyway.
/// Without this the token reaches `parse_addr_list`, which drops it as
/// unemittable — and the send goes out to everyone ELSE on the line, silently
/// missing the audience the user believed they had addressed. Loud is correct
/// here; quiet is the bug.
pub(crate) fn reject_unresolved_tokens(value: &str) -> Result<(), ApiError> {
    for token in value.split(',') {
        let token = token.trim();
        if token.starts_with(GROUP_TOKEN_PREFIX) {
            return Err(ApiError::bad_request(format!(
                "\"{token}\" is a group that could not be resolved; \
                 re-pick it in the composer"
            )));
        }
    }
    Ok(())
}

/// Record a group send whose mail has ALREADY gone as one message — the `to` and
/// `bcc` modes, where the composer expanded the audience itself and the daemon's
/// only job is attribution.
///
/// BEST-EFFORT: the mail is away. A failed bookkeeping write must not surface as
/// a failed send, and the derived history still finds this message by its
/// recipients, so the worst case is a history entry that reads as derived rather
/// than recorded.
pub(crate) async fn record_single(
    state: &ApiState,
    audience: &GroupAudience,
    subject: String,
    message_id: Option<i64>,
) {
    let group_id = audience.group_id;
    let mode = audience.mode;
    let recipients: Vec<GroupSendRecipient> = audience
        .addrs
        .iter()
        .map(|addr| GroupSendRecipient {
            addr: addr.clone(),
            message_id,
            status: GroupSendStatus::Sent,
            error: None,
        })
        .collect();
    let sent_at = Utc::now();
    if store_call(state, move |store, account_id| {
        store.record_group_send(account_id, group_id, &subject, mode, sent_at, &recipients)
    })
    .await
    .is_err()
    {
        eprintln!("squelch-api: group send {group_id} went out but was not recorded");
    }
}

/// Everything one fan-out needs, lifted off the request so the job owns it.
pub(crate) struct FanOut {
    pub audience: GroupAudience,
    pub subject: String,
    pub body: String,
    pub body_html: Option<String>,
    /// Already-minted per-send tracker, when the composer asked for one.
    ///
    /// ONE TOKEN FOR THE WHOLE BATCH, deliberately. Per-recipient tokens would
    /// make opens attributable to individuals, and a fan-out already looks to
    /// each recipient like a personal email — turning it into twelve separately
    /// surveilled ones is a different product decision than "attach a read
    /// receipt", and not one this feature gets to make on the user's behalf.
    pub pixel_url: Option<String>,
}

/// Start a fan-out: record the audience as pending, then settle it from a
/// detached task.
///
/// Returns the `group_sends` id, which is what the client watches. The pending
/// row is written BEFORE the task spawns and before any mail moves: a batch that
/// dies between the two must leave a record of an audience it did not reach, not
/// no record at all.
pub(crate) async fn start(state: &ApiState, plan: FanOut) -> Result<i64, ApiError> {
    let group_id = plan.audience.group_id;
    let subject = plan.subject.clone();
    let pending: Vec<GroupSendRecipient> = plan
        .audience
        .addrs
        .iter()
        .map(|addr| GroupSendRecipient {
            addr: addr.clone(),
            message_id: None,
            status: GroupSendStatus::Pending,
            error: None,
        })
        .collect();
    let sent_at = Utc::now();
    let group_send_id = store_call(state, move |store, account_id| {
        store.record_group_send(
            account_id,
            group_id,
            &subject,
            GroupMode::Individual,
            sent_at,
            &pending,
        )
    })
    .await?;

    audit_action(
        state,
        "send",
        Some(group_id.to_string()),
        &format!("started:fan_out:{}", plan.audience.addrs.len()),
    )
    .await;

    let state = state.clone();
    tokio::spawn(async move { run(state, group_send_id, plan).await });
    Ok(group_send_id)
}

/// Walk the audience, one message each.
///
/// NO EARLY RETURN. Every recipient is attempted even after one fails, because
/// the audience is a list of people and one bad address is not a reason to stop
/// writing to the other eleven. Each outcome is settled as it happens, so a
/// daemon that dies halfway leaves a truthful record of how far it got.
async fn run(state: ApiState, group_send_id: i64, plan: FanOut) {
    let client = match crate::handlers::write_client(&state) {
        Ok(c) => c,
        Err(_) => {
            // The credential was there when the route checked it and is not now.
            // Fail the whole batch loudly rather than leaving it pending forever.
            let _ = store_call(&state, move |store, account_id| {
                store.fail_pending_group_sends(account_id, "write credential unavailable")
            })
            .await;
            return;
        }
    };

    let total = plan.audience.addrs.len();
    let mut sent = 0usize;
    for (i, addr) in plan.audience.addrs.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(FAN_OUT_GAP).await;
        }
        match send_one(&state, &client, &plan, addr).await {
            Ok(message_id) => {
                sent += 1;
                settle(
                    &state,
                    group_send_id,
                    addr,
                    GroupSendStatus::Sent,
                    message_id,
                    None,
                )
                .await;
            }
            Err(reason) => {
                settle(
                    &state,
                    group_send_id,
                    addr,
                    GroupSendStatus::Failed,
                    None,
                    Some(&reason),
                )
                .await;
            }
        }
    }

    // The ledger gets the COUNTS and never the list. A fan-out's audience is
    // already written down in `group_send_recipients`, which is the place to
    // read it from.
    audit_action(
        &state,
        "send",
        Some(plan.audience.group_id.to_string()),
        &format!("ok:fan_out:{sent}/{total}"),
    )
    .await;
}

/// One message to one member. Returns the echoed local message id when the echo
/// landed, or a REDACTED reason on failure — the string is stored and shown, so
/// upstream detail never rides along in it.
async fn send_one(
    state: &ApiState,
    client: &GmailWriteClient,
    plan: &FanOut,
    addr: &str,
) -> Result<Option<i64>, String> {
    let parts = ReplyParts {
        to: addr.to_string(),
        // A fan-out is one-to-one BY CONSTRUCTION. That is the whole reason to
        // pick this mode over bcc, so neither list may ever be populated here.
        cc: None,
        bcc: None,
        subject: plan.subject.clone(),
        body: plan.body.clone(),
        in_reply_to: None,
        references: None,
        body_html: plan.body_html.clone(),
        pixel_url: plan.pixel_url.clone(),
    };
    let raw = build_reply_rfc822(&parts).map_err(|_| "could not compose".to_string())?;
    let sent = client
        .send(&raw, None)
        .await
        .map_err(|_| "gmail refused the message".to_string())?;
    Ok(crate::handlers::echo_sent(state, client, None, &sent).await)
}

/// Write one recipient's outcome. Best-effort in the sense that a failed write
/// cannot un-send the mail — but it is logged, because a pending row that never
/// settles is what makes a finished send look like a stuck one.
async fn settle(
    state: &ApiState,
    group_send_id: i64,
    addr: &str,
    status: GroupSendStatus,
    message_id: Option<i64>,
    error: Option<&str>,
) {
    let addr = addr.to_string();
    let error = error.map(str::to_string);
    if store_call(state, move |store, account_id| {
        store.set_group_send_result(
            account_id,
            group_send_id,
            &addr,
            status,
            message_id,
            error.as_deref(),
        )
    })
    .await
    .is_err()
    {
        eprintln!(
            "squelch-api: group send {group_send_id} could not record one recipient's outcome"
        );
    }
}
