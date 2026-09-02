//! Gmail sync: REST + polling. REST, not IMAP — the read-only `gmail.readonly`
//! scope works over REST and IMAP XOAUTH2 rejects it. First run backfills
//! `backfill_days` of INBOX+SENT and records the `historyId`; after that
//! `history.list` polls `messageAdded` under BOTH labels — INBOX for incoming
//! mail, SENT for whatever the user wrote from another client — a 404 falling
//! back to a full catch-up.
//! Tokens, headers and bodies are NEVER logged; ingest seals first (SECURITY.md).

pub mod html;
pub mod ingest;
pub mod notify_lane;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::config::{Config, ResolvedLlm, Stage2Provider};
use crate::credentials::CredentialStore;
use crate::error::{CoreError, Result};
use crate::metrics::{
    GmailErrorKind, NotifyDecision, NotifyLane, RevisitVerdict, Stage1Verdict, Stage2Verdict,
    SyncMetrics,
};
use crate::store::{
    ContactEntry, SPAM_SYNCED_AT_KEY, Stage2CapOverrides, Store, SyncState, TriagedMessage,
};
use crate::sync::ingest::{
    RawFetched, collect_mailboxes, format_recipients, ingest_with_rules, is_robot_address,
};
use crate::triage::events;
use crate::triage::extract::{self, CategoryExtractor, RowAction, banking, marketing, shipments};
use crate::triage::stage1_llm::{self, HEURISTIC_ONLY};
use crate::triage::stage2::{self, ClassifyOutcome, RowContext};
use crate::triage::{NO_BODY_SKIP_MODEL, STALE_SKIP_MODEL, retriage_forced};
use crate::triage::{Stage1RowAction, stage1_sealed_guard, stage2_sealed_guard};
use crate::types::{AccountId, SenderRule, Sensitivity};

/// Gmail REST base for the authenticated user. Fixed; not user-tunable. `pub`
/// so squelch-api's write path targets the same host from one definition.
pub const GMAIL_API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// The INBOX system label. `pub` because the write path archives by removing
/// exactly this label.
pub const LABEL_INBOX: &str = "INBOX";
/// The SENT system label.
const LABEL_SENT: &str = "SENT";
/// The SPAM system label — Gmail's own verdict, which squelch surfaces on its
/// spam page and never re-litigates. `pub` because the write path's "not spam"
/// removes exactly this label (and adds [`LABEL_INBOX`] back).
pub const LABEL_SPAM: &str = "SPAM";

/// The single `sync_state` row key for the REST engine's historyId cursor.
const HISTORY_KEY: &str = "history";

/// `sync_state` row key for the one-time Sent-contacts harvest's done flag
/// (`last_uid >= 1` = complete; absent/0 = redo on next daemon start).
const SENT_CONTACTS_KEY: &str = "sent_contacts";

/// `sync_state` row key for the one-time sent-RECIPIENTS backfill's done flag,
/// with the same semantics as [`SENT_CONTACTS_KEY`]. Distinct from it because
/// the two sweeps fill different columns and either can complete alone.
const SENT_RECIPIENTS_KEY: &str = "sent_recipients";

/// How many rows the recipients backfill claims from the store per batch. Only a
/// memory bound: the pass loops until the queue is empty.
const SENT_RECIPIENTS_BATCH: u32 = 500;

/// `wake_budget.thread_id` sentinel for the per-account-per-day Stage-2 budget.
/// Gmail thread ids are hex, so no real thread can collide with it.
const GLOBAL_BUDGET_KEY: &str = "__global__";

/// Prefix for the per-SENDER-per-day Stage-2 budget key in the same
/// `wake_budget` table (`thread_id = "sender:<addr>"`). Gmail thread ids are
/// hex, so this collides with neither a real thread nor `__global__`.
const SENDER_BUDGET_PREFIX: &str = "sender:";

/// `wake_budget.thread_id` sentinel for the Stage-1 daily budget. Stage-1 must
/// see every email, so a global cap is its only scope; the key is distinct from
/// the Stage-2 sentinel so the two stages' daily counts never collide.
const STAGE1_GLOBAL_BUDGET_KEY: &str = "__stage1_global__";
/// The `wake_budget` sentinel for scheduled re-evaluations. Its OWN key, not
/// Stage-1's: a revisit backlog must not be able to eat the budget that classifies
/// mail arriving today.
const REVISIT_BUDGET_KEY: &str = "__revisit_global__";
/// The usage-ledger category re-evaluation spend books under, so the cost of
/// keeping verdicts fresh is separable from the cost of forming them.
const REVISIT_USAGE_CATEGORY: &str = "revisit";
/// The `wake_budget` sentinel for the NOTIFY FAST LANE (docs/NOTIFY.md §11.5).
/// Its own key beside the other three, and for the sharpest version of the same
/// reason: this lane runs at INGEST, in front of a user waiting for a buzz, so
/// sharing a counter with a pass that grinds a queue would let a backlog spend
/// the notification budget before today's mail ever arrived.
///
/// It is declared HERE, beside the other three, rather than next to the
/// [`notify_lane`] code that spends it: a sentinel invented later, somewhere
/// else, is a sentinel nobody ever checks these for a collision against.
const NOTIFY_FAST_BUDGET_KEY: &str = "__notify_fast__";
/// The usage-ledger category the fast lane books under. NOT a literal here: it
/// is the one category with prices of its own, and both cost estimators price
/// it by matching this exact string, so a second spelling would not be a
/// missing row, it would be a row silently costed at the Stage-1 model's rates
/// (see [`crate::metrics::estimate_cost_usd`]).
const NOTIFY_USAGE_CATEGORY: &str = crate::metrics::NOTIFY_USAGE_CATEGORY;

/// Which sync path an ingest batch is on. Decides ONE thing: whether a
/// notification-worthy verdict may append an `events` row. Backfill never
/// notifies (a fresh install must not fire a hundred pushes for a month of
/// already-read mail); every incremental path may, kept safe by the first-sight
/// test plus the one-event-per-message key even when `catch_up()` re-scans the
/// whole backfill window.
///
/// It is answered ONCE, here at ingest, and stamped on the row as
/// `triage.notify_eligible_at` (see [`notify_eligible_stamp`]) — this is the one
/// site that knows which path it is on, and the emission sites downstream cannot
/// re-derive it from anything the row carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngestOrigin {
    /// First-run backfill. Structurally silent.
    Backfill,
    /// A history walk or a catch-up re-scan: mail that may be genuinely new.
    Incremental,
}

/// One message that [`SyncEngine::ingest_one`] COMMITTED, and everything the
/// two things behind it need: the embedder and the fast lane.
///
/// `embed_text` is `None` for the rows that must never enter the vector space
/// (sealed, provider spam) — a fact about the row, not a fact about whether
/// ingest succeeded. Those were the same `None` until the fast lane needed to
/// see a sealed row (docs/NOTIFY.md §11.6), and conflating them again would
/// silence the sealed ping without touching a line of the lane.
struct Ingested {
    id: i64,
    embed_text: Option<String>,
    /// THE WHOLE TRIAGED ROW, because [`notify_lane::candidate`] is a pure
    /// function of it and the caller is the only place it still exists.
    triaged: TriagedMessage,
}

/// What one call to [`SyncEngine::emit_event`] did. Returned rather than
/// swallowed because the emission sites are where the notify ledger's
/// `deliberate` rows get written (docs/NOTIFY.md §11.7), and a site cannot
/// record what it was not told: the "no event" outcomes are four different
/// facts about the mail, and folding them into `()` is precisely how 24.7% of
/// notify-worthy mail disappeared without a trace.
///
/// [`SyncEngine::record_deliberate`] maps this enum onto §11.4's closed
/// decision vocabulary, one arm each, so the mapping is total and lives in one
/// place rather than at three call sites that could drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emitted {
    /// An `events` row was appended, with this id. -> `sent`.
    New(i64),
    /// Worthy, but this message already had an event: `UNIQUE(message_id)`
    /// means a buzz is never rewritten, so the other lane got there first.
    /// -> `would_send`.
    AlreadyNotified,
    /// No event, and correctly so: sealed, sent, spam, never eligible, or
    /// simply below the line. Silent by design. -> `declined_by_model`, which
    /// is honest at the refine sites specifically: the queues select
    /// `sensitivity = 'normal'`, `is_sent = 0` and `is_spam = 0`, and a row
    /// with no eligibility stamp is filtered out before the ledger, so the only
    /// way a REFINE site reaches this arm is a verdict that scored below the
    /// line.
    NotWorthy,
    /// A standing Squelch/Filtered rule silenced it. -> `suppressed`, which is
    /// NOT rescuable: the user asked for this silence and no later lane may
    /// undo it (docs/NOTIFY.md §10).
    Suppressed,
    /// Worthy and eligible, but past `notify.rescue_window_secs` since we first
    /// saw the message. THE DROP THAT USED TO BE SILENT; ledgered and logged.
    /// -> `expired`.
    Expired,
    /// `append_event` itself failed (a locked WAL, a full disk). A worthy,
    /// in-window notification the user did not get, and the ONLY arm that
    /// writes no ledger row at all.
    ///
    /// It has no decision word because §11.4's vocabulary has none to spare
    /// that would not be a lie: `unavailable` means "no model answer" and is
    /// marked RESCUABLE, so a full disk would read on the §11.11 rollout query
    /// as a model-availability problem AND invite a later lane to retry a row
    /// whose event may in fact exist. The log line above the return is the
    /// whole record, exactly as it was before the ledger existed.
    Failed,
}

/// Whether an `Expired` refusal at this emission site is a MISSED NOTIFICATION
/// (and so earns a `deliberate/expired` ledger row and its counter) or merely a
/// re-reading of old mail.
///
/// The distinction exists because of `retriage_reset`, which nulls the model
/// stamps and sets `triage.retriage_at` while deliberately LEAVING
/// `notify_eligible_at` alone, and because `retriage_forced` then exempts those
/// rows from every pass's stale cutoff. So `retriage_reset(acct, None, 90)` hands
/// the Stage-1 apply site thousands of rows carrying stamps weeks old. The ones
/// that already notified are caught by the `message_has_event` read; the ones
/// that were below the line before and are above it now — which is the usual
/// REASON to run a re-triage, e.g. after lowering `notify.min_importance` — have
/// no event, refuse with `Expired`, and would each add a count. That is one
/// operator action inflating, by thousands, the single number §11.11 says decides
/// whether the window or the model moves.
///
/// The refusal itself is unchanged either way: mail the user read a fortnight ago
/// must not buzz. Only the bookkeeping differs, and for the same reason the code
/// already keeps `Expired` behind the worthiness question — a re-evaluation of old
/// mail is not a notification anybody missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpiryCount {
    /// This site is seeing the row on its ordinary path: an expiry here is a
    /// real miss.
    Miss,
    /// A human forced this row back through triage. Refuse, but do not book it.
    Rereading,
}

impl ExpiryCount {
    /// From a queue row's `triage.retriage_at`, which is the one field that
    /// distinguishes the two.
    fn from_retriage(retriage_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Self {
        if retriage_forced(retriage_at, now) {
            Self::Rereading
        } else {
            Self::Miss
        }
    }
}

/// MAY THIS MESSAGE EVER NOTIFY, and from when — the whole of docs/NOTIFY.md
/// §11.3, in one pure function so the engine and its tests cannot drift.
///
/// `Some(now)` only for mail we are seeing for the first time on an incremental
/// path, that is not the user's own sent copy, and whose sender-claimed `Date:`
/// was still inside `notify.freshness_window_secs` when we laid eyes on it.
/// Everything else is `None`, the silent direction.
///
/// [`events::is_fresh`] is asked HERE and nowhere else. That is the fix for the
/// bug this whole change exists for: asked at every emission site it measured
/// the sender's clock against the wall clock, so mail that WAS fresh when it
/// arrived and simply waited behind a queue of model calls aged out and was
/// dropped, silently, 24.7% of the time (docs/NOTIFY.md §2a). Asked once, it
/// answers the question it was always meant to ask.
fn notify_eligible_stamp(
    triaged: &TriagedMessage,
    origin: IngestOrigin,
    cfg: &crate::config::NotifyConfig,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let eligible = origin == IngestOrigin::Incremental
        && !triaged.message.is_sent
        && events::is_fresh(triaged.message.received_at, cfg, now);
    eligible.then_some(now)
}

/// WHICH GMAIL LABEL a fetch batch came from, and therefore how the rows land.
///
/// An enum rather than the pair of booleans it replaces. Two bools can spell
/// four states, only three of which mean anything, and the two that get passed
/// most often differ from each other by argument ORDER alone — the exact shape
/// of mistake that puts inbound mail in the sent bucket and makes it vanish from
/// every listing. One argument cannot be transposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mailbox {
    /// Ordinary received mail: the only batch that is triaged.
    Inbox,
    /// The user's own outbox. Neutral row, no LLM, seeds contacts.
    Sent,
    /// Gmail's SPAM label. Neutral row, no LLM, hidden from every surface but
    /// the human door's spam page.
    Spam,
}

impl Mailbox {
    fn is_sent(self) -> bool {
        matches!(self, Mailbox::Sent)
    }
    fn is_spam(self) -> bool {
        matches!(self, Mailbox::Spam)
    }
}

/// The SHIPMENTS extractor's own stale cutoff, from `carriers.max_age_days`.
///
/// Deliberately NOT [`PassSetup::stale_cutoff`], which every other pass shares:
/// that one is the Stage-2 max age (a week), and a week-old ceiling would
/// stale-skip most of the backfill the shipments trigger exists to catch — a
/// package ordered three weeks ago is still in flight, and the carrier poller
/// tracks it for `carriers.max_age_days`. ONE horizon for the whole shipments
/// feature, so a row the poller would still chase can never have been skipped
/// unread by the extractor.
fn ship_stale_cutoff(now: DateTime<Utc>, carrier_max_age_days: u32) -> DateTime<Utc> {
    now - ChronoDuration::days(carrier_max_age_days as i64)
}

/// Reconnect / retry backoff bounds for the outer driver loop.
const BACKOFF_START: Duration = Duration::from_secs(2);
const BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);

/// The longest the FIRST backfill waits on the embedder gate before going ahead
/// without it. The ceiling is not a deadline anybody is meant to hit, it is the
/// guarantee that a gate nobody ever opens (an init task wedged on a hung
/// download) degrades to exactly today's behaviour, an unembedded backfill the
/// vector pass drains later, rather than to a daemon that never syncs at all.
/// See [`SyncEngine::with_embedder_gate`].
///
/// THREE MINUTES, and short on purpose, because the wait is time a brand-new
/// tenant's mailbox is EMPTY: nothing is on the wire until the backfill runs.
/// A cached model loads in about a second and a cold download of the 126 MB
/// model takes tens of seconds on a normal link, so past this the init is not
/// coming back. Hosted makes that cost concrete today: readiness there is
/// TCP-only and there is no models volume yet, so every new signup's first run
/// IS the cold download, and a generous ceiling would show them an empty
/// mailbox for all of it. 15 minutes, the first cut of this, was also exactly
/// the documented sync-staleness alert threshold (900 s, see
/// `deploy/monitoring/README.md`), which would park every first-run tenant on
/// the alert line for the duration.
///
/// The trade against releasing early is bounded now: a backfill that starts
/// unembedded costs one vector pass at `embed.backfill_batch`, about +123 MB
/// at a batch of 8 under the shipped 256-token `embed.max_tokens` (+324 MB was
/// the same batch at the model's 512-token ceiling, and +1.7 GB was a batch of
/// 64 there). It is also paid AGAIN per retry, because a backfill that errors
/// before the cursor is stored comes back through here on the next lifecycle.
const EMBEDDER_GATE_CEILING: Duration = Duration::from_secs(3 * 60);

/// How a wait on the embedder gate ended. Every variant but `Settled` says the
/// same thing to a caller ("nothing more is coming, go ahead"); they are apart
/// because they read differently in a log and only one of them is worth a
/// metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderGate {
    /// The embedder init resolved, whichever way it resolved.
    Settled,
    /// The sender is gone, so nothing is ever going to open this gate.
    Dropped,
    /// [`EMBEDDER_GATE_CEILING`] passed first.
    TimedOut,
}

/// Park until the embedder gate opens, the ceiling passes, or whoever would have
/// opened it goes away. Returns at once on a gate that is already open, so the
/// steady state (a restart, model on disk) pays nothing.
///
/// Public because TWO callers wait on this one bit for two different reasons.
/// The first backfill waits for memory ([`SyncEngine::with_embedder_gate`]). The
/// daemon's one-time Sent sweeps wait for Gmail quota: their stagger is meant to
/// sit past the startup sync burst, and the gate can now push the start of that
/// burst minutes out, which would leave a metadata GET per Sent message racing a
/// 30-day raw backfill on one credential.
pub async fn wait_for_embedder_gate(gate: &mut tokio::sync::watch::Receiver<bool>) -> EmbedderGate {
    let deadline = tokio::time::Instant::now() + EMBEDDER_GATE_CEILING;
    wait_for_embedder_gate_until(gate, deadline).await
}

/// [`wait_for_embedder_gate`] against a FIXED deadline, for a caller that may
/// have to park more than once (the sync engine re-enters after a shutdown
/// wakeup that carried no shutdown). The deadline is computed once by that
/// caller, so re-entering cannot re-arm the ceiling and turn a bounded wait
/// into an unbounded one.
pub async fn wait_for_embedder_gate_until(
    gate: &mut tokio::sync::watch::Receiver<bool>,
    deadline: tokio::time::Instant,
) -> EmbedderGate {
    if *gate.borrow() {
        return EmbedderGate::Settled;
    }
    tokio::select! {
        settled = gate.wait_for(|settled| *settled) => match settled {
            Ok(_) => EmbedderGate::Settled,
            Err(_) => EmbedderGate::Dropped,
        },
        _ = tokio::time::sleep_until(deadline) => EmbedderGate::TimedOut,
    }
}

/// Collapse an untrusted header-derived string to printable ASCII before it
/// reaches the log: control chars, ANSI escapes and log-forging newlines become
/// `.`, and the result is capped so a pathological header can't flood the log.
fn sanitize_ascii(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '.'
            }
        })
        .take(max)
        .collect()
}

/// A stable, non-reversible tag (`sender#<12 hex of sha256>`) for a sender
/// address. `from_addr` is untrusted header-derived PII and must never be
/// logged; the tag still correlates repeated notices for the same sender.
fn redact_sender(from_addr: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(from_addr.as_bytes());
    let mut hex = String::with_capacity(12);
    for b in digest.iter().take(6) {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("sender#{hex}")
}

/// Decode a base64url (Gmail `format=raw`) payload into RFC822 bytes. Gmail
/// usually omits padding; both padded and unpadded input are accepted. Errors
/// are surfaced without content so one bad message can't poison the batch.
pub fn decode_raw_b64url(s: &str) -> Result<Vec<u8>> {
    // MEMORY GUARD: bound peak ingest memory ourselves rather than inheriting
    // Gmail's ~50MB limit. b64 length upper-bounds the decoded size.
    const MAX_RAW_BYTES: usize = 64 * 1024 * 1024;
    let t = s.trim();
    if t.len() / 4 * 3 > MAX_RAW_BYTES {
        return Err(CoreError::InvalidInput(
            "raw message exceeds the 64MB ingest bound".to_string(),
        ));
    }
    // Try no-pad first (Gmail's usual shape), then the padded variant.
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(t)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(t))
        .map_err(|e| CoreError::InvalidInput(format!("base64url decode failed: {e}")))
}

/// Decide whether the incremental poll can proceed or a fresh catch-up is
/// required. `expired` reflects an HTTP 404 from `history.list` (Gmail drops
/// history older than ~a week); a 0/absent `cursor` means first run. Pure, so
/// the 404-fallback path is unit-testable without a network.
pub fn history_poll_decision(cursor: Option<u64>, expired: bool) -> HistoryDecision {
    match cursor {
        Some(id) if id > 0 && !expired => HistoryDecision::Incremental(id),
        _ => HistoryDecision::FullCatchUp,
    }
}

/// The outcome of [`history_poll_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryDecision {
    /// Poll `history.list` starting from this historyId.
    Incremental(u64),
    /// historyId is absent or expired: do a fresh backfill-window catch-up.
    FullCatchUp,
}

/// Advance a historyId cursor to the max of itself and every `historyId`
/// observed in a page — never backwards. Pure, so it is unit-testable.
pub fn advance_history_cursor(current: u64, observed: impl IntoIterator<Item = u64>) -> u64 {
    observed.into_iter().fold(current, u64::max)
}

/// Drop from `ids` every id also in `claimed`, preserving order. Used to hand
/// each later label walk only what the earlier ones did NOT already ingest: a
/// self-addressed message carries both INBOX and SENT, and re-ingesting a
/// message under a hiding label (`is_sent`, `is_spam`) would take the visible
/// copy off every listing. Pure, so the precedence rule is unit-testable.
fn subtract_ids(ids: Vec<String>, claimed: &[String]) -> Vec<String> {
    if claimed.is_empty() {
        return ids;
    }
    let claimed: std::collections::HashSet<&str> = claimed.iter().map(|s| s.as_str()).collect();
    ids.into_iter()
        .filter(|id| !claimed.contains(id.as_str()))
        .collect()
}

// ---- Gmail REST response shapes (only the fields we consume) ---------------
// These model the GMAIL API's own JSON, never squelch's client-facing wire
// contracts. squelch-api's write path deserializes the same Gmail resources, so
// the shared ones are `pub` here and defined exactly once.

/// Clears the catch-up progress pair on the way out of `Syncer::catch_up`, on
/// every path including the `?` returns.
///
/// A guard rather than a call at the end, because the end is the one place a
/// catch-up reliably does NOT reach: it is a long walk over a network, and a
/// Gmail hiccup halfway through leaves by `?`. A pair left standing then would
/// report a run that is not happening, which is a worse lie than the silence
/// this replaced.
struct CatchUpGuard<'a>(&'a crate::metrics::SyncMetrics);

impl Drop for CatchUpGuard<'_> {
    fn drop(&mut self) {
        self.0.catchup_end();
    }
}

#[derive(Debug, Deserialize)]
struct MessageRef {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListMessagesResp {
    #[serde(default)]
    messages: Vec<MessageRef>,
    #[serde(default)]
    next_page_token: Option<String>,
}

/// A Gmail `users.messages.get` resource, across every format squelch asks for:
/// `format=raw` fills `raw`, `format=metadata` fills `payload.headers`, and a
/// field the requested format omits simply stays `None`/empty.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    /// base64url of the full RFC822 message (present with `format=raw`).
    #[serde(default)]
    pub raw: Option<String>,
    /// Milliseconds since epoch as a decimal string (Gmail's `internalDate`).
    #[serde(default)]
    pub internal_date: Option<String>,
    /// MIME structure (present with `format=metadata`/`full`); squelch reads
    /// only its headers.
    #[serde(default)]
    pub payload: Option<MessagePayload>,
}

/// The `payload` object of a Gmail message; only `headers` is consumed.
#[derive(Debug, Default, Deserialize)]
pub struct MessagePayload {
    #[serde(default)]
    pub headers: Vec<MessageHeader>,
}

/// A single Gmail `payload.headers[]` entry. Also what the contacts-seeding
/// tests build to exercise header parsing via `synthesize_rfc822_headers`.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileResp {
    history_id: String,
}

/// A Gmail `users.labels.get` resource; only the unread pair is read, and both
/// counters default to 0 because Gmail omits them on a label with nothing in it.
///
/// `id` is required for exactly that reason: with only defaulted fields, ANY
/// 200 body — `{}`, an error envelope, some proxy's interstitial — would decode
/// to a confident `(0, 0)` and overwrite real counts. Requiring the one field
/// every label resource carries turns those bodies into decode errors, which
/// the caller keeps its last known counts through.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelResp {
    /// Never read; its presence is the check.
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    messages_unread: i64,
    #[serde(default)]
    threads_unread: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryListResp {
    #[serde(default)]
    history: Vec<HistoryRecord>,
    #[serde(default)]
    next_page_token: Option<String>,
    /// The newest historyId as of this response.
    #[serde(default)]
    history_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryRecord {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    messages_added: Vec<HistoryMessageAdded>,
}

#[derive(Debug, Deserialize)]
struct HistoryMessageAdded {
    message: MessageRef,
}

/// Parse a decimal string historyId; malformed input yields 0 (treated as
/// "unknown", forcing a full catch-up rather than a panic).
fn parse_history_id(s: &str) -> u64 {
    s.trim().parse::<u64>().unwrap_or(0)
}

/// Which daily cap a budget-exhausted notice is about; each is rate-limited to
/// once per UTC day (see [`SyncEngine::warn_days`]).
#[derive(Debug, Clone, Copy)]
enum CapKind {
    Thread,
    Sender,
    Global,
    Stage1Global,
    Revisit,
    /// The notify fast lane's daily cap ([`NOTIFY_FAST_BUDGET_KEY`]). Its own
    /// kind, not Revisit's, or a capped fast lane would go unmentioned on any
    /// day a revisit notice had already been logged.
    NotifyFast,
    /// The notify fast lane's CONFIG-FAILURE park (see [`notify_lane`]).
    ///
    /// A SEPARATE SLOT FROM `NotifyFast`, though both are about the same lane
    /// and both are worth saying once a day rather than once a message. They
    /// are the only two diagnoses for "the fast lane stopped notifying", they
    /// are unrelated faults, and sharing one slot means whichever fires first
    /// on a UTC day silences the other for the rest of it: a mailbox that
    /// exhausts the cap at 09:00 would swallow the 14:00 line naming a broken
    /// allow-list, on the day somebody is reading the log precisely because
    /// notifications stopped. The [[fleet LLM outage]] was a config-level 400
    /// that read as normal for four days.
    NotifyFastConfig,
}

/// The preamble every LLM pass shares: resolved credentials, runtime cap
/// overrides, and the pass clock. `stale_cutoff` is deliberately the Stage-2
/// max-age for every pass, so all stages age rows out together.
struct PassSetup<'a> {
    api_key: &'a str,
    provider: Stage2Provider,
    /// The endpoint every classify call this pass makes posts to — resolved
    /// once at startup, gateway override already folded in.
    url: &'a str,
    caps: Stage2CapOverrides,
    /// UTC date key (`YYYY-MM-DD`) for the budget rows; one value per pass.
    day: String,
    stale_cutoff: DateTime<Utc>,
}

/// Outcome of a check-then-increment budget gate.
enum BudgetGate {
    Proceed,
    /// Cap hit: every remaining row this cycle is blocked.
    Exhausted,
    /// Budget read or increment failed: skip this row, try the next.
    SkipRow,
}

/// The last UTC day (`YYYY-MM-DD`) each cap kind's notice was emitted; re-armed
/// when the day rolls over, so a capped account logs once a day, not per poll.
#[derive(Default)]
struct WarnDays {
    thread: Option<String>,
    sender: Option<String>,
    global: Option<String>,
    stage1_global: Option<String>,
    revisit: Option<String>,
    notify_fast: Option<String>,
    notify_fast_config: Option<String>,
}

/// The account's daily-budget ledger plus the warn-once state that goes with it:
/// everything a cap gate needs, and nothing else.
///
/// IT IS A STRUCT RATHER THAN THREE METHODS ON [`SyncEngine`] because the fast
/// lane (docs/NOTIFY.md §11.5) is its own `Arc`-held object rather than a method
/// on the engine, and it has to gate on the same code AND — the part a second
/// copy would quietly break — the same [`WarnDays`]. "Once per UTC day per cap
/// kind" is a promise about a shared slot; two `WarnDays` would make it twice,
/// on the day somebody is reading the log because notifications stopped.
///
/// Borrowed, so neither holder pays for a clone to ask a question, and the
/// engine's own three methods below are thin wrappers over it so every existing
/// call site is unchanged.
struct BudgetLedger<'a, S: Store + ?Sized> {
    store: &'a S,
    account_id: AccountId,
    warn_days: &'a std::sync::Mutex<WarnDays>,
}

impl<S: Store + ?Sized> BudgetLedger<'_, S> {
    /// True at most once per UTC `day` per cap `kind`, so a persistently-capped
    /// account logs each notice once a day rather than every poll. Stamps the
    /// day as a side effect; a poisoned lock defaults to warning, never to
    /// silently swallowing the notice.
    fn warn_once(&self, kind: CapKind, day: &str) -> bool {
        let mut guard = match self.warn_days.lock() {
            Ok(g) => g,
            Err(_) => return true,
        };
        let slot = match kind {
            CapKind::Thread => &mut guard.thread,
            CapKind::Sender => &mut guard.sender,
            CapKind::Global => &mut guard.global,
            CapKind::Stage1Global => &mut guard.stage1_global,
            CapKind::Revisit => &mut guard.revisit,
            CapKind::NotifyFast => &mut guard.notify_fast,
            CapKind::NotifyFastConfig => &mut guard.notify_fast_config,
        };
        if slot.as_deref() == Some(day) {
            false
        } else {
            *slot = Some(day.to_string());
            true
        }
    }

    /// An account-scoped daily budget gate over one `wake_budget` sentinel key.
    /// INCREMENT-BEFORE-CALL, so a retry storm cannot exceed the cap: a call
    /// that is about to be made is charged whether or not it comes back.
    fn gate(
        &self,
        key: &str,
        day: &str,
        cap: u32,
        kind: CapKind,
        label: &str,
        tail: &str,
    ) -> BudgetGate {
        match self.store.stage2_budget_used(self.account_id, key, day) {
            Ok(used) if used >= cap => {
                if self.warn_once(kind, day) {
                    eprintln!(
                        "squelch: {label} daily budget exhausted ({used}/{cap}); {tail} stay queued"
                    );
                }
                return BudgetGate::Exhausted;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("squelch: {label} budget read failed ({e}); skipping row");
                return BudgetGate::SkipRow;
            }
        }
        if let Err(e) = self
            .store
            .stage2_increment_budget(self.account_id, key, day)
        {
            eprintln!("squelch: {label} budget increment failed ({e}); skipping row");
            return BudgetGate::SkipRow;
        }
        BudgetGate::Proceed
    }

    /// Give back the charge for a call that was rejected at CONFIG level.
    ///
    /// The charge-before-call rule above is a retry-storm guard and it stays.
    /// What it must not do is let a broken config spend the day's cap on 4xxs
    /// that cost nothing: those are rejected in ~0ms, spend no tokens, and are
    /// identical for every queued row, so a handful of cycles can exhaust a
    /// 500-call budget. Since the pass also STOPS on a config failure and
    /// leaves its rows queued, the un-refunded charge outlives the outage — the
    /// budget key is the UTC DAY, so a gateway fixed at noon stays capped until
    /// midnight UTC. Refunding here is what makes "the outage is over" and "the
    /// fleet is triaging again" the same moment.
    ///
    /// Best-effort by design: a refund that fails must never turn a config
    /// failure into a second, louder failure. The worst case is the behaviour
    /// this method exists to fix, which is where we already were.
    fn refund(&self, key: &str, day: &str, label: &str) {
        if let Err(e) = self.store.stage2_refund_budget(self.account_id, key, day) {
            eprintln!("squelch: {label} budget refund failed ({e}); the cap keeps the charge");
        }
    }
}

/// Everything the sync loop needs, resolved once at startup.
pub struct SyncEngine<S: Store, C: CredentialStore + ?Sized> {
    store: Arc<S>,
    creds: Arc<C>,
    account_id: AccountId,
    /// The account's own email; passed to ingest so the user's own address is
    /// excluded from the Sent-derived contacts table.
    account_email: String,
    config: Config,
    http: reqwest::Client,
    /// LLM key + provider + endpoint URL, resolved once at startup. `None`
    /// disables LLM triage gracefully: rows stay queued, one stderr notice,
    /// sync continues. The key is never logged.
    stage2_llm: Option<ResolvedLlm>,
    /// Embedder OVERRIDE; usually `None`, with [`SyncEngine::embedder`] falling
    /// back to the store's. Resolving per tick is what lets a LATE-attached
    /// embedder be picked up without a restart.
    embedder: Option<Arc<dyn crate::embed::Embedder>>,
    /// "The embedder init has RESOLVED, whichever way it resolved", flipped by
    /// the caller that builds the embedder in the background. The FIRST backfill
    /// waits on it and nothing else does. `None` means never wait: sync-only
    /// mode builds its embedder before the engine exists, and the tests have
    /// nothing to wait for. See [`SyncEngine::with_embedder_gate`].
    embedder_gate: Option<tokio::sync::watch::Receiver<bool>>,
    /// Manual-refresh signal: notifying it wakes the sleeping poll loop early.
    /// Coalescing is intentional — several pokes during one in-flight poll
    /// collapse into a single extra tick.
    refresh: Arc<tokio::sync::Notify>,
    /// POLL LANE -> REFINE LANE: poked once per poll tick that actually ingested
    /// something, so new mail starts through Stage-1 the moment it lands instead
    /// of waiting out whatever the refine lane is in the middle of.
    ///
    /// Not `Arc` and not shared outside the engine, because unlike `refresh`
    /// nothing outside the engine may poke it: it means "I just ingested rows",
    /// which is a statement of fact about this engine's own tick. Coalescing is
    /// what makes a busy tick cheap — `Notify` holds one permit, so twenty
    /// pokes during one grind are one extra pass.
    ///
    /// Poked by [`SyncEngine::fetch_raw_and_ingest`] and awaited by
    /// [`SyncEngine::refine_lane`]; see [`SyncEngine::run_lanes`] for why the
    /// two are separate futures at all.
    refine_wake: tokio::sync::Notify,
    /// ON-DEMAND SPAM SIGNAL: notifying it makes the poll loop run ONE spam
    /// window sync on its next pass. Separate from `refresh` because it is a
    /// different request — `refresh` means "check my mail now", this means "go
    /// and get the folder we deliberately do not track" — and because
    /// collapsing them would put the spam fetch back on every manual refresh,
    /// which is most of what this change removed.
    spam_refresh: Arc<tokio::sync::Notify>,
    /// Per-cap-kind last-warned UTC day. In-memory only; a restart re-arms them,
    /// and one fresh notice on restart is acceptable.
    ///
    /// `Arc` because the FAST LANE shares this exact mutex rather than keeping a
    /// second one; see [`BudgetLedger`] for why that matters.
    warn_days: Arc<std::sync::Mutex<WarnDays>>,
    /// THE FAST LANE (docs/NOTIFY.md §11.5), built ON FIRST USE rather than in
    /// [`SyncEngine::new`].
    ///
    /// The lane holds CLONES of what the engine resolved — the metrics registry,
    /// the LLM — and two builder methods replace exactly those after `new`
    /// returns: `with_metrics` (which every production daemon calls, and whose
    /// whole purpose is that the engine records into the registry `/metrics`
    /// serves) and `without_stage2_llm`. A lane built in `new` would therefore
    /// count into an orphan registry in production and run the model path in the
    /// tests that exist to prove the no-model path. `OnceLock` makes the
    /// builders' order irrelevant instead of making every future builder
    /// remember to rebuild it, and it is safe because both builders take
    /// `mut self` and so can only run before the engine moves into `run()`.
    ///
    /// RECORDED as the fifth deliberate departure in docs/NOTIFY.md §11.9, so
    /// a later reader diffing the code against the contract finds it named
    /// rather than re-deriving whether it was on purpose.
    notify_lane: std::sync::OnceLock<Arc<notify_lane::NotifyLane<S>>>,
    /// Set while the INBOX unread fetch is failing, so a persistent failure
    /// (revoked scope, Gmail outage) says so ONCE instead of once per poll. The
    /// next success re-arms it.
    unread_warned: AtomicBool,
    /// HAS A POLL EVER SUCCEEDED IN THIS PROCESS, and has one succeeded since the
    /// last one failed. Set by [`SyncEngine::poll_lane`] the moment `poll_once`
    /// returns `Ok`; cleared by [`SyncEngine::run`] when a lifecycle bubbles an
    /// `Err`. Read by [`SyncEngine::refine_lane`], which spends no model call
    /// while it is false.
    ///
    /// THIS EXISTS BECAUSE A CANCELLED MODEL CALL IS STILL BILLED. The lane split
    /// made the refine lane a sibling of the poll lane under one `select!`, so a
    /// Gmail `Err` now drops a refine future that may be mid-classify, and
    /// [`SyncEngine::gate_budget`] charges BEFORE the call (that is what makes the
    /// cap a cap) while nothing refunds a cancellation. `run()`'s backoff is
    /// initialised outside its loop and caps at [`BACKOFF_CAP`], so a credential
    /// that has gone `invalid_grant` retries every five minutes forever: without
    /// this gate, each cycle re-entered `run_lanes`, the refine lane ran
    /// synchronously through `gate_budget` and issued a POST before `poll_lane`
    /// reached its first real await, and the whole thing was cancelled one Gmail
    /// round trip later. `stage2.global_daily_cap` defaults to 120 and the budget
    /// key is the UTC DAY, so ten hours of Gmail downtime drained the entire day's
    /// Stage-2 budget on verdicts that never landed, and nothing escalated again
    /// until midnight even after the credential was fixed.
    ///
    /// FALSE AT STARTUP, deliberately, so not even the first cycle can spend on a
    /// dead credential: the cost is that the refine lane waits out one poll
    /// interval on a healthy daemon before its first steady-state round, which
    /// `run_once` has already covered on a first run (it runs the passes inline)
    /// and which is `sync.poll_secs` — 5 seconds by default — on every other.
    ///
    /// It restores exactly what the serial loop gave for free: `poll_once().await?`
    /// returned before any pass started, so a Gmail outage cost zero model calls.
    /// `Relaxed` throughout: this is a hint that costs at most one pass either way,
    /// never a lock.
    poll_healthy: AtomicBool,
    /// Scrape-facing counters. Its own registry unless the daemon shares one
    /// via [`SyncEngine::with_metrics`], so an engine built anywhere (tests, the
    /// contacts harvest, `run`) still records rather than branching on absence.
    metrics: Arc<SyncMetrics>,
    /// Gmail REST base for every call this engine makes. Always
    /// [`GMAIL_API_BASE`] in production; the tests point it at a loopback mock so
    /// the walk/cursor rules are asserted on the wire rather than on a struct.
    api_base: String,
}

impl<S: Store + 'static, C: CredentialStore + 'static + ?Sized> SyncEngine<S, C> {
    pub fn new(
        store: Arc<S>,
        creds: Arc<C>,
        account_id: AccountId,
        account_email: String,
        config: Config,
    ) -> Self {
        // Timeouts keep a hung connection from wedging the poll loop.
        // Redirects are REFUSED deliberately: this client carries credentials
        // (Gmail bearer token, LLM x-api-key) and reqwest re-sends custom
        // headers cross-host on a redirect. Gmail's API does not redirect, so
        // this matches the repo's other Google-facing clients.
        // 180s, not 60: the long pole on this client is now a reasoning-model
        // triage call at high effort, which legitimately thinks for minutes on a
        // hard row. A Gmail fetch that runs that long is already wedged, and
        // this timeout only exists to unwedge it.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client build");
        // Absence => graceful disable, one notice, no key material logged.
        let stage2_llm = config.stage2.resolve_llm();
        if stage2_llm.is_none() {
            eprintln!(
                "squelch: no Stage-2 API key set (SQUELCH_STAGE2_API_KEY / ANTHROPIC_API_KEY / \
                 OPENAI_API_KEY) — Stage-2 LLM triage disabled (ambiguous rows stay queued; \
                 sync continues)"
            );
        }
        Self {
            store,
            creds,
            account_id,
            account_email,
            config,
            http,
            stage2_llm,
            embedder: None,
            embedder_gate: None,
            refresh: Arc::new(tokio::sync::Notify::new()),
            refine_wake: tokio::sync::Notify::new(),
            spam_refresh: Arc::new(tokio::sync::Notify::new()),
            warn_days: Arc::new(std::sync::Mutex::new(WarnDays::default())),
            notify_lane: std::sync::OnceLock::new(),
            unread_warned: AtomicBool::new(false),
            // Nothing has polled yet, so nothing may spend yet.
            poll_healthy: AtomicBool::new(false),
            metrics: SyncMetrics::new(),
            api_base: GMAIL_API_BASE.to_string(),
        }
    }

    /// Share the daemon's [`SyncMetrics`] so what this engine records is what
    /// `/metrics` serves: create ONE at startup and hand a clone to each side.
    /// Without it the engine still counts, into a registry nobody scrapes.
    pub fn with_metrics(mut self, metrics: Arc<SyncMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Point every Gmail call at `base` instead of [`GMAIL_API_BASE`]. Test-only:
    /// a build that could be aimed at an arbitrary host would be a bearer-token
    /// exfiltration primitive.
    #[cfg(test)]
    fn with_api_base(mut self, base: String) -> Self {
        self.api_base = base;
        self
    }

    /// Force the no-model path. Test-only, and NOT the same thing as building
    /// with an empty config: [`Stage2Config::resolve_llm`] reads
    /// `SQUELCH_STAGE2_API_KEY` / `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` from the
    /// PROCESS ENVIRONMENT, so a developer with a key exported would otherwise
    /// silently run the tests that are about the heuristics-are-authoritative
    /// path down the model path instead, and they would fail (or worse, pass for
    /// the wrong reason) on their machine and not in CI.
    #[cfg(test)]
    fn without_stage2_llm(mut self) -> Self {
        self.stage2_llm = None;
        self
    }

    /// Share a manual-refresh [`Notify`](tokio::sync::Notify) so the human door's
    /// `POST /client/refresh` can wake the poll loop between intervals: create
    /// ONE at daemon startup and hand a clone to each side. Without it the engine
    /// still polls on its own interval, just never early.
    pub fn with_refresh(mut self, refresh: Arc<tokio::sync::Notify>) -> Self {
        self.refresh = refresh;
        self
    }

    /// Share the ON-DEMAND SPAM signal, so the human door's spam page can ask
    /// for a fetch of a folder this loop otherwise never touches. Wire the same
    /// handle into `ApiState::with_spam_refresh`; without it the endpoint is a
    /// no-op and the page stays on whatever was last fetched.
    pub fn with_spam_refresh(mut self, spam_refresh: Arc<tokio::sync::Notify>) -> Self {
        self.spam_refresh = spam_refresh;
        self
    }

    /// Attach an [`Embedder`](crate::embed::Embedder) OVERRIDE, for callers that
    /// build one eagerly and want it used even if the store's copy differs.
    /// Usually unnecessary — [`SyncEngine::embedder`] falls back to the store's —
    /// and absence keeps sync fully functional, just writing no vectors.
    pub fn with_embedder(mut self, embedder: Arc<dyn crate::embed::Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// The EFFECTIVE embedder for this tick: the override, else whatever is
    /// attached to the store RIGHT NOW — so an embedder attached in the
    /// background after startup is picked up without a restart. Until then this
    /// is `None`, ingest skips the vector write, and the backfill pass fills in.
    fn embedder(&self) -> Option<Arc<dyn crate::embed::Embedder>> {
        self.embedder.clone().or_else(|| self.store.embedder())
    }

    /// Hold the FIRST backfill until `gate` reads true, meaning the caller's
    /// embedder init has RESOLVED, whichever way it resolved. Nothing else in
    /// the engine waits on it: the poll loop and the catch-up run whether or not
    /// there is an embedder, exactly as they do today.
    ///
    /// Why it exists. `squelchd serve` builds the embedder on a background task
    /// so both doors come up at once, and a brand-new tenant's first run starts
    /// its 30-day backfill in that same instant. Without the gate the backfill
    /// races the model download: thousands of rows ingest with no vector because
    /// [`SyncEngine::embed_and_store`] no-ops when there is no embedder to call,
    /// and [`SyncEngine::backfill_missing_vectors`] then has to drain all of them
    /// in batches. A batch is what the memory cost scales with, and it is memory
    /// the allocator does not give back (see `EmbedConfig::backfill_batch`), so
    /// that race is how a tenant daemon ends up permanently over a 1 Gi pod
    /// limit. Waiting turns it back into the cheap path: ingest writes each
    /// vector as it goes, one message at a time, and the batch pass only ever
    /// sees leftovers.
    ///
    /// The wait is bounded by [`EMBEDDER_GATE_CEILING`]; past it the backfill
    /// proceeds exactly as it did before this existed.
    pub fn with_embedder_gate(mut self, gate: tokio::sync::watch::Receiver<bool>) -> Self {
        self.embedder_gate = Some(gate);
        self
    }

    /// The wait [`SyncEngine::with_embedder_gate`] describes. `true` means "go
    /// ahead with the backfill", which is every outcome except one: `false` says
    /// shutdown arrived while we were parked, and there is no point starting a
    /// 30-day backfill nobody will be around to finish.
    ///
    /// Returns at once, silently, with no gate or with one already open, so the
    /// steady state (a restart, where the model is on disk and the init resolves
    /// in well under the time it takes to reach here) logs nothing.
    async fn wait_for_embedder(&self, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> bool {
        // Cloned because waiting needs `&mut` and the engine is shared.
        let Some(mut gate) = self.embedder_gate.clone() else {
            return true;
        };
        if *gate.borrow() {
            return true;
        }
        eprintln!("squelch: waiting for the embedder before the first backfill");
        // One deadline for the whole wait, however many times it parks: a
        // wakeup on the shutdown watch that carries no shutdown re-enters the
        // select below, and a ceiling armed per entry would be no ceiling.
        let deadline = tokio::time::Instant::now() + EMBEDDER_GATE_CEILING;
        loop {
            tokio::select! {
                how = wait_for_embedder_gate_until(&mut gate, deadline) => {
                    match how {
                        EmbedderGate::Settled => {
                            eprintln!("squelch: embedder settled; starting the first backfill")
                        }
                        EmbedderGate::Dropped => eprintln!(
                            "squelch: embedder gate dropped; the first backfill is running without the \
                             embedder, and its vectors will be drained by the vector backfill pass"
                        ),
                        EmbedderGate::TimedOut => {
                            self.metrics.record_embedder_gate_timeout();
                            eprintln!(
                                "squelch: embedder still not settled after {}m; the first backfill is \
                                 running without the embedder, and its vectors will be drained by the \
                                 vector backfill pass",
                                EMBEDDER_GATE_CEILING.as_secs() / 60
                            )
                        }
                    }
                    return true;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return false;
                    }
                    // A wakeup that did not carry a shutdown (a same-value send,
                    // or the sender going away) is not permission to run the
                    // backfill unembedded: the gate is still the thing being
                    // waited on, and the deadline above still bounds it. Nothing
                    // in production sends `false` today; this is what keeps a
                    // future second signal on that watch from silently
                    // reopening the 2026-08-19 memory profile.
                }
            }
        }
    }

    /// Authenticated GET returning parsed JSON. A 404 surfaces as
    /// [`CoreError::NotFound`] so callers can branch on it (the
    /// expired-historyId fallback). Header and body are NEVER logged.
    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let resp = self.send_get(url).await?;
        match resp.status() {
            s if s.is_success() => resp
                .json::<T>()
                .await
                .map_err(|e| CoreError::Other(anyhow::anyhow!("gmail json decode: {e}"))),
            // NOT an error metric: this is the expired-historyId signal the
            // caller recovers from with a catch-up, and counting a routine
            // self-heal would make the errors family unalertable.
            StatusCode::NOT_FOUND => Err(CoreError::NotFound),
            s => {
                // Classified HERE, the last place the status is typed AND the
                // body is still readable: a 403 is a quota refusal or an
                // authorization one depending only on the reason string Google
                // puts in the body, and once this is an `anyhow` chain nothing
                // downstream can tell them apart. The body is read for that one
                // decision and never logged.
                let body = resp.text().await.unwrap_or_default();
                self.metrics
                    .record_gmail_error(classify_gmail_status(s.as_u16(), &body));
                Err(CoreError::Other(anyhow::anyhow!(
                    "gmail api status {}",
                    s.as_u16()
                )))
            }
        }
    }

    /// Send a GET with a Bearer token, retrying once on 401 with a fresh token.
    async fn send_get(&self, url: &str) -> Result<reqwest::Response> {
        let token = self.token_for_request().await?;
        let resp = self.bearer_get(url, &token.access_token).await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            // Redacted: the fact of a retry, never token/header content.
            eprintln!("squelch: gmail 401; refreshing token and retrying once");
            let token = self.token_for_request().await?;
            return self.bearer_get(url, &token.access_token).await;
        }
        Ok(resp)
    }

    /// The access token for one Gmail call. A credential failure never reaches
    /// a status code — the refresh exchange is what failed, `invalid_grant`
    /// included — so it is counted as `auth` on the typed variant here rather
    /// than string-matched out of an error chain later.
    async fn token_for_request(&self) -> Result<crate::credentials::OAuthToken> {
        self.creds
            .token(self.account_id)
            .await
            // The SUCCESS half, and it is not decoration: the failure below sets
            // a state a client renders as "your mailbox is disconnected", so
            // something has to clear it when the credential works again. Without
            // this the banner would latch on and send somebody who has already
            // reconnected round the same loop a second time.
            .inspect(|_| self.metrics.note_credential_ok())
            .inspect_err(|e| {
                if matches!(e, CoreError::Credential(_)) {
                    self.metrics.record_gmail_error(GmailErrorKind::Auth);
                }
            })
    }

    async fn bearer_get(&self, url: &str, access_token: &str) -> Result<reqwest::Response> {
        self.http
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| {
                // No status ever arrived: DNS, TLS, connect or the client's own
                // timeout. Distinct from `http` because the fix is different.
                self.metrics.record_gmail_error(GmailErrorKind::Network);
                CoreError::Other(anyhow::anyhow!("gmail request: {e}"))
            })
    }

    /// One full lifecycle: backfill if needed (establishing the historyId), then
    /// run the two steady-state lanes ([`Self::run_lanes`]) until an error
    /// bubbles up (caller retries with backoff) or shutdown.
    ///
    /// The first-run sequence below is deliberately still SERIAL: on a brand-new
    /// mailbox there is nothing to poll for concurrently, the backfill has to
    /// finish before the history cursor exists at all, and running the passes
    /// inline is what makes the first thing the user ever sees a triaged inbox
    /// rather than an untriaged one.
    async fn run_once(&self, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> Result<()> {
        eprintln!("squelch: gmail REST sync starting for <redacted account>");

        // First run (no history cursor) => full backfill + seed contacts.
        let cursor = self.load_history_cursor()?;
        if cursor.is_none() {
            // BEFORE the backfill, and only before the FIRST one: every row it
            // ingests gets its vector written at ingest, one message at a time,
            // provided the embedder is there to write it. Start without one and
            // the whole 30-day window lands unembedded, to be drained in batches
            // afterwards instead. See [`SyncEngine::with_embedder_gate`].
            if !self.wait_for_embedder(shutdown).await {
                return Ok(());
            }
            self.backfill().await?;
            // Stage-1, then Stage-2 over what Stage-1 escalated, then the
            // specialist extractors over each row's FINAL category.
            self.stage1_pass().await;
            self.stage2_pass().await;
            self.extract_pass().await;
        }

        self.backfill_missing_vectors().await;

        self.run_lanes(shutdown).await
    }

    /// First-run backfill: INBOX bodies over the window, then SENT headers to
    /// seed contacts, then persist the account's current historyId.
    ///
    /// The trim runs WHATEVER THE OUTCOME. A run that failed halfway through the
    /// window still parsed and stored everything up to the failure, and that
    /// peak is the highest watermark this process ever reaches; leaving it in
    /// glibc's arenas because the last call errored would make a failed first
    /// run the most expensive thing a pod ever does. See [`crate::mem`].
    async fn backfill(&self) -> Result<()> {
        let out = self.backfill_inner().await;
        crate::mem::trim_off_runtime().await;
        out
    }

    async fn backfill_inner(&self) -> Result<()> {
        let since = self.backfill_since();

        // INBOX bodies.
        let q = format!("newer_than:{}d", self.config.sync.backfill_days);
        let inbox_ids = self.list_message_ids(LABEL_INBOX, Some(&q)).await?;
        // Backfill NEVER notifies (see `IngestOrigin`).
        let n = self
            .fetch_raw_and_ingest(&inbox_ids, Mailbox::Inbox, IngestOrigin::Backfill)
            .await?;
        eprintln!("squelch: backfilled {n} INBOX messages");

        // SENT bodies, not just headers, so semantic recall covers WHAT THE USER
        // WROTE. Contacts still come from To/Cc; the row is stored neutral
        // (tier=noise, importance=0) and the is_sent exclusions keep it out of
        // triage/updates/search.
        let sent_ids = self.list_message_ids(LABEL_SENT, Some(&q)).await?;
        let seeded = self
            .fetch_raw_and_ingest(&sent_ids, Mailbox::Sent, IngestOrigin::Backfill)
            .await?;
        eprintln!("squelch: backfilled {seeded} SENT messages (bodies for recall + contacts)");

        // NO SPAM LEG HERE, deliberately. See `sync_spam_window`: the spam
        // folder is fetched only when somebody opens the page that shows it.

        // Establish the historyId cursor from the profile.
        let history_id = self.fetch_profile_history_id().await?;
        self.store_history_cursor(history_id)?;
        eprintln!("squelch: history cursor established (backfill window from {since})");
        Ok(())
    }

    /// THE STEADY STATE: two lanes, not one (docs/NOTIFY.md §11.2).
    ///
    /// This used to be a single serial conga line — poll, then Stage-1, then
    /// Stage-2, then the extractors, then the revisits, then sleep — and that
    /// shape is what made mail arrive LATE rather than SLOWLY. A refine pass
    /// spends one reasoning-model call per row and a batch can be minutes wide;
    /// for every one of those minutes the daemon was not asking Gmail whether
    /// anything new had landed, because the fetch was queued behind the
    /// thinking. New mail sat unfetched behind the classification of OLD mail,
    /// which is the exact inversion of what the user wants, and it is also how
    /// a notification aged past its freshness window before anybody had ever
    /// looked at the message (docs/NOTIFY.md §2a).
    ///
    /// So: fetching and refining are separate futures, polled concurrently by
    /// one `tokio::select!` over `&self`. NO `tokio::spawn`, no `'static`, no
    /// new `Arc`s — both lanes borrow the engine, which is what keeps this a
    /// restructuring of one function rather than a new ownership story.
    ///
    /// - The poll lane's `Err` (Gmail auth/quota) ends the `select!` and bubbles
    ///   to [`Self::run`]'s backoff exactly as it did when this was one loop.
    ///   The refine lane is then cancelled at its next await, which leaves the
    ///   in-flight row queued (`stage1_model_used` NULL) — the same state a
    ///   crash leaves it in, and the same state the next lifecycle picks up.
    ///   ONE THING THAT IS NEW, and it costs money rather than correctness: the
    ///   cancelled await can now be a model call, which the old serial loop
    ///   could not be (`poll_once().await?` returned before any pass started).
    ///   [`Self::gate_budget`] charges BEFORE the call so a retry storm cannot
    ///   exceed the cap, and nothing refunds a cancellation, so the cancelled
    ///   verdict is paid for out of a DAY-scoped budget and never lands.
    ///   The charge-before-call rule stays (a refund path would be a second way
    ///   to move the counter, and charging first is what makes the cap a cap);
    ///   what bounds the damage is the `poll_healthy` gate, which stops the
    ///   refine lane re-entering a pass on the next backoff cycle while the
    ///   credential is still broken. So a multi-hour Gmail outage costs at most
    ///   the one round that was in flight when it started, not one per cycle.
    /// - The refine lane never returns an `Err` at all: every failure inside the
    ///   passes is handled and logged internally, so a model outage must not be
    ///   able to bounce the Gmail lifecycle.
    /// - The two shutdown receivers are the SAME channel. The second is a clone
    ///   because `changed()` needs `&mut`, and a clone starts out having seen
    ///   the current value, which is why each lane still checks `borrow()` at
    ///   the top of its own loop rather than trusting the wakeup.
    async fn run_lanes(&self, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let mut refine_shutdown = shutdown.clone();
        tokio::select! {
            polled = self.poll_lane(shutdown) => polled,
            // Only ever completes on shutdown; it has no error to report.
            () = self.refine_lane(&mut refine_shutdown) => Ok(()),
        }
    }

    /// Poll `history.list` every `poll_secs`, ingesting `messageAdded` INBOX
    /// messages and advancing the cursor. A poll batch IS the coalesced batch.
    ///
    /// This lane touches Gmail and the store and nothing else: no model call
    /// runs here, so nothing that thinks can delay the next fetch. The refine
    /// work it used to do inline is [`Self::refine_lane`], woken by
    /// `refine_wake` the moment this lane ingests anything.
    async fn poll_lane(&self, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let interval = Duration::from_secs(self.config.sync.poll_secs);
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            // BEFORE the walk, not after: the walk is what can return Err and
            // bounce the whole lifecycle, and a mailbox stuck in that retry loop
            // should still have a fresh unread count for the human door.
            self.refresh_inbox_unread().await;
            // BEFORE the walk too, and for the same reason: the sweep is purely
            // local — one indexed UPDATE, no Gmail, no model spend — so hanging
            // it off a call that can Err would let a bad credential or a Gmail
            // outage starve it for as long as the outage lasts.
            self.reminder_pass();
            self.poll_once().await?;
            // GMAIL ANSWERED, so the refine lane may spend again. Set AFTER the
            // walk and before anything else, because this is the only proof in
            // the process that the credential works; `fetch_raw_and_ingest` has
            // already poked `refine_wake` from inside that call if it ingested
            // anything, and `Notify` holds the permit, so the lane wakes on the
            // poke and reads a flag that is true by the time it does. See the
            // field's own doc for what this is protecting.
            self.poll_healthy.store(true, Ordering::Relaxed);
            // THE freshness stamp for a healthy daemon: `run()` only records one
            // on its way out, and a mailbox that polls happily for a month never
            // goes there — staleness alerts would fire on the boot timestamp.
            //
            // IT STAYS IN THIS LANE, deliberately. Sync freshness measures
            // POLLING, not refinement: that is what the staleness alert was
            // always meant to mean, and stamping it from the refine lane would
            // let a wedged Gmail credential keep looking healthy for as long as
            // there were queued rows left to classify.
            self.metrics.stamp_sync_success();

            // A refresh poke that arrives mid-poll is not lost: `Notify` stores
            // one permit, so the next `notified()` returns at once and the loop
            // runs one more immediate tick.
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.refresh.notified() => {
                    eprintln!("squelch: manual refresh — polling now");
                }
                // THE SPAM PAGE ASKED. Run the window sync right here and then
                // fall through to an ordinary tick, so one click costs one spam
                // fetch and nothing else changes about the loop's rhythm.
                //
                // Its failure is swallowed on purpose. This is a fetch of a
                // folder nothing depends on, requested by a page somebody is
                // looking at; letting it bounce the lifecycle would mean a
                // Gmail hiccup on the spam tab stops the user's actual mail from
                // syncing. The page learns from the stamp not moving.
                _ = self.spam_refresh.notified() => {
                    if let Err(e) = self.sync_spam_window().await {
                        eprintln!("squelch: spam sync failed ({e})");
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }

    /// The thinking half: Stage-1, Stage-2, the specialist extractors and the
    /// scheduled re-evaluations, over whatever is queued RIGHT NOW.
    ///
    /// Runs one full round and then parks until [`Self::fetch_raw_and_ingest`]
    /// pokes `refine_wake` (new mail is queued, start on it immediately) or
    /// `poll_secs` elapses (the floor, so a queue that filled up some other way
    /// — a re-triage, a revisit falling due, a row an earlier round left behind
    /// — still drains without new mail having to arrive to trigger it).
    ///
    /// No `Result`: every failure inside these passes is already handled and
    /// logged where it happens, and a lane that could `Err` would be a lane that
    /// could bounce the Gmail lifecycle over a model outage.
    ///
    /// IT SPENDS NOTHING UNTIL A POLL HAS SUCCEEDED. See `poll_healthy` for the
    /// whole argument; the short version is that this lane is cancelled whenever
    /// the poll lane errors, a cancelled classify is charged and never refunded,
    /// and `run()` retries the pair every five minutes for as long as the
    /// credential stays broken.
    async fn refine_lane(&self, shutdown: &mut tokio::sync::watch::Receiver<bool>) {
        let interval = Duration::from_secs(self.config.sync.poll_secs);
        loop {
            if *shutdown.borrow() {
                return;
            }
            if self.poll_healthy.load(Ordering::Relaxed) {
                // Both stages refine within the same round; neither can crash the
                // lane (all failures are handled internally).
                self.stage1_pass().await;
                self.stage2_pass().await;
                // AFTER both stages, so it sees each row's FINAL category (Stage-2
                // may have overwritten Stage-1's).
                self.extract_pass().await;
                // LAST, and over OLD rows rather than the ones just ingested: a
                // re-evaluation competes with nothing this round, and a row it
                // re-escalates is picked up by the next one.
                self.revisit_pass().await;

                // Per-round, so an embedder attached after startup catches up on
                // rows ingested before it was ready, no restart needed.
                //
                // INSIDE THE GATE with the rest, even though it costs no model
                // call: a round that skipped Stage-1 has nothing new to embed,
                // and the next healthy round picks up whatever is missing. One
                // shape for the whole round is easier to reason about than a
                // half-round.
                self.backfill_missing_vectors().await;
            }

            // A poke that arrives mid-round is not lost, for the same reason a
            // mid-poll `refresh` is not: `Notify` holds one permit, so the next
            // `notified()` returns at once. It also COALESCES — twenty pokes
            // during one long round are one extra round, not twenty.
            tokio::select! {
                _ = self.refine_wake.notified() => {}
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
            }
        }
    }

    /// FETCH THE PROVIDER'S SPAM FOLDER, ONCE, ON REQUEST.
    ///
    /// The poll loop does not walk the SPAM label — not on backfill, not on a
    /// history tick, not on a catch-up. Spam is the largest thing in a mailbox
    /// and the least read, and every message is a body fetch, a parse and a
    /// sanitize; tracking it continuously would spend most of this daemon's
    /// budget keeping a page current that nobody has open. So the fetch happens
    /// when the page is opened, and the click pays for it.
    ///
    /// CAPPED at `sync.spam_max`, newest first. The cap is a bound on how long
    /// that click can take rather than a claim about which messages are in the
    /// window — see [`Self::list_message_ids_capped`].
    ///
    /// Rows land exactly as the label walk used to leave them: `is_spam = 1`,
    /// neutral tier, `'n/a'` stage markers, no embedding, no notification. The
    /// ingest branch owns all of that, so on-demand and routine fetches cannot
    /// disagree about what a spam row is.
    ///
    /// The completion stamp is written LAST and only on success, which is what
    /// makes it readable as "we looked": a half-finished sync leaves the old
    /// stamp, and the page keeps saying it is still checking.
    pub async fn sync_spam_window(&self) -> Result<usize> {
        let q = format!("newer_than:{}d", self.config.sync.backfill_days);
        let cap = self.config.sync.spam_max as usize;
        let ids = self
            .list_message_ids_capped(LABEL_SPAM, Some(&q), Some(cap))
            .await?;
        eprintln!("squelch: spam sync — {} message(s) to fetch", ids.len());
        // `Backfill`, not `Incremental`: this is a bulk read of old mail the
        // user asked to look at. `worthy_kind` already refuses a spam row, so
        // the origin is belt to that brace rather than the guarantee.
        let n = self
            .fetch_raw_and_ingest(&ids, Mailbox::Spam, IngestOrigin::Backfill)
            .await?;
        self.store.set_app_setting(
            self.account_id,
            SPAM_SYNCED_AT_KEY,
            &Utc::now().to_rfc3339(),
        )?;
        eprintln!("squelch: spam sync complete — {n} message(s)");
        Ok(n)
    }

    /// A single poll tick: consult the cursor, either run the incremental
    /// history walk or (on absent/expired cursor) a fresh catch-up.
    async fn poll_once(&self) -> Result<()> {
        let cursor = self.load_history_cursor()?;
        match history_poll_decision(cursor, false) {
            HistoryDecision::Incremental(start) => {
                match self.history_walk(start).await {
                    Ok(()) => Ok(()),
                    // Expired historyId (404): fall back to a fresh catch-up.
                    Err(CoreError::NotFound) => {
                        eprintln!("squelch: historyId expired; falling back to catch-up");
                        self.catch_up().await
                    }
                    Err(e) => Err(e),
                }
            }
            HistoryDecision::FullCatchUp => self.catch_up().await,
        }
    }

    /// One incremental poll: THREE label-filtered `history.list` walks from the
    /// SAME start cursor, INBOX then SENT then SPAM, with the cursor committed
    /// ONCE at the end at the max historyId any walk observed.
    ///
    /// The SENT walk is what makes mail the user writes from Gmail web or their
    /// phone land here at all: such a message never carries INBOX, so an
    /// INBOX-only walk is blind to it, and the api's post-send echo covers only
    /// what was sent through the daemon itself. The SPAM walk is blind to the
    /// other two for the same structural reason, which is why spam was invisible
    /// in this client until it got a walk of its own. All three start from the
    /// same `start_history_id` because one Gmail cursor spans the whole mailbox.
    ///
    /// ORDER IS LOAD-BEARING. A self-addressed message carries both INBOX and
    /// SENT and so appears in two walks; the store's upsert keeps the MINIMUM of
    /// each visibility flag, but the walk order is the first defense and does not
    /// depend on that clause holding. INBOX therefore runs FIRST and its ids are
    /// subtracted from both later batches, making the visible copy authoritative.
    ///
    /// Propagates [`CoreError::NotFound`] on an expired historyId so the caller
    /// can fall back to a catch-up.
    async fn history_walk(&self, start_history_id: u64) -> Result<()> {
        let (inbox_ids, inbox_cursor) = self
            .history_walk_label(start_history_id, LABEL_INBOX)
            .await?;
        if !inbox_ids.is_empty() {
            let n = self
                .fetch_raw_and_ingest(&inbox_ids, Mailbox::Inbox, IngestOrigin::Incremental)
                .await?;
            eprintln!("squelch: ingested {n} new INBOX messages");
        }

        // The SENT half is best-effort AGAINST THE CURSOR: any failure holds the
        // cursor where it is and returns, so the next poll re-walks both labels
        // from the same start rather than stepping over INBOX history that the
        // successful half already covered. Re-walking costs a few fetches and
        // changes nothing: `UNIQUE(account_id, gmail_msg_id)` collapses the
        // message rows and `UNIQUE(message_id)` on `events` collapses the
        // notifications.
        //
        // THE FAST LANE WOULD HAVE MADE THAT FALSE — a re-walked message still
        // inside `freshness_window_secs` is a fresh `notify_lane::candidate`,
        // and a second run is a second PAID model call and a second daily-cap
        // unit, neither of which `UNIQUE` collapses. It re-reads the ledger
        // first for exactly this path (`Store::notify_decision_exists`), so a
        // re-walk costs one indexed probe per message and still changes nothing.
        let (sent_ids, sent_cursor) =
            match self.history_walk_label(start_history_id, LABEL_SENT).await {
                Ok(v) => v,
                // An expired historyId is not a SENT-specific failure: the cursor
                // itself is gone and only a catch-up can recover it.
                Err(CoreError::NotFound) => return Err(CoreError::NotFound),
                Err(e) => {
                    eprintln!("squelch: sent history walk failed ({e}); holding the cursor");
                    return Ok(());
                }
            };

        let sent_only = subtract_ids(sent_ids, &inbox_ids);
        if !sent_only.is_empty() {
            // `Incremental`, like the INBOX half: the silence guarantee for the
            // user's own mail is NOT the origin. It is, in order,
            // `notify_eligible_stamp` refusing to stamp an `is_sent` row at all
            // (docs/NOTIFY.md §11.3), so there is nothing for any site to emit
            // from; `events::worthy_kind` refusing one with
            // `Err(Refusal::NotWorthy)` as defense in depth; and the 'n/a' stage
            // markers ingest stamps, which keep it out of both LLM queues.
            match self
                .fetch_raw_and_ingest(&sent_only, Mailbox::Sent, IngestOrigin::Incremental)
                .await
            {
                Ok(n) => eprintln!("squelch: ingested {n} new SENT messages"),
                Err(e) => {
                    eprintln!("squelch: sent ingest failed ({e}); holding the cursor");
                    return Ok(());
                }
            }
        }

        // AND NO SPAM WALK. A poll tick runs every few seconds; the spam folder
        // is the largest thing in a mailbox and the least looked at, so walking
        // it here would spend most of this daemon's fetch budget on mail nobody
        // asked to see. It is fetched on demand instead — see `sync_spam_window`.

        self.store_history_cursor(advance_history_cursor(
            start_history_id,
            [inbox_cursor, sent_cursor],
        ))?;
        Ok(())
    }

    /// Walk `history.list` from `start_history_id` under ONE label filter,
    /// returning the deduped `messageAdded` ids and the max historyId observed.
    /// Persists nothing: the caller owns the single cursor commit that covers
    /// every label. Propagates [`CoreError::NotFound`] on an expired historyId.
    async fn history_walk_label(
        &self,
        start_history_id: u64,
        label: &str,
    ) -> Result<(Vec<String>, u64)> {
        let mut cursor = start_history_id;
        let mut page_token: Option<String> = None;
        let mut new_ids: Vec<String> = Vec::new();

        loop {
            let mut url = format!(
                "{}/history?startHistoryId={start_history_id}\
                 &historyTypes=messageAdded&labelId={label}",
                self.api_base
            );
            if let Some(tok) = &page_token {
                url.push_str(&format!("&pageToken={tok}"));
            }
            let page: HistoryListResp = self.get_json(&url).await?;

            // Advance the cursor from every observed historyId (records + the
            // page-level newest id).
            let observed = page
                .history
                .iter()
                .filter_map(|r| r.id.as_deref().map(parse_history_id))
                .chain(page.history_id.as_deref().map(parse_history_id));
            cursor = advance_history_cursor(cursor, observed);

            for rec in &page.history {
                for added in &rec.messages_added {
                    new_ids.push(added.message.id.clone());
                }
            }

            match page.next_page_token {
                Some(tok) => page_token = Some(tok),
                None => break,
            }
        }

        // Dedup ids (a message can appear across pages); order is irrelevant —
        // dedup at the store keys on (account_id, gmail_msg_id).
        new_ids.sort_unstable();
        new_ids.dedup();
        Ok((new_ids, cursor))
    }

    /// Fresh catch-up: re-run the backfill-window INBOX fetch, then the SENT
    /// fetch over the same window (dedup makes both idempotent), and
    /// re-establish the historyId. Used on first run's poll and on an
    /// expired-history 404 — exactly the cases where the history walk cannot
    /// account for the gap, so the sent half has to be re-listed too or mail the
    /// user wrote from another client during it is lost for good.
    ///
    /// Trimmed on the way out either way, for the reason [`Self::backfill`]
    /// gives: a catch-up that dies mid-window has already paid the peak.
    async fn catch_up(&self) -> Result<()> {
        // THE PROGRESS PAIR IS SET UP FRONT AND CLEARED ON EVERY EXIT, including
        // the `?` paths in the body below, which is what `_guard` is for. A
        // catch-up is this loop's longest single call by orders of magnitude and
        // it used to emit nothing at all while it ran: no log, no counter, no
        // freshness stamp, because `poll_once` had not returned yet. Worse, most
        // of its work is upserting mail already stored, so the message count and
        // the database size sit still as well. On 2026-08-26 a tenant's mailbox
        // spent half an hour in here and the only honest thing anybody could say
        // from outside was that it was indistinguishable from wedged.
        //
        // It is held ACROSS the trim on purpose: handing the heap back is part
        // of the catch-up as seen from outside, so progress clears when the
        // whole thing is done rather than when the last fetch returns.
        let _guard = CatchUpGuard(&self.metrics);
        let out = self.catch_up_inner().await;
        crate::mem::trim_off_runtime().await;
        out
    }

    /// The catch-up body, split out so [`Self::catch_up`] can hold the progress
    /// guard and run the trim across every exit path, `?` included.
    async fn catch_up_inner(&self) -> Result<()> {
        let q = format!("newer_than:{}d", self.config.sync.backfill_days);
        let ids = self.list_message_ids(LABEL_INBOX, Some(&q)).await?;
        // BEFORE the first fetch, so the size of the job is known while it is
        // still a job rather than after it is a result. This is the line that
        // turns "silent for 30 minutes" into "re-fetching 4,500 messages".
        eprintln!(
            // "triage runs alongside it", not "triage waits for this to
            // finish": since the lane split (docs/NOTIFY.md §11.2) the refine
            // lane is a sibling future under the same `select!` and grinds the
            // rows this catch-up commits as it commits them. An operator who
            // read the old wording concluded that no model spend was happening
            // for the next half hour and that queued rows were frozen, and both
            // are now false.
            "squelch: catch-up re-fetching {} INBOX message(s) from the last {} days; \
             triage runs alongside it",
            ids.len(),
            self.config.sync.backfill_days
        );
        self.metrics.catchup_begin(ids.len() as u64);
        // A catch-up may carry genuinely new mail, so it is allowed to notify.
        // What keeps the whole-window re-scan from storming is the FIRST-SIGHT
        // STAMP (`triage.notify_eligible_at`, see [`notify_eligible_stamp`])
        // plus the one-event-per-message key, and the two halves of the window
        // are bounded by DIFFERENT parts of that rule:
        //
        // - Mail this re-scan has seen before ALREADY EXISTS, so
        //   `ingest_message` preserves whatever stamp it was given the first
        //   time — NULL for the backfill's rows, an hour-old timestamp for
        //   anything the last catch-up already saw — and the rescue ceiling or
        //   the NULL refuses it at every emission site.
        // - Mail that arrived while the historyId was dead has never been seen
        //   here and does NOT already exist, so preservation cannot be what
        //   bounds it. `events::is_fresh`, asked ONCE at first sight, is: a
        //   catch-up after a week-long outage stamps only the mail whose `Date:`
        //   is still inside `notify.freshness_window_secs` when we finally lay
        //   eyes on it, and everything older is stamped NULL and silent forever.
        //
        // It is no longer the freshness window doing the first job, which is
        // exactly the fix: asked at emission time it measured the SENDER's clock
        // against the wall clock, so a row that waited behind a queue of model
        // calls aged out and went silent.
        let n = self
            .fetch_raw_and_ingest(&ids, Mailbox::Inbox, IngestOrigin::Incremental)
            .await?;
        if n > 0 {
            eprintln!("squelch: catch-up ingested {n} INBOX messages");
        }

        // INBOX first, and its ids subtracted, for the same reason the history
        // walk orders itself that way: a self-addressed message is in both
        // listings and its visible copy must win the unique-key race.
        let sent_ids = self.list_message_ids(LABEL_SENT, Some(&q)).await?;
        let sent_only = subtract_ids(sent_ids, &ids);
        // EXTENDS the same run rather than starting a second one: from outside
        // this is one wait, and a progress bar that reached the end and then
        // restarted at zero would read as a loop rather than as two phases.
        self.metrics
            .catchup_begin_extend(ids.len() as u64 + sent_only.len() as u64);
        eprintln!(
            "squelch: catch-up re-fetching {} SENT message(s)",
            sent_only.len()
        );
        let sent_n = self
            .fetch_raw_and_ingest(&sent_only, Mailbox::Sent, IngestOrigin::Incremental)
            .await?;
        if sent_n > 0 {
            eprintln!("squelch: catch-up ingested {sent_n} SENT messages");
        }

        // No SPAM leg, same as the backfill and the walk: a catch-up is already
        // the longest thing this loop does, and the spam folder is fetched only
        // when the page that shows it is opened.

        let history_id = self.fetch_profile_history_id().await?;
        self.store_history_cursor(history_id)?;
        Ok(())
    }

    // ---- Sent-contacts harvest --------------------------------------------

    /// ONE-TIME sweep of the ENTIRE Sent mailbox — no window — seeding the
    /// contacts table from every To/Cc the account has ever written to.
    /// `format=metadata` with only To/Cc requested: headers, never bodies, so
    /// deep history costs one light call per sent message. Merged with MAX
    /// semantics ([`Store::merge_harvested_contacts`]) so overlap with the
    /// ingest path's own seeding and an interrupted re-run cannot double-count.
    ///
    /// Completion is a `sync_state` flag (`mailbox = 'sent_contacts'`); an
    /// interrupted pass leaves it unset and the next daemon start redoes the
    /// sweep from scratch — idempotent, just re-paged.
    pub async fn harvest_sent_contacts(&self) -> Result<()> {
        let done = self
            .store
            .sync_state(self.account_id, SENT_CONTACTS_KEY)?
            .map(|s| s.last_uid >= 1)
            .unwrap_or(false);
        if done {
            return Ok(());
        }

        let ids = self.list_message_ids(LABEL_SENT, None).await?;
        eprintln!(
            "squelch: sent-contacts harvest scanning {} sent messages (headers only)",
            ids.len()
        );

        let self_addr = self.account_email.trim().to_ascii_lowercase();
        // addr -> aggregate. Counting per message occurrence matches the ingest
        // path's per-send bump closely enough for ranking.
        let mut agg: std::collections::HashMap<String, ContactEntry> =
            std::collections::HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            let url = format!(
                "{}/messages/{id}?format=metadata\
                 &metadataHeaders=To&metadataHeaders=Cc",
                self.api_base
            );
            let msg: GmailMessage = self.get_json(&url).await?;
            let sent_at = parse_internal_date(msg.internal_date.as_deref());
            let headers = msg.payload.map(|p| p.headers).unwrap_or_default();
            // Through mail-parser via header synthesis, so grouped lists, quoted
            // display names and RFC2047 encoding parse exactly as ingest does.
            let blob = synthesize_rfc822_headers(&headers);
            let Some(parsed) = mail_parser::MessageParser::default().parse(blob.as_bytes()) else {
                continue;
            };
            for list in [parsed.to(), parsed.cc()].into_iter().flatten() {
                for mailbox in list.iter() {
                    let Some(addr) = mailbox.address() else {
                        continue;
                    };
                    let addr = addr.trim().to_ascii_lowercase();
                    // Same gate as ingest seeding: never the account itself,
                    // never robot/unsubscribe traffic.
                    if addr.is_empty() || addr == self_addr || is_robot_address(&addr) {
                        continue;
                    }
                    let name = mailbox
                        .name()
                        .map(|n| n.trim().to_string())
                        .filter(|n| !n.is_empty());
                    let entry = agg.entry(addr.clone()).or_insert_with(|| ContactEntry {
                        addr,
                        display_name: None,
                        sent_count: 0,
                        last_sent_at: None,
                    });
                    entry.sent_count += 1;
                    if entry.display_name.is_none() {
                        entry.display_name = name;
                    }
                    if sent_at > entry.last_sent_at {
                        entry.last_sent_at = sent_at;
                    }
                }
            }
            if (i + 1) % 1000 == 0 {
                eprintln!(
                    "squelch: sent-contacts harvest {}/{} messages scanned",
                    i + 1,
                    ids.len()
                );
            }
        }

        let batch: Vec<ContactEntry> = agg.into_values().collect();
        self.store
            .merge_harvested_contacts(self.account_id, &batch)?;
        self.store.set_sync_state(
            self.account_id,
            SENT_CONTACTS_KEY,
            &SyncState {
                uidvalidity: 1,
                last_uid: 1,
            },
        )?;
        eprintln!(
            "squelch: sent-contacts harvest complete — {} unique recipients",
            batch.len()
        );
        Ok(())
    }

    // ---- Sent-recipients backfill -----------------------------------------

    /// ONE-TIME sweep filling `messages.to_addrs` on sent rows ingested before
    /// that column existed, so the human door's sent listing can show who each
    /// message went to instead of a blank. Same shape as
    /// [`harvest_sent_contacts`](Self::harvest_sent_contacts): `format=metadata`
    /// with only To/Cc requested (headers, never bodies) on the READ credential,
    /// and a `sync_state` completion flag so it runs once per install.
    ///
    /// BEST-EFFORT AND NON-FATAL by construction: it is spawned beside the sync
    /// loop, never inside it, and any error returns early with the flag UNSET, so
    /// the next daemon start simply redoes whatever is still NULL. A message
    /// Gmail no longer has (404) is not an error — it is written as "" ("looked,
    /// nobody named"), which is what keeps one deleted message from re-queueing
    /// the whole pass forever.
    pub async fn backfill_sent_recipients(&self) -> Result<()> {
        let done = self
            .store
            .sync_state(self.account_id, SENT_RECIPIENTS_KEY)?
            .map(|s| s.last_uid >= 1)
            .unwrap_or(false);
        if done {
            return Ok(());
        }

        let mut filled = 0usize;
        // Per-message failures are SKIPPED, not fatal: one message with a
        // persistent 4xx quirk must not abort the sweep (or re-run the whole
        // pass on every daemon start forever). Skipped rows stay NULL and are
        // filtered out of each batch so the loop still terminates; the done
        // flag is only set after a pass with zero skips, so they retry on the
        // next start.
        let mut skipped: Vec<i64> = Vec::new();
        loop {
            let pending = self
                .store
                .sent_missing_recipients(self.account_id, SENT_RECIPIENTS_BATCH)?;
            let pending: Vec<_> = pending
                .iter()
                .filter(|r| !skipped.contains(&r.message_id))
                .collect();
            if pending.is_empty() {
                break;
            }
            for row in &pending {
                let url = format!(
                    "{}/messages/{}?format=metadata\
                     &metadataHeaders=To&metadataHeaders=Cc",
                    self.api_base, row.gmail_msg_id
                );
                let msg: GmailMessage = match self.get_json(&url).await {
                    Ok(msg) => msg,
                    // Gone upstream: record the absence rather than retrying it
                    // on every daemon start for the life of the install.
                    Err(CoreError::NotFound) => GmailMessage::default(),
                    Err(_) => {
                        skipped.push(row.message_id);
                        continue;
                    }
                };
                let headers = msg.payload.map(|p| p.headers).unwrap_or_default();
                // Through mail-parser via header synthesis, so grouped lists,
                // quoted display names and RFC2047 encoding render exactly as the
                // ingest path renders them.
                let blob = synthesize_rfc822_headers(&headers);
                let mut mailboxes: Vec<(String, Option<String>)> = Vec::new();
                if let Some(parsed) = mail_parser::MessageParser::default().parse(blob.as_bytes()) {
                    for list in [parsed.to(), parsed.cc()].into_iter().flatten() {
                        collect_mailboxes(list, &mut mailboxes);
                    }
                }
                // EVERY row is written, "" included: the store predicate is
                // `to_addrs IS NULL`, so a row left NULL would be handed back by
                // the next batch query and loop forever.
                let to = format_recipients(&mailboxes).unwrap_or_default();
                self.store
                    .set_message_to_addrs(self.account_id, row.message_id, &to)?;
                filled += 1;
            }
        }

        if skipped.is_empty() {
            self.store.set_sync_state(
                self.account_id,
                SENT_RECIPIENTS_KEY,
                &SyncState {
                    uidvalidity: 1,
                    last_uid: 1,
                },
            )?;
            if filled > 0 {
                eprintln!("squelch: sent-recipients backfill complete — {filled} messages filled");
            }
        } else {
            eprintln!(
                "squelch: sent-recipients backfill left {} of {} unfetched; retrying next start",
                skipped.len(),
                skipped.len() + filled
            );
        }
        Ok(())
    }

    // ---- Gmail REST calls --------------------------------------------------

    /// List all message ids under `label`, optionally narrowed by a Gmail search
    /// `q`. Paginates fully.
    async fn list_message_ids(&self, label: &str, q: Option<&str>) -> Result<Vec<String>> {
        self.list_message_ids_capped(label, q, None).await
    }

    /// [`list_message_ids`](Self::list_message_ids) with an optional ceiling on
    /// how many ids to collect, which also STOPS PAGINATING once it is reached.
    ///
    /// Gmail returns `messages.list` newest-first in practice, so a cap is a
    /// recent-N window. It is not a contract, which is why the only caller is
    /// the spam sync: there the cap is a bound on WORK ("do not fetch ten
    /// thousand bodies because somebody clicked a tab"), not a promise about
    /// which messages are in it, and a slightly wrong tail is a page that misses
    /// old junk. Nothing that has to be exact may use it.
    async fn list_message_ids_capped(
        &self,
        label: &str,
        q: Option<&str>,
        cap: Option<usize>,
    ) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!("{}/messages?labelIds={label}", self.api_base);
            if let Some(q) = q {
                url.push_str(&format!("&q={}", urlencode(q)));
            }
            if let Some(tok) = &page_token {
                url.push_str(&format!("&pageToken={tok}"));
            }
            let page: ListMessagesResp = self.get_json(&url).await?;
            ids.extend(page.messages.into_iter().map(|m| m.id));
            if let Some(cap) = cap
                && ids.len() >= cap
            {
                ids.truncate(cap);
                break;
            }
            match page.next_page_token {
                Some(tok) => page_token = Some(tok),
                None => break,
            }
        }
        Ok(ids)
    }

    /// Fetch each id `format=raw`, base64url-decode to RFC822, and run through
    /// the ingest pipeline. Sequential — rate limits are a non-issue at this
    /// volume. Returns the count ingested.
    async fn fetch_raw_and_ingest(
        &self,
        ids: &[String],
        mailbox: Mailbox,
        origin: IngestOrigin,
    ) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let rules = self.store.list_sender_rules(self.account_id)?;
        let mut count = 0usize;

        for id in ids {
            // Only a catch-up has a denominator, so this is a no-op on the
            // ordinary incremental path — `catchup_step` moves a gauge that is
            // zero unless `catch_up` set one up. A line every 250 messages
            // rather than every message: enough to prove movement in a log
            // somebody is tailing, few enough not to bury the lines that mean
            // something.
            if let Some((_, total)) = self.metrics.catchup_progress() {
                let done = self.metrics.catchup_step();
                if done.is_multiple_of(250) {
                    eprintln!("squelch: catch-up {done}/{total} messages");
                }
            }
            let url = format!("{}/messages/{id}?format=raw", self.api_base);
            let msg: GmailMessage = self.get_json(&url).await?;
            let raw_b64 = match &msg.raw {
                Some(r) => r,
                None => continue, // nothing to ingest
            };
            let raw = match decode_raw_b64url(raw_b64) {
                Ok(bytes) => bytes,
                Err(e) => {
                    // Redacted: id + error only, never content.
                    eprintln!("squelch: skipping message (decode error): {e}");
                    continue;
                }
            };
            let fetched = RawFetched {
                account_id: self.account_id,
                gmail_msg_id: if msg.id.is_empty() {
                    id.clone()
                } else {
                    msg.id.clone()
                },
                gmail_thread_id: msg.thread_id.clone(),
                raw,
                internal_date: parse_internal_date(msg.internal_date.as_deref()),
                is_sent: mailbox.is_sent(),
                is_spam: mailbox.is_spam(),
                account_addr: self.account_email.clone(),
            };
            // THE CLOCK IS READ PER MESSAGE AND AFTER ITS FETCH, not once for the
            // batch, and that is load bearing rather than tidy. This value is what
            // `notify_eligible_stamp` records as "the moment we first saw the
            // message" (docs/NOTIFY.md §11.3) and what the rescue ceiling is
            // measured from, and this loop is SEQUENTIAL over one `format=raw` GET
            // per id: `catch_up` runs it over the whole backfill window, which
            // [`Self::catch_up_inner`]'s own comment records taking half an hour
            // for a real tenant. Frozen at the top of the function, both
            // directions are wrong:
            //
            // - a genuinely new message fetched 30 minutes in would enter the
            //   Stage-1 queue with 30 minutes of its rescue window already spent,
            //   and expire before the refine lane ever reached it: a notification
            //   dropped AND booked as `deliberate/expired`, which would have the
            //   one number §11.11 reads reporting a miss that the stamp itself
            //   invented;
            // - past `MAX_FUTURE_SKEW_SECS` of batch runtime every remaining
            //   message would read as dated in the future, fail `is_fresh`, and be
            //   stamped NULL: permanently unnotifiable, at every site, forever.
            //   That is exactly §2a's silent drop, reintroduced on the
            //   post-outage path where notifications matter most.
            //
            // AFTER the GET rather than before it, so a slow fetch is charged to
            // the fetch and not to the message's rescue window. It costs one
            // `clock_gettime` per network round trip. `ingest_with_rules` reads
            // the same value; it is a pure function of the message and the clock,
            // so per-message is simply the more truthful "now" for it too.
            let now = Utc::now();
            let Some(ingested) = self.ingest_one(&fetched, &rules, now, origin)? else {
                count += 1;
                continue;
            };
            // THE FAST LANE, SPAWNED AND NEVER AWAITED (docs/NOTIFY.md §11.5).
            //
            // Here rather than after `embed_and_store` because the embedder is
            // ONNX inference on a `spawn_blocking` thread and can take seconds
            // per message: a notification queued behind it would be a
            // notification arriving after the thing it was about. And spawned
            // rather than awaited because the whole point of the lane is that a
            // model call in front of a user does not become a model call in
            // front of the next message's fetch.
            //
            // The candidate is built HERE, on the poll lane, because it is pure
            // and cheap and because it is the gate: most messages are not
            // candidates at all, and a `None` costs one spawn we never make.
            if let Some(c) = notify_lane::candidate(
                &ingested.triaged,
                ingested.id,
                &rules,
                &self.config.notify,
                |addr| {
                    self.store
                        .is_known_contact(self.account_id, addr)
                        .unwrap_or(false)
                },
            ) {
                tokio::spawn(self.notify_lane().clone().run(c));
            }
            if let Some(text) = ingested.embed_text {
                self.embed_and_store(ingested.id, text).await;
            }
            count += 1;
        }
        // POLL LANE -> REFINE LANE. Poked from here rather than from the poll
        // loop because this is the one place that knows BOTH facts the poke
        // needs: how many rows actually landed, and which sync path they landed
        // on. `Notify` stores a permit when nobody is waiting, so a poke that
        // arrives while the refine lane is mid-round wakes it as soon as it
        // finishes, and several pokes in one round collapse into one.
        //
        // BACKFILL DOES NOT POKE. The first backfill's refinement is run inline
        // by `run_once` before either lane starts, so a poke would only queue a
        // redundant round; and the poke means "mail that may be genuinely new is
        // waiting", which a backfill's month of already-read mail is not.
        if count > 0 && origin == IngestOrigin::Incremental {
            self.refine_wake.notify_one();
        }
        Ok(count)
    }

    /// Run one fetched message through the seal-first ingest pipeline and commit
    /// it atomically. `None` means NOTHING WAS COMMITTED (a sealed outbound
    /// copy, which is the one case ingest deliberately drops on the floor);
    /// otherwise the committed row, whose `embed_text` is `None` for the rows
    /// that must never enter the vector space.
    ///
    /// IT HANDS BACK THE `TriagedMessage` AND NOT JUST AN ID because the fast
    /// lane's gate ([`notify_lane::candidate`]) is a pure function of the
    /// triaged row, and a sealed row — which has no embed text and used to make
    /// this return `None` — is precisely one of the shapes it has something to
    /// say about (docs/NOTIFY.md §11.6). Collapsing "not embeddable" and "not
    /// interesting" into one `None` is what would silence it.
    fn ingest_one(
        &self,
        fetched: &RawFetched,
        rules: &[SenderRule],
        now: DateTime<Utc>,
        origin: IngestOrigin,
    ) -> Result<Option<Ingested>> {
        let mut triaged = ingest_with_rules(fetched, &self.config.stage1, now, rules, |addr| {
            self.store
                .is_known_contact(self.account_id, addr)
                .unwrap_or(false)
        });
        // THE ELIGIBILITY STAMP, computed BEFORE the store call because
        // `ingest_message` writes it on the triage row's FIRST INSERT only and
        // preserves it verbatim on conflict. `ingest_with_rules` deliberately
        // leaves it `None`: the pure pipeline does not know which sync path it
        // is on, and that fact is the whole decision.
        triaged.notify_eligible_at =
            notify_eligible_stamp(&triaged, origin, &self.config.notify, now);
        // A SEALED OUTBOUND COPY IS NOT COMMITTED — the same rule
        // [`ingest::ingest_sent`] holds the api's send echo to, and now that the
        // sent walk delivers outbound mail on every poll it has to hold here too.
        // `thread_guard_and_subject` 404s any thread containing a sealed message,
        // so storing the user's own reply that quotes an OTP would HIDE the
        // counterparty's mail they were reading a second ago. Seal detection runs
        // BEFORE ingest's `is_sent` branch precisely so this case is catchable.
        if fetched.is_sent && triaged.sensitivity != Sensitivity::Normal {
            return Ok(None);
        }
        let id = self.store.ingest_message(&triaged)?;
        // NOTHING IS EMITTED HERE ANY MORE. This site used to notify off a
        // CONFIDENT heuristic seed when no LLM was configured, which was the
        // whole of the "unless we have no model to wait for" case; the FAST LANE
        // now owns that path end to end (docs/NOTIFY.md §11.5), records it as
        // `model_used='heuristic'`, and applies the same live-rule and
        // rescue-window rules to it as to a model verdict. Two sites emitting
        // for the same reason would not double-notify — `UNIQUE(message_id)`
        // forbids it — but the SECOND one would book a `would_send` against a
        // buzz it had itself just sent, and the rescued/overturned joins in
        // §11.4 are read off exactly those rows.
        //
        // The deliberate lane is unaffected: it never ran without an LLM anyway.

        // STRUCTURAL EXCLUSION: sealed mail is never embedded. It is still a
        // committed row the fast lane may ping about, so this narrows the embed
        // text rather than dropping the whole outcome.
        let embed_text = if triaged.sensitivity != Sensitivity::Normal {
            None
        } else if triaged.message.is_spam {
            // NEITHER IS PROVIDER SPAM, for two reasons that point the same way.
            // It would be the largest single consumer of embedder time in the
            // daemon — spam outnumbers real mail — spent on the one category of
            // message semantic recall must never return. And an embedding is a
            // similarity claim: spam is written to imitate the mail it is
            // impersonating, so a vector space containing it puts convincing
            // forgeries next to the real thing in results the user reads as
            // "your mail".
            None
        } else {
            Some(crate::embed::message_embed_text(
                &triaged.message.subject,
                &triaged.message.body,
                self.config.embed.max_chars,
            ))
        };
        Ok(Some(Ingested {
            id,
            embed_text,
            triaged,
        }))
    }

    /// The fast lane, built on first use. See the [`SyncEngine::notify_lane`]
    /// field for why it is not built in `new`.
    fn notify_lane(&self) -> &Arc<notify_lane::NotifyLane<S>> {
        self.notify_lane.get_or_init(|| {
            Arc::new(notify_lane::NotifyLane::new(
                self.store.clone(),
                // The SAME client, so the lane shares the connection pool (and
                // the redirect refusal, which is what keeps a credentialed
                // header from being re-sent cross-host).
                self.http.clone(),
                self.config.notify.clone(),
                self.config.stage1.known_contact_importance,
                self.stage2_llm.clone(),
                self.metrics.clone(),
                self.account_id,
                self.warn_days.clone(),
            ))
        })
    }

    /// The sender's CURRENT rule disposition, for the refine emission sites —
    /// see [`events::current_rule`] for why a queued row cannot answer this
    /// itself. A store error reads as "no rule"; the surrounding pass has already
    /// logged any real store trouble by then.
    ///
    /// PER ROW, DELIBERATELY, and asked ONCE per row rather than the two or three
    /// times it used to be. Hoisting it to one read per pass would be cheaper
    /// still, and would break the case the freshness exists for: a pass spends a
    /// model call per row, so a batch can be minutes wide, and the reactive
    /// squelch — the user blocking a sender whose mail is being classified right
    /// now — must land on the rows still ahead of it. A verdict is stamped once,
    /// so a rule missed here is missed until a re-triage, not until the next tick.
    fn current_rule(&self, from_addr: &str) -> Option<crate::types::Disposition> {
        let rules = self.store.list_sender_rules(self.account_id).ok()?;
        events::current_rule(from_addr, &rules)
    }

    /// The single emission point for all three verdict sites (ingest heuristic,
    /// Stage-1 apply, Stage-2 apply); the decision itself lives in
    /// [`events::event_for`], which owns the seal invariant and the rescue
    /// ceiling. BEST-EFFORT: a store error is logged (ids only) and swallowed —
    /// a notification is never worth failing triage over. Store-side
    /// `UNIQUE(message_id)` makes a repeat call a silent no-op, which is what
    /// makes the refine passes and `catch_up()`'s re-scan safe to hook.
    ///
    /// Returns what happened so the caller can record it; the recording itself
    /// is [`SyncEngine::record_deliberate`], one layer up, because the FAST lane
    /// calls this decision through its own path and must not write a
    /// `deliberate` row by doing so.
    ///
    /// ALREADY-NOTIFIED IS SETTLED BEFORE EXPIRED, and the order is the whole
    /// honesty of the `expired` count. `worthy_kind` refuses on the rescue
    /// ceiling without ever touching the store, so it cannot know that the phone
    /// already buzzed — and the commonest shape of an expiry offer is exactly
    /// that: a message Stage-1 notified at 09:01 is offered again by Stage-2 at
    /// 11:00 and again by every later re-triage, each time with the same 09:00
    /// stamp and so each time "expired". Recording those would fill the
    /// missed-notification series with DELIVERED notifications. [`ExpiryCount`]
    /// is the second guard on the same series, for the rows a human deliberately
    /// dragged back.
    fn emit_event(&self, ctx: &events::EventContext<'_>, now: DateTime<Utc>) -> Emitted {
        let ev = match events::event_for(ctx, &self.config.notify, now) {
            Ok(ev) => ev,
            Err(events::Refusal::NotWorthy) => return Emitted::NotWorthy,
            Err(events::Refusal::Suppressed) => return Emitted::Suppressed,
            Err(events::Refusal::Expired) => {
                // The store read the refusal path skipped. A store error reads as
                // "no event": recording a drop we are unsure about is the honest
                // direction for a series whose whole job is to be believed when
                // it says a notification went missing.
                if self
                    .store
                    .message_has_event(self.account_id, ctx.message_id)
                    .unwrap_or(false)
                {
                    return Emitted::AlreadyNotified;
                }
                return Emitted::Expired;
            }
        };
        match self.store.append_event(&ev) {
            Ok(Some(id)) => {
                eprintln!(
                    "squelch: notification event {id} ({}) for message {}",
                    ev.kind.as_str(),
                    ev.message_id
                );
                Emitted::New(id)
            }
            // Already notified once; one event per message, ever.
            Ok(None) => Emitted::AlreadyNotified,
            Err(e) => {
                eprintln!(
                    "squelch: append_event failed ({e}); no notification for message {}",
                    ev.message_id
                );
                // A LOST NOTIFICATION, AND IT GETS THE LOG LINE ABOVE AND
                // NOTHING ELSE. The mail was worthy, eligible and in-window; the
                // store broke (SQLITE_BUSY under the two lanes' contention, a
                // full disk, a locked WAL) and the user was not told. See
                // [`Emitted::Failed`] for why §11.4's vocabulary has no word for
                // this and why inventing one out of `unavailable` would be worse
                // than the silence.
                Emitted::Failed
            }
        }
    }

    /// [`SyncEngine::emit_event`] plus the DELIBERATE-lane ledger row that
    /// docs/NOTIFY.md §11.7 requires of every refine emission site. The three
    /// sites (Stage-1 apply, the seed fallback, Stage-2 apply) call this and
    /// nothing else, so none of them can emit without recording or record
    /// without emitting.
    ///
    /// `model_used` is the STAGE's model id — or [`HEURISTIC_ONLY`] for the seed
    /// fallback, deliberately a different string from the fast lane's
    /// `heuristic`: both mean "no model scored this", but one of them means it
    /// AFTER a model was asked and refused, and the eval corpus the ledger exists
    /// to be (§4) is worth nothing if those two read the same.
    fn emit_deliberate(
        &self,
        ctx: &events::EventContext<'_>,
        now: DateTime<Utc>,
        counting: ExpiryCount,
        model_used: &str,
    ) -> Emitted {
        let emitted = self.emit_event(ctx, now);
        self.record_deliberate(ctx, emitted, counting, model_used);
        emitted
    }

    /// One `deliberate` ledger row for one refine-site outcome, plus its
    /// counter. BEST-EFFORT: a store error is logged (ids and the decision word,
    /// never a word of the mail) and swallowed — a ledger row is never worth
    /// failing triage over.
    ///
    /// THE INSERT IS THE DEDUPE, and that is the point of doing it this way. A
    /// single message is offered to a deliberate emission site repeatedly: the
    /// Stage-1 apply site, then the Stage-2 apply site behind it, then again
    /// after any re-triage (`retriage_reset` nulls the stage markers and leaves
    /// the eligibility stamp alone, on purpose). §11.4's `UNIQUE(message_id,
    /// lane)` keeps the FIRST answer and reports the rest as ignored, so hanging
    /// the counter off the insert makes the metric and the table agree by
    /// construction — one miss, one row, one count, however many sites offer it.
    /// Wave 1 had to spell that rule out in an in-memory `HashSet` because the
    /// table did not exist yet; the set is gone, and with it the ceiling it
    /// needed and the re-count a restart used to cost.
    ///
    /// TWO ROWS ARE DELIBERATELY NOT WRITTEN:
    ///
    /// - A message with NO `notify_eligible_at`. §11.4 restricts the table to
    ///   stamped messages, which is what keeps it from being 95% backfill.
    /// - An `Expired` on a row a human dragged back through triage (see
    ///   [`ExpiryCount`]). Not a row marked differently: NO ROW AT ALL, because
    ///   the ledger's decisions are facts about mail and "somebody re-read this"
    ///   is a fact about an operator.
    fn record_deliberate(
        &self,
        ctx: &events::EventContext<'_>,
        emitted: Emitted,
        counting: ExpiryCount,
        model_used: &str,
    ) {
        // NEVER ELIGIBLE, so not in the ledger at all.
        if ctx.notify_eligible_at.is_none() {
            return;
        }
        let decision = match emitted {
            Emitted::New(_) => NotifyDecision::Sent,
            Emitted::AlreadyNotified => NotifyDecision::WouldSend,
            Emitted::NotWorthy => NotifyDecision::DeclinedByModel,
            Emitted::Suppressed => NotifyDecision::Suppressed,
            Emitted::Expired if counting == ExpiryCount::Rereading => return,
            Emitted::Expired => NotifyDecision::Expired,
            // The store, not the mail. See [`Emitted::Failed`].
            Emitted::Failed => return,
        };
        let row = crate::store::NewNotifyDecision {
            account_id: self.account_id,
            message_id: ctx.message_id,
            lane: NotifyLane::Deliberate,
            decision,
            notify_importance: Some(ctx.importance),
            model_used: Some(model_used.to_string()),
            // FAST-LANE ONLY (§11.4). The deliberate lane's age is the triage
            // pipeline's own queue depth, and reporting that as a notification's
            // latency would make the one histogram anybody reads meaningless.
            latency_ms: None,
        };
        match self.store.record_notify_decision(&row) {
            // Inserted: this site is the first to answer for this message, so
            // this is the answer the counter and the eval corpus record.
            Ok(true) => {
                self.metrics.record_notify(NotifyLane::Deliberate, decision);
                if decision == NotifyDecision::Expired {
                    // REDACTED: the message id and nothing else. No sender, no
                    // subject, no one_line — this line exists to prove the drop
                    // happened and to point at the row, not to describe the mail.
                    eprintln!(
                        "squelch: notification expired for message {} (worthy, but past \
                         notify.rescue_window_secs since we first saw it)",
                        ctx.message_id
                    );
                }
            }
            // A later site re-offering a message this lane already answered
            // about. Append-only doing its job; nothing to say and nothing to
            // count.
            Ok(false) => {}
            Err(e) => eprintln!(
                "squelch: notify ledger write failed for message {} ({}): {e}",
                ctx.message_id,
                decision.as_str()
            ),
        }
    }

    /// Emit for a Stage-1 row whose model call did NOT produce a verdict, from
    /// the heuristic seed the row still carries. The decision itself is
    /// [`events::seed_context`]; this reads the seed and does the emitting.
    ///
    /// Only the fallback path calls this. The stale-skip path has no
    /// notification to lose: a row older than `stage2.max_age_days` is days past
    /// `notify.rescue_window_secs` from whenever we first saw it, so the ceiling
    /// would refuse the event whichever site offered it.
    fn emit_seed_event(&self, row: &crate::store::Stage1Queued) {
        let seed = match self
            .store
            .triage_seed_verdict(self.account_id, row.message_id)
        {
            Ok(Some(s)) => s,
            // Missing or sealed: nothing to notify about, and a store error here
            // is never worth failing triage over.
            Ok(None) => return,
            Err(e) => {
                eprintln!("squelch: seed read failed ({e}); no notification for that row");
                return;
            }
        };
        // The seed's deadline, in the shape the emission decision reads. Only
        // `due_at` is consulted (kind and amount are the extractors' business),
        // so this carries the date and says plainly where it came from.
        let deadline = seed.deadline.map(|due_at| crate::triage::DeadlineHit {
            kind: "bill".to_string(),
            amount: None,
            currency: None,
            due_at,
            past_due: seed.tier == crate::types::Tier::PastDue,
            source: "heuristic-seed".to_string(),
        });
        if let Some(ctx) = events::seed_context(
            row,
            &seed,
            deadline.as_ref(),
            self.current_rule(&row.from_addr),
        ) {
            let now = Utc::now();
            self.emit_deliberate(
                &ctx,
                now,
                ExpiryCount::from_retriage(row.retriage_at, now),
                HEURISTIC_ONLY,
            );
        }
    }

    /// Embed `text` off the async runtime and write the vector for
    /// `message_id`. No-op without an embedder. A failure logs a redacted
    /// one-liner (id + error kind, never body) and never propagates — the
    /// backfill pass recovers the vector, so ingest must not block on it.
    async fn embed_and_store(&self, message_id: i64, text: String) {
        let Some(embedder) = self.embedder() else {
            return;
        };
        let account_id = self.account_id;
        let store = self.store.clone();
        // ONNX inference is CPU-bound; keep it off the poll loop.
        let result = tokio::task::spawn_blocking(move || {
            let vec = embedder.embed(&text)?;
            store.upsert_message_vector(account_id, message_id, &vec)
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!(
                "squelch: embed failed for message {message_id} (recoverable via backfill): {e}"
            ),
            Err(e) => eprintln!("squelch: embed task join error for message {message_id}: {e}"),
        }
    }

    /// Embed every message still missing a vector, in throttled batches, so
    /// recall covers pre-existing rows and ingest-time embed failures. Sealed
    /// content is structurally absent — `messages_missing_vectors` selects only
    /// `sensitivity='normal'` (see docs/SECURITY.md). A failed batch logs a
    /// redacted one-liner and is retried on a later pass. No-op with no embedder.
    async fn backfill_missing_vectors(&self) {
        let Some(embedder) = self.embedder() else {
            return;
        };
        let batch = self.config.embed.backfill_batch.max(1);
        let max_chars = self.config.embed.max_chars;
        let account_id = self.account_id;
        let mut total = 0usize;

        loop {
            let missing = match self.store.messages_missing_vectors(account_id, batch) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("squelch: vector backfill query failed ({e}); stopping pass");
                    return;
                }
            };
            if missing.is_empty() {
                break;
            }
            let n = missing.len();
            // Flatten each message the SAME way ingest does. (Query text takes a
            // different road: search embeds the query as typed, uncut.)
            let store = self.store.clone();
            let embedder = embedder.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                let texts: Vec<String> = missing
                    .iter()
                    .map(|m| crate::embed::message_embed_text(&m.subject, &m.body, max_chars))
                    .collect();
                let vecs = embedder.embed_batch(&texts)?;
                for (m, vec) in missing.iter().zip(vecs.iter()) {
                    store.upsert_message_vector(account_id, m.message_id, vec)?;
                }
                Ok(())
            })
            .await;

            match result {
                Ok(Ok(())) => total += n,
                Ok(Err(e)) => {
                    eprintln!("squelch: vector backfill batch failed ({e}); stopping pass");
                    break;
                }
                Err(e) => {
                    eprintln!("squelch: vector backfill task join error ({e}); stopping pass");
                    break;
                }
            }

            // A short batch means we drained the queue; stop before re-querying.
            if n < batch {
                break;
            }
            // Throttle between batches so a large backfill doesn't peg the CPU
            // or starve the poll lane, which shares this task: the two lanes are
            // futures under one `select!`, not two runtimes, so a tight loop
            // here still delays the next `history.list`.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        if total > 0 {
            eprintln!("squelch: vector backfill embedded {total} message(s) for semantic recall");
            // A batch pass is the single largest transient this process makes:
            // ONNX arenas plus every flattened body, hundreds of MB on a first
            // run, and glibc keeps all of it unless asked. Worth doing with the
            // session still loaded (+324 MB to +290 MB on the measured pass),
            // and gated on `total > 0` so it is at most once per poll tick. See
            // [`crate::mem`].
            crate::mem::trim_off_runtime().await;
        }
    }

    /// This engine's view of the shared budget ledger — the same `WarnDays` the
    /// fast lane gates on, which is the point of [`BudgetLedger`] existing.
    fn budget(&self) -> BudgetLedger<'_, S> {
        BudgetLedger {
            store: &*self.store,
            account_id: self.account_id,
            warn_days: &self.warn_days,
        }
    }

    /// See [`BudgetLedger::warn_once`].
    fn warn_once_per_day(&self, kind: CapKind, day: &str) -> bool {
        self.budget().warn_once(kind, day)
    }

    /// Shared pass preamble; `None` when the LLM is disabled (no API key —
    /// the notice was already emitted at startup). Caps are re-read at the
    /// START of every pass so a client change via POST /client/triage-config
    /// applies within a cycle, no restart. Precedence: override > config/env
    /// > default.
    fn pass_setup(&self) -> Option<PassSetup<'_>> {
        let llm = self.stage2_llm.as_ref()?;
        let caps = self
            .store
            .stage2_cap_overrides(self.account_id)
            .unwrap_or_default();
        let now = Utc::now();
        Some(PassSetup {
            api_key: &llm.api_key,
            provider: llm.provider,
            url: &llm.url,
            caps,
            day: now.format("%Y-%m-%d").to_string(),
            stale_cutoff: now - ChronoDuration::days(self.config.stage2.max_age_days as i64),
        })
    }

    /// Unwrap a pass's queue read; a read error logs once and yields an empty
    /// queue, which the caller treats as "nothing to do".
    fn read_queue<T>(res: Result<Vec<T>>, label: &str) -> Vec<T> {
        match res {
            Ok(q) => q,
            Err(e) => {
                eprintln!("squelch: {label} queue read failed ({e}); skipping pass");
                Vec::new()
            }
        }
    }

    /// Check-then-increment the SHARED Stage-1 global daily budget (Stage-1
    /// and the extractors bill the same counter). The increment lands BEFORE
    /// the model call so an attempt counts even on error/retry — a retry storm
    /// can never exceed the cap. Stage-2's three-scope gate stays inline in
    /// [`Self::stage2_pass`]: it checks all three caps before incrementing any
    /// of them, which a per-key check-then-increment helper would break.
    fn gate_stage1_global_budget(
        &self,
        day: &str,
        cap: u32,
        label: &str,
        tail: &str,
    ) -> BudgetGate {
        self.gate_budget(
            STAGE1_GLOBAL_BUDGET_KEY,
            day,
            cap,
            CapKind::Stage1Global,
            label,
            tail,
        )
    }

    /// See [`BudgetLedger::gate`].
    fn gate_budget(
        &self,
        key: &str,
        day: &str,
        cap: u32,
        kind: CapKind,
        label: &str,
        tail: &str,
    ) -> BudgetGate {
        self.budget().gate(key, day, cap, kind, label, tail)
    }

    /// See [`BudgetLedger::refund`].
    fn refund_budget(&self, key: &str, day: &str, label: &str) {
        self.budget().refund(key, day, label)
    }

    /// Plan and store a message's scheduled re-evaluations from a verdict that
    /// just landed. Failures are logged and swallowed: a missing revisit is a
    /// row that ages badly, never a reason to fail the verdict that produced it.
    fn schedule_revisits(
        &self,
        message_id: i64,
        model_revisits: &[crate::triage::revisit::RevisitOut],
        deadline: Option<&crate::triage::DeadlineHit>,
        now: DateTime<Utc>,
    ) {
        if !self.config.revisit.enabled {
            return;
        }
        let planned = crate::triage::revisit::plan(
            model_revisits,
            deadline,
            &self.config.revisit.planner(),
            now,
        );
        if let Err(e) = self
            .store
            .revisits_schedule(self.account_id, message_id, &planned, now)
        {
            eprintln!("squelch: revisit scheduling failed ({e}); the row will not be re-evaluated");
        }
    }

    /// Bring back the mail whose reminders have come due.
    ///
    /// The poll tick is the clock: there is no timer per reminder, because a
    /// reminder is stored state and the daemon may well have been off when the
    /// moment passed. Whatever is due when the loop next runs fires then, so a
    /// restart costs punctuality and never a reminder.
    ///
    /// A CREDENTIAL OUTAGE COSTS NO MORE THAN A RESTART DOES, which is why the
    /// loop runs this AHEAD of `poll_once` rather than after it: nothing here
    /// talks to Gmail, so a mailbox stuck retrying an `invalid_grant` — or a
    /// weekend of Gmail being down — must not be what swallows Saturday's
    /// reminder.
    ///
    /// IT NO LONGER LANDS BEFORE `revisit_pass`, and that used to be stated here
    /// as a guarantee. The lane split (docs/NOTIFY.md §11.2) put this in the
    /// poll lane and `revisit_pass` in the refine lane, on independent clocks
    /// with independent wake sources, so the sweep can now read the standing
    /// band before this tick's reminders have fired and miss a mail that came
    /// back a moment ago. It is a race, not a lost reminder: both lanes cycle at
    /// `poll_secs`, the row stays `open` once it fires, and the next sweep sees
    /// it. One round of latency on a re-evaluation, self-correcting.
    ///
    /// Synchronous and infallible from the loop's point of view: one indexed
    /// UPDATE, and an error is logged and dropped. A store hiccup must not bounce
    /// the whole sync lifecycle over a feature that will retry in a minute.
    fn reminder_pass(&self) {
        match self.store.fire_due_reminders(self.account_id, Utc::now()) {
            Ok(ids) if !ids.is_empty() => {
                eprintln!("squelch: {} reminder(s) came due", ids.len())
            }
            Ok(_) => {}
            Err(e) => eprintln!("squelch: reminder sweep failed ({e}); skipping"),
        }
    }

    /// Re-evaluate verdicts whose moment has passed.
    ///
    /// Two things feed this pass. Messages whose classifier named a date, and
    /// messages that have simply sat in the standing band too long — the sweep
    /// exists because the failure being fixed is a model not thinking about
    /// tomorrow, so a design that only re-checks what the model remembered to
    /// flag would inherit the bug it is meant to fix.
    ///
    /// A revisit is an ordinary Stage-1 call with the prior verdict attached, so
    /// it re-enters the normal pipeline: the router can escalate the new verdict,
    /// and the extractors see the new category. Rows the account owner has
    /// corrected by hand are excluded in SQL and again in the apply's guard.
    async fn revisit_pass(&self) {
        let rcfg = &self.config.revisit;
        if !rcfg.enabled {
            return;
        }
        let Some(PassSetup {
            api_key,
            provider,
            url,
            day,
            ..
        }) = self.pass_setup()
        else {
            return;
        };
        let cfg = &self.config.stage1;
        let now = Utc::now();

        // ---- The staleness sweep --------------------------------------------
        // Rows in the standing band that nobody has acted on and nothing has
        // looked at in a full window. After long enough, such a row is either
        // misfiled or finished; either way the user is looking at something they
        // should not be. Scheduled at `now`, so it is picked up by the read just
        // below — and `older_than` is the query's cooldown too, which is what
        // keeps a row swept this tick from being swept again on the next one.
        if let Some(window) = rcfg.fye_stale_window() {
            let older_than = now - window;
            match self.store.revisit_stale_standing(
                self.account_id,
                older_than,
                rcfg.max_per_message_lifetime,
                rcfg.batch_per_cycle,
            ) {
                Ok(ids) => {
                    for message_id in ids {
                        let req = [crate::triage::revisit::RevisitRequest {
                            at: now,
                            why: format!(
                                "no action taken in {} days; check whether this still matters",
                                window.num_days()
                            ),
                            source: crate::triage::revisit::RevisitSource::FyeStale,
                        }];
                        if let Err(e) =
                            self.store
                                .revisits_schedule(self.account_id, message_id, &req, now)
                        {
                            eprintln!("squelch: staleness sweep could not schedule ({e})");
                        }
                    }
                }
                Err(e) => eprintln!("squelch: staleness sweep query failed ({e}); skipping"),
            }
        }

        // ---- Due re-evaluations ---------------------------------------------
        let queued = Self::read_queue(
            self.store.revisit_queue(
                self.account_id,
                now,
                rcfg.max_per_message_lifetime,
                rcfg.batch_per_cycle,
            ),
            "revisit",
        );
        if queued.is_empty() {
            return;
        }

        let mut rescored = 0usize;
        // ONE RE-EVALUATION PER MESSAGE PER PASS. A message can hold several
        // pending revisits (`max_per_message`), and after an outage they all come
        // due together — but the first one's apply rebuilds the schedule and
        // DELETES the rest, so the others would each spend a frontier-model call
        // to overwrite the verdict that just landed, reasoning from a
        // `prior_tier` the batch read before any of this happened.
        let mut seen_messages: HashSet<i64> = HashSet::new();
        for row in &queued {
            // SEALED GUARD: the queue excludes sealed rows in SQL; re-check
            // before every classify call (docs/SECURITY.md).
            if row.sensitivity != Sensitivity::Normal {
                eprintln!("squelch: revisit sealed guard tripped; skipping row");
                continue;
            }
            // Leave the duplicate PENDING rather than firing it: if this pass's
            // apply landed, `schedule_revisits` has already removed it, and if it
            // did not, the row deserves its own turn on a later cycle.
            if !seen_messages.insert(row.message_id) {
                continue;
            }

            match self.gate_budget(
                REVISIT_BUDGET_KEY,
                &day,
                rcfg.daily_cap,
                CapKind::Revisit,
                "revisit",
                "remaining re-evaluations",
            ) {
                BudgetGate::Exhausted => break,
                BudgetGate::SkipRow => continue,
                BudgetGate::Proceed => {}
            }

            let outcome =
                stage1_llm::classify_revisit_at(&self.http, url, api_key, cfg, provider, row, now)
                    .await;

            // FIRE-ONCE: stamped whatever happened below. A revisit that failed
            // still consumed its turn, and leaving it pending would retry the
            // same failing row every cycle for as long as the daemon runs.
            let fire = |label: &str| {
                if let Err(e) =
                    self.store
                        .revisit_mark_fired(self.account_id, row.revisit_id, Utc::now())
                {
                    eprintln!("squelch: revisit mark-fired failed after {label} ({e})");
                }
            };

            match outcome {
                Ok(stage1_llm::ClassifyOutcome::Ok(out, usage)) => {
                    if let Some(u) = usage {
                        let _ = self.store.extract_bump_usage(
                            self.account_id,
                            &day,
                            REVISIT_USAGE_CATEGORY,
                            u.into(),
                        );
                    }
                    // FIRE BEFORE RE-SCHEDULING, because `schedule_revisits`
                    // deletes every PENDING revisit for this message — this one
                    // included — and a deleted row can no longer be marked fired.
                    // Firing is what charges `triage.revisit_count`, so the other
                    // order silently spends nothing: the lifetime budget stays at
                    // zero forever and a row that keeps asking to be looked at
                    // again never terminates.
                    fire("apply");
                    let applied = stage1_llm::apply_revisit_result(
                        row,
                        &out,
                        &cfg.model,
                        cfg.known_contact_importance,
                        self.current_rule(&row.from_addr),
                        &self.config.router(),
                        Utc::now(),
                    );
                    match self.store.revisit_apply(&applied) {
                        Err(e) => eprintln!("squelch: revisit apply failed ({e})"),
                        // The row was sealed, resolved, or hand-corrected between
                        // the queue read and the apply. Nothing landed, and the
                        // schedule must not be rebuilt from a verdict that did
                        // not stick.
                        Ok(false) => {}
                        Ok(true) => {
                            rescored += 1;
                            self.metrics.record_revisit(RevisitVerdict::Ok);
                            self.schedule_revisits(
                                row.message_id,
                                &out.revisit,
                                applied.deadline.as_ref(),
                                Utc::now(),
                            );
                        }
                    }
                }
                Ok(stage1_llm::ClassifyOutcome::Failed(kind))
                    if crate::triage::llm::is_config_failure(&kind) =>
                {
                    // A config-level rejection (bad credential, disallowed
                    // model, spent gateway budget) is shared by every row, not
                    // a verdict about this one: leave the revisit PENDING
                    // (no `fire`) so it is retried once the config is fixed.
                    eprintln!(
                        "squelch: revisit config-level failure ({kind}); \
                         re-evaluations stay scheduled"
                    );
                    self.metrics.record_llm_config_failure();
                    self.refund_budget(REVISIT_BUDGET_KEY, &day, "revisit");
                    break;
                }
                Ok(stage1_llm::ClassifyOutcome::Refused)
                | Ok(stage1_llm::ClassifyOutcome::Failed(_)) => {
                    self.metrics.record_revisit(RevisitVerdict::Fallback);
                    fire("refusal/permanent failure");
                }
                Err(e) => {
                    // Retryable class exhausted / transport error: leave it
                    // pending for a later cycle.
                    eprintln!("squelch: revisit classify failed ({e}); still scheduled");
                }
            }
        }

        if rescored > 0 {
            eprintln!("squelch: re-evaluated {rescored} message(s) whose moment had passed");
        }
    }

    /// Run one Stage-1 LLM refine pass over rows still carrying their ingest
    /// heuristic seed (`stage1_model_used IS NULL`): sealed guard, GLOBAL
    /// Stage-1 budget with increment-before-call so retries can't exceed it,
    /// classify, apply — which stamps `stage1_model_used` and sets
    /// `needs_stage2`. On refusal or permanent error the row keeps its seed
    /// values stamped `heuristic-only` and the seed's own `needs_stage2` decides
    /// escalation. Budget exhaustion defers rows without loss; no failure
    /// crashes the sync loop. No-op when the LLM is disabled (no API key).
    async fn stage1_pass(&self) {
        let Some(PassSetup {
            api_key,
            provider,
            url,
            caps,
            day,
            stale_cutoff,
        }) = self.pass_setup()
        else {
            return;
        };
        let cfg = &self.config.stage1;
        let global_daily_cap = caps.stage1_global_daily_cap.unwrap_or(cfg.global_daily_cap);

        let queued = Self::read_queue(
            self.store
                .stage1_queue(self.account_id, cfg.batch_per_cycle),
            "stage-1",
        );
        if queued.is_empty() {
            return;
        }

        let mut refined = 0usize;
        let mut fallback = 0usize;
        let mut stale_skipped = 0usize;
        let mut in_tok = 0u64;
        let mut out_tok = 0u64;

        for row in &queued {
            // SEALED GUARD: the queue already excludes sealed rows in SQL;
            // re-check before every classify call (docs/SECURITY.md).
            // ONE ORDERED DECISION per row — sealed guard, then the stale skip
            // and the re-triage force that overrules it. It lives in
            // `route_stage1_row` for the reason the extract pass's lives in
            // `route_extract_row`: the ordering is the invariant, and it is
            // testable there without an LLM or a store.
            match crate::triage::route_stage1_row(row, stale_cutoff, Utc::now()) {
                Stage1RowAction::Sealed => {
                    // Re-run the guard purely to log its redacted message.
                    if let Err(e) = stage1_sealed_guard(row) {
                        eprintln!("squelch: stage-1 sealed guard tripped ({e}); skipping row");
                    }
                    continue;
                }
                // SKIP-STALE: mark processed WITHOUT a model call, keeping the
                // seed. STALE_SKIP_MODEL, never HEURISTIC_ONLY: the seed stands
                // either way, but "too old to ask" and "asked, no answer" are
                // opposite facts and the row is the only place either is recorded.
                Stage1RowAction::Stale => {
                    let _ = self.store.stage1_mark_processed(
                        self.account_id,
                        row.message_id,
                        STALE_SKIP_MODEL,
                    );
                    stale_skipped += 1;
                    self.metrics.record_stage1(Stage1Verdict::StaleSkipped);
                    continue;
                }
                Stage1RowAction::Run => {}
            }

            // GLOBAL budget check (Stage-1's ONLY scope). Once hit, every
            // remaining row this cycle stays queued, unstamped.
            match self.gate_stage1_global_budget(
                &day,
                global_daily_cap,
                "stage-1 global",
                "remaining rows",
            ) {
                BudgetGate::Exhausted => break,
                BudgetGate::SkipRow => continue,
                BudgetGate::Proceed => {}
            }

            let outcome = stage1_llm::classify(&self.http, url, api_key, cfg, provider, row).await;
            match outcome {
                Ok(stage1_llm::ClassifyOutcome::Ok(out, usage)) => {
                    if let Some(u) = usage {
                        in_tok += u.input_tokens;
                        out_tok += u.output_tokens;
                        if let Err(e) =
                            self.store
                                .stage1_bump_usage(self.account_id, &day, u.into())
                        {
                            eprintln!("squelch: stage-1 usage ledger bump failed ({e})");
                        }
                    }
                    // The rule as it stands NOW, not as it stood at ingest: a
                    // rule the user added since must still be honored, and one
                    // they deleted must stop being. Asked ONCE for the row, and
                    // used by both the apply and the emission below — the two
                    // are one decision and must not be able to disagree.
                    let rule = self.current_rule(&row.from_addr);
                    let applied = stage1_llm::apply_result_with_rule(
                        row,
                        &out,
                        &cfg.model,
                        cfg.known_contact_importance,
                        rule,
                        &self.config.router(),
                        Utc::now(),
                    );
                    match self.store.stage1_apply(&applied) {
                        Err(e) => {
                            eprintln!("squelch: stage-1 apply failed ({e}); row stays queued");
                        }
                        // TOCTOU: the row was sealed by hand while this pass held
                        // it, so the guarded UPDATE matched nothing and no verdict
                        // landed. Emitting on a bare Ok would snapshot sender +
                        // one_line for a now-sealed row.
                        Ok(false) => {}
                        Ok(true) => {
                            refined += 1;
                            self.metrics.record_stage1(Stage1Verdict::Ok);
                            self.schedule_revisits(
                                row.message_id,
                                &out.revisit,
                                applied.deadline.as_ref(),
                                Utc::now(),
                            );
                            // The refined verdict is final, so it emits whatever
                            // the seed thought; the ingest-time eligibility stamp
                            // is what stops this pass storming a fresh install's
                            // backlog, and what lets a SLOW verdict on genuinely
                            // new mail still land — this site is where the old
                            // `Date:`-based window ate notifications.
                            //
                            // A FORCED RE-TRIAGE DOES NOT BOOK AN EXPIRY HERE.
                            // `retriage_forced` rows bypass this pass's stale
                            // gate by design, so a `retriage_reset` over a wide
                            // window walks weeks-old rows past this site; the
                            // ones it newly rates worthy are exactly the point of
                            // running it, and every one of them is refused as
                            // expired. See [`ExpiryCount`].
                            let emit_now = Utc::now();
                            self.emit_deliberate(
                                &events::EventContext {
                                    account_id: self.account_id,
                                    message_id: row.message_id,
                                    thread_id: &row.thread_id,
                                    sender: &row.from_addr,
                                    one_line: &applied.one_line,
                                    notify_eligible_at: row.notify_eligible_at,
                                    sensitivity: row.sensitivity,
                                    // The Stage-1 queue selects `m.is_sent = 0`
                                    // and `m.is_spam = 0`.
                                    is_sent: false,
                                    is_spam: false,
                                    // The queue only excludes rows a rule decided
                                    // AT INGEST, so this is the rule as it stands
                                    // NOW, catching rules added since.
                                    rule,
                                    tier: applied.tier,
                                    importance: applied.importance,
                                    deadline: applied.deadline.as_ref(),
                                },
                                emit_now,
                                ExpiryCount::from_retriage(row.retriage_at, emit_now),
                                &cfg.model,
                            );
                        }
                    }
                }
                Ok(stage1_llm::ClassifyOutcome::Failed(kind))
                    if crate::triage::llm::is_config_failure(&kind) =>
                {
                    // CONFIG-LEVEL FAILURE (4xx shared by every row: bad key,
                    // disallowed model, spent gateway budget). Heuristic
                    // fallback is for verdicts about THIS row; a rejected
                    // config is not one, so leave the row queued
                    // (stage1_model_used stays NULL) and stop the pass instead
                    // of burning the cap on calls that fail identically.
                    eprintln!(
                        "squelch: stage-1 config-level failure ({kind}) at message {}; the \
                         resolved key/endpoint/model is wrong for the gateway; rows stay queued",
                        row.message_id
                    );
                    self.metrics.record_llm_config_failure();
                    self.refund_budget(STAGE1_GLOBAL_BUDGET_KEY, &day, "stage-1 global");
                    break;
                }
                Ok(stage1_llm::ClassifyOutcome::Refused)
                | Ok(stage1_llm::ClassifyOutcome::Failed(_)) => {
                    // HEURISTIC FALLBACK: keep the seed values and mark processed
                    // so the row cannot loop; the ingest-time needs_stage2 seed
                    // survives and drives escalation.
                    let _ = self.store.stage1_mark_processed(
                        self.account_id,
                        row.message_id,
                        HEURISTIC_ONLY,
                    );
                    fallback += 1;
                    self.metrics.record_stage1(Stage1Verdict::Fallback);
                    // AND THE SEED NOTIFIES, because nothing else will. Ingest
                    // now defers its emission to this pass on the promise that a
                    // model verdict is coming; a refusal or a permanent failure
                    // is that promise breaking, and `UNIQUE(message_id)` means a
                    // notification skipped here is skipped forever. So this is
                    // the "no model to wait for" case after all, and the seed is
                    // authoritative exactly as it is with no API key configured.
                    self.emit_seed_event(row);
                }
                Err(e) => {
                    // Retryable class exhausted / transport error. Leave the row
                    // queued (stage1_model_used stays NULL) for a future cycle.
                    eprintln!("squelch: stage-1 {e}; row stays queued");
                }
            }
        }

        if refined > 0 || fallback > 0 || stale_skipped > 0 {
            eprintln!(
                "squelch: stage-1 refined {refined} rows (model={}, in_tok={in_tok}, \
                 out_tok={out_tok}); heuristic-fallback {fallback}; stale-skipped {stale_skipped}",
                cfg.model
            );
        }
    }

    /// Run one SPECIALIST-EXTRACTOR pass — hence AFTER both stage passes.
    ///
    /// TWO SOURCES, run as two sequential sections, deliberately not one SQL
    /// union: they select on different predicates (a final LLM `category` vs the
    /// ingest-stamped `ship_extract_model='pending'` trigger), stamp different
    /// marker columns, and age rows out on different clocks — and one message may
    /// legitimately appear in both (an order confirmation is a receipt-bearing
    /// marketing mail as often as not).
    ///   1. CATEGORY-ROUTED — banking and marketing, via
    ///      [`Store::extract_queue`](crate::store::Store::extract_queue).
    ///   2. SHIPMENTS — the trigger queue
    ///      ([`Store::ship_extract_queue`](crate::store::Store::ship_extract_queue)).
    ///
    /// Per row in either: sealed guard, stale skip, then check + increment the
    /// SHARED Stage-1 daily budget (extractors run on the Stage-1 model and share
    /// its cap) before dispatching. Token usage bills to the extractor's OWN
    /// ledger category. Budget exhaustion defers rows without loss; per-row
    /// failures are logged redacted and never crash the sync loop. No-op when
    /// there is no API key.
    ///
    /// `batch_per_cycle` is PER SOURCE, so a tick can take up to that many
    /// category rows AND that many shipment rows. That is not a doubled budget:
    /// the shared Stage-1 daily cap is the real spend bound, and the batch size
    /// only decides how fast a backlog drains.
    async fn extract_pass(&self) {
        let Some(PassSetup {
            api_key,
            provider,
            url,
            caps,
            day,
            stale_cutoff,
        }) = self.pass_setup()
        else {
            return;
        };
        // Extractors run on the STAGE-1 (small) model and share its config +
        // cap: extract calls count against the SAME daily counter as Stage-1,
        // runtime override included.
        let cfg = &self.config.stage1;
        let global_daily_cap = caps.stage1_global_daily_cap.unwrap_or(cfg.global_daily_cap);

        let mut extracted = 0usize;
        let mut skipped = 0usize;
        let mut ship_extracted = 0usize;
        let mut ship_skipped = 0usize;
        let mut in_tok = 0u64;
        let mut out_tok = 0u64;

        // ---- SOURCE 1: the CATEGORY-ROUTED specialists ---------------------
        let categories = extract::extractable_categories();
        let queued = if categories.is_empty() {
            Vec::new()
        } else {
            Self::read_queue(
                self.store
                    .extract_queue(self.account_id, &categories, cfg.batch_per_cycle),
                "extract",
            )
        };

        // Set when a specialist reports a bad credential. The two sources below
        // share one resolved key, so a failure in the first is a failure in the
        // second: without this the shipments queue would spend the whole daily
        // cap re-proving the same misconfiguration.
        let mut auth_failed = false;

        for row in &queued {
            // ONE ORDERED DECISION per row — sealed guard (the queue already
            // excludes sealed rows in SQL; re-check anyway, docs/SECURITY.md),
            // then the stale skip, then the empty-body refusal, then the
            // extractor lookup. It lives in
            // `route_extract_row` so a new specialist cannot be added behind a
            // guard that does not know about it.
            let extractor = match extract::route_extract_row(row, stale_cutoff, Utc::now()) {
                RowAction::Sealed => {
                    // Re-run the guard purely to log its redacted message.
                    if let Err(e) = extract::extract_sealed_guard(row) {
                        eprintln!("squelch: extract sealed guard tripped ({e}); skipping row");
                    }
                    continue;
                }
                // SKIP-STALE: mark extracted WITHOUT a model call, so an old row
                // neither spends budget nor sits queued forever.
                RowAction::Stale => {
                    let _ = self.store.extract_mark_processed(
                        self.account_id,
                        row.message_id,
                        STALE_SKIP_MODEL,
                    );
                    skipped += 1;
                    continue;
                }
                // A row whose category has no handler is marked processed so it
                // cannot loop.
                RowAction::NoExtractor => {
                    let _ = self.store.extract_mark_processed(
                        self.account_id,
                        row.message_id,
                        "skip-no-extractor",
                    );
                    skipped += 1;
                    continue;
                }
                // SKIP-NO-BODY: an empty body gives an extractor nothing to
                // read, and a model handed nothing invents something. Marked
                // processed with its OWN stamp rather than the stale one, so
                // "we refused to guess" and "it was too old to bother" stay
                // two different facts in the row.
                RowAction::NoBody => {
                    let _ = self.store.extract_mark_processed(
                        self.account_id,
                        row.message_id,
                        NO_BODY_SKIP_MODEL,
                    );
                    skipped += 1;
                    continue;
                }
                RowAction::Run(extractor) => extractor,
            };

            // SHARED Stage-1 global budget. Once hit, every remaining row this
            // cycle stays queued, unstamped.
            match self.gate_stage1_global_budget(&day, global_daily_cap, "extract", "extract rows")
            {
                BudgetGate::Exhausted => break,
                BudgetGate::SkipRow => continue,
                BudgetGate::Proceed => {}
            }

            // ROUTE BY CATEGORY: each specialist owns its own prompt, schema and
            // ledger line, so the row's category decides which one runs.
            match extractor {
                CategoryExtractor::Marketing => {
                    match marketing::classify(&self.http, url, api_key, cfg, provider, row).await {
                        Ok(marketing::ExtractOutcome::Ok(out, usage)) => {
                            if let Some(u) = usage {
                                in_tok += u.input_tokens;
                                out_tok += u.output_tokens;
                                if let Err(e) = self.store.extract_bump_usage(
                                    self.account_id,
                                    &day,
                                    marketing::LEDGER_CATEGORY,
                                    u.into(),
                                ) {
                                    eprintln!("squelch: extract usage ledger bump failed ({e})");
                                }
                            }
                            let applied = marketing::apply_result(row, &out, &cfg.model);
                            if let Err(e) = self.store.marketing_apply(&applied) {
                                // The call is already paid for: mark processed rather
                                // than re-buying it every cycle.
                                eprintln!(
                                    "squelch: marketing apply failed ({e}); row marked apply-failed"
                                );
                                let _ = self.store.extract_mark_processed(
                                    self.account_id,
                                    row.message_id,
                                    "apply-failed",
                                );
                            } else {
                                extracted += 1;
                            }
                        }
                        Ok(marketing::ExtractOutcome::Failed(kind))
                            if crate::triage::llm::is_config_failure(&kind) =>
                        {
                            // CONFIG-LEVEL FAILURE: a fact about the key,
                            // model, or gateway budget, not about this row.
                            // Marking it processed would foreclose the row
                            // forever even after the config is fixed, so leave
                            // it queued and stop the pass.
                            eprintln!(
                                "squelch: extract config-level failure ({kind}); the resolved \
                                 key/endpoint/model is wrong for the gateway; rows stay queued"
                            );
                            self.metrics.record_llm_config_failure();
                            self.refund_budget(STAGE1_GLOBAL_BUDGET_KEY, &day, "extract");
                            auth_failed = true;
                            break;
                        }
                        Ok(marketing::ExtractOutcome::Refused)
                        | Ok(marketing::ExtractOutcome::Failed(_)) => {
                            let _ = self.store.extract_mark_processed(
                                self.account_id,
                                row.message_id,
                                "extract-failed",
                            );
                            skipped += 1;
                        }
                        Err(e) => {
                            eprintln!("squelch: extract {e}; row stays queued");
                        }
                    }
                }
                CategoryExtractor::Banking => {
                    let outcome =
                        banking::classify(&self.http, url, api_key, cfg, provider, row).await;
                    match outcome {
                        Ok(banking::ExtractOutcome::Ok(out, usage)) => {
                            if let Some(u) = usage {
                                in_tok += u.input_tokens;
                                out_tok += u.output_tokens;
                                if let Err(e) = self.store.extract_bump_usage(
                                    self.account_id,
                                    &day,
                                    banking::LEDGER_CATEGORY,
                                    u.into(),
                                ) {
                                    eprintln!("squelch: extract usage ledger bump failed ({e})");
                                }
                            }
                            let applied = banking::apply_result(row, &out, &cfg.model);
                            if let Err(e) = self.store.banking_apply(&applied) {
                                // Failure sentinel rather than a re-queue: the call is
                                // already paid for, a store failure is unlikely to heal
                                // on a retry, and leaving the row queued would re-buy a
                                // call every cycle. Only the Banking record is lost — the
                                // email itself is still in the inbox.
                                eprintln!(
                                    "squelch: banking apply failed ({e}); row marked apply-failed"
                                );
                                let _ = self.store.extract_mark_processed(
                                    self.account_id,
                                    row.message_id,
                                    "apply-failed",
                                );
                            } else {
                                extracted += 1;
                            }
                        }
                        Ok(banking::ExtractOutcome::Failed(kind))
                            if crate::triage::llm::is_config_failure(&kind) =>
                        {
                            // See the marketing arm: a config-level rejection is
                            // not a verdict about this row, and marking it
                            // processed would forfeit it permanently.
                            eprintln!(
                                "squelch: extract config-level failure ({kind}); the resolved \
                                 key/endpoint/model is wrong for the gateway; rows stay queued"
                            );
                            self.metrics.record_llm_config_failure();
                            self.refund_budget(STAGE1_GLOBAL_BUDGET_KEY, &day, "extract");
                            auth_failed = true;
                            break;
                        }
                        Ok(banking::ExtractOutcome::Refused)
                        | Ok(banking::ExtractOutcome::Failed(_)) => {
                            // Mark processed so the row cannot loop; no specialist row is
                            // written, so nothing appears in the Banking zone.
                            let _ = self.store.extract_mark_processed(
                                self.account_id,
                                row.message_id,
                                "extract-failed",
                            );
                            skipped += 1;
                        }
                        Err(e) => {
                            // Retryable class exhausted / transport error: leave the row
                            // queued (extractor_model_used stays NULL) for a later cycle.
                            eprintln!("squelch: extract {e}; row stays queued");
                        }
                    }
                }
            }
        }

        // ---- SOURCE 2: the SHIPMENTS trigger queue -------------------------
        // Its own stale clock: see `ship_stale_cutoff`.
        let ship_cutoff = ship_stale_cutoff(Utc::now(), self.config.carriers.max_age_days);
        let ship_queued = if auth_failed {
            Vec::new()
        } else {
            Self::read_queue(
                self.store
                    .ship_extract_queue(self.account_id, cfg.batch_per_cycle),
                "ship-extract",
            )
        };

        for row in &ship_queued {
            // SEALED GUARD: the queue already excludes sealed rows in SQL;
            // re-check anyway before every classify call (docs/SECURITY.md).
            if let Err(e) = extract::extract_sealed_guard(row) {
                eprintln!("squelch: ship-extract sealed guard tripped ({e}); skipping row");
                continue;
            }

            // SKIP-STALE: mark processed WITHOUT a model call, so an old row
            // neither spends budget nor sits queued forever — and here too, a
            // hand-requested re-triage runs however old the mail is.
            if row.received_at < ship_cutoff && !retriage_forced(row.retriage_at, Utc::now()) {
                let _ =
                    self.store
                        .ship_extract_mark(self.account_id, row.message_id, STALE_SKIP_MODEL);
                ship_skipped += 1;
                continue;
            }

            // SHARED Stage-1 global budget, incremented before the call.
            match self.gate_stage1_global_budget(
                &day,
                global_daily_cap,
                "ship-extract",
                "shipment rows",
            ) {
                BudgetGate::Exhausted => break,
                BudgetGate::SkipRow => continue,
                BudgetGate::Proceed => {}
            }

            match shipments::classify(&self.http, url, api_key, cfg, provider, row).await {
                Ok(shipments::ExtractOutcome::Ok(out, usage)) => {
                    if let Some(u) = usage {
                        in_tok += u.input_tokens;
                        out_tok += u.output_tokens;
                        if let Err(e) = self.store.extract_bump_usage(
                            self.account_id,
                            &day,
                            shipments::LEDGER_CATEGORY,
                            u.into(),
                        ) {
                            eprintln!("squelch: ship-extract usage ledger bump failed ({e})");
                        }
                    }
                    let applied = shipments::apply_result(row, &out, &cfg.model);
                    match self.store.shipments_extract_apply(&applied) {
                        // The call is already paid for, and a store failure is
                        // unlikely to heal on a retry: mark the row rather than
                        // re-buying it every cycle.
                        Err(e) => {
                            eprintln!(
                                "squelch: shipments apply failed ({e}); row marked apply-failed"
                            );
                            let _ = self.store.ship_extract_mark(
                                self.account_id,
                                row.message_id,
                                "apply-failed",
                            );
                        }
                        // `false` is a REAL outcome, not a failure: a negative
                        // verdict (or a row sealed mid-pass) is a decision that
                        // wrote no tracked record. The marker is already stamped.
                        Ok(false) => ship_skipped += 1,
                        Ok(true) => ship_extracted += 1,
                    }
                }
                Ok(shipments::ExtractOutcome::Failed(kind))
                    if crate::triage::llm::is_config_failure(&kind) =>
                {
                    // See the marketing arm: the config is wrong for every
                    // row, so stamping this one would forfeit a shipping email
                    // permanently over a config mistake.
                    eprintln!(
                        "squelch: ship-extract config-level failure ({kind}); the resolved \
                         key/endpoint/model is wrong for the gateway; rows stay queued"
                    );
                    self.metrics.record_llm_config_failure();
                    self.refund_budget(STAGE1_GLOBAL_BUDGET_KEY, &day, "ship-extract");
                    break;
                }
                Ok(shipments::ExtractOutcome::Refused)
                | Ok(shipments::ExtractOutcome::Failed(_)) => {
                    // Mark processed so the row cannot loop; no shipment record is
                    // written, so nothing changes in the shipments zone.
                    let _ = self.store.ship_extract_mark(
                        self.account_id,
                        row.message_id,
                        "extract-failed",
                    );
                    ship_skipped += 1;
                }
                Err(e) => {
                    // Retryable class exhausted / transport error: the row stays
                    // 'pending' and a later cycle retries it.
                    eprintln!("squelch: ship-extract {e}; row stays queued");
                }
            }
        }

        if extracted > 0 || skipped > 0 || ship_extracted > 0 || ship_skipped > 0 {
            eprintln!(
                "squelch: extract processed {extracted} rows (model={}, in_tok={in_tok}, \
                 out_tok={out_tok}); skipped {skipped}; shipments {ship_extracted}, \
                 skipped {ship_skipped}",
                cfg.model
            );
        }
    }

    /// Run one Stage-2 LLM triage pass over the queued (non-confident) rows:
    /// up to `batch_per_cycle` rows (`model_used IS NULL AND
    /// sensitivity='normal'`), sequentially. Per row — sealed guard, the three
    /// daily budget checks, increment BEFORE the call so retry storms can't
    /// exceed a cap, classify, apply. Budget exhaustion leaves rows queued. Any
    /// per-row failure is logged redacted and never crashes the sync loop.
    /// No-op when Stage-2 is disabled (no API key).
    async fn stage2_pass(&self) {
        let Some(PassSetup {
            api_key,
            provider,
            url,
            caps,
            day,
            stale_cutoff,
        }) = self.pass_setup()
        else {
            return;
        };
        let cfg = &self.config.stage2;
        let thread_daily_cap = caps.thread_daily_cap.unwrap_or(cfg.thread_daily_cap);
        let sender_daily_cap = caps.sender_daily_cap.unwrap_or(cfg.sender_daily_cap);
        let global_daily_cap = caps.global_daily_cap.unwrap_or(cfg.global_daily_cap);

        let queued = Self::read_queue(
            self.store
                .stage2_queue(self.account_id, cfg.batch_per_cycle),
            "stage-2",
        );
        if queued.is_empty() {
            return;
        }

        let mut processed = 0usize;
        let mut stale_skipped = 0usize;
        let mut in_tok = 0u64;
        let mut out_tok = 0u64;

        for row in &queued {
            // SEALED GUARD: the queue already excludes sealed rows in SQL;
            // re-check before every classify call (docs/SECURITY.md).
            if let Err(e) = stage2_sealed_guard(row) {
                eprintln!("squelch: stage-2 sealed guard tripped ({e}); skipping row");
                continue;
            }

            // SKIP-STALE: mark processed WITHOUT a model call, keeping Stage-1
            // values, so the row neither spends budget nor sits queued forever.
            // A hand-requested re-triage overrules the cutoff: half a re-triage,
            // with Stage-1 redone and the escalation it asked for skipped, is not
            // the verdict the user asked to be redone.
            if row.received_at < stale_cutoff && !retriage_forced(row.retriage_at, Utc::now()) {
                let _ = self.store.stage2_mark_processed(
                    self.account_id,
                    row.message_id,
                    STALE_SKIP_MODEL,
                );
                stale_skipped += 1;
                self.metrics.record_stage2(Stage2Verdict::StaleSkipped);
                continue;
            }

            // GLOBAL budget check: once the account cap is hit, BREAK — every
            // remaining row this cycle is blocked.
            match self
                .store
                .stage2_budget_used(self.account_id, GLOBAL_BUDGET_KEY, &day)
            {
                Ok(used) if used >= global_daily_cap => {
                    if self.warn_once_per_day(CapKind::Global, &day) {
                        eprintln!(
                            "squelch: stage-2 global daily budget exhausted ({used}/{global_daily_cap}); \
                             remaining rows stay queued"
                        );
                    }
                    break; // global cap blocks every remaining row this cycle
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("squelch: stage-2 global budget read failed ({e}); skipping row");
                    continue;
                }
            }

            // PER-THREAD budget check; the notice names the capped thread.
            match self
                .store
                .stage2_budget_used(self.account_id, &row.thread_id, &day)
            {
                Ok(used) if used >= thread_daily_cap => {
                    if self.warn_once_per_day(CapKind::Thread, &day) {
                        // thread_id is Gmail hex, but sanitize defensively in
                        // case a malformed cursor ever supplies otherwise.
                        eprintln!(
                            "squelch: stage-2 per-thread daily budget exhausted for thread {} \
                             ({used}/{thread_daily_cap}); those rows stay queued",
                            sanitize_ascii(&row.thread_id, 64)
                        );
                    }
                    continue; // this thread is capped; try the next row
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("squelch: stage-2 thread budget read failed ({e}); skipping row");
                    continue;
                }
            }

            // PER-SENDER budget check, keyed by from_addr: stops one chatty
            // sender fanning many DIFFERENT threads from burning the budget.
            let sender_key = format!("{SENDER_BUDGET_PREFIX}{}", row.from_addr);
            match self
                .store
                .stage2_budget_used(self.account_id, &sender_key, &day)
            {
                Ok(used) if used >= sender_daily_cap => {
                    if self.warn_once_per_day(CapKind::Sender, &day) {
                        // from_addr is UNTRUSTED header PII: log the
                        // non-reversible tag, never the address.
                        eprintln!(
                            "squelch: stage-2 per-sender daily budget exhausted for sender {} \
                             ({used}/{sender_daily_cap}); those rows stay queued",
                            redact_sender(&row.from_addr)
                        );
                    }
                    continue; // this sender is capped; try the next row
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("squelch: stage-2 sender budget read failed ({e}); skipping row");
                    continue;
                }
            }

            // Increment ALL THREE budgets BEFORE the call so the attempt counts
            // even if it errors or retries.
            if let Err(e) =
                self.store
                    .stage2_increment_budget(self.account_id, GLOBAL_BUDGET_KEY, &day)
            {
                eprintln!("squelch: stage-2 global budget increment failed ({e}); skipping row");
                continue;
            }
            if let Err(e) =
                self.store
                    .stage2_increment_budget(self.account_id, &row.thread_id, &day)
            {
                eprintln!("squelch: stage-2 thread budget increment failed ({e}); skipping row");
                continue;
            }
            if let Err(e) = self
                .store
                .stage2_increment_budget(self.account_id, &sender_key, &day)
            {
                eprintln!("squelch: stage-2 sender budget increment failed ({e}); skipping row");
                continue;
            }

            let ctx = RowContext::from_queued(row, cfg.max_body_chars);
            let outcome = stage2::classify(&self.http, url, api_key, cfg, provider, &ctx).await;

            match outcome {
                Ok(ClassifyOutcome::Ok(out, usage)) => {
                    if let Some(u) = usage {
                        in_tok += u.input_tokens;
                        out_tok += u.output_tokens;
                        // USAGE LEDGER, best-effort: a ledger write failure must
                        // not affect triage.
                        if let Err(e) =
                            self.store
                                .stage2_bump_usage(self.account_id, &day, u.into())
                        {
                            eprintln!("squelch: stage-2 usage ledger bump failed ({e})");
                        }
                    }
                    let applied = stage2::apply_result(
                        row,
                        &out,
                        &cfg.model,
                        self.config.stage1.known_contact_importance,
                        Utc::now(),
                    );
                    match self.store.stage2_apply(&applied) {
                        Err(e) => {
                            eprintln!("squelch: stage-2 apply failed ({e}); row stays queued");
                        }
                        // TOCTOU: sealed by hand mid-pass, so no verdict landed
                        // and there is nothing to notify for.
                        Ok(false) => {}
                        Ok(true) => {
                            processed += 1;
                            self.metrics.record_stage2(Stage2Verdict::Ok);
                            // Stage-2's schedule REPLACES Stage-1's pending one:
                            // this is the verdict that stands, so its idea of
                            // when to look again is the one that should.
                            self.schedule_revisits(
                                row.message_id,
                                &out.revisit,
                                applied.deadline.as_ref(),
                                Utc::now(),
                            );
                            // Same re-triage exemption as the Stage-1 site above:
                            // a row a human dragged back is being READ AGAIN, not
                            // missed. See [`ExpiryCount`].
                            let emit_now = Utc::now();
                            self.emit_deliberate(
                                &events::EventContext {
                                    account_id: self.account_id,
                                    message_id: row.message_id,
                                    thread_id: &row.thread_id,
                                    sender: &row.from_addr,
                                    one_line: &applied.one_line,
                                    notify_eligible_at: row.notify_eligible_at,
                                    sensitivity: row.sensitivity,
                                    // The Stage-2 queue selects `m.is_sent = 0`
                                    // and `m.is_spam = 0`.
                                    is_sent: false,
                                    is_spam: false,
                                    // Read NOW, not at ingest: the row only
                                    // records the rule in force when it was
                                    // queued, so a sender squelched since then
                                    // would otherwise still push.
                                    rule: self.current_rule(&row.from_addr),
                                    tier: applied.tier,
                                    importance: applied.importance,
                                    deadline: applied.deadline.as_ref(),
                                },
                                emit_now,
                                ExpiryCount::from_retriage(row.retriage_at, emit_now),
                                &cfg.model,
                            );
                        }
                    }
                }
                Ok(ClassifyOutcome::Refused) => {
                    // Keep Stage-1 values; mark processed so it doesn't loop.
                    // Redacted: no body/subject logged. The stamp is
                    // STAGE2_REFUSED, never the model id: a row stamped with
                    // the model reads as "the model said this", and during the
                    // 2026-08-19 outage that lie cost the diagnosis its first
                    // hour. Same lesson as stale-skip vs heuristic-only.
                    eprintln!("squelch: stage-2 refusal (redacted); keeping stage-1 values");
                    self.metrics.record_stage2(Stage2Verdict::Refused);
                    let _ = self.store.stage2_mark_processed(
                        self.account_id,
                        row.message_id,
                        stage2::STAGE2_REFUSED,
                    );
                }
                Ok(ClassifyOutcome::Failed(kind)) => {
                    // CONFIG-LEVEL FAILURE (4xx shared by every row: bad key,
                    // disallowed model, spent gateway budget). Leave the row
                    // queued and STOP the pass: the remaining rows would fail
                    // identically while burning the daily caps, and a row
                    // marked processed here would be foreclosed from triage
                    // even after the config is fixed (the 2026-08-19
                    // model-allowlist outage foreclosed two days of mail
                    // exactly this way).
                    if crate::triage::llm::is_config_failure(&kind) {
                        eprintln!(
                            "squelch: stage-2 config-level failure ({kind}) at message {}; the \
                             resolved key/endpoint/model is wrong for the gateway; rows stay \
                             queued",
                            row.message_id
                        );
                        self.metrics.record_llm_config_failure();
                        self.metrics.record_stage2(Stage2Verdict::Retryable);
                        // ALL THREE budgets this row charged before the call,
                        // in the same order they were taken. Refunding only the
                        // global one would leave the thread and sender caps
                        // silently eroded by an outage, which is the same bug
                        // one scope down: a thread cap is small, so a handful
                        // of config failures could park one conversation for
                        // the rest of the day.
                        self.refund_budget(GLOBAL_BUDGET_KEY, &day, "stage-2 global");
                        self.refund_budget(&row.thread_id, &day, "stage-2 thread");
                        self.refund_budget(&sender_key, &day, "stage-2 sender");
                        break;
                    }
                    // Row-level permanent failure (truncation/parse): mark the
                    // row processed so it cannot loop, stamped with the failure
                    // kind rather than the model id — the model never answered.
                    // `kind` is already redacted.
                    eprintln!("squelch: stage-2 permanent failure ({kind}); marking row failed");
                    self.metrics.record_stage2(Stage2Verdict::Failed);
                    let _ = self.store.stage2_mark_processed(
                        self.account_id,
                        row.message_id,
                        &stage2::failed_stamp(&kind),
                    );
                }
                Err(e) => {
                    // Retryable class exhausted / transport error: leave the row
                    // queued for a later cycle. `e` is redacted.
                    eprintln!("squelch: stage-2 {e}; row stays queued");
                    self.metrics.record_stage2(Stage2Verdict::Retryable);
                }
            }
        }

        if processed > 0 || stale_skipped > 0 {
            eprintln!(
                "squelch: stage-2 processed {processed} rows (model={}, in_tok={in_tok}, \
                 out_tok={out_tok}); stale-skipped {stale_skipped}",
                cfg.model
            );
        }
    }

    /// `users.labels.get(INBOX)` -> Gmail's own unread counts, into the store.
    ///
    /// The ONLY source for these numbers: the read scope cannot see (or write)
    /// read state through any other call, and our tables hold just the backfill
    /// window, so nothing local can answer "how much unread mail is sitting in
    /// Gmail right now".
    ///
    /// Cannot fail a cycle — sync's job is mail, and a stale count is a far
    /// better outcome than a poll loop that gives up over a cosmetic number. On
    /// failure of EITHER half (the fetch or the store write) the previous row
    /// stands, no clearing and no zeroing, and the notice is printed once per
    /// failing streak rather than every poll.
    async fn refresh_inbox_unread(&self) {
        let url = format!("{}/labels/{LABEL_INBOX}", self.api_base);
        // Both halves land in one Result so both go through the one latch:
        // whichever way this fails, it fails every poll tick, and a branch that
        // printed on its own would be the branch that spams.
        let outcome = match self.get_json::<LabelResp>(&url).await {
            Ok(label) => self
                .store
                .set_inbox_unread(self.account_id, label.messages_unread, label.threads_unread)
                .map_err(|e| format!("storing the inbox unread counts failed ({e})")),
            Err(e) => Err(format!("inbox unread count fetch failed ({e})")),
        };
        match outcome {
            // Re-armed only by a whole cycle that worked, fetch and write both.
            Ok(()) => self.unread_warned.store(false, Ordering::Relaxed),
            // `swap` is the arm-and-test: only the transition into failure
            // prints. Error strings from this crate carry no secrets.
            Err(why) => {
                if !self.unread_warned.swap(true, Ordering::Relaxed) {
                    eprintln!("squelch: {why}; keeping the last known counts");
                }
            }
        }
    }

    /// `users.getProfile` -> the account's current historyId.
    async fn fetch_profile_history_id(&self) -> Result<u64> {
        let url = format!("{}/profile", self.api_base);
        let profile: ProfileResp = self.get_json(&url).await?;
        Ok(parse_history_id(&profile.history_id))
    }

    // ---- historyId cursor persistence (sync_state, key='history') ----------

    fn load_history_cursor(&self) -> Result<Option<u64>> {
        Ok(self
            .store
            .sync_state(self.account_id, HISTORY_KEY)?
            .map(|s| s.last_uid))
    }

    fn store_history_cursor(&self, history_id: u64) -> Result<()> {
        self.store.set_sync_state(
            self.account_id,
            HISTORY_KEY,
            &SyncState {
                uidvalidity: 0,
                last_uid: history_id,
            },
        )
    }

    fn backfill_since(&self) -> DateTime<Utc> {
        Utc::now() - ChronoDuration::days(self.config.sync.backfill_days as i64)
    }

    fn rules_for_stage2_note() -> &'static str {
        // Documentation anchor for the Stage-2 queue predicate: non-confident
        // rows are the ones left with model_used NULL.
        "model_used IS NULL AND sensitivity='normal'"
    }

    /// The top-level driver: loop, retrying with exponential backoff on any
    /// error, until shutdown is signalled.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let _ = Self::rules_for_stage2_note();
        let mut backoff = BACKOFF_START;
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            match self.run_once(&mut shutdown).await {
                Ok(()) => {
                    self.metrics.record_sync_ok();
                    return Ok(());
                }
                Err(e) => {
                    self.metrics.record_sync_error();
                    // GMAIL IS THE THING THAT FAILED — only the poll lane can
                    // return an `Err` at all — so the refine lane must not spend
                    // on the next cycle until a poll has proved otherwise. Without
                    // this, every backoff cycle re-entered `run_lanes` and burned
                    // a Stage-1 and a Stage-2 unit of a DAY-scoped cap on classify
                    // calls that were cancelled one Gmail round trip later. See
                    // the `poll_healthy` field.
                    self.poll_healthy.store(false, Ordering::Relaxed);
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                    // Error strings from this crate never carry secrets.
                    eprintln!(
                        "squelch: sync error ({e}); retrying in {}s",
                        backoff.as_secs()
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() { return Ok(()); }
                        }
                    }
                    backoff = (backoff * 2).min(BACKOFF_CAP);
                }
            }
        }
    }
}

/// Which [`GmailErrorKind`] a non-2xx Gmail response is.
///
/// 429 is unambiguous. 403 is NOT: Google returns it both for "you are asking
/// too fast" and for "this credential may not do that", separated only by the
/// `reason` in the JSON body. Everything else is `http`, which is the bucket
/// that means "read the logs".
fn classify_gmail_status(status: u16, body: &str) -> GmailErrorKind {
    const QUOTA_REASONS: [&str; 4] = [
        "ratelimitexceeded",
        "userratelimitexceeded",
        "quotaexceeded",
        "dailylimitexceeded",
    ];
    match status {
        401 => GmailErrorKind::Auth,
        429 => GmailErrorKind::Quota,
        // Bounded: an error body is a few hundred bytes, and the reason sits at
        // the front of it. Never logged, only matched.
        403 => {
            let head: String = body.chars().take(2048).collect::<String>().to_lowercase();
            if QUOTA_REASONS.iter().any(|r| head.contains(r)) {
                GmailErrorKind::Quota
            } else {
                // A scope/permission 403 stays in `http`: it is rare, it is not
                // a credential this daemon can refresh its way out of, and the
                // stderr line already names it.
                GmailErrorKind::Http
            }
        }
        _ => GmailErrorKind::Http,
    }
}

/// Minimal percent-encoding for a Gmail `q` value. Enough for `newer_than:Nd`
/// and simple queries; arbitrary user queries are never built here.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Gmail `internalDate` is milliseconds-since-epoch as a decimal string. `pub`
/// so squelch-api's send-echo parses it from one definition.
pub fn parse_internal_date(s: Option<&str>) -> Option<DateTime<Utc>> {
    let ms: i64 = s?.trim().parse().ok()?;
    DateTime::from_timestamp_millis(ms)
}

/// Rebuild a header-only RFC822 blob from Gmail metadata headers so mail-parser
/// runs over it unchanged; the trailing blank line ends the header section
/// (empty body). Used by the Sent-contacts harvest (`format=metadata` carries
/// headers only) and the contacts-seeding tests.
fn synthesize_rfc822_headers(headers: &[MessageHeader]) -> String {
    let mut out = String::new();
    for h in headers {
        // HEADER INJECTION GUARD: Gmail names and values are single-line, but
        // upstream is never trusted blindly — a CR/LF in either would splice a
        // synthetic header into the blob.
        if h.name.contains('\r')
            || h.name.contains('\n')
            || h.value.contains('\r')
            || h.value.contains('\n')
        {
            continue;
        }
        out.push_str(&h.name);
        out.push_str(": ");
        out.push_str(&h.value);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out
}

/// Type alias helper so callers can name the concrete rule slice.
pub type Rules = Vec<SenderRule>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Stage1Config;
    use crate::store::SpamScope;
    use crate::store::SqliteStore;
    use crate::types::{Disposition, NewMessage, Tier, TriageAxis};

    /// REGRESSION GUARD: the shipments section must NOT age rows out on the
    /// shared Stage-2 clock. A 30-day-old order confirmation is exactly what the
    /// trigger's migration backfill exists to catch, and the seven-day window
    /// every other pass uses would stale-skip it unread.
    #[test]
    fn the_shipments_stale_cutoff_is_the_carrier_horizon_not_the_stage2_one() {
        let cfg = crate::config::Config::default();
        let now = Utc::now();

        let ship = ship_stale_cutoff(now, cfg.carriers.max_age_days);
        assert_eq!(ship, now - ChronoDuration::days(45));

        // What `PassSetup` hands the other passes, computed the same way.
        let stage2 = now - ChronoDuration::days(cfg.stage2.max_age_days as i64);
        assert!(ship < stage2, "the shipments horizon is the wider one");

        let month_old = now - ChronoDuration::days(30);
        assert!(
            month_old >= ship,
            "a 30-day-old order still reaches the model"
        );
        assert!(
            month_old < stage2,
            "...though every other pass would skip it"
        );

        // Tracks the config rather than a constant, so raising the poller's
        // horizon widens the extractor's with it.
        assert_eq!(ship_stale_cutoff(now, 10), now - ChronoDuration::days(10));
    }

    /// The 403 split is the whole reason this classifier reads the body: Google
    /// spends one status on "too fast" and on "not allowed", and only the reason
    /// string tells them apart.
    #[test]
    fn gmail_status_classification_splits_403_on_the_reason() {
        let quota_body = r#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#;
        assert_eq!(
            classify_gmail_status(403, quota_body),
            GmailErrorKind::Quota
        );
        assert_eq!(
            classify_gmail_status(403, r#"{"error":{"errors":[{"reason":"forbidden"}]}}"#),
            GmailErrorKind::Http
        );
        assert_eq!(classify_gmail_status(429, ""), GmailErrorKind::Quota);
        assert_eq!(classify_gmail_status(401, ""), GmailErrorKind::Auth);
        assert_eq!(classify_gmail_status(500, ""), GmailErrorKind::Http);
    }

    /// Build a RawFetched from an RFC822 string, as the transport layer would.
    /// The account's own address is fixed to `me@example.com` in these fixtures.
    fn fixture(account_id: AccountId, msgid: &str, eml: &str, is_sent: bool) -> RawFetched {
        RawFetched {
            account_id,
            gmail_msg_id: msgid.to_string(),
            gmail_thread_id: None,
            raw: eml.as_bytes().to_vec(),
            internal_date: Some(Utc::now()),
            is_sent,
            is_spam: false,
            account_addr: "me@example.com".to_string(),
        }
    }

    /// End-to-end through the real store: ingest_with_rules -> ingest_message.
    fn ingest_into(
        store: &SqliteStore,
        account_id: AccountId,
        f: &RawFetched,
        now: DateTime<Utc>,
    ) -> i64 {
        let rules = store.list_sender_rules(account_id).unwrap();
        let triaged = ingest_with_rules(f, &Stage1Config::default(), now, &rules, |addr| {
            store.is_known_contact(account_id, addr).unwrap_or(false)
        });
        store.ingest_message(&triaged).unwrap()
    }

    // ---- notification events at the ingest call site -----------------------
    //
    // These drive the real pipeline through the real store; only the HTTP fetch
    // above it is out of reach, which is why the helper repeats the engine's two
    // lines of gating instead of calling `ingest_one` directly.

    /// The engine's ingest path with NO LLM configured, driven end to end:
    /// ingest, build the candidate, run the REAL fast lane. Returns
    /// `(message_id, emitted_event_id)`.
    ///
    /// IT IS NO LONGER A MIRROR OF AN EMISSION SITE, because there is no longer
    /// an emission site at ingest to mirror. The confident-seed-with-no-model
    /// path moved into [`notify_lane`] (docs/NOTIFY.md §11.5, `Model` step 1),
    /// which is what these tests now drive: the same three lines
    /// `fetch_raw_and_ingest` runs, awaited instead of spawned so an assertion
    /// cannot race the decision.
    ///
    /// The eligibility stamp is computed by the engine's own
    /// [`notify_eligible_stamp`], not re-derived, so a change to the §11.3 rule
    /// cannot pass these tests while breaking the daemon.
    async fn ingest_and_notify(
        store: &Arc<SqliteStore>,
        account_id: AccountId,
        f: &RawFetched,
        now: DateTime<Utc>,
        origin: IngestOrigin,
    ) -> (i64, Option<i64>) {
        let cfg = crate::config::NotifyConfig::default();
        let (id, triaged, rules) = ingest_stamped(store, account_id, f, now, origin, &cfg);
        let before = store.latest_event_id(account_id).unwrap();
        if let Some(c) = notify_lane::candidate(&triaged, id, &rules, &cfg, |addr| {
            store.is_known_contact(account_id, addr).unwrap_or(false)
        }) {
            let lane = Arc::new(notify_lane::NotifyLane::new(
                store.clone(),
                reqwest::Client::new(),
                cfg,
                Stage1Config::default().known_contact_importance,
                // NO MODEL, which is the whole premise: with nothing to wait for
                // the confident seed is the final word.
                None,
                SyncMetrics::new(),
                account_id,
                Arc::new(std::sync::Mutex::new(WarnDays::default())),
            ));
            lane.run(c).await;
        }
        let emitted = store
            .events_after(account_id, before, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.message_id == id)
            .map(|e| e.id);
        (id, emitted)
    }

    /// The engine's ingest path with an LLM configured: the row is committed
    /// carrying its eligibility stamp and NOTHING is emitted, because the
    /// promise is that a model verdict is coming and the refine site will emit.
    /// That is the state every refine-site test needs to start from.
    ///
    /// Returns the row's stamp so a test can assert on it directly — the whole
    /// point of §11.3 is that this one value, not the sender's `Date:`, decides
    /// what the refine sites are allowed to do.
    fn ingest_deferring_to_refine(
        store: &SqliteStore,
        account_id: AccountId,
        f: &RawFetched,
        now: DateTime<Utc>,
        origin: IngestOrigin,
    ) -> (i64, Option<DateTime<Utc>>) {
        let cfg = crate::config::NotifyConfig::default();
        let (id, triaged, _) = ingest_stamped(store, account_id, f, now, origin, &cfg);
        (id, triaged.notify_eligible_at)
    }

    /// Shared spine of the two helpers above: triage, stamp, commit.
    fn ingest_stamped(
        store: &SqliteStore,
        account_id: AccountId,
        f: &RawFetched,
        now: DateTime<Utc>,
        origin: IngestOrigin,
        cfg: &crate::config::NotifyConfig,
    ) -> (i64, TriagedMessage, Vec<crate::types::SenderRule>) {
        let rules = store.list_sender_rules(account_id).unwrap();
        let mut triaged = ingest_with_rules(f, &Stage1Config::default(), now, &rules, |addr| {
            store.is_known_contact(account_id, addr).unwrap_or(false)
        });
        triaged.notify_eligible_at = notify_eligible_stamp(&triaged, origin, cfg, now);
        let id = store.ingest_message(&triaged).unwrap();
        (id, triaged, rules)
    }

    /// The eligibility stamp as the DATABASE holds it, read back through a
    /// store method rather than off the `TriagedMessage` the test just built.
    /// That round trip is the assertion: a stamp that never reached SQLite, or
    /// one the `DO UPDATE SET` quietly rewrote, would pass every in-memory check
    /// and still leave the emission sites reading the wrong clock.
    ///
    /// `triage_seed_verdict` because it is the one read that carries the column
    /// for ANY normal row, sent copies included — the queue SELECTs filter
    /// `m.is_sent = 0`, so they cannot see half of what these tests assert on.
    fn stamp_of(
        store: &SqliteStore,
        account_id: AccountId,
        message_id: i64,
    ) -> Option<DateTime<Utc>> {
        store
            .triage_seed_verdict(account_id, message_id)
            .unwrap()
            .expect("a triage row for this message")
            .notify_eligible_at
    }

    /// An ops-alert EML dated `at`: automated sender + alert language, so it
    /// lands Signal / importance 75 / CONFIDENT.
    fn alert_eml(at: DateTime<Utc>) -> String {
        format!(
            "From: Monitoring <alerts@monitoring.example>\r\n\
             To: me@example.com\r\n\
             Subject: Incident: checkout api is down\r\n\
             Date: {}\r\n\
             \r\n\
             A high-severity incident was opened for the checkout service.\r\n",
            at.to_rfc2822()
        )
    }

    #[tokio::test]
    async fn fresh_worthy_ingest_emits_exactly_one_event() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eml = alert_eml(now);
        let f = fixture(acct, "g-alert", &eml, false);

        let (mid, ev_id) =
            ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental).await;
        let ev_id = ev_id.expect("a fresh confident alert above the line must notify");

        let ev = store.event_by_id(acct, ev_id).unwrap().expect("event row");
        assert_eq!(ev.message_id, mid);
        assert_eq!(ev.kind, crate::types::EventKind::Surfaced);
        assert_eq!(ev.tier, Tier::Signal);
        assert_eq!(ev.sender, "alerts@monitoring.example");
        assert_eq!(store.latest_event_id(acct).unwrap(), ev_id);

        // AND THE ROW CARRIES ITS STAMP, read back out of SQLite. The event
        // above proves the decision; this proves the FACT the decision was made
        // from survived the write, which is what every later refine site reads.
        assert_eq!(
            stamp_of(&store, acct, mid),
            Some(now),
            "the stamp is the moment we first saw the message, to the nanosecond"
        );

        // RE-INGEST (history overlap / catch-up re-scan) must stay silent.
        let (mid2, again) =
            ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental).await;
        assert_eq!(mid2, mid, "same message row");
        assert_eq!(again, None, "one event per message, ever");
        assert_eq!(store.events_after(acct, 0, 100).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn backfill_never_emits() {
        // A fresh install backfills a month of already-read mail. Not one push.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eml = alert_eml(now);
        let f = fixture(acct, "g-alert", &eml, false);

        let (mid, ev) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Backfill).await;
        assert_eq!(ev, None, "backfill is structurally silent");
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        assert_eq!(store.latest_event_id(acct).unwrap(), 0);

        // NULL, not "old": the mail itself is minutes fresh and the verdict is
        // above the line. The origin alone decided it, and the NULL is what
        // makes the silence hold at every later emission site too, not just at
        // this one.
        assert_eq!(stamp_of(&store, acct, mid), None);
    }

    #[tokio::test]
    async fn stale_mail_is_silent_even_at_the_top_tier() {
        // THE FIRST-SIGHT TEST: a past-due bill from a KNOWN biller is the
        // loudest verdict the pipeline can produce, and mail that was already
        // three days old when we first saw it earns no stamp, so it is silent
        // anyway. This is what makes `catch_up()`'s whole-window re-scan safe.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let old = now - ChronoDuration::days(3);

        // Seed the biller as a known contact so the bill lands CONFIDENT PastDue.
        let seed = format!(
            "From: me@example.com\r\n\
             To: Utility <billing@utilityco.example>\r\n\
             Subject: account setup\r\n\
             Date: {}\r\n\
             \r\n\
             hello\r\n",
            old.to_rfc2822()
        );
        let sf = fixture(acct, "g-seed", &seed, /* is_sent */ true);
        ingest_and_notify(&store, acct, &sf, now, IngestOrigin::Incremental).await;

        let eml = format!(
            "From: Utility <billing@utilityco.example>\r\n\
             To: me@example.com\r\n\
             Subject: PAST DUE: Your electric bill\r\n\
             Date: {}\r\n\
             \r\n\
             Amount due $84.20. This payment is overdue.\r\n",
            old.to_rfc2822()
        );
        let f = fixture(acct, "g-pastdue", &eml, false);
        let (mid, ev) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental).await;
        assert_eq!(
            ev, None,
            "old mail is silent no matter what the verdict says"
        );
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        assert_eq!(
            stamp_of(&store, acct, mid),
            None,
            "mail already stale at first sight earns no stamp, so it is silent \
             at every emission site and not just this one"
        );

        // Sanity: the guard stopped it, not a mis-triage.
        let updates = store
            .ranked_updates(acct, old - ChronoDuration::days(1), None)
            .unwrap();
        let bill = updates
            .iter()
            .find(|u| u.id == mid)
            .expect("bill surfaced in the client");
        assert_eq!(bill.tier, Tier::PastDue);
    }

    #[tokio::test]
    async fn sealed_mail_never_emits_an_event() {
        // SEAL INVARIANT end to end: an OTP must never reach a lock screen.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eml = format!(
            "From: Bank <noreply@bank.example>\r\n\
             To: me@example.com\r\n\
             Subject: Your verification code\r\n\
             Date: {}\r\n\
             \r\n\
             Your one-time passcode is 483920. Enter this code to continue.\r\n",
            now.to_rfc2822()
        );
        let f = fixture(acct, "g-otp", &eml, false);
        let (_, ev) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental).await;
        assert_eq!(ev, None, "sealed mail must never notify");
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        assert_eq!(
            store.sealed_messages(acct).unwrap().len(),
            1,
            "it WAS sealed"
        );
    }

    #[tokio::test]
    async fn the_users_own_sent_copy_is_never_stamped() {
        // The user's own outbox is on the INCREMENTAL path and is as fresh as
        // mail gets, so neither of the other two arms of the stamp rule stops
        // it. `is_sent` is its own arm for exactly that reason: buzzing someone
        // about the mail they just sent is the most obviously wrong
        // notification the system could produce.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let eml = format!(
            "From: me@example.com\r\n\
             To: Utility <billing@utilityco.example>\r\n\
             Subject: PAST DUE: Your electric bill\r\n\
             Date: {}\r\n\
             \r\n\
             Paying this now, sorry for the delay.\r\n",
            now.to_rfc2822()
        );
        let f = fixture(acct, "g-mine", &eml, /* is_sent */ true);
        let (mid, ev) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental).await;

        assert_eq!(
            stamp_of(&store, acct, mid),
            None,
            "a sent copy is unstamped"
        );
        assert_eq!(ev, None);
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
    }

    #[test]
    fn a_re_ingest_keeps_the_first_sight_stamp_and_a_null_stays_null() {
        // WRITTEN ONCE, ON FIRST INSERT. Both halves matter and they fail in
        // opposite directions: a stamp that got REFRESHED on every re-ingest
        // would hand a row an unbounded rescue window, so a catch-up touching a
        // day-old message could buzz for it; a NULL that got FILLED IN on
        // re-ingest would let the first catch-up after a backfill push the
        // month of archived mail the backfill deliberately silenced.
        //
        // Both are live paths, not hypotheticals: the history walk overlaps and
        // `catch_up` re-fetches the whole window, so almost every row in a
        // long-lived database is ingested more than once.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let first = Utc::now();
        // Inside the freshness window, so a naive recompute would produce a
        // NEW stamp rather than nothing. That is what makes this discriminate.
        let again = first + ChronoDuration::minutes(5);

        let eml = alert_eml(first);
        let f = fixture(acct, "g-alert", &eml, false);
        let (mid, _) =
            ingest_deferring_to_refine(&store, acct, &f, first, IngestOrigin::Incremental);
        assert_eq!(stamp_of(&store, acct, mid), Some(first));

        let (mid2, _) =
            ingest_deferring_to_refine(&store, acct, &f, again, IngestOrigin::Incremental);
        assert_eq!(mid2, mid, "same message row");
        assert_eq!(
            stamp_of(&store, acct, mid),
            Some(first),
            "the ORIGINAL stamp, not the fresher one: the rescue window is \
             measured from when we first saw the mail, and a re-scan is not a \
             new sighting"
        );

        // The other direction: a backfilled row re-ingested as fresh
        // incremental mail stays NULL.
        let back = alert_eml(first).replace("alerts@monitoring.example", "alerts@second.example");
        let bf = fixture(acct, "g-back", &back, false);
        let (bmid, _) =
            ingest_deferring_to_refine(&store, acct, &bf, first, IngestOrigin::Backfill);
        assert_eq!(stamp_of(&store, acct, bmid), None);
        let (bmid2, _) =
            ingest_deferring_to_refine(&store, acct, &bf, again, IngestOrigin::Incremental);
        assert_eq!(bmid2, bmid);
        assert_eq!(
            stamp_of(&store, acct, bmid),
            None,
            "a NULL stays NULL: the backfill's silence is permanent, and a \
             later catch-up cannot talk it out of it"
        );
    }

    #[tokio::test]
    async fn squelched_sender_and_noise_are_both_silent() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        // A SQUELCH rule outranks the heuristic that would otherwise have
        // surfaced this sender (proved above).
        store
            .set_sender_rule(
                acct,
                "*@monitoring.example",
                "not urgent",
                Disposition::Squelch,
            )
            .unwrap();
        let eml = alert_eml(now);
        let f = fixture(acct, "g-alert", &eml, false);
        let (_, ev) = ingest_and_notify(&store, acct, &f, now, IngestOrigin::Incremental).await;
        assert_eq!(ev, None, "a squelch-ruled sender is silent");

        // Plain below-the-line noise: fresh, confident, and simply not important.
        let news = format!(
            "From: News <hello@newsletter.example>\r\n\
             To: me@example.com\r\n\
             Subject: This week in widgets\r\n\
             Date: {}\r\n\
             \r\n\
             Lots of widget news. Click here to unsubscribe from these emails.\r\n",
            now.to_rfc2822()
        );
        let nf = fixture(acct, "g-news", &news, false);
        let (_, ev) = ingest_and_notify(&store, acct, &nf, now, IngestOrigin::Incremental).await;
        assert_eq!(ev, None, "noise below the line is silent");

        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
    }

    /// Mirror of the engine's STAGE-1 apply emission site for a row the pass
    /// ALREADY HOLDS: apply via the real `stage1_apply`, and emit only when the
    /// guarded UPDATE matched, consulting the rule list as it stands NOW.
    fn refine_row_and_notify(
        store: &SqliteStore,
        account_id: AccountId,
        row: &crate::store::Stage1Queued,
        tier: Tier,
        importance: u8,
        now: DateTime<Utc>,
    ) -> Option<i64> {
        let cfg = crate::config::NotifyConfig::default();
        let applied = crate::store::Stage1Applied {
            message_id: row.message_id,
            account_id,
            importance,
            tier,
            one_line: "refined one-liner".into(),
            reason: "stage-1".into(),
            field_reasons: crate::types::FieldReasons::default(),
            stage1_model_used: "claude-haiku-4-5".into(),
            needs_stage2: false,
            escalation_reason: None,
            deadline: None,
            category: None,
        };
        // TOCTOU gate, as the engine has it: a verdict that did not land
        // (`false` — sealed mid-pass) must not emit.
        if !store.stage1_apply(&applied).unwrap() {
            return None;
        }
        let rules = store.list_sender_rules(account_id).unwrap();
        let rule = events::current_rule(&row.from_addr, &rules);
        let ctx = stage1_ctx(account_id, row, rule, tier, importance);
        events::event_for(&ctx, &cfg, now)
            .ok()
            .and_then(|ev| store.append_event(&ev).unwrap())
    }

    /// The `EventContext` the engine's Stage-1 apply site builds, in one place
    /// so the two mirrors above and below cannot drift from each other. The
    /// `one_line` is a fixed string because no test here is about the wording.
    ///
    /// `notify_eligible_at` comes OFF THE QUEUED ROW, exactly as the engine
    /// reads it: `received_at` (the sender's `Date:`) is no longer part of the
    /// emission decision at all.
    fn stage1_ctx<'a>(
        account_id: AccountId,
        row: &'a crate::store::Stage1Queued,
        rule: Option<Disposition>,
        tier: Tier,
        importance: u8,
    ) -> events::EventContext<'a> {
        events::EventContext {
            account_id,
            message_id: row.message_id,
            thread_id: &row.thread_id,
            sender: &row.from_addr,
            one_line: "refined one-liner",
            notify_eligible_at: row.notify_eligible_at,
            sensitivity: row.sensitivity,
            is_sent: false,
            is_spam: false,
            rule,
            tier,
            importance,
            deadline: None,
        }
    }

    /// The model id a deliberate emission site records, standing in for
    /// `stage1.model` / `stage2.model` in the tests that call
    /// [`SyncEngine::emit_deliberate`] directly. A fixed string so a ledger
    /// assertion is about the plumbing rather than about the default config.
    const STAGE_MODEL: &str = "claude-opus-5";

    /// Every ledger row this account has, oldest first — the eval read, over a
    /// window wide enough that nothing a test wrote can fall out of it.
    fn ledger(store: &SqliteStore, acct: AccountId) -> Vec<crate::store::NotifyDecisionRow> {
        store
            .notify_decisions_since(acct, Utc::now() - ChronoDuration::hours(24), 1000)
            .unwrap()
    }

    /// [`refine_row_and_notify`] when nothing is racing the queue read: fetch
    /// the queued row by id first.
    fn refine_and_notify(
        store: &SqliteStore,
        account_id: AccountId,
        message_id: i64,
        tier: Tier,
        importance: u8,
        now: DateTime<Utc>,
    ) -> Option<i64> {
        let row = store
            .stage1_queue(account_id, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == message_id)
            .expect("the row is queued for the stage-1 refine pass");
        refine_row_and_notify(store, account_id, &row, tier, importance, now)
    }

    #[test]
    fn a_row_sealed_mid_pass_lands_no_verdict_and_emits_nothing() {
        // TOCTOU: the pass SELECTs its queue, the user seals one of the held rows
        // (an OTP they spotted), and only THEN does the pass apply. Emitting on a
        // bare Ok would snapshot sender + one_line for a now-sealed message.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let f = fixture(acct, "g-alert", &alert_eml(now), false);
        // ELIGIBLE, deliberately: the silence below has to come from the TOCTOU
        // gate and nothing else. Ingesting this as backfill would leave the row
        // unstamped and it would be silent for a reason the test is not about.
        let (mid, stamp) =
            ingest_deferring_to_refine(&store, acct, &f, now, IngestOrigin::Incremental);
        assert!(stamp.is_some(), "fresh incremental mail is notify-eligible");

        // The pass is already holding the queued row...
        let row = store
            .stage1_queue(acct, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == mid)
            .expect("queued");
        // ...when the seal lands.
        store
            .correct_triage(acct, mid, TriageAxis::Sensitivity, "sealed", None, now)
            .unwrap()
            .unwrap();

        assert_eq!(
            refine_row_and_notify(&store, acct, &row, Tier::PastDue, 100, now),
            None,
            "a verdict that did not land must not notify"
        );
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
    }

    #[test]
    fn future_dated_backlog_mail_stays_silent_through_the_refine_pass() {
        // The `Date:` header is SENDER-CONTROLLED and ingest prefers it over
        // Gmail's internalDate. The refine passes grind the backlog
        // `received_at DESC` — future-dated rows FIRST — so without an upper
        // edge on the freshness window a fresh install storms on mail dated 2030.
        //
        // The forgery is now caught EARLIER: `is_fresh` is asked once, at ingest,
        // and mail dated four years out never earns a stamp, so the refine sites
        // have nothing to be fooled by. This ingests on the INCREMENTAL path on
        // purpose — the path that is allowed to stamp — so the silence is the
        // future-date ceiling and not the origin.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let eml = alert_eml(now + ChronoDuration::days(365 * 4));
        let f = fixture(acct, "g-liar", &eml, false);
        let (mid, stamp) =
            ingest_deferring_to_refine(&store, acct, &f, now, IngestOrigin::Incremental);
        assert_eq!(
            stamp, None,
            "a sender-controlled Date: cannot buy notify eligibility"
        );

        // Sanity: the lying header really is what the row carries.
        let row = store
            .stage1_queue(acct, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == mid)
            .expect("queued");
        assert!(
            row.received_at > now + ChronoDuration::days(1000),
            "the Date: header won"
        );
        assert_eq!(row.notify_eligible_at, None, "and the row carries no stamp");

        assert_eq!(
            refine_and_notify(&store, acct, mid, Tier::PastDue, 100, now),
            None,
            "an unstamped row can never notify, loud verdict or not"
        );
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
    }

    #[test]
    fn a_late_verdict_inside_the_hour_buzzes_and_past_it_is_counted() {
        // THE WHOLE POINT OF WAVE 1, at the site that decides it. Both rows are
        // notify-eligible and both carry the loudest verdict there is; the only
        // difference is how long the refine pass took to reach them. Inside the
        // rescue window the buzz lands (this is the 24.7% the old `Date:`-based
        // window ate); past it the drop is COUNTED, where it used to be silent.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1");
        let window = eng.config.notify.rescue_window_secs as i64;

        let late = events::EventContext {
            account_id: acct,
            message_id: 101,
            thread_id: "t-late",
            sender: "alerts@monitoring.example",
            one_line: "incident opened",
            notify_eligible_at: Some(now - ChronoDuration::seconds(window - 1)),
            sensitivity: Sensitivity::Normal,
            is_sent: false,
            is_spam: false,
            rule: None,
            tier: Tier::Signal,
            importance: 90,
            deadline: None,
        };
        assert!(
            matches!(
                eng.emit_deliberate(&late, now, ExpiryCount::Miss, STAGE_MODEL),
                Emitted::New(_)
            ),
            "an hour-late verdict on fresh mail still buzzes"
        );

        let expired = events::EventContext {
            message_id: 102,
            thread_id: "t-expired",
            notify_eligible_at: Some(now - ChronoDuration::seconds(window + 1)),
            ..late
        };
        assert_eq!(
            eng.emit_deliberate(&expired, now, ExpiryCount::Miss, STAGE_MODEL),
            Emitted::Expired
        );
        assert_eq!(
            store.events_after(acct, 0, 100).unwrap().len(),
            1,
            "the expired one appended nothing"
        );

        // AND IT IS ON /metrics, which is the difference between this drop and
        // the one that ran at 24.7% for fourteen days with nobody able to see it.
        let text = crate::metrics::render(&eng.metrics, None);
        assert!(
            text.contains(
                "squelchd_notify_decisions_total{lane=\"deliberate\",decision=\"expired\"} 1\n"
            ),
            "the expiry must be counted"
        );
        // AND THE BUZZ THAT LANDED IS ON IT TOO. Both directions, because the
        // §11.11 rollout query is a ratio: a lane that only ever reported its
        // failures would read as a lane that only ever fails.
        assert!(text.contains(
            "squelchd_notify_decisions_total{lane=\"deliberate\",decision=\"sent\"} 1\n"
        ));
        // The metric is the ledger's shadow, never a second bookkeeping: one row
        // per message, carrying the stage's model id and no latency.
        let rows = ledger(&store, acct);
        assert_eq!(
            rows.iter()
                .map(|r| (r.message_id, r.lane, r.decision))
                .collect::<Vec<_>>(),
            vec![
                (101, NotifyLane::Deliberate, NotifyDecision::Sent),
                (102, NotifyLane::Deliberate, NotifyDecision::Expired),
            ]
        );
        assert!(rows.iter().all(|r| r.latency_ms.is_none()));
        assert!(
            rows.iter()
                .all(|r| r.model_used.as_deref() == Some(STAGE_MODEL))
        );
    }

    #[test]
    fn a_stage1_verdict_half_an_hour_late_buzzes_and_two_hours_late_expires() {
        // THE RESCUE WINDOW, end to end through the store rather than off a
        // hand-built context. Both rows are real: ingested on the incremental
        // path when they were fresh, stamped, written to SQLite, and read back
        // out through the Stage-1 queue SELECT the pass actually uses. The only
        // difference between them is how long the refine lane took to arrive.
        //
        // Under the OLD `Date:`-based window BOTH of these were silent, and
        // silently so: half an hour is twice the freshness window, so the very
        // notification this whole issue is about (a deadline the model took a
        // busy afternoon to reach) was deleted rather than delayed.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1");

        // Ingested WHEN THEY WERE FRESH, which is the only way to get an
        // honest stamp: the engine's own `notify_eligible_stamp` runs, so
        // neither row is handed a timestamp the daemon could not have produced.
        let half_hour_ago = now - ChronoDuration::minutes(30);
        let two_hours_ago = now - ChronoDuration::hours(2);
        let late = fixture(acct, "g-late", &alert_eml(half_hour_ago), false);
        let (late_mid, late_stamp) = ingest_deferring_to_refine(
            &store,
            acct,
            &late,
            half_hour_ago,
            IngestOrigin::Incremental,
        );
        assert_eq!(late_stamp, Some(half_hour_ago));
        let stale = alert_eml(two_hours_ago).replace("checkout api", "billing api");
        let stale = fixture(acct, "g-stale", &stale, false);
        let (stale_mid, stale_stamp) = ingest_deferring_to_refine(
            &store,
            acct,
            &stale,
            two_hours_ago,
            IngestOrigin::Incremental,
        );
        assert_eq!(stale_stamp, Some(two_hours_ago));

        let queued = store.stage1_queue(acct, 100).unwrap();
        let row_for = |mid: i64| {
            queued
                .iter()
                .find(|r| r.message_id == mid)
                .expect("queued for the stage-1 refine pass")
        };

        // Thirty minutes late, inside the hour: it still buzzes.
        let row = row_for(late_mid);
        assert_eq!(
            row.notify_eligible_at,
            Some(half_hour_ago),
            "the stamp survived the write and the queue SELECT"
        );
        let emitted = eng.emit_deliberate(
            &stage1_ctx(acct, row, None, Tier::Deadline, 90),
            now,
            ExpiryCount::Miss,
            STAGE_MODEL,
        );
        assert!(
            matches!(emitted, Emitted::New(_)),
            "a verdict half an hour late is a rescue, not a miss: {emitted:?}"
        );

        // Two hours late, past the ceiling: no event, and RECORDED.
        let row = row_for(stale_mid);
        assert_eq!(
            eng.emit_deliberate(
                &stage1_ctx(acct, row, None, Tier::Deadline, 90),
                now,
                ExpiryCount::Miss,
                STAGE_MODEL
            ),
            Emitted::Expired
        );
        assert_eq!(
            store.events_after(acct, 0, 100).unwrap().len(),
            1,
            "only the rescued one appended"
        );

        // EXACTLY ONCE, AND PROVEN BY OFFERING IT AGAIN. A row that escalates is
        // offered at the Stage-1 apply site and then AGAIN at the Stage-2 apply
        // site behind it, minutes to hours later, with the same stamp and so the
        // same refusal both times. Recording each would overstate the very number
        // §11.11 says decides whether the window moves — by 2x on every escalated
        // row, and by a whole mailbox on one `retriage`. `UNIQUE(message_id,
        // lane)` is what stops it, and the counter hangs off that insert.
        assert_eq!(
            eng.emit_deliberate(
                &stage1_ctx(acct, row, None, Tier::Deadline, 90),
                now + ChronoDuration::minutes(30),
                ExpiryCount::Miss,
                STAGE_MODEL
            ),
            Emitted::Expired,
            "still expired, still refused"
        );
        let text = crate::metrics::render(&eng.metrics, None);
        assert!(
            text.contains(
                "squelchd_notify_decisions_total{lane=\"deliberate\",decision=\"expired\"} 1\n"
            ),
            "one MISS, one count, however many sites offer it: {text}"
        );
        assert_eq!(
            ledger(&store, acct)
                .iter()
                .filter(|r| r.message_id == stale_mid)
                .count(),
            1,
            "and one row, not two"
        );
    }

    #[test]
    fn a_message_already_notified_is_never_counted_as_a_miss() {
        // THE COUNTER'S HONESTY, and the failure mode that would have ruined it:
        // `worthy_kind` refuses on the rescue ceiling WITHOUT touching the store,
        // so it cannot know the phone already buzzed. The commonest expiry offer
        // in a real mailbox is exactly that shape — Stage-1 notifies at 09:01,
        // the row carries `needs_stage2`, the Stage-2 queue is ninety minutes
        // deep, and the second offer arrives past the hour with the same 09:00
        // stamp. Booking that as a missed notification would fill the one series
        // the rollout decision is read off with notifications that were
        // DELIVERED.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1");

        let stamped = events::EventContext {
            account_id: acct,
            message_id: 301,
            thread_id: "t-escalated",
            sender: "alerts@monitoring.example",
            one_line: "incident opened",
            notify_eligible_at: Some(now),
            sensitivity: Sensitivity::Normal,
            is_sent: false,
            is_spam: false,
            rule: None,
            tier: Tier::Signal,
            importance: 90,
            deadline: None,
        };

        // Stage-1, ten minutes in: the buzz lands.
        let first = eng.emit_deliberate(
            &stamped,
            now + ChronoDuration::minutes(10),
            ExpiryCount::Miss,
            STAGE_MODEL,
        );
        assert!(matches!(first, Emitted::New(_)), "{first:?}");

        // Stage-2, ninety minutes in: past the ceiling, but the user HAS been
        // told. `AlreadyNotified` is the honest answer, and §11.4 already
        // reserves `would_send` for it — nothing missed, nothing to count.
        assert_eq!(
            eng.emit_deliberate(
                &stamped,
                now + ChronoDuration::minutes(90),
                ExpiryCount::Miss,
                STAGE_MODEL
            ),
            Emitted::AlreadyNotified,
            "a delivered notification is not a miss, however late the second look"
        );
        // And a re-triage hours later says the same thing, as many times as the
        // user runs one: `retriage_reset` nulls the stage markers and leaves the
        // eligibility stamp alone, so every worthy row in its window comes back
        // through here looking exactly like this.
        assert_eq!(
            eng.emit_deliberate(
                &stamped,
                now + ChronoDuration::hours(9),
                ExpiryCount::Miss,
                STAGE_MODEL
            ),
            Emitted::AlreadyNotified
        );

        let text = crate::metrics::render(&eng.metrics, None);
        assert!(
            text.contains(
                "squelchd_notify_decisions_total{lane=\"deliberate\",decision=\"expired\"} 0\n"
            ),
            "no miss to count: {text}"
        );
        // The ledger says the same: ONE deliberate row, and it says `sent`. The
        // two `would_send` offers behind it are the append-only rule refusing to
        // let a later look overwrite the answer that actually reached the phone.
        let rows = ledger(&store, acct);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].decision, NotifyDecision::Sent);
        assert_eq!(
            store.events_after(acct, 0, 100).unwrap().len(),
            1,
            "and one buzz is never rewritten"
        );
    }

    #[tokio::test]
    async fn a_catch_up_rescan_cannot_manufacture_a_stamp_at_the_ingest_site() {
        // NULL IS FOREVER, AND NULL IS SILENT AT EVERY EMISSION SITE — including
        // the FAST LANE, which is the site that starts from a value ingest
        // computed itself rather than one it read back.
        //
        // The path is real on a no-LLM daemon: the first run backfills 30 days
        // (every row NULL), the historyId later expires, and `catch_up` re-fetches
        // the whole window on the INCREMENTAL path. A backfilled row whose `Date:`
        // happens to be inside the freshness window at catch-up time computes a
        // fresh `Some(now)` in memory; `ingest_message` correctly keeps the stored
        // NULL, and without the lane's re-read it would buzz off the discarded
        // value for a row the database says may never notify.
        //
        // Driven through the ENGINE's own `ingest_one` and then the REAL lane,
        // not a mirror of either: the whole finding was that the two lines
        // diverged.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        // The no-model path is the subject: with no model to wait for, the
        // confident heuristic seed is authoritative and the lane emits from it.
        // Forced rather than inferred from an empty config, because
        // `resolve_llm` reads the process environment.
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1").without_stage2_llm();
        let rules = store.list_sender_rules(acct).unwrap();

        // The lane exactly as `fetch_raw_and_ingest` would spawn it, awaited
        // rather than spawned so the assertions below cannot race it.
        let run = async |ing: &Ingested| {
            if let Some(c) =
                notify_lane::candidate(&ing.triaged, ing.id, &rules, &eng.config.notify, |addr| {
                    store.is_known_contact(acct, addr).unwrap_or(false)
                })
            {
                eng.notify_lane().clone().run(c).await;
            }
        };

        // First sight is a BACKFILL, so no stamp and no event, however loud.
        let f = fixture(acct, "g-backfilled", &alert_eml(now), false);
        let ing = eng
            .ingest_one(&f, &rules, now, IngestOrigin::Backfill)
            .unwrap()
            .expect("a normal message is committed");
        let mid = ing.id;
        run(&ing).await;
        assert_eq!(stamp_of(&store, acct, mid), None, "backfill never stamps");
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());

        // The catch-up re-scan, on the incremental path, seconds later: the same
        // Gmail id, still inside the freshness window, so the in-memory stamp
        // this time is `Some` and the row IS a candidate.
        let again = now + ChronoDuration::seconds(30);
        let ing = eng
            .ingest_one(&f, &rules, again, IngestOrigin::Incremental)
            .unwrap()
            .expect("the same row, re-ingested");
        assert_eq!(ing.id, mid, "UNIQUE(account_id, gmail_msg_id) collapsed it");
        assert!(
            ing.triaged.notify_eligible_at.is_some(),
            "the in-memory stamp is the fresh one; the re-read is what discards it"
        );
        run(&ing).await;
        assert_eq!(
            stamp_of(&store, acct, mid),
            None,
            "the DO UPDATE SET preserved the stored NULL"
        );
        assert!(
            store.events_after(acct, 0, 100).unwrap().is_empty(),
            "and the lane emitted from the stamp the STORE kept, not the one \
             ingest just computed"
        );
        assert!(
            store
                .notify_decisions_since(acct, now - ChronoDuration::hours(1), 100)
                .unwrap()
                .is_empty(),
            "an unstamped row is not in the ledger at all, not even as a decline"
        );

        // Control: the same message seen for the FIRST time on the incremental
        // path does stamp and does notify, so the silence above is the re-scan
        // rule and not a dead code path.
        let fresh = fixture(acct, "g-first-sight", &alert_eml(now), false);
        let ing = eng
            .ingest_one(&fresh, &rules, now, IngestOrigin::Incremental)
            .unwrap()
            .expect("committed");
        assert!(stamp_of(&store, acct, ing.id).is_some());
        run(&ing).await;
        assert_eq!(
            store.events_after(acct, 0, 100).unwrap().len(),
            1,
            "first sight on an incremental path is exactly what MAY notify"
        );
    }

    #[test]
    fn a_re_triaged_row_expires_without_being_booked_as_a_miss() {
        // THE COUNTER'S SECOND HONESTY PROBLEM, after already-notified.
        // `retriage_reset` nulls the model stamps, sets `retriage_at`, and
        // deliberately leaves `notify_eligible_at` alone; `retriage_forced` then
        // exempts those rows from every pass's stale gate. So
        // `retriage_reset(acct, None, 90)` walks weeks-old rows straight to the
        // apply sites, and the ones that were below the line before and are above
        // it now — the usual REASON to run one, e.g. after lowering
        // `notify.min_importance` — have no event, refuse as expired, and would
        // each add a count. Thousands of them, from one operator action, to the
        // one number docs/NOTIFY.md §11.11 says decides whether the window moves.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1");
        let window = eng.config.notify.rescue_window_secs as i64;

        let old = events::EventContext {
            account_id: acct,
            message_id: 401,
            thread_id: "t-retriaged",
            sender: "alerts@monitoring.example",
            one_line: "incident opened",
            notify_eligible_at: Some(now - ChronoDuration::seconds(window * 24)),
            sensitivity: Sensitivity::Normal,
            is_sent: false,
            is_spam: false,
            rule: None,
            tier: Tier::Signal,
            importance: 90,
            deadline: None,
        };

        // THE REFUSAL IS UNCHANGED. Mail the user read a fortnight ago must not
        // buzz, whoever asked for it to be looked at again.
        assert_eq!(
            eng.emit_deliberate(&old, now, ExpiryCount::Rereading, STAGE_MODEL),
            Emitted::Expired,
            "a re-triage rescues nothing past the ceiling"
        );
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());

        // ...and it is not booked. Only the bookkeeping differs — and it is not
        // booked as anything ELSE either: NO ROW AT ALL, because the ledger's
        // decisions are facts about mail and "an operator re-read this" is not
        // one. A row here would also be the one that STICKS, since append-only
        // keeps the first answer: the real verdict behind the re-triage would
        // then find the slot taken.
        let text = crate::metrics::render(&eng.metrics, None);
        assert!(
            text.contains(
                "squelchd_notify_decisions_total{lane=\"deliberate\",decision=\"expired\"} 0\n"
            ),
            "a re-reading of old mail is not a notification anybody missed: {text}"
        );
        assert!(
            ledger(&store, acct).is_empty(),
            "and the ledger is untouched"
        );

        // Control: the SAME row on its ordinary path is a miss, so the silence
        // above is the flag and not a dead counter. A different message id
        // because the ledger's UNIQUE is per (message, lane).
        let missed = events::EventContext {
            message_id: 402,
            ..old
        };
        assert_eq!(
            eng.emit_deliberate(&missed, now, ExpiryCount::Miss, STAGE_MODEL),
            Emitted::Expired
        );
        let text = crate::metrics::render(&eng.metrics, None);
        assert!(
            text.contains(
                "squelchd_notify_decisions_total{lane=\"deliberate\",decision=\"expired\"} 1\n"
            ),
            "{text}"
        );
        let rows = ledger(&store, acct);
        assert_eq!(rows.len(), 1, "one row, for the miss only");
        assert_eq!(rows[0].message_id, 402);
        assert_eq!(rows[0].decision, NotifyDecision::Expired);

        // And the flag is derived from the row, not chosen by hand at the call
        // site: this is the one field that separates the two.
        assert_eq!(
            ExpiryCount::from_retriage(Some(now), now),
            ExpiryCount::Rereading
        );
        assert_eq!(ExpiryCount::from_retriage(None, now), ExpiryCount::Miss);
    }

    #[test]
    fn the_stage2_queue_carries_the_stamp_to_its_apply_site() {
        // THE THIRD CARRIER (docs/NOTIFY.md §11.3 names `Stage1Queued`,
        // `Stage2Queued` and `SeedVerdict`), and the one whose SELECT nothing
        // else reads the stamp through. A column-index slip in that projection
        // would surface only as a queue read error, which `read_queue` logs and
        // turns into an EMPTY QUEUE: Stage-2 would quietly stop notifying and
        // every existing stage2_queue test would still pass, because they assert
        // that rows come back rather than what is on them.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1");
        let window = eng.config.notify.rescue_window_secs as i64;

        // Ingested fresh on the incremental path, so the stamp is one the daemon
        // could actually have produced rather than one the test invented.
        let half_hour_ago = now - ChronoDuration::minutes(30);
        let f = fixture(acct, "g-escalated", &alert_eml(half_hour_ago), false);
        let (mid, stamp) =
            ingest_deferring_to_refine(&store, acct, &f, half_hour_ago, IngestOrigin::Incremental);
        assert_eq!(stamp, Some(half_hour_ago));

        // The Stage-1 apply that escalates it, through the real store method:
        // the row only reaches the Stage-2 queue by that hop, and the stamp has
        // to survive it.
        let escalate = crate::store::Stage1Applied {
            message_id: mid,
            account_id: acct,
            importance: 60,
            tier: Tier::Signal,
            one_line: "refined one-liner".into(),
            reason: "stage-1".into(),
            field_reasons: crate::types::FieldReasons::default(),
            stage1_model_used: "claude-haiku-4-5".into(),
            needs_stage2: true,
            escalation_reason: None,
            deadline: None,
            category: None,
        };
        assert!(store.stage1_apply(&escalate).unwrap());

        let queued = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(queued.len(), 1, "the row reached the Stage-2 queue");
        let row = &queued[0];
        assert_eq!(
            row.notify_eligible_at,
            Some(half_hour_ago),
            "the stamp survived the write, the escalation and the Stage-2 SELECT"
        );
        // The neighbours in the projection, so an index slip cannot pass by
        // landing the stamp in the right variable and everything else one over.
        assert!(row.is_known_contact.eq(&false));
        assert_eq!(row.escalation_reason, None);
        assert_eq!(row.retriage_at, None);

        // And it decides at the apply site, in both directions.
        fn ctx<'a>(acct: AccountId, r: &'a crate::store::Stage2Queued) -> events::EventContext<'a> {
            events::EventContext {
                account_id: acct,
                message_id: r.message_id,
                thread_id: &r.thread_id,
                sender: &r.from_addr,
                one_line: "refined one-liner",
                notify_eligible_at: r.notify_eligible_at,
                sensitivity: r.sensitivity,
                is_sent: false,
                is_spam: false,
                rule: None,
                tier: Tier::Signal,
                importance: 90,
                deadline: None,
            }
        }
        assert!(
            matches!(
                eng.emit_deliberate(&ctx(acct, row), now, ExpiryCount::Miss, STAGE_MODEL),
                Emitted::New(_)
            ),
            "half an hour late is inside the rescue window"
        );

        // The same row read back the same way, offered past the ceiling: refused,
        // and `AlreadyNotified` rather than a second miss, because the phone
        // already buzzed at the site above.
        assert_eq!(
            eng.emit_deliberate(
                &ctx(acct, row),
                half_hour_ago + ChronoDuration::seconds(window + 1),
                ExpiryCount::Miss,
                STAGE_MODEL
            ),
            Emitted::AlreadyNotified
        );
    }

    #[test]
    fn squelching_a_sender_silences_rows_already_queued() {
        // THE REACTIVE SQUELCH: the mail is already in the Stage-1 queue when the
        // user squelches the sender, and the 'rule' marker is stamped at INGEST
        // only — so the refine site must read the rule list live or push mail
        // from a sender the user just silenced.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        // Incremental and fresh, so the row IS notify-eligible and the refine
        // site would push it; with an LLM configured, ingest defers to that site
        // rather than emitting itself.
        let f = fixture(acct, "g-alert", &alert_eml(now), false);
        let (mid, _) = ingest_deferring_to_refine(&store, acct, &f, now, IngestOrigin::Incremental);

        store
            .set_sender_rule(
                acct,
                "*@monitoring.example",
                "not urgent",
                Disposition::Squelch,
            )
            .unwrap();
        assert_eq!(
            refine_and_notify(&store, acct, mid, Tier::PastDue, 100, now),
            None,
            "a sender squelched AFTER the row was queued must not push"
        );

        // Control: the same verdict from an unruled sender does notify, so the
        // silence above is the rule and not the harness.
        let free = alert_eml(now).replace("alerts@monitoring.example", "alerts@other.example");
        let ff = fixture(acct, "g-other", &free, false);
        let (mid2, _) =
            ingest_deferring_to_refine(&store, acct, &ff, now, IngestOrigin::Incremental);
        assert!(
            refine_and_notify(&store, acct, mid2, Tier::PastDue, 100, now).is_some(),
            "unruled sender, same verdict: notifies"
        );
        assert_eq!(store.events_after(acct, 0, 100).unwrap().len(), 1);
    }

    // ---- budget-notice log redaction (PII safety) -------------------------

    #[test]
    fn redact_sender_hides_the_address_but_stays_stable() {
        let a = redact_sender("attacker@evil.example");
        assert!(a.starts_with("sender#"), "tagged form: {a}");
        assert_eq!(a.len(), "sender#".len() + 12, "12 hex chars of sha256");
        assert!(
            !a.contains("attacker") && !a.contains("evil"),
            "address must not leak: {a}"
        );
        let hex = &a["sender#".len()..];
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "hex only: {hex}"
        );
        // Deterministic (correlatable across a day) and injective per sender.
        assert_eq!(a, redact_sender("attacker@evil.example"));
        assert_ne!(a, redact_sender("someone@else.example"));
    }

    #[test]
    fn sanitize_ascii_strips_control_and_caps_length() {
        // Newlines (log-forging), ANSI escapes, and RTL-override become '.'.
        let clean = sanitize_ascii("abc\n\x1b[31mDEF\u{202e}", 64);
        assert!(!clean.contains('\n') && !clean.contains('\u{1b}') && !clean.contains('\u{202e}'));
        assert!(
            clean.starts_with("abc."),
            "printable kept, control replaced: {clean}"
        );
        // Pathologically long header can't flood the log.
        assert_eq!(sanitize_ascii(&"a".repeat(200), 10).chars().count(), 10);
    }

    // ---- base64url raw decode ---------------------------------------------

    #[test]
    fn decode_raw_b64url_no_pad_round_trips() {
        let eml = "From: a@b.com\r\nSubject: hi\r\n\r\nbody\r\n";
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(eml);
        let out = decode_raw_b64url(&enc).unwrap();
        assert_eq!(out, eml.as_bytes());
    }

    #[test]
    fn decode_raw_b64url_accepts_padded_and_web_safe() {
        // 4 bytes => 6 base64 chars + '==' padding; values force '-'/'_' web-safe.
        let bytes: Vec<u8> = vec![0xfb, 0xff, 0xbf, 0xf0];
        let padded = base64::engine::general_purpose::URL_SAFE.encode(&bytes);
        assert!(padded.contains('='), "expected padding in this fixture");
        assert!(
            padded.contains('-') || padded.contains('_'),
            "expected web-safe chars in this fixture"
        );
        let out = decode_raw_b64url(&padded).unwrap();
        assert_eq!(out, bytes);
    }

    #[test]
    fn decode_raw_b64url_rejects_garbage() {
        assert!(decode_raw_b64url("!!!not base64!!!").is_err());
    }

    // ---- history cursor advance -------------------------------------------

    #[test]
    fn advance_history_cursor_takes_max_never_regresses() {
        assert_eq!(advance_history_cursor(100, [50, 75, 40]), 100);
        assert_eq!(advance_history_cursor(100, [150, 120, 200]), 200);
        assert_eq!(advance_history_cursor(0, std::iter::empty()), 0);
        assert_eq!(advance_history_cursor(10, [10]), 10);
    }

    // ---- 404 / expired-history fallback decision --------------------------

    #[test]
    fn history_decision_incremental_when_cursor_present_and_fresh() {
        assert_eq!(
            history_poll_decision(Some(4242), false),
            HistoryDecision::Incremental(4242)
        );
    }

    #[test]
    fn history_decision_full_catchup_on_expired() {
        assert_eq!(
            history_poll_decision(Some(4242), true),
            HistoryDecision::FullCatchUp
        );
    }

    #[test]
    fn history_decision_full_catchup_when_absent_or_zero() {
        assert_eq!(
            history_poll_decision(None, false),
            HistoryDecision::FullCatchUp
        );
        assert_eq!(
            history_poll_decision(Some(0), false),
            HistoryDecision::FullCatchUp
        );
    }

    // ---- header synthesis for metadata-only sent seeding ------------------

    #[test]
    fn synthesize_headers_seeds_recipients_not_self() {
        // From is the account itself; contacts come from To/Cc recipients.
        let headers = vec![
            MessageHeader {
                name: "From".into(),
                value: "me@example.com".into(),
            },
            MessageHeader {
                name: "To".into(),
                value: "alice@friends.com".into(),
            },
            MessageHeader {
                name: "Cc".into(),
                value: "bob@friends.com".into(),
            },
            MessageHeader {
                name: "Subject".into(),
                value: "re: lunch".into(),
            },
            MessageHeader {
                name: "Date".into(),
                value: "Mon, 7 Jul 2026 10:00:00 +0000".into(),
            },
        ];
        let raw = synthesize_rfc822_headers(&headers);
        assert!(raw.ends_with("\r\n\r\n"));

        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let mut f = fixture(acct, "g-sent", &raw, true);
        f.raw = raw.into_bytes();
        ingest_into(&store, acct, &f, Utc::now());
        assert!(store.is_known_contact(acct, "alice@friends.com").unwrap());
        assert!(store.is_known_contact(acct, "bob@friends.com").unwrap());
        // The account's own address is a contact only because `ensure_account`
        // seeds it — the From header must not add to it.
        let me = store.search_contacts(acct, "me@example.com", 10).unwrap();
        assert_eq!(me[0].sent_count, 1);
    }

    #[test]
    fn synthesize_headers_drops_injected_newlines() {
        let headers = vec![MessageHeader {
            name: "From".into(),
            value: "x@y.com\r\nBcc: evil@z.com".into(),
        }];
        let raw = synthesize_rfc822_headers(&headers);
        assert!(!raw.contains("Bcc"), "CRLF-injected header must be dropped");
    }

    // ---- internalDate parsing ---------------------------------------------

    #[test]
    fn parse_internal_date_millis() {
        // 2026-07-07T10:00:00Z = 1783591200000 ms.
        let dt = parse_internal_date(Some("1783591200000")).unwrap();
        assert_eq!(dt.timestamp(), 1783591200);
        assert!(parse_internal_date(None).is_none());
        assert!(parse_internal_date(Some("garbage")).is_none());
    }

    #[test]
    fn parse_history_id_handles_bad_input() {
        assert_eq!(parse_history_id("12345"), 12345);
        assert_eq!(parse_history_id(""), 0);
        assert_eq!(parse_history_id("not-a-number"), 0);
    }

    // ---- ingest pipeline invariants (unchanged behavior) ------------------

    #[test]
    fn sealed_otp_stored_sealed_with_importance_zero() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your one-time passcode is 483920. Enter this code to continue.\r\n";
        let f = fixture(acct, "g-otp", eml, false);
        ingest_into(&store, acct, &f, Utc::now());

        let updates = store
            .ranked_updates(acct, Utc::now() - ChronoDuration::days(1), None)
            .unwrap();
        assert!(updates.is_empty(), "sealed OTP must not surface");

        let sealed = store.sealed_messages(acct).unwrap();
        assert_eq!(sealed.len(), 1);
        assert_eq!(sealed[0].sealed_kind.as_deref(), Some("otp"));
    }

    #[test]
    fn dated_bill_stored_as_deadline_with_deadlines_row() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let eml = "From: Acme <invoices@acme.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Invoice #4402 from Acme\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your invoice total is $1,299.00. Payment due by August 15, 2026.\r\n";
        let now = DateTime::parse_from_rfc3339("2026-07-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let f = fixture(acct, "g-bill", eml, false);
        ingest_into(&store, acct, &f, now);

        let updates = store
            .ranked_updates(acct, now - ChronoDuration::days(1), None)
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].tier, Tier::Deadline);

        let deadlines = store.deadlines(acct, Some(365)).unwrap();
        assert_eq!(deadlines.len(), 1, "a deadlines row must be written");
        assert_eq!(deadlines[0].amount, Some(1299.00));
        assert!(!deadlines[0].past_due);
    }

    #[test]
    fn past_due_bill_lands_past_due_tier() {
        // A CONFIDENT PastDue requires a TRUSTED sender, so seed the biller as a
        // known contact first: a legit past-due from a known biller still screams.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // Contacts are derived from Sent-mail recipients.
        let seed = "From: me@example.com\r\n\
                    To: Utility <billing@utilityco.com>\r\n\
                    Subject: account setup\r\n\
                    Date: Mon, 7 Jul 2026 09:00:00 +0000\r\n\
                    \r\n\
                    hello\r\n";
        let sf = fixture(acct, "g-seed", seed, /* is_sent */ true);
        ingest_into(&store, acct, &sf, Utc::now());
        assert!(
            store
                .is_known_contact(acct, "billing@utilityco.com")
                .unwrap()
        );

        let eml = "From: Utility <billing@utilityco.com>\r\n\
                   Subject: PAST DUE: Your electric bill\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Amount due $84.20. This payment is overdue.\r\n";
        let now = DateTime::parse_from_rfc3339("2026-07-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let f = fixture(acct, "g-pastdue", eml, false);
        ingest_into(&store, acct, &f, now);

        let updates = store
            .ranked_updates(acct, now - ChronoDuration::days(1), None)
            .unwrap();
        // The seed sent-message is excluded from ranked_updates; only the
        // past-due bill surfaces, at the top tier for a KNOWN sender.
        let bill = updates
            .iter()
            .find(|u| u.one_line.contains("PAST DUE"))
            .expect("past-due bill update present");
        assert_eq!(bill.tier, Tier::PastDue);
        let deadlines = store.deadlines(acct, None).unwrap();
        assert!(deadlines[0].past_due);
    }

    #[test]
    fn sent_message_seeds_recipient_contacts_never_self_and_skips_inbox() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // The user (me@example.com) sends to Alice, cc Bob. From == self.
        let eml = "From: me@example.com\r\n\
                   To: Alice <alice@friends.com>\r\n\
                   Cc: bob@friends.com\r\n\
                   Subject: re: lunch\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   sounds good\r\n";
        let now = Utc::now();
        let f = fixture(acct, "g-sent", eml, /* is_sent */ true);
        ingest_into(&store, acct, &f, now);

        // Recipients become contacts; nobody else does.
        assert!(store.is_known_contact(acct, "alice@friends.com").unwrap());
        assert!(store.is_known_contact(acct, "bob@friends.com").unwrap());
        assert!(!store.is_known_contact(acct, "stranger@nowhere.io").unwrap());
        // The account's own address is a contact — `ensure_account` seeds it —
        // but NOT by way of this send: writing to Alice from your own address is
        // not evidence you write to yourself, so the seeded count stands where
        // it was.
        let me = store.search_contacts(acct, "me@example.com", 10).unwrap();
        assert_eq!(me.len(), 1);
        assert_eq!(me[0].sent_count, 1, "From: self must not bump self");

        // Sent mail must NOT pollute the ranked inbox.
        let updates = store
            .ranked_updates(acct, now - ChronoDuration::days(1), None)
            .unwrap();
        assert!(
            updates.is_empty(),
            "sent mail must never surface in ranked_updates"
        );

        // And it must not appear in search results either.
        let hits = store.search(acct, "lunch", 10, 0).unwrap();
        assert!(hits.is_empty(), "sent mail must not appear in search");
    }

    #[test]
    fn a_note_the_user_mails_themselves_stands_as_signal() {
        // Gmail hands a self-addressed message to BOTH label walks and the INBOX
        // walk wins, so it arrives here as ordinary received mail whose From is
        // the user. With no contact row for self that read as a stranger writing
        // in, and Stage-1 dropped a note-to-self to noise.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let eml = "From: Braelyn <me@example.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: remember the milk\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   and the eggs\r\n";
        let now = Utc::now();
        let f = fixture(acct, "g-self", eml, /* is_sent */ false);
        let id = ingest_into(&store, acct, &f, now);

        // A window wide enough to reach the fixture's own `Date:` header.
        let updates = store
            .ranked_updates(acct, now - ChronoDuration::days(365), None)
            .unwrap();
        let note = updates
            .iter()
            .find(|u| u.id == id)
            .expect("a note to self is mail the user meant to see");
        assert_eq!(note.tier, Tier::Signal);
        assert_eq!(
            note.importance,
            Stage1Config::default().known_contact_importance
        );
    }

    // ---- HTML body: ingest sanitize + human-door serving ------------------

    #[test]
    fn html_email_stores_sanitized_html_served_by_client_thread_view() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // Dangerous markup (script, onerror, javascript: href, form) alongside
        // benign table/img/style content.
        let eml = "From: News <news@substack.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Weekly\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\
                   \r\n\
                   <html><body><script>steal()</script>\
                   <table><tr><td style=\"color:red\">Hello</td></tr></table>\
                   <img src=\"https://cdn.example.com/x.png\" onerror=\"evil()\">\
                   <a href=\"javascript:evil()\">bad</a>\
                   <form action=\"http://evil\"><input name=\"pw\"></form>\
                   </body></html>\r\n";
        let f = fixture(acct, "g-html", eml, false);
        ingest_into(&store, acct, &f, Utc::now());

        // gmail_thread_id is None in `fixture`, so thread_id falls back to the
        // message id "g-html".
        let view = store
            .thread_view_with_html(acct, "g-html")
            .expect("thread present");
        let msg = &view.messages[0];
        let html = msg.html.as_deref().expect("html stored");

        // Dangerous constructs are gone.
        assert!(!html.to_lowercase().contains("script"));
        assert!(!html.contains("steal"));
        assert!(!html.to_lowercase().contains("onerror"));
        assert!(!html.contains("evil"));
        assert!(!html.to_lowercase().contains("javascript:"));
        assert!(!html.to_lowercase().contains("<form"));
        assert!(!html.to_lowercase().contains("<input"));
        // Benign content survives recognizably.
        assert!(html.contains("<table"));
        assert!(html.contains("style=\"color:red\""));
        assert!(html.contains("https://cdn.example.com/x.png"));

        // The flattened text path still feeds triage/FTS.
        assert!(msg.content.contains("Hello"));
        assert!(!msg.content.contains('<'));
    }

    #[test]
    fn plaintext_email_leaves_html_null() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let eml = "From: Alice <alice@friends.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: hi\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   plain text only, no markup\r\n";
        let f = fixture(acct, "g-plain", eml, false);
        ingest_into(&store, acct, &f, Utc::now());

        let view = store.thread_view_with_html(acct, "g-plain").unwrap();
        assert!(
            view.messages[0].html.is_none(),
            "plain-text-only mail must leave html NULL"
        );
        assert!(view.messages[0].content.contains("plain text only"));
    }

    #[test]
    fn sync_state_round_trips_history_id() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        assert!(store.sync_state(acct, HISTORY_KEY).unwrap().is_none());

        // A historyId larger than u32::MAX, to prove the field holds it.
        let big = (u32::MAX as u64) + 123_456;
        store
            .set_sync_state(
                acct,
                HISTORY_KEY,
                &SyncState {
                    uidvalidity: 0,
                    last_uid: big,
                },
            )
            .unwrap();
        let s = store.sync_state(acct, HISTORY_KEY).unwrap().unwrap();
        assert_eq!(s.last_uid, big);
    }

    #[test]
    fn urlencode_escapes_spaces_and_reserved() {
        assert_eq!(urlencode("newer_than:30d"), "newer_than:30d");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("x&y"), "x%26y");
    }

    #[test]
    fn subtract_ids_drops_the_claimed_and_keeps_order() {
        let claimed = vec!["b".to_string(), "d".to_string()];
        let ids = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        assert_eq!(
            subtract_ids(ids, &claimed),
            vec!["a".to_string(), "c".into()]
        );
        // Nothing claimed: the batch passes through untouched.
        assert_eq!(
            subtract_ids(vec!["a".to_string()], &[]),
            vec!["a".to_string()]
        );
    }

    // ---- the two-walk incremental poll, on the wire ------------------------
    //
    // `history.list` is LABEL-FILTERED, so an INBOX-only poll is structurally
    // blind to mail the user sends from Gmail web or their phone. Everything
    // under test here — walk ORDER, the single cursor commit, a cursor HELD on a
    // sent-side failure — lives in the sequence of HTTP calls and cannot be
    // asserted on a struct, so these drive the real engine against an axum app on
    // an ephemeral loopback port (the shape tests/push_relay.rs already uses).

    use async_trait::async_trait;
    use axum::extract::{Path as AxumPath, Query, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// One label's scripted `history.list` answer.
    #[derive(Clone, Default)]
    struct LabelHistory {
        /// `messageAdded` records, as (historyId, message ids).
        records: Vec<(u64, Vec<String>)>,
        /// The page-level newest historyId.
        top: u64,
        /// Answer 500 instead: an outage that hits ONE label's walk.
        fail: bool,
    }

    impl LabelHistory {
        fn added(top: u64, records: &[(u64, &str)]) -> Self {
            Self {
                records: records
                    .iter()
                    .map(|(hid, id)| (*hid, vec![id.to_string()]))
                    .collect(),
                top,
                fail: false,
            }
        }
        fn quiet(top: u64) -> Self {
            Self {
                records: Vec::new(),
                top,
                fail: false,
            }
        }
        fn broken() -> Self {
            Self {
                records: Vec::new(),
                top: 0,
                fail: true,
            }
        }
    }

    #[derive(Default)]
    struct GmailState {
        /// labelId -> its `history.list` answer.
        history: HashMap<String, LabelHistory>,
        /// labelIds -> the ids `messages.list` returns (the catch-up path).
        listing: HashMap<String, Vec<String>>,
        /// gmail message id -> RFC822 source.
        bodies: HashMap<String, String>,
        /// labelId -> the verbatim 200 body `labels.get` answers with. Absent
        /// (or `None`) means 500: an unscripted label must not read as a real
        /// zero. Whole bodies rather than a counter pair because the shapes
        /// under test include ones that are not label resources at all.
        labels: HashMap<String, Option<Value>>,
        /// `users.getProfile`'s historyId.
        profile_history_id: u64,
        /// gmail message id -> how long `messages.get` stalls before answering.
        /// A FIXTURE, never an assertion: it exists so a test can make one batch
        /// take measurable wall-clock time between two ids and then assert on the
        /// two timestamps the engine wrote, which is the only deterministic way
        /// to tell a per-message clock from a per-batch one.
        slow: HashMap<String, Duration>,
        /// Every call this mock served, in order, as `verb:key`.
        seen: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct MockGmail(Arc<Mutex<GmailState>>);

    impl MockGmail {
        fn history(&self, label: &str, h: LabelHistory) -> &Self {
            self.0.lock().unwrap().history.insert(label.to_string(), h);
            self
        }
        fn listing(&self, label: &str, ids: &[&str]) -> &Self {
            self.0.lock().unwrap().listing.insert(
                label.to_string(),
                ids.iter().map(|s| s.to_string()).collect(),
            );
            self
        }
        fn body(&self, id: &str, eml: String) -> &Self {
            self.0.lock().unwrap().bodies.insert(id.to_string(), eml);
            self
        }
        /// Make `messages.get` for `id` take `d` before it answers.
        fn slow_body(&self, id: &str, d: Duration) -> &Self {
            self.0.lock().unwrap().slow.insert(id.to_string(), d);
            self
        }
        /// Script `labels.get` for one label: `Some(counts)` answers a
        /// well-formed label resource, `None` answers 500 (the outage the
        /// stored counts must survive).
        fn label(&self, label: &str, unread: Option<(i64, i64)>) -> &Self {
            let body = unread.map(|(messages, threads)| {
                json!({
                    "id": label,
                    "messagesUnread": messages,
                    "threadsUnread": threads,
                })
            });
            self.0
                .lock()
                .unwrap()
                .labels
                .insert(label.to_string(), body);
            self
        }
        /// Script `labels.get` with a verbatim 200 body, for the shapes
        /// [`MockGmail::label`] cannot express: a label Gmail sent without
        /// counters, or a 200 that is not a label resource.
        fn label_body(&self, label: &str, body: Value) -> &Self {
            self.0
                .lock()
                .unwrap()
                .labels
                .insert(label.to_string(), Some(body));
            self
        }
        fn profile(&self, history_id: u64) -> &Self {
            self.0.lock().unwrap().profile_history_id = history_id;
            self
        }
        fn seen(&self) -> Vec<String> {
            self.0.lock().unwrap().seen.clone()
        }
        fn calls(&self, key: &str) -> usize {
            self.seen().iter().filter(|s| *s == key).count()
        }
        /// Index of `key` in the call log; panics if it was never served.
        fn at(&self, key: &str) -> usize {
            self.seen()
                .iter()
                .position(|s| s == key)
                .unwrap_or_else(|| panic!("{key} was never called; saw {:?}", self.seen()))
        }
    }

    async fn mock_history(
        State(g): State<MockGmail>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        let label = q.get("labelId").cloned().unwrap_or_default();
        let mut st = g.0.lock().unwrap();
        st.seen.push(format!("history:{label}"));
        let h = st.history.get(&label).cloned().unwrap_or_default();
        if h.fail {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let records: Vec<Value> = h
            .records
            .iter()
            .map(|(hid, ids)| {
                json!({
                    "id": hid.to_string(),
                    "messagesAdded": ids
                        .iter()
                        .map(|m| json!({ "message": { "id": m } }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        Json(json!({ "history": records, "historyId": h.top.to_string() })).into_response()
    }

    async fn mock_messages_list(
        State(g): State<MockGmail>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let label = q.get("labelIds").cloned().unwrap_or_default();
        let mut st = g.0.lock().unwrap();
        st.seen.push(format!("list:{label}"));
        let ids = st.listing.get(&label).cloned().unwrap_or_default();
        Json(json!({
            "messages": ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
        }))
    }

    async fn mock_message_get(
        State(g): State<MockGmail>,
        AxumPath(id): AxumPath<String>,
    ) -> Response {
        // Taken and DROPPED before the await: a std guard held across an await
        // point would deadlock the single-threaded test runtime.
        let stall = g.0.lock().unwrap().slow.get(&id).copied();
        if let Some(d) = stall {
            tokio::time::sleep(d).await;
        }
        let mut st = g.0.lock().unwrap();
        st.seen.push(format!("get:{id}"));
        match st.bodies.get(&id) {
            // Both shapes at once: `raw` for the ingest fetches, and the
            // `payload.headers` a `format=metadata` caller reads. Gmail sends one
            // or the other per the requested format; serving both keeps the mock
            // format-agnostic, and each caller reads only its own field.
            Some(eml) => Json(json!({
                "id": id,
                "raw": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(eml),
                "payload": { "headers": mock_headers(eml) },
                "internalDate": Utc::now().timestamp_millis().to_string(),
            }))
            .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }

    /// The header block of an RFC822 fixture as Gmail's `payload.headers[]`.
    /// Fixtures are single-line headers, so no continuation handling. Values are
    /// trimmed of the CR the fixtures' CRLF endings leave behind — Gmail's own
    /// values are bare, and `synthesize_rfc822_headers` drops anything carrying
    /// one as an injection attempt.
    fn mock_headers(eml: &str) -> Vec<Value> {
        eml.lines()
            .take_while(|l| !l.trim().is_empty())
            .filter_map(|l| l.split_once(": "))
            .map(|(name, value)| json!({ "name": name.trim(), "value": value.trim() }))
            .collect()
    }

    async fn mock_label(State(g): State<MockGmail>, AxumPath(id): AxumPath<String>) -> Response {
        let mut st = g.0.lock().unwrap();
        st.seen.push(format!("label:{id}"));
        match st.labels.get(&id).cloned().flatten() {
            Some(body) => Json(body).into_response(),
            None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }

    async fn mock_profile(State(g): State<MockGmail>) -> Json<Value> {
        let st = g.0.lock().unwrap();
        Json(json!({ "historyId": st.profile_history_id.to_string() }))
    }

    /// Bind the mock on an ephemeral loopback port; returns its API base.
    async fn serve_mock(g: MockGmail) -> String {
        let app = Router::new()
            .route("/history", get(mock_history))
            .route("/messages", get(mock_messages_list))
            .route("/messages/{id}", get(mock_message_get))
            .route("/labels/{id}", get(mock_label))
            .route("/profile", get(mock_profile))
            .with_state(g);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// A credential store handing out one fixed token; the mock ignores it.
    struct FixedToken;

    #[async_trait]
    impl CredentialStore for FixedToken {
        async fn token(&self, _account: AccountId) -> Result<crate::credentials::OAuthToken> {
            Ok(crate::credentials::OAuthToken {
                access_token: "test-token".to_string(),
                refresh_token: None,
                expires_at: None,
            })
        }
    }

    fn engine(
        store: Arc<SqliteStore>,
        acct: AccountId,
        base: &str,
    ) -> SyncEngine<SqliteStore, FixedToken> {
        engine_with_config(store, acct, base, Config::default())
    }

    /// [`engine`] with the config spelled out, for the tests whose whole subject
    /// is a config field (an LLM key, a poll interval).
    fn engine_with_config(
        store: Arc<SqliteStore>,
        acct: AccountId,
        base: &str,
        config: Config,
    ) -> SyncEngine<SqliteStore, FixedToken> {
        SyncEngine::new(
            store,
            Arc::new(FixedToken),
            acct,
            "me@example.com".to_string(),
            config,
        )
        .with_api_base(base.to_string())
    }

    /// Open an in-memory store with a live history cursor at `cursor`.
    fn store_at_cursor(cursor: Option<u64>) -> (Arc<SqliteStore>, AccountId) {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        if let Some(c) = cursor {
            store
                .set_sync_state(
                    acct,
                    HISTORY_KEY,
                    &SyncState {
                        uidvalidity: 0,
                        last_uid: c,
                    },
                )
                .unwrap();
        }
        (store, acct)
    }

    /// THE FAST LANE'S BUDGET IS ITS OWN, on both axes.
    ///
    /// The KEY, because `wake_budget` is one table keyed by `(account, day,
    /// thread_id)`: a sentinel that collided with another pass's would not read
    /// as a bug, it would read as a lane that mysteriously ran out of budget
    /// early — and since this lane runs at ingest behind a backlog that runs
    /// whenever, the collision would eat today's notifications to pay for a
    /// week-old queue. Gmail thread ids are hex, so none of the four can collide
    /// with a real thread either.
    ///
    /// The CAP KIND, because the exhausted notice is rate-limited to one per
    /// UTC day PER KIND: sharing a slot with `Revisit` would mean a capped fast
    /// lane went unmentioned on any day a revisit notice had already fired,
    /// which is precisely the day somebody would be looking.
    ///
    /// And the lane's two OWN notices do not share a slot with each other.
    /// "Cap exhausted" and "config-level failure, lane parked ten minutes" are
    /// the only two diagnoses for a fast lane that stopped notifying, they are
    /// unrelated faults, and one slot means the first of the day swallows the
    /// second.
    #[test]
    fn the_notify_fast_budget_shares_neither_a_key_nor_a_warn_slot() {
        let keys = [
            GLOBAL_BUDGET_KEY,
            STAGE1_GLOBAL_BUDGET_KEY,
            REVISIT_BUDGET_KEY,
            NOTIFY_FAST_BUDGET_KEY,
        ];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b, "two passes sharing one budget counter");
            }
            assert!(
                a.starts_with("__") && a.ends_with("__"),
                "a sentinel must not look like a Gmail thread id: {a}"
            );
            assert!(!a.starts_with(SENDER_BUDGET_PREFIX));
        }

        let (store, acct) = store_at_cursor(None);
        let engine = engine(store, acct, "http://127.0.0.1:1");
        // Once a day, then rearmed when the day rolls over.
        assert!(engine.warn_once_per_day(CapKind::NotifyFast, "2026-09-01"));
        assert!(!engine.warn_once_per_day(CapKind::NotifyFast, "2026-09-01"));
        assert!(engine.warn_once_per_day(CapKind::NotifyFast, "2026-09-02"));
        // And a revisit notice on the same day does not consume the fast lane's.
        assert!(engine.warn_once_per_day(CapKind::Revisit, "2026-09-02"));
        assert!(!engine.warn_once_per_day(CapKind::NotifyFast, "2026-09-02"));
        // NOR DOES THE LANE'S OWN CONFIG-FAILURE NOTICE, in either order: the
        // cap notice has already fired on both days above, and the park still
        // gets its line — the one naming the failure kind, on the day the
        // gateway's allow-list went wrong.
        assert!(engine.warn_once_per_day(CapKind::NotifyFastConfig, "2026-09-01"));
        assert!(engine.warn_once_per_day(CapKind::NotifyFastConfig, "2026-09-02"));
        assert!(!engine.warn_once_per_day(CapKind::NotifyFastConfig, "2026-09-02"));
        // And the other way round: a config park on a fresh day must not
        // silence that day's exhausted-cap notice.
        assert!(engine.warn_once_per_day(CapKind::NotifyFastConfig, "2026-09-03"));
        assert!(engine.warn_once_per_day(CapKind::NotifyFast, "2026-09-03"));
    }

    /// The ledger category the fast lane books under is the SAME STRING both
    /// cost estimators price off. It is the one category with prices of its
    /// own, so a second spelling would not drop the row, it would cost it at the
    /// Stage-1 model's rates and overstate the cheapest pass in the pipeline.
    #[test]
    fn the_notify_ledger_category_is_the_one_the_cost_estimators_price() {
        assert_eq!(NOTIFY_USAGE_CATEGORY, "notify");
        assert_eq!(NOTIFY_USAGE_CATEGORY, crate::metrics::NOTIFY_USAGE_CATEGORY);
        assert_ne!(NOTIFY_USAGE_CATEGORY, REVISIT_USAGE_CATEGORY);
    }

    fn cursor_of(store: &SqliteStore, acct: AccountId) -> Option<u64> {
        store
            .sync_state(acct, HISTORY_KEY)
            .unwrap()
            .map(|s| s.last_uid)
    }

    /// Mail the user WROTE, dated `at` so it passes the first-sight freshness
    /// test — an old `Date:` must not be what makes these tests quiet. (Sent
    /// mail earns no eligibility stamp anyway; that is the point being tested,
    /// so it has to be the ONLY reason it is silent.)
    fn sent_eml(at: DateTime<Utc>, to: &str, subject: &str) -> String {
        format!(
            "From: me@example.com\r\n\
             To: {to}\r\n\
             Subject: {subject}\r\n\
             Date: {}\r\n\
             \r\n\
             writing this from the phone app\r\n",
            at.to_rfc2822()
        )
    }

    fn spam_eml(at: DateTime<Utc>, subject: &str) -> String {
        format!(
            "From: winner@prize-draw.example\r\n\
             To: me@example.com\r\n\
             Subject: {subject}\r\n\
             Date: {}\r\n\
             \r\n\
             Claim your prize now. Reply with your bank details.\r\n",
            at.to_rfc2822()
        )
    }

    // ---- the spam folder is fetched ON DEMAND, never on the poll loop -------

    /// THE POINT OF THE WHOLE ARRANGEMENT: an ordinary poll tick must not so
    /// much as ASK about the SPAM label. Asserted against the mock's call log
    /// rather than against what landed, because "no spam rows appeared" would
    /// also pass if the walk ran and the folder happened to be empty — and the
    /// cost this avoids is the request, not the row.
    #[tokio::test]
    async fn a_poll_tick_never_touches_the_spam_label() {
        let (store, acct) = store_at_cursor(Some(100));
        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::quiet(100));
        g.history(LABEL_SENT, LabelHistory::quiet(100));
        // Scripted so a walk WOULD find something, which is what makes the
        // absence of the call meaningful.
        g.history(LABEL_SPAM, LabelHistory::added(140, &[(140, "g-spam")]));
        g.body("g-spam", spam_eml(Utc::now(), "you have won"));
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        assert!(
            !g.seen().iter().any(|c| c.contains(LABEL_SPAM)),
            "a poll tick asked about SPAM: {:?}",
            g.seen()
        );
    }

    /// And a catch-up — the expensive whole-window re-listing — does not either.
    #[tokio::test]
    async fn a_catch_up_never_lists_the_spam_label() {
        let (store, acct) = store_at_cursor(None);
        let g = MockGmail::default();
        g.listing(LABEL_INBOX, &[]);
        g.listing(LABEL_SENT, &[]);
        g.listing(LABEL_SPAM, &["g-spam"]);
        g.body("g-spam", spam_eml(Utc::now(), "you have won"));
        g.profile(500);
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        assert!(
            !g.seen().iter().any(|c| c.contains(LABEL_SPAM)),
            "a catch-up listed SPAM: {:?}",
            g.seen()
        );
    }

    /// Asked for explicitly, it fetches — and the row lands exactly as the old
    /// label walk left it: flagged spam, neutral, out of both LLM queues.
    #[tokio::test]
    async fn an_on_demand_spam_sync_fetches_and_lands_neutral() {
        let (store, acct) = store_at_cursor(Some(100));
        let g = MockGmail::default();
        g.listing(LABEL_SPAM, &["g-spam"]);
        g.body("g-spam", spam_eml(Utc::now(), "you have won"));
        let base = serve_mock(g.clone()).await;

        let n = engine(store.clone(), acct, &base)
            .sync_spam_window()
            .await
            .unwrap();
        assert_eq!(n, 1);

        // It is spam, and it is invisible to the ordinary mailbox.
        let inbox = store
            .attention_updates(
                acct,
                Utc::now() - ChronoDuration::days(1),
                None,
                None,
                None,
                false,
                crate::store::SpamScope::Exclude,
            )
            .unwrap();
        assert!(inbox.is_empty(), "spam must not reach the mailbox listing");

        let spam = store
            .attention_updates(
                acct,
                Utc::now() - ChronoDuration::days(1),
                None,
                None,
                None,
                false,
                crate::store::SpamScope::Only,
            )
            .unwrap();
        assert_eq!(spam.len(), 1, "and it must reach the spam page");

        // Never triaged: the same 'n/a' markers the sent branch gets.
        let row = store
            .triage_debug(acct, spam[0].update.id)
            .unwrap()
            .expect("triage row");
        assert_eq!(row.tier, "noise");
        assert_eq!(row.importance, 0);
        assert_eq!(row.stage1_model_used.as_deref(), Some("n/a"));

        // And the completion stamp is written, which is what lets the page say
        // "we looked" rather than guessing.
        assert!(
            store
                .get_app_setting(acct, SPAM_SYNCED_AT_KEY)
                .unwrap()
                .is_some()
        );
    }

    /// THE STAMP IS WRITTEN ONLY ON SUCCESS. A page that read a stamp moved by a
    /// failed fetch would tell somebody their provider filtered nothing when
    /// nobody managed to look.
    #[tokio::test]
    async fn a_failed_spam_sync_leaves_no_stamp() {
        let (store, acct) = store_at_cursor(Some(100));
        // No listing scripted for SPAM and a mock that fails unscripted keys.
        let dead = "http://127.0.0.1:1".to_string();
        let out = engine(store.clone(), acct, &dead).sync_spam_window().await;
        assert!(out.is_err());
        assert!(
            store
                .get_app_setting(acct, SPAM_SYNCED_AT_KEY)
                .unwrap()
                .is_none(),
            "a failed fetch must not look like a completed one"
        );
    }

    /// The cap is a bound on how long one click can take: `spam_max` messages,
    /// and pagination stops once it is reached rather than walking the folder.
    #[tokio::test]
    async fn the_spam_sync_stops_at_the_cap() {
        let (store, acct) = store_at_cursor(Some(100));
        let g = MockGmail::default();
        let ids: Vec<String> = (0..10).map(|i| format!("g-spam-{i}")).collect();
        let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        g.listing(LABEL_SPAM, &refs);
        for id in &ids {
            g.body(id, spam_eml(Utc::now(), "you have won"));
        }
        let base = serve_mock(g.clone()).await;

        let mut config = Config::default();
        config.sync.spam_max = 3;
        let eng = SyncEngine::new(
            store.clone(),
            Arc::new(FixedToken),
            acct,
            "me@example.com".to_string(),
            config,
        )
        .with_api_base(base.clone());

        assert_eq!(eng.sync_spam_window().await.unwrap(), 3);
        let fetched = g.seen().iter().filter(|c| c.starts_with("get:")).count();
        assert_eq!(fetched, 3, "the cap bounds the FETCHES, not just the rows");
    }

    #[tokio::test]
    async fn a_sent_history_record_ingests_neutral_and_silent() {
        // Sent from the phone: SENT only, never INBOX. Before the sent walk this
        // message simply did not exist locally until the next full backfill.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::quiet(100));
        g.history(LABEL_SENT, LabelHistory::added(140, &[(140, "g-phone")]));
        g.body(
            "g-phone",
            sent_eml(now, "alice@friends.com", "lunch thursday"),
        );
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        // The row landed...
        let view = store.thread_view(acct, "g-phone").unwrap();
        assert_eq!(view.messages.len(), 1);
        let mid = view.messages[0].id;

        // ...NEUTRAL, and out of both LLM queues: no triage spend on the user's
        // own writing, ever.
        let row = store.triage_debug(acct, mid).unwrap().expect("triage row");
        assert_eq!(row.tier, "noise");
        assert_eq!(row.importance, 0);
        assert_eq!(row.stage1_model_used.as_deref(), Some("n/a"));
        assert!(!row.needs_stage2);
        assert!(store.stage1_queue(acct, 10).unwrap().is_empty());
        assert!(store.stage2_queue(acct, 10).unwrap().is_empty());

        // THE SILENCE INVARIANT: never a push for mail the user sent themselves.
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        assert_eq!(store.latest_event_id(acct).unwrap(), 0);

        // And it stays out of the bands and out of search, as sent mail must.
        assert!(
            store
                .ranked_updates(acct, now - ChronoDuration::days(1), None)
                .unwrap()
                .is_empty()
        );
        assert!(store.search(acct, "lunch", 10, 0).unwrap().is_empty());

        // The recipients still seed contacts, which is half the point of having
        // the mail at all.
        assert!(store.is_known_contact(acct, "alice@friends.com").unwrap());
        assert_eq!(cursor_of(&store, acct), Some(140));
    }

    /// A sent row exactly as an install predating `to_addrs` left it: recipients
    /// NULL, triage row present.
    fn old_sent_row(store: &SqliteStore, acct: AccountId, gmail: &str) -> i64 {
        let id = store
            .upsert_message(&crate::types::NewMessage {
                account_id: acct,
                gmail_msg_id: gmail.to_string(),
                thread_id: format!("t-{gmail}"),
                from_addr: "me@example.com".to_string(),
                from_name: Some("Me".to_string()),
                subject: "lunch thursday".to_string(),
                received_at: Utc::now(),
                snippet: String::new(),
                body: "writing this from the phone app".to_string(),
                body_html: None,
                is_sent: true,
                is_spam: false,
                to_addrs: None,
                list_unsubscribe: None,
                list_unsub_one_click: false,
                auth_pass: None,
            })
            .unwrap();
        store
            .set_triage(
                id,
                acct,
                0,
                Tier::Noise,
                crate::types::Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
        id
    }

    #[tokio::test]
    async fn the_sent_recipients_backfill_fills_old_rows_exactly_once() {
        // Sent mail ingested before `to_addrs` existed has no recipients to show.
        // The one-shot backfill fetches headers only and fills them, then its
        // sync_state flag keeps it from ever paying for that walk again.
        let (store, acct) = store_at_cursor(Some(100));
        let id = old_sent_row(&store, acct, "g-old");

        let g = MockGmail::default();
        g.body(
            "g-old",
            sent_eml(Utc::now(), "Alice <alice@friends.com>", "lunch thursday"),
        );
        let base = serve_mock(g.clone()).await;
        let eng = engine(store.clone(), acct, &base);

        eng.backfill_sent_recipients().await.unwrap();
        let listed = store.sent_listing(acct, 10, 0).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].to, "Alice <alice@friends.com>");
        assert!(store.sent_missing_recipients(acct, 10).unwrap().is_empty());

        // Second run: the flag is set, so not one more Gmail call.
        let calls = g.calls("get:g-old");
        eng.backfill_sent_recipients().await.unwrap();
        assert_eq!(
            g.calls("get:g-old"),
            calls,
            "the sweep runs once per install"
        );
    }

    #[tokio::test]
    async fn the_sent_recipients_backfill_records_a_message_gmail_no_longer_has() {
        // A 404 is an answer, not a failure: the row is marked "looked, nobody
        // named" so it leaves the queue. Left NULL it would be handed back by the
        // very next batch query, and the pass would never terminate.
        let (store, acct) = store_at_cursor(Some(100));
        old_sent_row(&store, acct, "g-gone");

        let base = serve_mock(MockGmail::default()).await;
        engine(store.clone(), acct, &base)
            .backfill_sent_recipients()
            .await
            .unwrap();

        assert!(store.sent_missing_recipients(acct, 10).unwrap().is_empty());
        assert_eq!(store.sent_listing(acct, 10, 0).unwrap()[0].to, "");
    }

    #[tokio::test]
    async fn a_failed_sent_recipients_backfill_leaves_the_flag_unset() {
        // A non-404 error SKIPS the message (its row stays NULL) and the pass
        // finishes Ok — one message with a persistent 4xx quirk must not abort
        // the sweep for everything behind it. But the done flag is only set on
        // a clean pass, so the next daemon start redoes whatever is still NULL.
        let (store, acct) = store_at_cursor(Some(100));
        old_sent_row(&store, acct, "g-old");

        // No route at all: every fetch is a transport error, not a 404.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let dead = format!("http://{addr}");

        engine(store.clone(), acct, &dead)
            .backfill_sent_recipients()
            .await
            .unwrap();
        assert!(
            store
                .sync_state(acct, SENT_RECIPIENTS_KEY)
                .unwrap()
                .is_none(),
            "an interrupted sweep must retry on the next start"
        );
        assert_eq!(store.sent_missing_recipients(acct, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_sealed_outbound_copy_is_never_committed() {
        // The user replies from their phone to a thread quoting an OTP. Seal
        // detection fires on the reply, and `thread_guard_and_subject` 404s any
        // thread holding a sealed row — so committing it would hide the
        // counterparty's mail the user is reading. Nothing is written at all.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();
        let reply = format!(
            "From: me@example.com\r\n\
             To: Bank <noreply@bank.example>\r\n\
             Subject: Re: Your verification code\r\n\
             Date: {}\r\n\
             \r\n\
             got it, thanks\r\n\
             > Your one-time passcode is 483920. Enter this code to continue.\r\n",
            now.to_rfc2822()
        );

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::quiet(100));
        g.history(
            LABEL_SENT,
            LabelHistory::added(170, &[(170, "g-sealed-out")]),
        );
        g.body("g-sealed-out", reply);
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        assert!(
            store.thread_view(acct, "g-sealed-out").is_err(),
            "no row is committed for a sealed outbound copy"
        );
        assert!(
            store.sealed_messages(acct).unwrap().is_empty(),
            "not committed-then-sealed: not committed at all"
        );
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        // The poll is otherwise normal: the cursor still commits.
        assert_eq!(cursor_of(&store, acct), Some(170));
    }

    #[tokio::test]
    async fn a_self_addressed_send_keeps_its_visible_inbox_copy() {
        // Mail to yourself carries BOTH labels, so both filtered walks return it.
        // The message upsert writes `is_sent = excluded.is_sent` on conflict, so a
        // sent copy landing second would hide the message from every band.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();
        let eml = format!(
            "From: me@example.com\r\n\
             To: me@example.com\r\n\
             Subject: note to self about the lease\r\n\
             Date: {}\r\n\
             \r\n\
             remember to countersign\r\n",
            now.to_rfc2822()
        );

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::added(150, &[(150, "g-self")]));
        g.history(LABEL_SENT, LabelHistory::added(150, &[(150, "g-self")]));
        g.body("g-self", eml);
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        // ONE row, and it is the visible one.
        let view = store.thread_view(acct, "g-self").unwrap();
        assert_eq!(view.messages.len(), 1, "one row, not two");
        let visible = store
            .ranked_updates(acct, now - ChronoDuration::days(1), None)
            .unwrap();
        assert_eq!(
            visible.len(),
            1,
            "the inbox copy must win the unique-key race"
        );
        assert_eq!(visible[0].id, view.messages[0].id);

        // The INBOX walk ran FIRST and claimed the id, so the sent half never even
        // re-fetched it.
        assert!(
            g.at("history:INBOX") < g.at("history:SENT"),
            "{:?}",
            g.seen()
        );
        assert_eq!(g.calls("get:g-self"), 1, "fetched once, not twice");
    }

    #[tokio::test]
    async fn a_failing_sent_walk_keeps_the_inbox_batch_and_holds_the_cursor() {
        // Losing the sent half is survivable; losing INBOX progress is not. The
        // cursor stays put and the next poll re-walks both labels from it.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();
        let eml = format!(
            "From: Alice <alice@friends.com>\r\n\
             To: me@example.com\r\n\
             Subject: the lease\r\n\
             Date: {}\r\n\
             \r\n\
             sending the countersigned copy over\r\n",
            now.to_rfc2822()
        );

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::added(150, &[(150, "g-in")]));
        g.history(LABEL_SENT, LabelHistory::broken());
        g.body("g-in", eml);
        let base = serve_mock(g.clone()).await;

        // The poll SUCCEEDS: a sent-side outage is not worth a backoff cycle.
        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        assert_eq!(
            store
                .ranked_updates(acct, now - ChronoDuration::days(1), None)
                .unwrap()
                .len(),
            1,
            "the inbox batch is ingested even though the sent walk failed"
        );
        assert_eq!(
            cursor_of(&store, acct),
            Some(100),
            "a partial poll must not commit the cursor"
        );

        // The re-walk next poll is idempotent: still one row, still one event.
        let events_before = store.events_after(acct, 0, 100).unwrap().len();
        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();
        assert_eq!(store.thread_view(acct, "g-in").unwrap().messages.len(), 1);
        assert_eq!(
            store.events_after(acct, 0, 100).unwrap().len(),
            events_before
        );
        assert_eq!(cursor_of(&store, acct), Some(100));
    }

    #[tokio::test]
    async fn the_cursor_commits_once_at_the_max_across_both_walks() {
        // One commit, at the highest historyId either walk saw — committing per
        // walk would let the second one rewind the first.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::added(150, &[(150, "g-in")]));
        g.history(LABEL_SENT, LabelHistory::added(220, &[(220, "g-out")]));
        g.body(
            "g-in",
            format!(
                "From: Alice <alice@friends.com>\r\n\
                 To: me@example.com\r\n\
                 Subject: the lease\r\n\
                 Date: {}\r\n\
                 \r\n\
                 countersigned copy attached\r\n",
                now.to_rfc2822()
            ),
        );
        g.body("g-out", sent_eml(now, "alice@friends.com", "re: the lease"));
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        assert_eq!(cursor_of(&store, acct), Some(220), "max across both walks");
        // Both landed, each on its own side of the visibility line.
        assert_eq!(
            store
                .ranked_updates(acct, now - ChronoDuration::days(1), None)
                .unwrap()
                .len(),
            1,
            "only the received message is in a band"
        );
        assert_eq!(store.thread_view(acct, "g-out").unwrap().messages.len(), 1);
        assert!(store.is_known_contact(acct, "alice@friends.com").unwrap());
    }

    #[tokio::test]
    async fn catch_up_lists_inbox_and_sent_over_the_same_window() {
        // No cursor: the history walk cannot account for the gap, so both labels
        // are re-listed or everything written from another client is lost.
        let (store, acct) = store_at_cursor(None);
        let now = Utc::now();

        let g = MockGmail::default();
        // "g-self" is in BOTH listings, as a self-addressed message is.
        g.listing(LABEL_INBOX, &["g-in", "g-self"]);
        g.listing(LABEL_SENT, &["g-out", "g-self"]);
        g.body(
            "g-in",
            format!(
                "From: Alice <alice@friends.com>\r\n\
                 To: me@example.com\r\n\
                 Subject: the lease\r\n\
                 Date: {}\r\n\
                 \r\n\
                 countersigned copy attached\r\n",
                now.to_rfc2822()
            ),
        );
        g.body("g-out", sent_eml(now, "bob@friends.com", "re: the lease"));
        g.body(
            "g-self",
            format!(
                "From: me@example.com\r\n\
                 To: me@example.com\r\n\
                 Subject: note to self\r\n\
                 Date: {}\r\n\
                 \r\n\
                 countersign this\r\n",
                now.to_rfc2822()
            ),
        );
        g.profile(900);
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        assert!(g.at("list:INBOX") < g.at("list:SENT"), "{:?}", g.seen());
        assert_eq!(store.thread_view(acct, "g-out").unwrap().messages.len(), 1);
        assert!(store.is_known_contact(acct, "bob@friends.com").unwrap());
        // The dual-listed message keeps its visible copy, exactly as in the walk.
        let visible: Vec<String> = store
            .ranked_updates(acct, now - ChronoDuration::days(1), None)
            .unwrap()
            .into_iter()
            .map(|u| u.thread_id)
            .collect();
        assert!(visible.contains(&"g-in".to_string()));
        assert!(visible.contains(&"g-self".to_string()));
        assert_eq!(g.calls("get:g-self"), 1, "fetched once, not twice");
        assert_eq!(cursor_of(&store, acct), Some(900));
    }

    #[tokio::test]
    async fn a_first_backfill_waits_for_the_embedder_gate() {
        // A brand-new tenant: no cursor, so the very next thing `run_once` would
        // do is the 30-day backfill. Behind a shut gate it does not do it, and
        // "does not" is asserted on the WIRE rather than on a flag, because the
        // failure this prevents is thousands of rows ingested with no vector.
        let (store, acct) = store_at_cursor(None);
        let g = MockGmail::default();
        g.listing(LABEL_INBOX, &[]);
        g.listing(LABEL_SENT, &[]);
        g.profile(900);
        let base = serve_mock(g.clone()).await;

        // The sender is HELD for the length of the wait: dropping it says "the
        // init task is gone, nothing will ever settle this", which is a reason
        // to stop waiting, not to keep waiting.
        let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        // Sent before the call, but the receiver has not observed it, so its
        // `changed()` fires the instant the engine parks on the gate. That is
        // the shutdown-during-the-wait path, and it is what makes this test
        // terminate rather than sit here for the ceiling.
        shutdown_tx.send(true).unwrap();

        engine(store.clone(), acct, &base)
            .with_embedder_gate(gate_rx)
            .run_once(&mut shutdown_rx)
            .await
            .unwrap();

        assert!(
            g.seen().is_empty(),
            "a shut gate holds the first backfill off the wire, saw {:?}",
            g.seen()
        );
        assert_eq!(
            cursor_of(&store, acct),
            None,
            "and leaves no cursor, so the next start is still a first run"
        );

        // Same engine, same mailbox, one bit different: an OPEN gate is not
        // waited on at all and the backfill goes straight out.
        let (_open_tx, open_rx) = tokio::sync::watch::channel(true);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        // This one lands after the (skipped) wait, on the poll loop's own
        // shutdown check, so the backfill runs to completion first.
        shutdown_tx.send(true).unwrap();

        engine(store.clone(), acct, &base)
            .with_embedder_gate(open_rx)
            .run_once(&mut shutdown_rx)
            .await
            .unwrap();

        assert!(g.at("list:INBOX") < g.at("list:SENT"), "{:?}", g.seen());
        assert_eq!(
            cursor_of(&store, acct),
            Some(900),
            "an open gate releases the first backfill"
        );
    }

    #[tokio::test]
    async fn an_engine_with_no_embedder_gate_backfills_at_once() {
        // Sync-only mode builds its embedder before the engine exists, and the
        // rest of the tests have nothing to wait for. Absence of a gate must
        // therefore mean "never wait", not "wait for something that will never
        // arrive" — that reading would wedge `squelchd run` on startup.
        let (store, acct) = store_at_cursor(None);
        let g = MockGmail::default();
        g.listing(LABEL_INBOX, &[]);
        g.listing(LABEL_SENT, &[]);
        g.profile(900);
        let base = serve_mock(g.clone()).await;

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        shutdown_tx.send(true).unwrap();

        engine(store.clone(), acct, &base)
            .run_once(&mut shutdown_rx)
            .await
            .unwrap();

        assert_eq!(
            cursor_of(&store, acct),
            Some(900),
            "no gate, no wait: the first backfill ran on this call"
        );
    }

    /// An engine parked on the gate, for the three tests below. No mock: they
    /// drive the WAIT, and the wait touches neither Gmail nor the store, so the
    /// base is a port nothing is listening on and reaching it would be the bug.
    fn gated_engine(
        gate: tokio::sync::watch::Receiver<bool>,
        metrics: Arc<SyncMetrics>,
    ) -> SyncEngine<SqliteStore, FixedToken> {
        let (store, acct) = store_at_cursor(None);
        engine(store, acct, "http://127.0.0.1:1")
            .with_metrics(metrics)
            .with_embedder_gate(gate)
    }

    /// `changed()` fires on ANY send and on the last sender dropping, so the
    /// value is the only thing that says a shutdown happened. Read the wakeup
    /// alone as one and the cost is not a slow first backfill, it is no sync at
    /// all: `run_once` returns `Ok` on a reported shutdown and `run` reads that
    /// as "we are done" and stops the lifecycle.
    ///
    /// MUTATION 1: `_ = shutdown.changed() => return false`, dropping the value
    /// check, the obvious reading: the wait reports "shut down" and the sync
    /// lifecycle ends. MUTATION 2: the arm returning `true` instead of
    /// re-parking: the backfill runs unembedded on a wakeup that asked for
    /// nothing. This asserts both halves: the wait is still parked after the
    /// wakeup, and it releases only when the gate opens.
    #[tokio::test(start_paused = true)]
    async fn a_shutdown_wakeup_carrying_false_is_not_a_shutdown() {
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        // Unseen by this receiver, so `changed()` completes the instant the
        // engine parks. The VALUE stays false: nobody asked for a shutdown.
        shutdown_tx.send(false).unwrap();
        let e = gated_engine(gate_rx, SyncMetrics::new());

        let wait = e.wait_for_embedder(&mut shutdown_rx);
        tokio::pin!(wait);
        // Still parked after the wakeup: under the paused clock a one-second
        // timeout elapses before the three-minute ceiling could.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut wait)
                .await
                .is_err(),
            "a false shutdown wakeup released the first backfill"
        );
        gate_tx.send(true).unwrap();
        let go_ahead = tokio::time::timeout(EMBEDDER_GATE_CEILING / 2, &mut wait)
            .await
            .expect("the gate opening releases the wait");
        assert!(go_ahead, "false on the shutdown watch is not a shutdown");
    }

    /// The gate's sender dropping is the init task GONE. Nothing is ever going
    /// to open it, so waiting the ceiling out would buy a brand-new tenant
    /// minutes of empty mailbox for an answer that has already arrived.
    ///
    /// MUTATION: folding `Err` into the "keep waiting" side (an `Err` arm that
    /// falls through to the sleep, or no `Err` arm at all). Under a paused clock
    /// that runs to the ceiling, which this timeout is half of.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_embedder_gate_releases_the_backfill_at_once() {
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        drop(gate_tx);
        // Never signalled: the shutdown arm must not be what ends this wait.
        let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let e = gated_engine(gate_rx, SyncMetrics::new());

        let go_ahead = tokio::time::timeout(
            EMBEDDER_GATE_CEILING / 2,
            e.wait_for_embedder(&mut shutdown_rx),
        )
        .await
        .expect("a gate nobody holds any more is not worth waiting on");
        assert!(go_ahead, "a dropped gate releases the first backfill");
    }

    /// The ceiling arm: a gate whose sender is alive and never sends (an init
    /// wedged on a hung download) degrades to the behaviour that predates the
    /// gate — an unembedded backfill the vector pass drains later — rather than
    /// to a daemon that never syncs. And it SAYS SO to the scrape, because this
    /// is the one event that re-arms the memory the gate exists to avoid, and
    /// nobody reads a tenant's stderr until something has already fallen over.
    ///
    /// Time is paused, so the ceiling is a fact here and not three real minutes.
    ///
    /// MUTATIONS: no ceiling arm at all (the outer timeout fires instead of the
    /// wait returning); a ceiling that reports a shutdown (`go_ahead`); a
    /// ceiling that does not wait, e.g. `Duration::ZERO` (`elapsed`); and the
    /// counter left unrecorded (the exposition assert).
    #[tokio::test(start_paused = true)]
    async fn the_gate_ceiling_releases_the_backfill_and_says_so_to_the_scrape() {
        let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let metrics = SyncMetrics::new();
        let e = gated_engine(gate_rx, metrics.clone());

        let started = tokio::time::Instant::now();
        let go_ahead = tokio::time::timeout(
            EMBEDDER_GATE_CEILING * 2,
            e.wait_for_embedder(&mut shutdown_rx),
        )
        .await
        .expect("a wedged init must not wedge sync");
        assert!(go_ahead, "the ceiling releases the backfill");
        assert!(
            started.elapsed() >= EMBEDDER_GATE_CEILING,
            "and only after the ceiling, not instead of waiting"
        );
        assert!(
            crate::metrics::render(&metrics, None)
                .contains("squelchd_embedder_gate_timeouts_total 1\n"),
            "a ceiling nobody can scrape is a ceiling nobody knows fired"
        );
    }

    #[tokio::test]
    async fn a_message_the_api_already_echoed_redelivers_as_a_no_op() {
        // The api echoes a send at send time; the sent walk then delivers the very
        // same Gmail id on the next poll.
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();
        let eml = sent_eml(now, "alice@friends.com", "re: the lease");

        let echoed = ingest::ingest_sent(
            &store,
            acct,
            "me@example.com",
            "g-echo",
            None,
            eml.clone().into_bytes(),
            Some(now),
            now,
        )
        .unwrap()
        .expect("the echo commits a row");

        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::quiet(100));
        g.history(LABEL_SENT, LabelHistory::added(160, &[(160, "g-echo")]));
        g.body("g-echo", eml);
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        let view = store.thread_view(acct, "g-echo").unwrap();
        assert_eq!(view.messages.len(), 1, "one row, not a second copy");
        assert_eq!(view.messages[0].id, echoed, "the same local row");
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        assert!(
            store
                .ranked_updates(acct, now - ChronoDuration::days(1), None)
                .unwrap()
                .is_empty(),
            "re-delivery must not promote the echo into a band"
        );
        let row = store
            .triage_debug(acct, echoed)
            .unwrap()
            .expect("triage row");
        assert_eq!(row.tier, "noise");
        assert_eq!(row.importance, 0);
    }

    #[tokio::test]
    async fn the_inbox_unread_counts_refresh_and_survive_a_failed_fetch() {
        // Gmail owns these numbers: nothing local can be re-derived into them,
        // so a failed fetch must leave the last known pair standing rather than
        // clear it or read as "0 unread".
        let (store, acct) = store_at_cursor(Some(100));

        let g = MockGmail::default();
        g.label(LABEL_INBOX, Some((214, 190)));
        let base = serve_mock(g.clone()).await;
        let engine = engine(store.clone(), acct, &base);

        engine.refresh_inbox_unread().await;
        let got = store.inbox_unread(acct).unwrap().expect("counts stored");
        assert_eq!((got.messages, got.threads), (214, 190));
        assert_eq!(g.calls("label:INBOX"), 1, "one labels.get per refresh");

        // Gmail goes down mid-run: the refresh swallows it (a poll cycle must
        // not die over a cosmetic number) and the stored pair is untouched.
        g.label(LABEL_INBOX, None);
        engine.refresh_inbox_unread().await;
        let kept = store.inbox_unread(acct).unwrap().expect("last counts kept");
        assert_eq!((kept.messages, kept.threads), (214, 190));
        assert_eq!(kept.fetched_at, got.fetched_at, "the stamp did not move");

        // Recovery overwrites with the newer truth.
        g.label(LABEL_INBOX, Some((3, 3)));
        engine.refresh_inbox_unread().await;
        let fresh = store.inbox_unread(acct).unwrap().unwrap();
        assert_eq!((fresh.messages, fresh.threads), (3, 3));
    }

    #[tokio::test]
    async fn a_200_that_is_not_a_label_keeps_the_counts_but_a_counterless_label_zeroes_them() {
        // The two 200s that look alike to a defaulted decoder and must not be
        // treated alike: Gmail dropping the counters (a real zero) versus a
        // body that is not a label at all (no answer, keep the last one).
        let (store, acct) = store_at_cursor(Some(100));

        let g = MockGmail::default();
        g.label(LABEL_INBOX, Some((214, 190)));
        let base = serve_mock(g.clone()).await;
        let engine = engine(store.clone(), acct, &base);
        engine.refresh_inbox_unread().await;
        let seeded = store.inbox_unread(acct).unwrap().expect("counts stored");

        // Not label resources: an empty object and an error envelope served
        // with a 200. Required `id` makes each a decode error, so the seeded
        // pair stands untouched — the stamp included.
        for body in [json!({}), json!({ "error": { "code": 403 } })] {
            g.label_body(LABEL_INBOX, body);
            engine.refresh_inbox_unread().await;
            let kept = store.inbox_unread(acct).unwrap().expect("last counts kept");
            assert_eq!((kept.messages, kept.threads), (214, 190));
            assert_eq!(kept.fetched_at, seeded.fetched_at, "the stamp did not move");
        }

        // A label resource WITHOUT the counters is Gmail's way of saying zero:
        // it omits them on a label with nothing unread, so this one is stored.
        g.label_body(LABEL_INBOX, json!({ "id": LABEL_INBOX, "name": "INBOX" }));
        engine.refresh_inbox_unread().await;
        let zeroed = store
            .inbox_unread(acct)
            .unwrap()
            .expect("zero is an answer");
        assert_eq!((zeroed.messages, zeroed.threads), (0, 0));
    }

    #[tokio::test]
    async fn a_gmail_outage_cannot_starve_the_reminder_sweep() {
        // THE SWEEP IS LOCAL, so it runs AHEAD of the walk. A tick that bounces
        // — an expired credential, a Gmail outage — takes everything after
        // `poll_once` with it, and a reminder hung off the far side of that call
        // would sit unfired for as long as the outage lasts: a weekend of Gmail
        // being down means Saturday's reminder never arrives.
        let (store, acct) = store_at_cursor(Some(100));
        let mid = store
            .upsert_message(&NewMessage {
                account_id: acct,
                gmail_msg_id: "g-parked".into(),
                thread_id: "t-parked".into(),
                from_addr: "alice@friends.com".into(),
                from_name: None,
                subject: "the thing you parked".into(),
                received_at: Utc::now() - ChronoDuration::days(1),
                snippet: String::new(),
                body: "body".into(),
                body_html: None,
                is_sent: false,
                is_spam: false,
                to_addrs: None,
                list_unsubscribe: None,
                list_unsub_one_click: false,
                auth_pass: None,
            })
            .unwrap();
        store
            .set_triage(
                mid,
                acct,
                0,
                Tier::Noise,
                crate::types::Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
        store
            .set_reminder(acct, mid, Utc::now() - ChronoDuration::minutes(1))
            .unwrap();

        // Gmail is down for this mailbox: the INBOX walk 500s, so the tick
        // returns Err and the loop hands it to the caller's backoff.
        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::broken());
        let base = serve_mock(g.clone()).await;
        let (_tx, mut shutdown) = tokio::sync::watch::channel(false);
        assert!(
            engine(store.clone(), acct, &base)
                .poll_lane(&mut shutdown)
                .await
                .is_err(),
            "the poll itself failed, which is the whole premise"
        );

        // ...and the reminder fired anyway.
        let rows = store
            .attention_updates(
                acct,
                Utc::now() - ChronoDuration::days(30),
                None,
                None,
                None,
                false,
                SpamScope::Exclude,
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, crate::types::AttentionStatus::Open);
        assert!(rows[0].remind_at.is_none(), "the pending stamp MOVED");
        assert!(rows[0].reminded_at.is_some(), "it came back on time");
    }

    // ---- the two lanes, on the wire ----------------------------------------
    //
    // Nothing else in this file drives a refine pass against a mock LLM through
    // the engine; the emission sites are tested by hand-written mirrors. This
    // one property is worth the harness because it is the property the whole
    // change is about (docs/NOTIFY.md §11.2, §11.10).

    /// THE STAMP IS READ PER MESSAGE, NOT PER BATCH. One
    /// `fetch_raw_and_ingest` call over two ids, with the second one's fetch
    /// made to take real time, and the two stamps the engine wrote must differ
    /// by roughly that much.
    ///
    /// A batch-wide `let now = Utc::now()` passes every other test in this file:
    /// the engine's own emission tests call `ingest_one` with an injected clock,
    /// so only this path can tell. What it costs in production is the whole
    /// point of docs/NOTIFY.md §11.3. `catch_up` runs this loop over the entire
    /// backfill window, one sequential `format=raw` GET per id, and
    /// `catch_up_inner`'s own comment records a tenant spending half an hour in
    /// there. Frozen at the top:
    ///
    /// - a genuinely new message fetched 30 minutes in enters the Stage-1 queue
    ///   with half its rescue window already spent, and expires before the refine
    ///   lane reaches it: a notification dropped AND booked as `deliberate/expired`
    ///   in the ledger the rollout decision is read off;
    /// - past an hour of batch runtime every remaining message reads as dated in
    ///   the future, fails `is_fresh`, and is stamped NULL — unnotifiable at every
    ///   site, forever. That is §2a's silent drop, reintroduced on the
    ///   post-outage path where notifications matter most.
    ///
    /// The stall is a FIXTURE, not an assertion: the test asserts on two
    /// timestamps the engine wrote to SQLite, and a slow machine only widens the
    /// gap it is checking for.
    #[tokio::test]
    async fn the_eligibility_stamp_is_taken_per_message_not_per_batch() {
        const STALL: Duration = Duration::from_millis(400);

        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();

        // BOTH IDS IN ONE BATCH: one history record, so `history_walk` makes a
        // single `fetch_raw_and_ingest` call over both.
        let g = MockGmail::default();
        g.history(
            LABEL_INBOX,
            LabelHistory::added(141, &[(140, "g-early"), (141, "g-late")]),
        );
        g.history(LABEL_SENT, LabelHistory::quiet(100));
        g.body("g-early", alert_eml(now));
        g.body(
            "g-late",
            alert_eml(now).replace("checkout api", "billing api"),
        );
        g.slow_body("g-late", STALL);
        let base = serve_mock(g.clone()).await;

        engine(store.clone(), acct, &base)
            .poll_once()
            .await
            .unwrap();

        let id_of = |thread: &str| store.thread_view(acct, thread).unwrap().messages[0].id;
        let early = stamp_of(&store, acct, id_of("g-early")).expect("stamped at first sight");
        let late = stamp_of(&store, acct, id_of("g-late")).expect("stamped at first sight");

        let gap = late - early;
        assert!(
            gap >= ChronoDuration::milliseconds(STALL.as_millis() as i64 / 2),
            "the second message was fetched {STALL:?} after the first and must be \
             stamped accordingly; the stamps are {gap:?} apart, which is a clock \
             read once for the whole batch"
        );
    }

    /// THE POKE, on its own. Deleting `refine_wake.notify_one()` from
    /// `fetch_raw_and_ingest` used to leave the whole suite green, because
    /// `poll_secs` defaults to 5 and that is also the refine lane's sleep floor:
    /// the lane woke on the timer and every test still passed, with the contract's
    /// "start on new mail immediately" (docs/NOTIFY.md §11.2) quietly downgraded
    /// to "up to poll_secs late".
    ///
    /// Asserted on the `Notify` PERMIT rather than on the lane's timing, which is
    /// what makes it deterministic and free of sleeps in both directions:
    /// `tokio::time::timeout` polls its inner future before its timer, and
    /// `notified()` is immediately ready exactly when a permit is stored. So a
    /// ZERO budget is a synchronous "is there a permit", not a wait.
    #[tokio::test]
    async fn ingesting_new_mail_pokes_the_refine_lane_and_a_backfill_does_not() {
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();
        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::added(140, &[(140, "g-new")]));
        g.history(LABEL_SENT, LabelHistory::quiet(100));
        g.listing(LABEL_INBOX, &["g-old"]);
        g.listing(LABEL_SENT, &[]);
        g.body("g-new", alert_eml(now));
        g.body(
            "g-old",
            alert_eml(now).replace("checkout api", "billing api"),
        );
        let base = serve_mock(g.clone()).await;
        let eng = engine(store.clone(), acct, &base);

        let poked = || tokio::time::timeout(Duration::ZERO, eng.refine_wake.notified());

        // A BACKFILL DOES NOT POKE, even though it ingests: `run_once` refines
        // the first backfill inline before either lane exists, and the poke means
        // "mail that may be genuinely new is waiting", which a month of already
        // read mail is not.
        let n = eng
            .fetch_raw_and_ingest(
                &["g-old".to_string()],
                Mailbox::Inbox,
                IngestOrigin::Backfill,
            )
            .await
            .unwrap();
        assert_eq!(n, 1, "the backfill really did ingest a row");
        assert!(
            poked().await.is_err(),
            "a backfill must leave the refine lane parked"
        );

        // An incremental tick that ingests DOES poke, and the permit is stored
        // before `poll_once` even returns, so a lane that is mid-round when it
        // lands still wakes as soon as it finishes.
        eng.poll_once().await.unwrap();
        assert!(
            poked().await.is_ok(),
            "new mail must start through Stage-1 immediately, not at the next \
             poll_secs tick"
        );

        // ...and the permit COALESCES rather than accumulating: one round is
        // queued, not one per message.
        assert!(
            poked().await.is_err(),
            "the permit was consumed by the wait above; Notify holds exactly one"
        );
    }

    /// The Stage-1 verdict the held mock eventually answers with. Its content
    /// does not matter to this test — only that the call BLOCKS until we say so
    /// and then completes cleanly, so the lane is parked rather than erroring.
    const HELD_STAGE1_VERDICT: &str = r#"{"importance":72,"tier":"signal","has_deadline":false,
        "deadline_iso":null,"deadline_kind":null,"one_line":"a real person",
        "reason":"personal","importance_reason":"known","deadline_reason":null,
        "confident":true}"#;

    /// An Anthropic-shaped `/v1/messages` that HOLDS the first request open on a
    /// oneshot the test owns. This is the stand-in for the real thing the refine
    /// lane parks in: a reasoning-model call that legitimately thinks for
    /// minutes on a hard row.
    #[derive(Clone)]
    struct HeldLlm {
        /// Requests the mock has RECEIVED (not answered), so the test can wait
        /// for the lane to actually be inside the call rather than sleeping and
        /// hoping.
        seen: Arc<std::sync::atomic::AtomicUsize>,
        /// Taken by the first request and awaited; every later request is
        /// answered at once, so releasing the hold does not wedge the tail of
        /// the test.
        release: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    }

    async fn held_llm_messages(State(s): State<HeldLlm>) -> Json<Value> {
        s.seen.fetch_add(1, Ordering::SeqCst);
        // The std guard is dropped at the end of this statement, BEFORE the
        // await below: holding a std::sync lock across an await point would
        // deadlock the single-threaded test runtime.
        let held = s.release.lock().unwrap().take();
        if let Some(rx) = held {
            let _ = rx.await;
        }
        Json(json!({
            "content": [{ "type": "text", "text": HELD_STAGE1_VERDICT }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 10, "output_tokens": 5 },
        }))
    }

    /// Bind the held LLM on its own ephemeral loopback port. Its OWN server, not
    /// a route bolted onto [`serve_mock`]: a request that hangs forever is the
    /// entire point of this mock, and axum serves connections concurrently but
    /// there is no reason to make the Gmail routes share a fate with it.
    ///
    /// Plain http is deliberate and allowed: `base_url_transport_ok` permits it
    /// for loopback exactly so a test can do this.
    async fn serve_held_llm(s: HeldLlm) -> String {
        let app = Router::new()
            .route("/v1/messages", post(held_llm_messages))
            .with_state(s);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// Poll `ready` until it holds or `budget` runs out. The sleep is the POLL
    /// INTERVAL, never the assertion: the caller asserts on the returned bool,
    /// so a slow machine costs a few more iterations and a broken lane split
    /// costs the budget and then fails.
    async fn holds_within(budget: Duration, mut ready: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if ready() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A GMAIL OUTAGE COSTS NO MODEL CALLS, which is what the serial loop gave
    /// for free and the lane split gave away.
    ///
    /// The shape that was found: a credential goes `invalid_grant` with rows in
    /// the queues. `run()` -> `run_once` -> `run_lanes`; `select!` polls both
    /// lanes, the refine lane runs synchronously from its first poll through
    /// `gate_budget` (which charges BEFORE the call, deliberately) and issues a
    /// classify POST before `poll_lane` reaches a real await; `poll_once` then
    /// `Err`s, `run_lanes` returns, and the refine future is dropped mid-call.
    /// No verdict lands, the row stays queued, nothing refunds. `run()`'s backoff
    /// is initialised outside its loop and caps at five minutes, so the outage
    /// re-enters `run_lanes` up to 288 times a day and re-picks the same
    /// head-of-queue row every time. `stage2.global_daily_cap` defaults to 120
    /// against a UTC-DAY budget key, so ten hours of downtime drained the whole
    /// day's escalation budget on calls nobody ever read, and nothing escalated
    /// again until midnight even once the credential was fixed.
    #[tokio::test]
    async fn a_gmail_outage_spends_nothing_on_the_model() {
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();

        // A row Stage-1 would classify the moment it got the chance.
        let f = fixture(acct, "g-queued", &alert_eml(now), false);
        ingest_deferring_to_refine(&store, acct, &f, now, IngestOrigin::Incremental);
        assert_eq!(store.stage1_queue(acct, 10).unwrap().len(), 1);

        // GMAIL IS DOWN: `history.list` 500s, so `poll_once` Errs on every tick.
        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::broken());
        g.history(LABEL_SENT, LabelHistory::broken());
        let gmail_base = serve_mock(g.clone()).await;

        // A model that answers IMMEDIATELY: nothing here is about a slow call, so
        // the hold is empty and any request the lane makes is a request it
        // completes. That makes a nonzero count unambiguous.
        let llm = HeldLlm {
            seen: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            release: Arc::new(Mutex::new(None)),
        };
        let llm_base = serve_held_llm(llm.clone()).await;
        let mut config = Config::default();
        config.stage2.anthropic_api_key = Some("sk-ant-test".to_string());
        config.stage2.anthropic_base_url = Some(llm_base);
        config.stage2.stage2_provider = Some(Stage2Provider::Anthropic);
        let eng = engine_with_config(store.clone(), acct, &gmail_base, config);

        // Three backoff cycles, exactly as `run()` drives them: `run_lanes`,
        // `Err`, clear the health flag, round again.
        let (_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        for cycle in 0..3 {
            let out = eng.run_lanes(&mut shutdown_rx).await;
            assert!(out.is_err(), "cycle {cycle}: the poll lane must bubble");
            eng.poll_healthy.store(false, Ordering::Relaxed);
        }
        assert_eq!(
            llm.seen.load(Ordering::SeqCst),
            0,
            "a broken credential must not keep charging a DAY-scoped budget for \
             verdicts that are cancelled one Gmail round trip later"
        );
        assert_eq!(
            store.stage1_queue(acct, 10).unwrap().len(),
            1,
            "and the row is still queued, waiting for a lane that can finish"
        );

        // CONTROL, and it is the whole test: the row, the queue, the key and the
        // mock are all live, so the silence above is the gate and not a harness
        // that could never have spent anything.
        eng.stage1_pass().await;
        assert_eq!(
            llm.seen.load(Ordering::SeqCst),
            1,
            "the same engine spends the moment a pass actually runs"
        );
    }

    /// THE KEYSTONE. Mail that arrives while the refine lane is inside a model
    /// call is still fetched, ingested and stored.
    ///
    /// This is the whole point of the wave. Before the split there was ONE loop:
    /// poll, then Stage-1, then Stage-2, then the extractors, then the revisits,
    /// then sleep. `stage1_pass` awaits one classify call per queued row, so
    /// while the model thought about message A the loop was not at the Gmail
    /// call at all — message B did not arrive late, it did not arrive. On a
    /// reasoning model at high effort that window is minutes wide, and a manual
    /// refresh from the app could not shorten it either: the poke only wakes a
    /// loop that is asleep, and this one was busy.
    ///
    /// ON THE OLD SERIAL LOOP THIS TEST HANGS AND FAILS: the mock never answers
    /// until the test releases it, the test never releases it until B lands, and
    /// B cannot land because the fetch is queued behind the model call. That
    /// deadlock is the bug, reproduced.
    ///
    /// It is also why the drop in docs/NOTIFY.md §2a was silent: A's own
    /// notification aged out of the freshness window while the lane sat here.
    #[tokio::test]
    async fn mail_arriving_during_a_model_call_is_still_ingested() {
        let (store, acct) = store_at_cursor(Some(100));
        let now = Utc::now();
        let eml = |subject: &str| {
            format!(
                "From: Alice <alice@friends.com>\r\n\
                 To: me@example.com\r\n\
                 Subject: {subject}\r\n\
                 Date: {}\r\n\
                 \r\n\
                 a body worth classifying\r\n",
                now.to_rfc2822()
            )
        };

        // A is on the wire from the first tick; B is added later, by hand, once
        // the refine lane is provably stuck.
        let g = MockGmail::default();
        g.history(LABEL_INBOX, LabelHistory::added(140, &[(140, "g-a")]));
        g.history(LABEL_SENT, LabelHistory::quiet(100));
        g.body("g-a", eml("the lease"));
        let gmail_base = serve_mock(g.clone()).await;

        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let llm = HeldLlm {
            seen: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            release: Arc::new(Mutex::new(Some(release_rx))),
        };
        let llm_base = serve_held_llm(llm.clone()).await;

        let mut config = Config::default();
        config.stage2.anthropic_api_key = Some("sk-ant-test".to_string());
        config.stage2.anthropic_base_url = Some(llm_base);
        // FORCED, not inferred: `resolve_llm` reads the ambient environment
        // first, and a developer with only `OPENAI_API_KEY` exported would
        // otherwise resolve an OpenAI provider whose URL is the real
        // api.openai.com rather than the loopback mock.
        config.stage2.stage2_provider = Some(Stage2Provider::Anthropic);
        // THE FAST LANE IS OFF FOR THIS TEST, and its absence is the subject
        // staying the subject. It resolves to the SAME loopback mock, so with it
        // on, every ingested message would park a spawned task on the held
        // oneshot and step 3's "exactly one call, still held" would be counting
        // the notify lane's calls as well as the refine lane's. What is being
        // proved here is that the POLL lane is not queued behind the REFINE
        // lane; the fast lane has its own tests, and it is spawned rather than
        // awaited precisely so it cannot block either of them.
        config.notify.fast_enabled = false;

        let refresh = Arc::new(tokio::sync::Notify::new());
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let eng = engine_with_config(store.clone(), acct, &gmail_base, config)
            .with_refresh(refresh.clone());

        // `join!` rather than `spawn`: both halves run on this one task, so the
        // engine only makes progress at its own await points and this test is
        // driving the real `run_once`, cursor and all.
        let driver = eng.run_once(&mut shutdown_rx);
        let checker = async {
            // 1. The refine lane is INSIDE the model call. Asserted on the
            //    mock's own counter, not on elapsed time.
            let called = holds_within(Duration::from_secs(20), || {
                llm.seen.load(Ordering::SeqCst) > 0
            })
            .await;
            assert!(
                called,
                "the refine lane never reached the model; nothing is being blocked, \
                 so the rest of this test would prove nothing"
            );

            // 2. New mail lands in the mailbox and the app pokes refresh, which
            //    is the exact sequence a user watching for a reply performs.
            g.history(LABEL_INBOX, LabelHistory::added(180, &[(180, "g-b")]));
            g.body("g-b", eml("the countersigned copy"));
            refresh.notify_one();

            // 3. THE ASSERTION. B is stored while the model call for A is still
            //    outstanding.
            let landed = holds_within(Duration::from_secs(10), || {
                store
                    .thread_view(acct, "g-b")
                    .map(|v| !v.messages.is_empty())
                    .unwrap_or(false)
            })
            .await;
            assert!(
                landed,
                "message B never landed: the poll lane is still queued behind the \
                 refine lane's model call"
            );

            // ...and the hold really was still on, so B did not simply arrive
            // after a serial loop finished with A. A's row is still queued with
            // no Stage-1 stamp, which is where a row sits mid-classify.
            assert_eq!(
                llm.seen.load(Ordering::SeqCst),
                1,
                "the mock answered nothing: exactly one call, still held"
            );
            let a_id = store.thread_view(acct, "g-a").unwrap().messages[0].id;
            assert!(
                store
                    .triage_debug(acct, a_id)
                    .unwrap()
                    .expect("A has a triage row")
                    .stage1_model_used
                    .is_none(),
                "A is still mid-classify, which is what B overtook"
            );

            // 4. Release, then shut down, so the driver returns rather than
            //    leaving the mock's connection open at the end of the test.
            release_tx.send(()).unwrap();
            shutdown_tx.send(true).unwrap();
        };
        let (ran, ()) = tokio::join!(driver, checker);
        ran.expect("the lifecycle ends on shutdown, not on an error");
    }

    // ---- the deliberate lane's ledger writes (docs/NOTIFY.md §11.7) ---------
    //
    // THE CROSS-LANE FACTS ARE JOINS, NOT COLUMNS (§11.4), so every test here
    // drives BOTH lanes over one real message and reads the pair of rows back:
    // the fast lane through `NotifyLane::run`, exactly as `fetch_raw_and_ingest`
    // spawns it, and the deliberate lane through `emit_deliberate`, exactly as
    // the Stage-1 apply site calls it. Asserting on one lane's row alone would
    // pass on a build where the other lane never wrote one, which is the shape
    // of bug that makes `rescued` read zero forever.

    /// A loopback Anthropic-shaped endpoint that answers every notify call with
    /// `notify_importance`. Its OWN server rather than a route bolted onto
    /// [`serve_mock`]: nothing here is about Gmail.
    async fn serve_notify_llm(importance: i64) -> String {
        let body = json!({
            "content": [{"type": "text", "text": json!({
                "notify_importance": importance,
                "one_line": "the notify model wrote this",
            }).to_string()}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 900, "output_tokens": 20},
        });
        async fn answer(State(body): State<Arc<Value>>) -> Json<Value> {
            Json((*body).clone())
        }
        // Mounted at "/" because a `ResolvedLlm.url` is the WHOLE endpoint, not a
        // base the caller suffixes: `serve_held_llm` above answers at
        // `/v1/messages` only because it is reached through
        // `stage2.anthropic_base_url`, which does the suffixing.
        let app = Router::new()
            .route("/", post(answer))
            .with_state(Arc::new(body));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// Ingest one message and run the FAST lane over it, awaited rather than
    /// spawned so an assertion cannot race the decision. `llm` `None` is the
    /// no-model daemon, where a confident seed is the final word.
    async fn ingest_and_fast_lane(
        store: &Arc<SqliteStore>,
        acct: AccountId,
        f: &RawFetched,
        now: DateTime<Utc>,
        origin: IngestOrigin,
        llm: Option<&str>,
    ) -> i64 {
        let cfg = crate::config::NotifyConfig::default();
        let (id, triaged, rules) = ingest_stamped(store, acct, f, now, origin, &cfg);
        if let Some(c) = notify_lane::candidate(&triaged, id, &rules, &cfg, |addr| {
            store.is_known_contact(acct, addr).unwrap_or(false)
        }) {
            let lane = Arc::new(notify_lane::NotifyLane::new(
                store.clone(),
                reqwest::Client::new(),
                cfg,
                Stage1Config::default().known_contact_importance,
                llm.map(|u| ResolvedLlm {
                    api_key: "sk-test".to_string(),
                    provider: Stage2Provider::Anthropic,
                    url: u.to_string(),
                }),
                SyncMetrics::new(),
                acct,
                Arc::new(std::sync::Mutex::new(WarnDays::default())),
            ));
            lane.run(c).await;
        }
        id
    }

    /// The one row `lane` wrote about `message_id`.
    fn lane_row(
        store: &SqliteStore,
        acct: AccountId,
        message_id: i64,
        lane: NotifyLane,
    ) -> Option<crate::store::NotifyDecisionRow> {
        ledger(store, acct)
            .into_iter()
            .find(|r| r.message_id == message_id && r.lane == lane)
    }

    /// A plain personal note: normal, not spam, and NOT confident, so the seed
    /// decides nothing and only a model's score can. The fast lane's decline
    /// therefore has to come from the model, which is what makes it a labeled
    /// example rather than a heuristic shrug.
    fn note_eml(at: DateTime<Utc>) -> String {
        format!(
            "From: Dana <dana@elsewhere.example>\r\n\
             To: me@example.com\r\n\
             Subject: quick question about thursday\r\n\
             Date: {}\r\n\
             \r\n\
             Are you free thursday afternoon? Let me know either way.\r\n",
            at.to_rfc2822()
        )
    }

    #[tokio::test]
    async fn a_decline_the_deliberate_lane_overrules_is_the_rescue_join() {
        // THE ROW THE WHOLE LEDGER EXISTS FOR (docs/NOTIFY.md §4): a labeled
        // false negative of the notify model, on real mail, produced for free.
        // The fast lane scores it 20 and stays quiet; Opus, minutes later, calls
        // it urgent and buzzes. Both facts survive, which is the point — folding
        // them into one mutable verdict would delete the decline at exactly the
        // moment it became interesting, and notify accuracy would read 100%
        // forever.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let url = serve_notify_llm(20).await;
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1");

        let f = fixture(acct, "g-note", &note_eml(now), false);
        let mid =
            ingest_and_fast_lane(&store, acct, &f, now, IngestOrigin::Incremental, Some(&url))
                .await;

        let fast = lane_row(&store, acct, mid, NotifyLane::Fast).expect("the fast lane recorded");
        assert_eq!(fast.decision, NotifyDecision::DeclinedByModel);
        assert_eq!(fast.notify_importance, Some(20));
        assert!(
            store.events_after(acct, 0, 100).unwrap().is_empty(),
            "a decline appends nothing"
        );

        // The deliberate lane, arriving with the opposite verdict.
        let row = store
            .stage1_queue(acct, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == mid)
            .expect("queued");
        assert!(matches!(
            eng.emit_deliberate(
                &stage1_ctx(acct, &row, None, Tier::PastDue, 95),
                now,
                ExpiryCount::Miss,
                STAGE_MODEL,
            ),
            Emitted::New(_)
        ));

        let slow = lane_row(&store, acct, mid, NotifyLane::Deliberate).expect("recorded");
        assert_eq!(slow.decision, NotifyDecision::Sent, "RESCUED");
        assert_eq!(slow.notify_importance, Some(95), "the APPLIED importance");
        assert_eq!(slow.model_used.as_deref(), Some(STAGE_MODEL));
        assert_eq!(
            slow.latency_ms, None,
            "latency is a fast-lane column: this one measures queue depth"
        );
        assert_eq!(store.events_after(acct, 0, 100).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_fast_buzz_the_deliberate_lane_agrees_with_records_would_send() {
        // CONFIRMED, the join that says the fast lane was RIGHT. Nothing happens
        // to the notification — §11.1's fourth call is that a sent buzz is never
        // rewritten — but the agreement is the true-positive half of the corpus,
        // and a lane that recorded only its rescues would read as a lane that is
        // wrong every time it is looked at.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1");

        // No model: the ops alert seeds Signal/75 CONFIDENT, so the fast lane
        // buzzes off the seed, which is `heuristic` and not `heuristic-only`.
        let f = fixture(acct, "g-alert", &alert_eml(now), false);
        let mid =
            ingest_and_fast_lane(&store, acct, &f, now, IngestOrigin::Incremental, None).await;
        let fast = lane_row(&store, acct, mid, NotifyLane::Fast).expect("recorded");
        assert_eq!(fast.decision, NotifyDecision::Sent);
        assert_eq!(store.events_after(acct, 0, 100).unwrap().len(), 1);

        let row = store
            .stage1_queue(acct, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == mid)
            .expect("queued");
        assert_eq!(
            eng.emit_deliberate(
                &stage1_ctx(acct, &row, None, Tier::Signal, 90),
                now,
                ExpiryCount::Miss,
                STAGE_MODEL,
            ),
            Emitted::AlreadyNotified
        );

        let slow = lane_row(&store, acct, mid, NotifyLane::Deliberate).expect("recorded");
        assert_eq!(slow.decision, NotifyDecision::WouldSend, "CONFIRMED");
        assert_eq!(
            store.events_after(acct, 0, 100).unwrap().len(),
            1,
            "and the buzz that landed is untouched"
        );
        // The two model ids are the two lanes' answers and must not read alike:
        // `heuristic` is "no model was configured", `claude-opus-5` is a verdict.
        assert_eq!(fast.model_used.as_deref(), Some("heuristic"));
        assert_eq!(slow.model_used.as_deref(), Some(STAGE_MODEL));
    }

    #[tokio::test]
    async fn a_fast_buzz_the_deliberate_lane_rates_below_the_line_is_overturned() {
        // OVERTURNED, the false-positive half. Braelyn's call is that a wrong
        // buzz is survivable; survivable is not the same as unmeasured, and this
        // row against the rescue row is the pair that decides whether the
        // threshold moves (docs/NOTIFY.md §4).
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1");

        let f = fixture(acct, "g-alert", &alert_eml(now), false);
        let mid =
            ingest_and_fast_lane(&store, acct, &f, now, IngestOrigin::Incremental, None).await;
        assert_eq!(
            lane_row(&store, acct, mid, NotifyLane::Fast)
                .unwrap()
                .decision,
            NotifyDecision::Sent
        );

        let row = store
            .stage1_queue(acct, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == mid)
            .expect("queued");
        // Noise, well under `notify.min_importance`: the deliberate verdict
        // disagrees with the buzz that already went out.
        assert_eq!(
            eng.emit_deliberate(
                &stage1_ctx(acct, &row, None, Tier::Noise, 10),
                now,
                ExpiryCount::Miss,
                STAGE_MODEL,
            ),
            Emitted::NotWorthy
        );

        let slow = lane_row(&store, acct, mid, NotifyLane::Deliberate).expect("recorded");
        assert_eq!(slow.decision, NotifyDecision::DeclinedByModel, "OVERTURNED");
        assert_eq!(slow.notify_importance, Some(10));
        assert_eq!(
            store.events_after(acct, 0, 100).unwrap().len(),
            1,
            "and the buzz is NOT retracted: UNIQUE(message_id) stands"
        );
    }

    #[test]
    fn a_rule_added_after_ingest_records_suppressed_and_never_a_decline() {
        // §10's fourth bullet, in the ledger: `suppressed` is not
        // `declined_by_model`, and only the latter is rescuable. Two things ride
        // on the distinction. A rescue path that asks "was this declined?" would
        // otherwise fire on the one class of row the user explicitly asked never
        // to hear from; and every squelched sender's mail would land in the pile
        // the notify model is graded against, so its false-negative rate would
        // read catastrophic for a reason that has nothing to do with the model.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1");

        let f = fixture(acct, "g-alert", &alert_eml(now), false);
        let (mid, stamp) =
            ingest_deferring_to_refine(&store, acct, &f, now, IngestOrigin::Incremental);
        assert!(stamp.is_some(), "eligible, so the silence is the RULE");

        // THE REACTIVE SQUELCH: the rule arrives while the row is already queued,
        // which is why the site reads the list live.
        store
            .set_sender_rule(
                acct,
                "*@monitoring.example",
                "not urgent",
                Disposition::Squelch,
            )
            .unwrap();
        let row = store
            .stage1_queue(acct, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == mid)
            .expect("queued");
        let rule = eng.current_rule(&row.from_addr);
        assert_eq!(rule, Some(Disposition::Squelch));
        assert_eq!(
            eng.emit_deliberate(
                // PastDue/100, the loudest verdict there is: the rule outranks it.
                &stage1_ctx(acct, &row, rule, Tier::PastDue, 100),
                now,
                ExpiryCount::Miss,
                STAGE_MODEL,
            ),
            Emitted::Suppressed
        );

        let slow = lane_row(&store, acct, mid, NotifyLane::Deliberate).expect("recorded");
        assert_eq!(slow.decision, NotifyDecision::Suppressed);
        assert!(store.events_after(acct, 0, 100).unwrap().is_empty());
        let text = crate::metrics::render(&eng.metrics, None);
        assert!(text.contains(
            "squelchd_notify_decisions_total{lane=\"deliberate\",decision=\"suppressed\"} 1\n"
        ));
        assert!(
            text.contains(
                "squelchd_notify_decisions_total{lane=\"deliberate\",\
                 decision=\"declined_by_model\"} 0\n"
            ),
            "a standing rule is not the model's judgement: {text}"
        );
    }

    #[test]
    fn a_backfilled_row_writes_no_ledger_row_at_the_refine_site() {
        // §11.4: the table holds only messages that carry a `notify_eligible_at`
        // stamp. A first run backfills thirty days, none of it stamped, and every
        // one of those rows walks the Stage-1 apply site behind it — so without
        // this guard the ledger would be 95% mail nobody was ever going to be
        // notified about, and the §11.11 rollout query would be reading noise.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();
        let eng = engine(store.clone(), acct, "http://127.0.0.1:1");

        let f = fixture(acct, "g-backfilled", &alert_eml(now), false);
        let (mid, stamp) =
            ingest_deferring_to_refine(&store, acct, &f, now, IngestOrigin::Backfill);
        assert_eq!(stamp, None, "backfill never stamps");

        let row = store
            .stage1_queue(acct, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == mid)
            .expect("queued");
        assert_eq!(
            eng.emit_deliberate(
                &stage1_ctx(acct, &row, None, Tier::PastDue, 100),
                now,
                ExpiryCount::Miss,
                STAGE_MODEL,
            ),
            Emitted::NotWorthy,
            "an unstamped row can never notify"
        );
        assert!(
            ledger(&store, acct).is_empty(),
            "and it is not in the ledger at all, not even as a decline"
        );
        // Not even as a counter: the metric and the table are one bookkeeping.
        let text = crate::metrics::render(&eng.metrics, None);
        assert!(text.contains(
            "squelchd_notify_decisions_total{lane=\"deliberate\",\
             decision=\"declined_by_model\"} 0\n"
        ));

        // Control: the SAME mail on the incremental path is stamped, and records.
        let g = fixture(acct, "g-fresh", &alert_eml(now), false);
        let (mid2, stamp2) =
            ingest_deferring_to_refine(&store, acct, &g, now, IngestOrigin::Incremental);
        assert!(stamp2.is_some());
        let row = store
            .stage1_queue(acct, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.message_id == mid2)
            .expect("queued");
        eng.emit_deliberate(
            &stage1_ctx(acct, &row, None, Tier::PastDue, 100),
            now,
            ExpiryCount::Miss,
            STAGE_MODEL,
        );
        assert_eq!(ledger(&store, acct).len(), 1, "so the guard is the STAMP");
    }
}
