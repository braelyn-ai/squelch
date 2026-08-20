//! Storage abstraction.
//!
//! rusqlite is synchronous, so `Store` is a SYNC trait and `SqliteStore` wraps
//! its `Connection` in a `Mutex`; async callers wrap calls in
//! `tokio::task::spawn_blocking`.

pub mod search_query;
pub mod sqlite;

pub use search_query::{SearchFilter, parse_search_query};
pub use sqlite::SqliteStore;

use crate::error::Result;
use crate::triage::extract::shipments::ShipmentsApplied;
use crate::triage::{CalendarInfo, CarrierTrack, DeadlineHit, ReceiptInfo, ShipmentInfo};
use crate::types::{
    AccountId, AttachmentInfo, AttentionStatus, AttentionUpdate, AuditEntry, Banking,
    CalendarUpdate, Deadline, Disposition, Event, EventKind, FieldReasons, NewMessage, Receipt,
    SealedKind, SearchHit, SenderRule, Sensitivity, ShredCandidate, StoreStats, ThreadView, Tier,
    TriageAxis, TriageFeedback, UnsubscribeRecord, Update,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Server-side bucket for the sitrep chassis, selected by the `band` param on
/// `/client/updates`. See [`Store::attention_updates`].
///
/// - `Standing` — mail owed the user's attention, immune to the surfacing clock
///   and never rotating out until resolved: a dated obligation (tier
///   `past_due`/`deadline`) OR live correspondence — a thread the user has
///   written in, or a sender the user has written to — with status != 'done'.
///   Participation only widens membership: it never unseals anything, and it
///   never surfaces the user's own sent rows, which no band lists.
/// - `New` — `surfaced_at IS NULL`: never surfaced through ANY door.
/// - `Open` — status = 'open', ordered by `age * importance` descending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SitrepBand {
    Standing,
    New,
    Open,
}

/// One attachment's `(filename, mime, data)`; `data` is `None` when the bytes
/// were not stored (the part was over the ingest cap).
pub type AttachmentBytes = (String, String, Option<Vec<u8>>);

impl SitrepBand {
    pub fn parse(s: &str) -> Option<SitrepBand> {
        match s {
            "standing" => Some(SitrepBand::Standing),
            "new" => Some(SitrepBand::New),
            "open" => Some(SitrepBand::Open),
            _ => None,
        }
    }
}

/// The Gmail sync cursor for one (account, mailbox key), persisted in
/// `sync_state`. The only row is keyed `mailbox = 'history'`: `uidvalidity` is
/// unused (0) and `last_uid` holds the account's monotonic `historyId`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncState {
    pub uidvalidity: u32,
    /// The account's `historyId` cursor.
    pub last_uid: u64,
}

/// Gmail's own unread counts for INBOX, as the sync loop last saw them.
///
/// This is the mailbox's truth, not ours: nothing local tracks reads (the read
/// scope cannot write them), and the ingest window covers only a slice of the
/// inbox, so these numbers can only come from Gmail. `fetched_at` says how stale
/// they are — a fetch failure keeps the previous row rather than clearing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboxUnread {
    pub messages: i64,
    pub threads: i64,
    pub fetched_at: DateTime<Utc>,
}

/// One Sent-derived contact, as both the autocomplete hit shape and the
/// Sent-history harvest's merge input (same fields either way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactEntry {
    pub addr: String,
    /// Mailbox display name from Sent To/Cc headers, when one was ever seen.
    pub display_name: Option<String>,
    pub sent_count: i64,
    pub last_sent_at: Option<DateTime<Utc>>,
}

/// A fully-triaged message committed in one transaction by
/// [`Store::ingest_message`]: message row and triage row together, so a sealed
/// message is never observable as normal mail (docs/SECURITY.md §4).
#[derive(Debug, Clone)]
pub struct TriagedMessage {
    pub message: NewMessage,
    /// Sent mail only: To/Cc addresses seeding the contacts table (the account's
    /// own address is filtered out at ingest). Contacts come exclusively from
    /// recipients of sent mail, never from inbound senders.
    pub recipients: Vec<String>,
    pub sensitivity: Sensitivity,
    pub sealed_kind: Option<SealedKind>,
    pub importance: u8,
    pub tier: Tier,
    pub one_line: String,
    pub reason: String,
    /// Per-property Stage-1 justifications (importance / deadline / tier), stored
    /// as the triage row's `field_reasons` JSON and served HUMAN-DOOR ONLY. Empty
    /// for sealed / sent mail.
    pub field_reasons: FieldReasons,
    pub matched_rule: Option<i64>,
    /// The Stage-1 deadline hit. Only ever `Some` for non-sealed mail.
    pub deadline: Option<DeadlineHit>,
    /// A detected shipment/package. Runs independently of the triage tier, and
    /// only ever `Some` for non-sealed mail.
    pub shipment: Option<ShipmentInfo>,
    /// The LOOSE shipping signal
    /// ([`has_loose_shipping_signal`](crate::triage::shipment::has_loose_shipping_signal)):
    /// `true` queues the row for the shipments EXTRACTOR by stamping
    /// `triage.ship_extract_model='pending'`. Much wider than `shipment`, which
    /// needs a tracking number the regex could attribute — an order confirmation
    /// with neither sets this and leaves `shipment` `None`. Always `false` for
    /// sealed and sent mail, exactly like `shipment`.
    pub ship_extract: bool,
    /// A detected receipt (money already paid). Independent of the tier AND of
    /// shipment detection — one mail can be both. Only ever `Some` for non-sealed
    /// mail. Ingest AUTO-RESOLVES the triage row (`status='done'`) so a receipt
    /// lives only in the Receipts category instead of the attention bands.
    pub receipt: Option<ReceiptInfo>,
    /// A detected calendar update (invite / update / cancellation / RSVP). Same
    /// shape as receipts: tier-independent, non-sealed only, and ingest
    /// AUTO-RESOLVES the triage row so it lives only in the Calendar category.
    pub calendar: Option<CalendarInfo>,
    /// Attachments extracted from the RFC822 (real parts AND cid-inline), each
    /// already capped (over-cap parts carry `data: None`), written in the SAME
    /// ingest transaction. Present for sealed mail too — the byte-serving
    /// endpoint is what guards sealed parents.
    pub attachments: Vec<AttachmentInfo>,
    /// `false` when Stage-1 was not confident: the row keeps `model_used IS NULL`
    /// so the Stage-2 queue predicate picks it up.
    pub confident: bool,
}

/// The full body of one sealed message. HUMAN-DOOR-ONLY: returned solely by
/// [`Store::sealed_body`] from the squelch-api reveal endpoint, which audits
/// before this value leaves the process (docs/SECURITY.md §4).
#[derive(Debug, Clone)]
pub struct SealedBody {
    pub id: i64,
    pub account_id: AccountId,
    pub thread_id: String,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub subject: String,
    pub received_at: DateTime<Utc>,
    pub sealed_kind: Option<String>,
    pub body: String,
    /// The sanitized HTML body stored at ingest, when the mail had one; served
    /// through this one audited reveal door.
    pub body_html: Option<String>,
}

/// The Gmail ids + header fields an action endpoint needs to act on a message,
/// and no body. HUMAN-DOOR-ONLY: produced solely by
/// [`SqliteStore::action_message_ref`](sqlite::SqliteStore::action_message_ref),
/// which excludes sealed rows in SQL so an action can never target sealed mail.
#[derive(Debug, Clone)]
pub struct ActionMessageRef {
    /// Local message id.
    pub id: i64,
    pub account_id: AccountId,
    /// The Gmail-side message id (`users.messages.{id}`), used for modify/get.
    pub gmail_msg_id: String,
    /// The Gmail-side thread id, used as `threadId` when sending a reply.
    pub thread_id: String,
    /// Original sender — the default reply recipient.
    pub from_addr: String,
    pub from_name: Option<String>,
    pub subject: String,
}

/// The full triage state of one message, for the developer-mode inspector.
/// SERIALIZED SHAPE IS A WIRE CONTRACT — the client decodes this. Carries
/// verdicts/markers/reasons but never body content; human door only, and sealed
/// rows are never returned.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TriageDebug {
    pub message_id: i64,
    pub subject: String,
    pub importance: i64,
    pub tier: String,
    pub category: Option<String>,
    pub one_line: String,
    pub reason: String,
    pub field_reasons: Option<crate::types::FieldReasons>,
    pub deadline: Option<String>,
    pub matched_rule_id: Option<i64>,
    pub status: String,
    pub surfaced_at: Option<String>,
    pub resolved_at: Option<String>,
    pub stage1_model_used: Option<String>,
    pub model_used: Option<String>,
    pub needs_stage2: bool,
    pub extractor_model_used: Option<String>,
    pub created_at: String,
    /// The Gmail-side thread id, joined from `messages`. APPENDED LAST on
    /// purpose: an older client decoding this shape must keep working.
    pub thread_id: String,
    /// When a human last asked for this row to be re-triaged, `None` when
    /// nobody ever has. APPENDED LAST for the reason `thread_id` was.
    pub retriage_at: Option<String>,
}

/// The stored unsubscribe intent for one NON-SEALED message, resolved by
/// [`Store::message_unsub_fields`] for `POST /client/unsubscribe`. Sealed rows
/// are excluded in SQL, so missing and sealed are indistinguishable (`None`).
#[derive(Debug, Clone)]
pub struct MessageUnsub {
    /// The sender address as stored (the caller lowercases it for the wire).
    pub from_addr: String,
    /// Raw `List-Unsubscribe` header value, or `None`.
    pub list_unsubscribe: Option<String>,
    /// RFC 8058 one-click advertised.
    pub list_unsub_one_click: bool,
    /// The SANITIZED body, for the footer-link fallback when no header exists.
    /// Sanitized rather than raw on purpose: it is the markup the reader was
    /// actually shown, and scripts and handlers are already gone from it.
    pub body_html: Option<String>,
}

/// A row to append to the human-door audit log.
#[derive(Debug, Clone)]
pub struct NewAuditEntry {
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<String>,
}

/// A notification-worthy event appended to the `events` log. Produced ONLY by
/// [`crate::triage::events`]; every field besides the ids is a denormalized
/// snapshot of the verdict at emission time. Sealed mail never produces one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    pub account_id: AccountId,
    pub message_id: i64,
    pub thread_id: String,
    pub kind: EventKind,
    pub tier: Tier,
    pub importance: u8,
    pub sender: String,
    pub one_line: String,
    /// RFC3339 deadline snapshot, or `None`.
    pub deadline: Option<String>,
}

/// One registered APNs device.
///
/// PRIVACY: `token` is capability material — written by the human door, read by
/// the pusher, NEVER logged and never exposed on the agent door.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: i64,
    pub account_id: AccountId,
    pub token: String,
    /// Free-form platform tag; `ios` today.
    pub platform: String,
    pub created_at: DateTime<Utc>,
    /// Refreshed on every re-registration — iOS re-hands its token each launch,
    /// so this, not `created_at`, is the liveness signal.
    pub last_registered_at: DateTime<Utc>,
}

/// One issued human-door device token, described WITHOUT its secret.
///
/// This is the shape both `token list` and the auth middleware get back: there
/// is deliberately no field that could carry the plaintext or the stored hash,
/// so no caller can print one by accident. See
/// [`SqliteStore::verify_device_token`](sqlite::SqliteStore::verify_device_token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceToken {
    pub id: i64,
    pub account_id: AccountId,
    /// Operator/device-supplied label; never secret.
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// Last time this token authenticated a request, to within a minute — the
    /// verify path throttles the write. `None` until first use.
    pub last_used_at: Option<DateTime<Utc>>,
    /// Set once, forever. A tombstoned token can never authenticate again.
    pub revoked_at: Option<DateTime<Utc>>,
}

/// A device token at the ONE moment its plaintext exists.
///
/// Returned by [`SqliteStore::issue_device_token`](sqlite::SqliteStore::issue_device_token)
/// and by a successful pairing claim, and by then the store holds only the hash
/// — so if the caller drops `token` without showing it, that credential is gone
/// for good. No `Serialize`: the wire shapes are the API's own structs, which
/// keeps a stray `.json()` on this type from being the leak.
#[derive(Clone)]
pub struct IssuedDeviceToken {
    pub id: i64,
    pub account_id: AccountId,
    pub name: String,
    /// PLAINTEXT `sqd_…`. Never stored, never logged, shown exactly once.
    pub token: String,
}

impl std::fmt::Debug for IssuedDeviceToken {
    /// The plaintext is REDACTED. `{:?}` on a struct is how secrets reach logs
    /// by accident, and this type exists precisely to carry one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedDeviceToken")
            .field("id", &self.id)
            .field("account_id", &self.account_id)
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A freshly minted pairing code, plaintext included, for the operator to read
/// aloud or scan. Superseded by the next mint and by its own TTL.
#[derive(Clone)]
pub struct MintedPairingCode {
    pub id: i64,
    /// Display form, `XXXX-XXXX`. The claim normalizes, so the dashes are
    /// cosmetic and callers may hand this string straight to a QR or deep link.
    pub code: String,
    pub expires_at: DateTime<Utc>,
}

impl std::fmt::Debug for MintedPairingCode {
    /// Redacted for the same reason as [`IssuedDeviceToken`]: a code is a
    /// short-lived credential, and short-lived is not the same as harmless.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedPairingCode")
            .field("id", &self.id)
            .field("code", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// One observed open of a tracked outbound message.
///
/// SERIALIZED SHAPE IS A WIRE CONTRACT — this is the element type of
/// `GET /client/messages/{id}/opens`. `opened_at` is unix seconds.
///
/// `classification` describes the FETCHER, never a human: `proxied` means Gmail's
/// image proxy asked for the pixel (which may be a cache warm rather than a
/// read), `unknown` is everything else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageOpen {
    pub opened_at: i64,
    pub user_agent: Option<String>,
    pub classification: String,
}

/// Open classification for a fetch Gmail's image proxy made.
pub const OPEN_PROXIED: &str = "proxied";
/// Open classification for every other fetcher.
pub const OPEN_UNKNOWN: &str = "unknown";

/// The message a tracking token points at, resolved in one hop from the token.
/// Only ever `Some` when the tracker exists, belongs to the account, and its
/// echo backfilled a local message id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedMessage {
    /// Local `messages.id` of the echoed copy of the sent mail.
    pub message_id: i64,
    pub thread_id: String,
    pub subject: String,
    /// The sent row's `From:` — the account's own address.
    pub from_addr: String,
}

/// One local draft: an unsent composition addressed either at a message being
/// replied to, or at the account's single new-message slot
/// (`reply_to_message_id: None`).
///
/// HUMAN-DOOR ONLY — produced by
/// [`SqliteStore::upsert_draft`](sqlite::SqliteStore::upsert_draft) and friends
/// for `/client/drafts`. Nothing here is synced to Gmail Drafts and no agent-door
/// read reaches the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub id: i64,
    pub account_id: AccountId,
    /// The message this replies to; `None` is the new-message draft.
    pub reply_to_message_id: Option<i64>,
    pub to_addr: String,
    pub subject: String,
    pub body: String,
    /// First save of this draft; an edit keeps it.
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One non-confident triage row queued for the Stage-2 LLM pass, with message
/// context and the matched Filtered-rule's `want_text`. Produced by
/// [`Store::stage2_queue`], whose predicate (`model_used IS NULL AND
/// sensitivity='normal'`) excludes sealed rows in SQL, so this never represents
/// sealed mail.
#[derive(Debug, Clone)]
pub struct Stage2Queued {
    /// Local message id (triage.message_id).
    pub message_id: i64,
    pub account_id: AccountId,
    /// Gmail thread id — the per-thread budget key.
    pub thread_id: String,
    pub from_addr: String,
    pub subject: String,
    pub body: String,
    /// Drives the pass loop's SKIP-STALE check: rows older than
    /// `stage2_max_age_days` are marked `model_used='stale-skip'` without
    /// spending a model call.
    pub received_at: DateTime<Utc>,
    /// `true` if the sender is a Sent-derived contact. Feeds the TRUSTED CONTEXT
    /// block and gates unknown-sender deadline capping.
    pub is_known_contact: bool,
    /// The matched Filtered rule's `want_text`, presented in the TRUSTED CONTEXT
    /// block as the account owner's standing instruction for this sender.
    pub rule_want_text: Option<String>,
    /// WHY the router escalated this row
    /// ([`crate::triage::router::EscalationReason::as_str`]). Shown to the
    /// model, because "look harder" without saying at what is a worse question
    /// than the one Stage-1 already answered.
    pub escalation_reason: Option<String>,
    /// What this sender's mail has historically been worth to the account owner.
    pub sender_history: SenderHistory,
    /// Other messages in the same thread, oldest first. SEALED SIBLINGS ARE
    /// EXCLUDED IN SQL: a sealed message's subject must never reach a model,
    /// including as another row's context.
    pub thread: Vec<ThreadSibling>,
    /// Always `'normal'` for queued rows; carried so the sealed guard can assert.
    pub sensitivity: Sensitivity,
    /// When a human last asked for this row to be re-triaged; see
    /// [`Stage1Queued::retriage_at`]. Overrides the SKIP-STALE check above.
    pub retriage_at: Option<DateTime<Utc>>,
}

/// How the account owner has treated this sender before. Aggregates, not
/// content: a count of prior mail, how much of it surfaced, and how often they
/// overrode the verdict — the cheapest honest answer to "is this sender someone
/// we get right?"
#[derive(Debug, Clone, Default)]
pub struct SenderHistory {
    /// Prior non-sealed, inbound messages from this address.
    pub total: i64,
    /// How many landed at signal or in the standing band.
    pub surfaced: i64,
    /// How many the account owner corrected by hand.
    pub corrected: i64,
}

/// One other message in the escalated row's thread. Verdict and envelope only,
/// never a body: the point is whether the owner is IN this conversation, which
/// the metadata answers without a second body's worth of tokens or a second
/// body's worth of injection surface.
#[derive(Debug, Clone)]
pub struct ThreadSibling {
    pub from_addr: String,
    pub subject: String,
    /// UNTRUSTED: model-authored from email content, neutralized before it
    /// renders in a prompt.
    pub one_line: String,
    pub tier: Tier,
    pub received_at: DateTime<Utc>,
    /// `true` when this sibling is the account owner's own message — the single
    /// most useful bit in the thread, since a conversation the owner has replied
    /// in is one they have already voted for.
    pub is_sent: bool,
}

/// The store-facing outcome of applying a parsed Stage-2 result onto a triage
/// row (pure mapping lives in `triage::stage2::apply_result`). When `deadline`
/// is `Some`, a `deadlines` row is (re)written.
#[derive(Debug, Clone)]
pub struct Stage2Applied {
    pub message_id: i64,
    pub account_id: AccountId,
    pub importance: u8,
    pub tier: Tier,
    pub one_line: String,
    pub reason: String,
    /// Per-property Stage-2 justifications describing the values THIS apply
    /// stores, fully replacing any Stage-1 reasons. Served HUMAN-DOOR ONLY.
    pub field_reasons: FieldReasons,
    /// Stamped onto `model_used`, marking the row processed so the queue
    /// predicate no longer selects it.
    pub model_used: String,
    /// A deadline to (re)write for this message, if the model extracted one.
    pub deadline: Option<DeadlineHit>,
    /// Stamped onto `triage.category`; `None` leaves the column untouched.
    pub category: Option<String>,
}

/// One row queued for the Stage-1 LLM refine pass: an ingested, NON-SEALED,
/// non-rule-decided message still carrying its heuristic seed values
/// (`stage1_model_used IS NULL AND sensitivity='normal'`). The predicate
/// excludes sealed rows in SQL, so this never represents sealed mail.
#[derive(Debug, Clone)]
pub struct Stage1Queued {
    pub message_id: i64,
    pub account_id: AccountId,
    /// Carried for context/logging; Stage-1 budgets only GLOBALLY, never
    /// per-thread.
    pub thread_id: String,
    pub from_addr: String,
    pub subject: String,
    pub body: String,
    /// Drives the deadline sanity bounds and the stale-skip check.
    pub received_at: DateTime<Utc>,
    /// `true` if the sender is a Sent-derived contact. Feeds the TRUSTED CONTEXT
    /// block and gates the unknown-sender deadline cap.
    pub is_known_contact: bool,
    /// `true` if the account owner has ever corrected a triage verdict for this
    /// sender address. Feeds [`crate::triage::router::EscalationReason::SenderCorrected`]:
    /// a human override is the strongest evidence in the system that a sender is
    /// one we get wrong, so their next message earns the harder look.
    pub sender_corrected: bool,
    /// Always `'normal'` for queued rows; carried so the sealed guard can assert.
    pub sensitivity: Sensitivity,
    /// `triage.retriage_at`: when a human last asked for THIS row to be
    /// re-triaged, `None` when nobody ever has. Read through
    /// [`crate::triage::retriage_forced`], which is what lets an explicit
    /// re-triage of old mail bypass the pass's stale skip.
    pub retriage_at: Option<DateTime<Utc>>,
}

/// A triage row's HEURISTIC SEED verdict, read back when the Stage-1 model call
/// did not produce one. Just enough to decide a notification: the seed is what
/// the user will actually see on that row, so it is what the notification has to
/// describe.
#[derive(Debug, Clone)]
pub struct SeedVerdict {
    pub tier: Tier,
    pub importance: u8,
    pub one_line: String,
    /// The ingest-time escalation seed, which is the STORED form of the
    /// heuristic's own confidence (`needs_stage2 = !confident`). A row still
    /// bound for Stage-2 has another verdict coming and must not notify from
    /// this one.
    pub needs_stage2: bool,
    pub deadline: Option<DateTime<Utc>>,
}

/// One message whose scheduled re-evaluation has come due. Carries the PRIOR
/// verdict, because the re-classification's whole job is to answer "does this
/// still hold?" and it cannot do that without knowing what it is revising.
///
/// The queue predicate excludes sealed rows, sent mail, rows past their revisit
/// budget, and — critically — rows the account owner has corrected by hand.
#[derive(Debug, Clone)]
pub struct RevisitQueued {
    /// The `triage_revisits` row id, stamped fired when this is consumed.
    pub revisit_id: i64,
    pub message_id: i64,
    pub account_id: AccountId,
    pub thread_id: String,
    pub from_addr: String,
    pub subject: String,
    pub body: String,
    pub received_at: DateTime<Utc>,
    /// When this revisit was scheduled for (not when it fired).
    pub revisit_at: DateTime<Utc>,
    /// Why it was scheduled. UNTRUSTED for `source == "model"`.
    pub reason: String,
    /// `model` | `deadline` | `fye_stale`.
    pub source: String,
    pub prior_tier: Tier,
    pub prior_importance: u8,
    /// The prior one-liner. Model-authored from untrusted email; neutralized
    /// before it renders in a prompt.
    pub prior_one_line: String,
    pub is_known_contact: bool,
    pub sender_corrected: bool,
    /// Always `'normal'` for queued rows; carried so the sealed guard can assert.
    pub sensitivity: Sensitivity,
}

/// The store-facing outcome of applying a parsed Stage-1 LLM result onto a
/// triage row: stamps `stage1_model_used` (leaving the Stage-1 queue) and sets
/// `needs_stage2` (whether the row escalates).
#[derive(Debug, Clone)]
pub struct Stage1Applied {
    pub message_id: i64,
    pub account_id: AccountId,
    pub importance: u8,
    pub tier: Tier,
    pub one_line: String,
    pub reason: String,
    /// Per-property justifications describing the STORED values, replacing the
    /// heuristic seed reasons. Human-door only.
    pub field_reasons: FieldReasons,
    /// The Stage-1 model id to stamp `stage1_model_used` with.
    pub stage1_model_used: String,
    /// `true` when [`crate::triage::router::should_escalate`] found a reason:
    /// sets `needs_stage2=1` so the Stage-2 queue predicate picks the row up.
    pub needs_stage2: bool,
    /// The routing reason's slug, stored so the escalation MIX is inspectable
    /// after the fact. Tuning the router without knowing which arm is firing is
    /// guesswork, and the arms are the whole design.
    pub escalation_reason: Option<&'static str>,
    /// A deadline to (re)write for this message, if the model extracted one.
    pub deadline: Option<DeadlineHit>,
    /// Routing category (`general` | `invoice` | `banking_statement` |
    /// `transaction_alert`); `None` leaves `triage.category` untouched.
    pub category: Option<String>,
}

/// One row queued for a specialist EXTRACTOR pass: a NON-SEALED triage row whose
/// LLM-assigned `category` has a registered extractor and that has not yet been
/// extracted (`extractor_model_used IS NULL`). The query gates on
/// `sensitivity='normal'` and on a real LLM category (sealed rows carry
/// `category=NULL`), so this never represents sealed mail.
#[derive(Debug, Clone)]
pub struct ExtractQueued {
    pub message_id: i64,
    pub account_id: AccountId,
    /// Carried for context/logging; the extract pass shares the Stage-1 GLOBAL
    /// budget scope.
    pub thread_id: String,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub subject: String,
    pub body: String,
    /// The routing category that selected this row; decides which specialist
    /// extractor runs.
    pub category: String,
    /// Drives the stored `banking.received_at` and the stale-skip check.
    pub received_at: DateTime<Utc>,
    /// Always `'normal'` for queued rows; carried so the sealed guard can assert.
    pub sensitivity: Sensitivity,
    /// When a human last asked for this row to be re-triaged; see
    /// [`Stage1Queued::retriage_at`]. Overrides the stale skip in
    /// [`crate::triage::extract::route_extract_row`] and in the shipments loop.
    pub retriage_at: Option<DateTime<Utc>>,
}

/// The store-facing outcome of running the marketing extractor on a row: a
/// `marketing` upsert plus the extractor marker.
#[derive(Debug, Clone)]
pub struct MarketingApplied {
    pub message_id: i64,
    pub account_id: AccountId,
    pub brand: Option<String>,
    pub offer: Option<String>,
    pub discount: Option<String>,
    /// Shape-validated promo code, or `None`. Never a sentence or a URL.
    pub code: Option<String>,
    /// `YYYY-MM-DD`, or `None` when absent/implausible.
    pub expires_at: Option<String>,
    pub received_at: DateTime<Utc>,
    /// Stamped onto `triage.extractor_model_used` so the queue stops selecting
    /// the row. Unlike banking, this write does NOT resolve the triage row.
    pub extractor_model_used: String,
}

/// One extracted promotion, for the client's marketing surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketingOffer {
    pub message_id: i64,
    pub thread_id: String,
    pub sender: String,
    pub subject: String,
    pub brand: Option<String>,
    pub offer: Option<String>,
    pub discount: Option<String>,
    pub code: Option<String>,
    pub expires_at: Option<String>,
    pub received_at: DateTime<Utc>,
}

/// The store-facing outcome of the banking specialist extractor: a `banking`
/// row upsert plus the extractor marker and (for records) an auto-resolve.
pub struct BankingApplied {
    pub message_id: i64,
    pub account_id: AccountId,
    /// `statement` | `transaction_alert` — derived from the row's category, not
    /// the model.
    pub kind: String,
    pub institution: Option<String>,
    pub amount: Option<f64>,
    pub currency: Option<String>,
    /// Masked last-4 tail (`…1234`) or `None` — never a full account number
    /// (post-validated in the extractor's apply).
    pub account_hint: Option<String>,
    pub received_at: DateTime<Utc>,
    /// Stamped onto `triage.extractor_model_used` so the queue stops selecting
    /// the row.
    pub extractor_model_used: String,
    /// `true` => also resolve the triage row to `status='done'`: banking
    /// statements/alerts are RECORDS and must leave the attention bands.
    pub auto_resolve: bool,
}

/// A day's Stage-2 API usage for one account. Cost is NOT stored — the human
/// door computes it from config-driven per-MTok prices at read time.
/// `input_tokens` is the UNCACHED prompt remainder; prompt-cache writes and
/// reads are separate columns because they price differently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stage2Usage {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

/// One day's Stage-2 usage row carrying its `day` key, for the human-door usage
/// history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stage2UsageDay {
    /// UTC date key, `YYYY-MM-DD`.
    pub day: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

/// One call's token counts, as the ledger records them: `input` is the UNCACHED
/// prompt remainder, with prompt-cache writes and reads in their own fields
/// because they price differently. A struct rather than four positional u64s so
/// a transposed input/output can't compile — which is also why there is no
/// positional constructor: build it as a field-named literal (production goes
/// through `From<Usage>`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTokens {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

/// Runtime daily-cap overrides from `app_settings`. `None` means no override
/// row, so the caller falls back to config/env then the built-in default; only
/// values parsing as an integer in `1..=100000` are surfaced, a malformed or
/// out-of-range value counts as absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stage2CapOverrides {
    pub thread_daily_cap: Option<u32>,
    pub sender_daily_cap: Option<u32>,
    pub global_daily_cap: Option<u32>,
    /// The Stage-1 GLOBAL daily-cap override (Stage-1 has only a global cap).
    pub stage1_global_daily_cap: Option<u32>,
}

/// A NON-SEALED message still needing an embedding vector, for the startup
/// backfill pass. Carries only the text the embedder consumes.
#[derive(Debug, Clone)]
pub struct MissingVector {
    pub message_id: i64,
    pub subject: String,
    pub body: String,
}

/// A locally-stored sealed message, exposed ONLY to the TUI. This type never
/// crosses the MCP boundary.
#[derive(Debug, Clone)]
pub struct SealedMessage {
    pub id: i64,
    pub account_id: AccountId,
    pub thread_id: String,
    pub from_addr: String,
    pub subject: String,
    pub received_at: DateTime<Utc>,
    pub sealed_kind: Option<String>,
}

/// One row of the user's OWN sent mail, for the human door's sent listing
/// ([`Store::sent_listing`]). HUMAN-DOOR ONLY: the agent door has no sent
/// surface, and every other listing filters `is_sent = 0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    pub id: i64,
    pub thread_id: String,
    /// Display recipients, comma-joined `Name <addr>`; `""` when the row predates
    /// the recipients backfill or its headers named nobody.
    pub to: String,
    pub subject: String,
    pub snippet: String,
    /// `messages.received_at` VERBATIM — the stored RFC3339 string, not a
    /// re-formatted parse of it.
    pub sent_at: String,
    /// Recorded read receipts for this message (`message_opens` through
    /// `send_trackers`). 0 for an untracked send, which is the default.
    pub opens: i64,
}

/// A sent message still missing its display recipients, for the one-shot
/// recipients backfill. Carries only what the Gmail metadata fetch needs.
#[derive(Debug, Clone)]
pub struct SentMissingRecipients {
    pub message_id: i64,
    pub gmail_msg_id: String,
}

/// The squelch local store. Implemented by [`SqliteStore`].
///
/// SECURITY: every method that can feed the MCP surface (`ranked_updates`,
/// `thread_view`, `deadlines`) MUST exclude `sensitivity = 'sealed'` in the SQL
/// itself. `sealed_messages` is the sole local-only escape hatch (TUI).
pub trait Store: Send + Sync {
    /// Insert or update a message (and its FTS body + derived contacts).
    /// Returns the local message id.
    fn upsert_message(&self, msg: &NewMessage) -> Result<i64>;

    /// Ranked, MCP-facing updates. Sealed rows are excluded in SQL.
    fn ranked_updates(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        min_importance: Option<u8>,
    ) -> Result<Vec<Update>>;

    /// MCP-facing thread view. Returns `NotFound` for a sealed thread so it is
    /// indistinguishable from a nonexistent one.
    ///
    /// SECURITY: the returned [`ThreadView`] carries text ONLY — never HTML.
    /// Two methods returning two types is the structural guarantee that html
    /// never crosses /mcp; the html-bearing variant is
    /// [`Store::thread_view_with_html`].
    fn thread_view(&self, account_id: AccountId, thread_id: &str) -> Result<ThreadView>;

    /// Resolve a LOCAL MESSAGE id to its `thread_id`, for the `get_thread`
    /// forgiveness path. Unknown and sealed both return `NotFound` so sealed rows
    /// never leak thread existence. The resolved thread may still contain sealed
    /// messages — the caller re-runs the full guard via `thread_view`.
    fn thread_id_for_message(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<String>>;

    /// HUMAN-DOOR-ONLY thread view: same sealed/nonexistent -> `NotFound`
    /// behavior as [`Store::thread_view`], but each message also carries its
    /// sanitized `html` (`None` for plain-text-only mail). MUST NOT be called
    /// from MCP, sync, or triage.
    ///
    /// Its `is_sent` is AUTHORSHIP, not the stored column: the stored flag OR a
    /// case-insensitive `from_addr` == account-email match, because the column is
    /// a visibility flag that stays 0 on self-addressed mail the user did write.
    /// See [`crate::types::ClientMessage::is_sent`].
    fn thread_view_with_html(
        &self,
        account_id: AccountId,
        thread_id: &str,
    ) -> Result<crate::types::ClientThreadView>;

    /// HUMAN-DOOR-ONLY attachment byte fetch. `data` is `None` when the bytes
    /// were not stored (over the ingest cap — the endpoint answers 410).
    ///
    /// SECURITY: the query requires the PARENT message's `sensitivity='normal'`,
    /// so an attachment on sealed mail returns `Ok(None)`, indistinguishable
    /// from an unknown id.
    fn attachment_bytes(
        &self,
        account_id: AccountId,
        attachment_id: i64,
    ) -> Result<Option<AttachmentBytes>>;

    /// MCP-facing deadlines within `within_days` (None = all). Sealed excluded.
    fn deadlines(&self, account_id: AccountId, within_days: Option<u32>) -> Result<Vec<Deadline>>;

    /// Upsert a shipment keyed by `(account_id, tracking_number)`. A later mail
    /// about the same tracking number UPDATES the row through the no-regress
    /// status state machine (a delivered shipment is never walked back),
    /// refreshing `last_update`/`last_message_id` and adopting a longer
    /// `item_name`.
    ///
    /// An update the state machine ACCEPTS also resets `poll_failures` to 0 —
    /// mail is the second witness that un-retires a shipment the carrier has
    /// been rejecting, since the only other one (a successful poll) is
    /// unreachable once the row has left the poll queue.
    ///
    /// SECURITY: callers run this ONLY for non-sealed mail, so `shipments` holds
    /// no sealed rows by construction and reads need no sealed join.
    fn upsert_shipment(
        &self,
        account_id: AccountId,
        message_id: i64,
        shipment: &ShipmentInfo,
        seen_at: DateTime<Utc>,
    ) -> Result<i64>;

    /// List shipments for the account, most-recently-updated first;
    /// `include_delivered=false` restricts to en-route (status != 'delivered').
    /// Sealed rows are structurally absent, so no sealed filter is required.
    ///
    /// `policy` carries the three READ-SIDE hides, all of which leave the row
    /// live in the table and all of which reverse themselves:
    ///
    /// * [`suppress_failed_ambiguous_at`] hides rows whose tracking number is an
    ///   AMBIGUOUS SHAPE (anything that does not identify its own carrier — see
    ///   [`is_ambiguous_tracking_shape`](crate::triage::is_ambiguous_tracking_shape))
    ///   AND which the carrier has permanently rejected that many times: a number
    ///   no carrier will acknowledge, in a shape a retailer item/order id shares,
    ///   is a phantom. One successful poll zeroes the counter and brings it back.
    ///   Callers pass the carrier poller's retirement cap; 0 would hide every
    ///   ambiguous row.
    /// * [`stale_after_days`] hides rows whose `last_update` is older than that.
    ///   `last_update` moves ONLY on a user-visible change, so "stale" is
    ///   literally "nothing has happened to this package in N days". 0 disables.
    /// * `cleared_at` hides a row the user cleared, but ONLY while
    ///   `last_update <= cleared_at`. There is no un-clear: the comparison IS the
    ///   revival, so the first update to land after the clear brings the row back
    ///   with no second write anywhere.
    ///
    /// NONE of this touches [`Store::list_pollable_shipments`], deliberately: a
    /// hidden row keeps being polled, because a poll is the most likely source of
    /// the update that un-hides it.
    ///
    /// [`suppress_failed_ambiguous_at`]: crate::config::ShipmentListPolicy::suppress_failed_ambiguous_at
    /// [`stale_after_days`]: crate::config::ShipmentListPolicy::stale_after_days
    fn list_shipments(
        &self,
        account_id: AccountId,
        include_delivered: bool,
        policy: crate::config::ShipmentListPolicy,
    ) -> Result<Vec<crate::types::Shipment>>;

    /// USER CLEAR: stamp `shipments.cleared_at = at`, taking the row out of
    /// [`Store::list_shipments`] until something advances its `last_update` past
    /// that stamp. Returns `false` for an unknown id (or another account's).
    ///
    /// IDEMPOTENT, and re-clearing RESTAMPS: a row that was cleared, revived by
    /// an update, and cleared again must hide against the LATER stamp, so the
    /// write is unconditional rather than "only if NULL".
    ///
    /// There is no `unclear_shipment`, by design. Un-hiding is the comparison in
    /// the listing, and the events that should un-hide a package (a poll that
    /// moved it, an email that advanced it) already write `last_update`.
    fn clear_shipment(
        &self,
        account_id: AccountId,
        shipment_id: i64,
        at: DateTime<Utc>,
    ) -> Result<bool>;

    /// ONE-SHOT REPAIR: re-run shipment detection over every shipment row's
    /// feeder message and delete the rows the current detector no longer yields
    /// that number from, returning how many were deleted.
    ///
    /// EXACTLY ONCE PER ACCOUNT, and the store enforces it: the pass records its
    /// own done-flag in the SAME TRANSACTION as its deletions, and every later
    /// call returns 0 without judging anything. Callers do not gate it. The
    /// atomicity matters because the keep test is the REGEX detector — rows the
    /// shipments extractor wrote are precisely the ones it cannot reproduce, so a
    /// pass that ran but went unrecorded would reap them on the next start.
    ///
    /// Rows with no feeder message are left alone, as are rows carrying carrier
    /// evidence (a raw status or any poll attempt) or extractor evidence (an
    /// order reference): the one-shot exists to reap pre-tightening regex
    /// phantoms and nothing else.
    fn shipments_redetect_cleanup(&self, account_id: AccountId) -> Result<u64>;

    /// Shipments worth a carrier-API poll: not yet delivered, on a carrier that
    /// HAS an API ("ups" | "usps" | "fedex" | "dhl" — Amazon and "unknown" have
    /// none), first seen at or after `min_first_seen`, and under `max_failures`
    /// permanent poll failures. Never-polled rows come first, then the
    /// least-recently-polled, so a caller taking a prefix spends its budget
    /// evenly. Sealed rows are structurally absent, as for every shipment read.
    ///
    /// DELIBERATELY BLIND TO THE LISTING FILTERS. A row the user cleared, and a
    /// row hidden as stale, are BOTH still polled — a poll is exactly what
    /// produces the `last_update` that brings them back, so filtering them here
    /// would make hiding permanent and silently break revival. Only
    /// [`Store::list_shipments`] filters. Do not "optimize" this.
    fn list_pollable_shipments(
        &self,
        account_id: AccountId,
        min_first_seen: DateTime<Utc>,
        max_failures: u32,
    ) -> Result<Vec<crate::types::Shipment>>;

    /// Apply one carrier-API result to a shipment, returning whether `status`
    /// changed (`false` for an unknown id). The carrier is ground truth, so the
    /// status is REPLACED through
    /// [`ShipmentStatus::reconcile_carrier`](crate::triage::ShipmentStatus::reconcile_carrier)
    /// rather than ratcheted — except when `track.status` is `None`, which
    /// leaves it untouched. `carrier_status_raw`/`eta` always take the carrier's
    /// values, `last_polled_at` always advances, and `poll_failures` resets;
    /// `delivered_at` fills once and is never overwritten. `last_update` moves
    /// ONLY when something user-visible changed, so polling does not churn the
    /// Sitrep sort order.
    ///
    /// `last_message_id` is NEVER touched — no message backs a poll, so the
    /// row's click target stays the last accepted email.
    fn apply_carrier_track(
        &self,
        account_id: AccountId,
        shipment_id: i64,
        track: &CarrierTrack,
        polled_at: DateTime<Utc>,
    ) -> Result<bool>;

    /// Record a poll attempt that produced no track. `last_polled_at` always
    /// advances (so the shipment rotates through the queue); `poll_failures`
    /// bumps only for a PERMANENT failure — an unknown or expired tracking
    /// number, not a transient network or rate-limit error.
    ///
    /// EVERY ANSWERED ATTEMPT IS RECORDED HERE, permanent or not. A row asked
    /// about and then left unstamped keeps the head of a queue that sorts
    /// never-polled first, so it is polled again, and again, ahead of everything
    /// behind it — one deterministically-failing number would own a carrier's
    /// entire throughput. Whether the failure counts is the caller's judgment
    /// (see [`crate::carriers::poller`]); that it happened is not.
    fn record_poll_outcome(
        &self,
        account_id: AccountId,
        shipment_id: i64,
        polled_at: DateTime<Utc>,
        permanent_failure: bool,
    ) -> Result<()>;

    /// Upsert a receipt keyed by `(account_id, message_id)` — a re-ingest of the
    /// same message updates in place (idempotent).
    ///
    /// SECURITY: callers run this ONLY for non-sealed mail, so `receipts` holds
    /// no sealed rows by construction.
    fn upsert_receipt(
        &self,
        account_id: AccountId,
        message_id: i64,
        from_addr: &str,
        from_name: Option<&str>,
        receipt: &ReceiptInfo,
        received_at: DateTime<Utc>,
    ) -> Result<i64>;

    /// Receipts received within the last `days`, newest first. Sealed rows are
    /// structurally absent, so no sealed filter is required.
    fn list_receipts(&self, account_id: AccountId, days: u32) -> Result<Vec<Receipt>>;

    /// Upsert a calendar update keyed by `(account_id, message_id)` — a re-ingest
    /// of the same message updates in place (idempotent).
    ///
    /// SECURITY: callers run this ONLY for non-sealed mail, so
    /// `calendar_updates` holds no sealed rows by construction.
    fn upsert_calendar_update(
        &self,
        account_id: AccountId,
        message_id: i64,
        calendar: &CalendarInfo,
        received_at: DateTime<Utc>,
    ) -> Result<i64>;

    /// Calendar updates RECEIVED within the last `hours` (mail arrival window,
    /// NOT event start time), newest first, each joined to its `thread_id` so the
    /// client can open the mail. Sealed rows are structurally absent.
    fn list_calendar_updates(
        &self,
        account_id: AccountId,
        hours: u32,
    ) -> Result<Vec<CalendarUpdate>>;

    // SPECIALIST EXTRACTORS (categorize-then-extract): the LLM assigns a
    // `category`; a category with a registered extractor queues the row for a
    // structured second pass. Sealed rows carry `category=NULL` and are
    // structurally excluded from every extractor queue.

    /// Up to `limit` rows queued for a specialist extractor: NON-SEALED,
    /// non-sent rows whose `category` is in `categories` and whose
    /// `extractor_model_used IS NULL`, newest first. Rows that already produced a
    /// RECEIPT are excluded so a receipt and a banking row never double-create.
    fn extract_queue(
        &self,
        account_id: AccountId,
        categories: &[&str],
        limit: usize,
    ) -> Result<Vec<ExtractQueued>>;

    /// Up to `limit` rows the SHIPMENTS extractor still owes a verdict:
    /// NON-SEALED, non-sent rows with `ship_extract_model='pending'`, newest
    /// first. Deliberately NOT [`Store::extract_queue`] — that one routes on
    /// `triage.category` (no shipping category exists) and excludes
    /// receipt-bearing messages, which most order confirmations are. The trigger
    /// is stamped at INGEST from a loose shipping signal, so a queued row may
    /// still carry a NULL category, surfaced here as `""`.
    fn ship_extract_queue(&self, account_id: AccountId, limit: usize)
    -> Result<Vec<ExtractQueued>>;

    /// Stamp `triage.ship_extract_model` with a PROCESSED marker (the extractor
    /// model id, or a `'stale-skip'` / `'apply-failed'` / `'extract-failed'`
    /// sentinel), taking the row out of [`Store::ship_extract_queue`]. Guarded by
    /// `sensitivity='normal'`, exactly like [`Store::extract_mark_processed`].
    fn ship_extract_mark(&self, account_id: AccountId, message_id: i64, marker: &str)
    -> Result<()>;

    /// Apply one SHIPMENTS-EXTRACTOR verdict IN ONE TRANSACTION: stamp
    /// `triage.ship_extract_model` (leaving [`Store::ship_extract_queue`]), then
    /// reconcile the package IDENTITY the model found against what the regex
    /// detector already wrote. Returns whether a tracked record was written or
    /// updated — a `shipments` row, or a staged `shipment_orders` row.
    ///
    /// The marker is stamped FIRST, guarded by `sensitivity='normal'`. If that
    /// guard matches nothing the message was SEALED mid-pass, and the call writes
    /// NOTHING derived from it and returns `Ok(false)` — the same TOCTOU rule as
    /// [`Store::stage1_apply`].
    ///
    /// DELETION RULE — a row is deleted ONLY on POSITIVE EVIDENCE that it is a
    /// phantom: the extractor named a DIFFERENT tracking number for this mail, or
    /// declared it not a shipment at all. An extraction that names NO number is
    /// silence, not a verdict, and deletes nothing — the detector that minted the
    /// row already passed a carrier-signal gate, a tracking-label context gate and
    /// an item/order hard negative, while the extractor drops a number it sees
    /// echoed in the order field.
    ///
    /// Every delete is then bounded three ways: to rows THIS message CREATED
    /// (`shipments.created_by_message_id`, immutable — `last_message_id` moves to
    /// the latest feeder and would put another mail's package in range), to
    /// AMBIGUOUS tracking SHAPES only (see
    /// [`is_ambiguous_tracking_shape`](crate::triage::is_ambiguous_tracking_shape)),
    /// so a model false negative can never destroy a real `1Z…` / `TBA…` / IMpb
    /// package, and NEVER to a row a carrier has answered about or been polled
    /// for. Then, by which identity the model found:
    ///
    /// * TRACKING NUMBER — upsert the shipment (status still flows through
    ///   [`ShipmentStatus::merge`](crate::triage::ShipmentStatus::merge), so a
    ///   delivered package never walks back), write `item_name` with
    ///   `item_name_source='llm'` (which beats any 'regex' name outright, since
    ///   the upsert's longer-name-wins heuristic otherwise keeps regex junk, and
    ///   yields to another 'llm' name only when that one is longer), record
    ///   `order_ref` with its merchant, and
    ///   PROMOTE any staged `shipment_orders` row under that reference — donating
    ///   its item name, and that name's PROVENANCE, if the shipment has none —
    ///   then delete it.
    /// * ORDER REFERENCE ONLY — adopt the item name onto the shipment already
    ///   carrying that reference, or STAGE the purchase in `shipment_orders`
    ///   until a ship notice arrives with a number.
    /// * NEITHER — name adoption only, and only when the thread holds exactly one
    ///   shipment and that row has no name. Status, `last_message_id` and
    ///   `last_update` are never touched: a mail with no identity has no claim on
    ///   a row's lifecycle.
    ///
    /// ORDER REFERENCES ARE MERCHANT-SCOPED. "Order #1042" is unique only within
    /// the shop that issued it, so both the `shipments` lookup and the staging key
    /// pair it with `order_merchant`, the registrable domain of the feeding
    /// message's sender. Where several rows still match (an order that split into
    /// two packages), the name is donated to NEITHER — ambiguous identity does not
    /// guess.
    ///
    /// `last_message_id` is only ever set by the tracking-number upsert, always to
    /// a real message id — the seal-time delete keys on it — and every item-name
    /// write stamps `item_name_msg` (which MESSAGE) and `item_name_source` (which
    /// MECHANISM), so sealing scrubs a donated name even from a row another
    /// message feeds, and resets the source with it.
    fn shipments_extract_apply(&self, applied: &ShipmentsApplied) -> Result<bool>;

    /// DEV RE-TRIAGE: clear the LLM markers on non-sealed, non-sent inbound rows
    /// so they re-enter the Stage-1 queue, deleting their stale `banking`,
    /// `marketing` and `shipment_orders` rows (extraction recreates them) and
    /// re-pending any row that ever carried a shipping signal. `shipments` rows
    /// SURVIVE — they are identity-keyed by tracking number and carry
    /// carrier-poll state no re-run can recover — but the ITEM NAMES the reset
    /// messages contributed are cleared in both shipment tables, by
    /// `item_name_msg` provenance, exactly as sealing does. A name is a model
    /// verdict, and re-triage is redoing that verdict; leaving it in place means
    /// a re-extraction that finds no name (or says it was never a shipment)
    /// silently keeps the old one forever.
    /// Rule-decided rows (`stage1_model_used='rule'`)
    /// and sealed/sent rows (`'n/a'`) are NEVER touched — rules are authoritative
    /// and sealed mail re-enters no queue. `message_id=None` scopes to the
    /// trailing `days` of inbound mail; `Some(id)` to that one message.
    fn retriage_reset(
        &self,
        account_id: AccountId,
        message_id: Option<i64>,
        days: u32,
    ) -> Result<u64>;

    /// Mark an extract-queued row PROCESSED without writing a specialist row —
    /// stamp `extractor_model_used` only. Used on the skip / refusal / permanent-
    /// error paths so the row does not loop. Guarded by `sensitivity='normal'`.
    fn extract_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        extractor_model_used: &str,
    ) -> Result<()>;

    /// Apply a parsed banking extraction IN ONE TRANSACTION: upsert the `banking`
    /// row, stamp `triage.extractor_model_used` (leaving the extract queue), and
    /// when `auto_resolve` is set resolve the triage row to `status='done'`.
    /// Guarded by `sensitivity='normal'`.
    fn banking_apply(&self, applied: &BankingApplied) -> Result<i64>;

    /// Banking records, newest-received first. Sealed rows are structurally
    /// absent, so no sealed filter is required.
    fn list_banking(&self, account_id: AccountId) -> Result<Vec<Banking>>;

    /// Upsert a sender rule. Returns the rule id.
    ///
    /// Rejects (`InvalidInput`) a `Filtered` rule with empty/whitespace
    /// `want_text`: the want text IS the rule's instruction, and without one the
    /// rule silently degrades. Same validation in
    /// [`Store::set_sender_rule_audited`] and [`Store::update_sender_rule`].
    fn set_sender_rule(
        &self,
        account_id: AccountId,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<i64>;

    /// AGENT-DOOR upsert: [`Store::set_sender_rule`] plus the audit row IN THE
    /// SAME TRANSACTION. FAIL-CLOSED — if the audit insert fails the rule write
    /// rolls back, because an agent write must never land untraced. Audit fields
    /// are written verbatim (the MCP door supplies actor="agent",
    /// action="rule.set").
    fn set_sender_rule_audited(
        &self,
        account_id: AccountId,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
        audit: &NewAuditEntry,
    ) -> Result<i64>;

    fn list_sender_rules(&self, account_id: AccountId) -> Result<Vec<SenderRule>>;

    /// Overwrite an existing sender rule by id (scoped to `account_id`),
    /// restamping `updated_at`. `false` => unknown id => caller returns 404.
    fn update_sender_rule(
        &self,
        account_id: AccountId,
        id: i64,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<bool>;

    /// Store a message plus its triage (and any deadline) in ONE transaction.
    /// The ONLY ingest path the sync engine uses: a sealed classification commits
    /// with the row it seals, so there is no window in which a sealed message is
    /// queryable as normal mail (docs/SECURITY.md §4).
    fn ingest_message(&self, triaged: &TriagedMessage) -> Result<i64>;

    /// True if `addr` appears in this account's Sent-derived contacts (the
    /// "people I know" signal the sync engine feeds to Stage-1).
    fn is_known_contact(&self, account_id: AccountId, addr: &str) -> Result<bool>;

    /// HUMAN-DOOR ONLY (`/client/contacts`): rank Sent-derived contacts for a
    /// typed fragment — recipient autocomplete. MUST NOT be reachable from MCP;
    /// the agent door never learns who the user writes to.
    fn search_contacts(
        &self,
        account_id: AccountId,
        q: &str,
        limit: u32,
    ) -> Result<Vec<ContactEntry>>;

    /// Merge the Sent-history harvest's per-address aggregate (MAX semantics —
    /// idempotent across harvest re-runs and overlap with ingest seeding).
    fn merge_harvested_contacts(&self, account_id: AccountId, batch: &[ContactEntry])
    -> Result<()>;

    /// Read the sync cursor for a mailbox key, if one has been persisted.
    fn sync_state(&self, account_id: AccountId, mailbox: &str) -> Result<Option<SyncState>>;

    /// Upsert the sync cursor for a mailbox key.
    fn set_sync_state(&self, account_id: AccountId, mailbox: &str, state: &SyncState)
    -> Result<()>;

    /// The last Gmail INBOX unread counts the sync loop stored, or `None` when
    /// none were ever fetched (old DB, or every fetch so far has failed). `None`
    /// is NOT zero and callers must keep the distinction: zero unread is a real
    /// answer, "we do not know" is not.
    fn inbox_unread(&self, account_id: AccountId) -> Result<Option<InboxUnread>>;

    /// Overwrite this account's Gmail INBOX unread counts, stamping `fetched_at`
    /// now. Only ever called with numbers that came back from Gmail, so a failed
    /// fetch leaves the previous row in place.
    fn set_inbox_unread(&self, account_id: AccountId, messages: i64, threads: i64) -> Result<()>;

    /// LOCAL-ONLY (TUI): list sealed messages. This is the ONLY method that
    /// exposes sealed content and must never be reachable from MCP.
    fn sealed_messages(&self, account_id: AccountId) -> Result<Vec<SealedMessage>>;

    // HUMAN-DOOR additions (squelch-api /client/*). These MUST NOT be called
    // from MCP, sync, or triage. `search` still excludes sealed rows; the
    // sealed_* / audit methods are the human door's privileged surface.

    /// HUMAN-DOOR-ONLY: ranked updates carrying attention-lifecycle fields for
    /// the sitrep chassis, `band` applying a server-side bucket (see
    /// [`SitrepBand`]). Sealed rows are excluded in SQL.
    ///
    /// The returned `surfaced_at` is the PRE-stamp value — this method never
    /// mutates the ledger; the caller stamps with [`Store::mark_surfaced`] AFTER
    /// the serialization set is computed.
    fn attention_updates(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        min_importance: Option<u8>,
        status: Option<AttentionStatus>,
        band: Option<SitrepBand>,
    ) -> Result<Vec<AttentionUpdate>>;

    /// SEEN-LEDGER stamp, in ONE transaction: for each non-sealed message id set
    /// `surfaced_at=now` only if currently NULL, and promote `status`
    /// `new`->`open`. Sealed rows are guarded out in SQL. Returns the
    /// first-surface count (rows whose `surfaced_at` transitioned from NULL).
    fn mark_surfaced(&self, account_id: AccountId, message_ids: &[i64]) -> Result<usize>;

    /// Set the attention status of one message's triage row. `Done` stamps
    /// `resolved_at=now`; `Open`/`New` clear it. Sealed rows are excluded in SQL,
    /// so missing and sealed both return `false`.
    fn set_attention_status(
        &self,
        account_id: AccountId,
        message_id: i64,
        status: AttentionStatus,
    ) -> Result<bool>;

    /// Resolve EVERY still-open triage row from one sender address, returning how
    /// many moved. Case-insensitive on `from_addr`; sealed rows excluded in SQL,
    /// exactly as `set_attention_status`.
    ///
    /// For the two actions that are verdicts on a SENDER rather than on a
    /// message — unsubscribing, and a squelch rule. Resolving only the thread
    /// the reader happened to be looking at leaves the rest of that sender's
    /// mail sitting in the bands, which is indistinguishable from the action not
    /// having worked. Never call this for a per-message action.
    fn resolve_sender(&self, account_id: AccountId, sender_addr: &str) -> Result<usize>;

    /// FTS5 keyword search over non-sealed messages. `limit`/`offset` paginate.
    /// SECURITY: sealed rows are excluded in SQL, exactly like `ranked_updates`.
    fn search(
        &self,
        account_id: AccountId,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>>;

    /// [`Store::search`] with the operator half of the query applied ([`from:`,
    /// `after:`, `before:`](search_query)). `text` is the already-parsed search
    /// text — this method never sees a raw query string, so the operators are
    /// parsed exactly once, at the door.
    ///
    /// When `text` is empty and `filter` is not, this is a FILTER-ONLY LISTING:
    /// newest-first over `messages` with no FTS MATCH at all. Both shapes keep
    /// `search`'s guarantees — sealed rows excluded, sent mail excluded.
    fn search_filtered(
        &self,
        account_id: AccountId,
        text: &str,
        filter: &SearchFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>>;

    /// Delete a sender rule by id (scoped to `account_id`). Returns whether a
    /// row was removed.
    fn delete_sender_rule(&self, account_id: AccountId, id: i64) -> Result<bool>;

    /// HUMAN-DOOR-ONLY: the full body of exactly one sealed message. Reachable
    /// only from the squelch-api reveal endpoint, which appends the audit row
    /// BEFORE calling this and answers `no-store`. `NotFound` when the message
    /// does not exist or is not sealed.
    fn sealed_body(&self, account_id: AccountId, message_id: i64) -> Result<SealedBody>;

    /// HUMAN-DOOR-ONLY (`GET /client/sent`): the user's own sent mail, newest
    /// first (`received_at DESC, id DESC`), `limit`/`offset` paginating exactly
    /// as [`Store::search`] does. MUST NOT be reachable from MCP — the agent
    /// door has no sent surface at all.
    ///
    /// SECURITY: this is the one listing that reads `is_sent = 1`, so its sealed
    /// guard FAILS CLOSED — the triage join is an INNER join AND requires
    /// `sensitivity != 'sealed'`, which excludes a sent row missing its triage
    /// row rather than defaulting it to visible.
    fn sent_listing(
        &self,
        account_id: AccountId,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SentMessage>>;

    /// Up to `limit` sent messages with no `to_addrs` yet, newest-first: the
    /// queue for the one-shot recipients backfill (rows ingested before the
    /// column existed).
    fn sent_missing_recipients(
        &self,
        account_id: AccountId,
        limit: u32,
    ) -> Result<Vec<SentMissingRecipients>>;

    /// Set one SENT message's display recipients; `false` when no such sent row
    /// exists. `""` is a legitimate value — it records that the headers were read
    /// and named nobody, which is what takes the row out of the backfill queue.
    /// Received mail is never touched (`is_sent = 1` is in the predicate).
    fn set_message_to_addrs(
        &self,
        account_id: AccountId,
        message_id: i64,
        to_addrs: &str,
    ) -> Result<bool>;

    /// Append a row to the human-door audit log. Returns the new row id.
    fn append_audit(&self, account_id: AccountId, entry: &NewAuditEntry) -> Result<i64>;

    // NOTIFICATION EVENTS: the durable, monotonic log the delivery adapters read.
    // WRITTEN ONLY BY THE SYNC ENGINE via `triage::events` — never from a store
    // write method, because only the engine knows which sync path it is on and
    // whether the mail is fresh. Every reader carries its OWN cursor; there is
    // deliberately no global 'delivered' flag.

    /// Append one event, at most once per message ever (INSERT OR IGNORE on
    /// `UNIQUE(message_id)`), which is what makes re-ingest and the refine passes
    /// idempotent. `None` when the message already has an event.
    ///
    /// A real insert also pokes the in-process broadcast (see
    /// [`SqliteStore::attach_event_notifier`]) so an SSE reader wakes without
    /// polling; that send is best-effort, no receivers is normal.
    fn append_event(&self, ev: &NewEvent) -> Result<Option<i64>>;

    /// Events with `id > after_id`, oldest first, capped at `limit`. The replay
    /// query behind `GET /client/events?after=<cursor>`.
    fn events_after(
        &self,
        account_id: AccountId,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<Event>>;

    /// One event by id, scoped to the account. `None` for an unknown id — this
    /// is what the iOS Notification Service Extension calls after an opaque
    /// push, so it must never be more informative than that.
    fn event_by_id(&self, account_id: AccountId, id: i64) -> Result<Option<Event>>;

    /// The newest event id for the account, or `0` when there are none — the
    /// starting cursor for a client that has never connected.
    fn latest_event_id(&self, account_id: AccountId) -> Result<i64>;

    // REGISTERED PUSH DEVICES. Written by the human door, read by the APNs
    // pusher, invisible to the agent door.

    /// Register a device token, or refresh an already-known one. IDEMPOTENT
    /// (UPSERT on `UNIQUE(token)`): iOS re-hands its token each launch, so a
    /// re-register updates `last_registered_at` rather than forking a row.
    ///
    /// A token registered to ANOTHER account is refused with
    /// [`CoreError::InvalidInput`] and nothing is written — re-registration must
    /// never silently repoint a device's pushes at a different account.
    fn upsert_device(&self, account_id: AccountId, token: &str, platform: &str) -> Result<Device>;

    /// Every registered device for the account, oldest first — a stable order, so
    /// a push fan-out and its response array line up reproducibly.
    fn list_devices(&self, account_id: AccountId) -> Result<Vec<Device>>;

    /// Drop one device by token, scoped to the account. Two callers: the human
    /// door's DELETE, and the pusher on APNs `410 Unregistered`.
    fn delete_device_by_token(&self, account_id: AccountId, token: &str) -> Result<bool>;

    // UNSUBSCRIBE (human door). All four are `/client/*`-only, and sealed mail is
    // invisible to them.

    /// The stored unsubscribe fields for a NON-SEALED message. `None` for a
    /// missing OR sealed message (indistinguishable), which the caller maps to
    /// 404.
    fn message_unsub_fields(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<MessageUnsub>>;

    /// DEV DEBUG: the full triage row for one NON-SEALED message. `None` for
    /// missing OR sealed (indistinguishable). Human door only.
    fn triage_debug(&self, account_id: AccountId, message_id: i64) -> Result<Option<TriageDebug>>;

    /// Upsert the unsubscribe row for `(account_id, sender)`. On conflict this
    /// RESETS the violation ledger (`violation_count -> 0`, `last_violation_at`
    /// and `resolution -> NULL`): the user re-asked, so the 72h grace clock
    /// restarts. `sender` is stored verbatim — the caller lowercases it.
    fn upsert_unsubscribe(
        &self,
        account_id: AccountId,
        sender: &str,
        method: &str,
        source_message_id: Option<i64>,
        requested_at: DateTime<Utc>,
    ) -> Result<()>;

    /// List the account's unsubscribe records, newest `requested_at` first.
    fn list_unsubscribes(&self, account_id: AccountId) -> Result<Vec<UnsubscribeRecord>>;

    /// Set the `resolution` (blocked|dismissed) on an existing unsubscribe row,
    /// disarming the violation detector for that sender. `false` when no row
    /// exists (=> caller returns 404).
    fn set_unsubscribe_resolution(
        &self,
        account_id: AccountId,
        sender: &str,
        resolution: &str,
    ) -> Result<bool>;

    /// Upsert one extracted promotion + stamp `triage.extractor_model_used`.
    /// Unlike [`Store::banking_apply`] this does NOT resolve the triage row.
    fn marketing_apply(&self, applied: &MarketingApplied) -> Result<()>;

    /// Extracted promotions, newest first, received within the last `days`,
    /// capped at `limit`. Structurally sealed-free.
    fn marketing_offers(
        &self,
        account_id: AccountId,
        days: u32,
        limit: u32,
    ) -> Result<Vec<MarketingOffer>>;

    /// Apply a human's triage correction and record it as feedback in ONE
    /// transaction — the training row and the state it describes must never
    /// disagree.
    ///
    /// Sets the triage row's `axis` column to `to_value` and stamps the row
    /// human-decided (`stage1_model_used`/`model_used` = 'human',
    /// `needs_stage2` = 0) so no later LLM pass can silently overwrite the human,
    /// then appends a `triage_feedback` row with the full prior snapshot.
    ///
    /// `None` when the message does not exist for this account. The caller
    /// validates `to_value` against [`TriageAxis::allowed`] first.
    fn correct_triage(
        &self,
        account_id: AccountId,
        message_id: i64,
        axis: TriageAxis,
        to_value: &str,
        note: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<TriageFeedback>>;

    /// Recorded corrections, newest first, capped at `limit` — the refinement
    /// dataset.
    fn list_triage_feedback(
        &self,
        account_id: AccountId,
        limit: u32,
    ) -> Result<Vec<TriageFeedback>>;

    // AUTH-MAIL SHREDDER (retention). Human-door-only; see the `shred_log` block
    // in schema.sql for the policy.

    /// Auth mail (`triage.sensitivity = 'sealed'`) received at or before `cutoff`
    /// and not already in `shred_log`, oldest first, capped at `limit`. Rows
    /// without a `gmail_msg_id` are skipped — the trash call has nothing to
    /// address.
    fn shred_candidates(
        &self,
        account_id: AccountId,
        cutoff: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ShredCandidate>>;

    /// How many candidates [`Store::shred_candidates`] would return, unbounded.
    /// Drives the "N waiting" figure without materializing the rows.
    fn shred_pending_count(&self, account_id: AccountId, cutoff: DateTime<Utc>) -> Result<i64>;

    /// Record one shredded message. Called ONLY after Gmail confirms the trash,
    /// so the ledger never claims a deletion that did not happen;
    /// `UNIQUE(account_id, message_id)` makes a re-run a no-op.
    fn record_shred(
        &self,
        account_id: AccountId,
        candidate: &ShredCandidate,
        shredded_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Ledger totals for the Auth page: shreds since `recent_since`, the
    /// all-time count, and the most recent shred time.
    fn shred_counts(
        &self,
        account_id: AccountId,
        recent_since: DateTime<Utc>,
    ) -> Result<(i64, i64, Option<DateTime<Utc>>)>;

    /// The most recent audit rows, newest first, capped at `limit`. A row gains
    /// `target_sender`/`target_subject` when its `target` parses as a message id
    /// that exists for the account.
    fn list_audit(&self, account_id: AccountId, limit: u32) -> Result<Vec<AuditEntry>>;

    /// Per-tier / sealed / sync-cursor summary counts for the account. The
    /// band counts are windowed to `bands_since` so they agree with the list
    /// queries they head; the inventory counts are all-time.
    fn stats(&self, account_id: AccountId, bands_since: DateTime<Utc>) -> Result<StoreStats>;

    // STAGE-2 additions, supporting the LLM triage pass in the sync loop. The
    // queue predicate is `model_used IS NULL AND sensitivity='normal'`, so sealed
    // rows are structurally excluded.

    /// Up to `limit` rows queued for the Stage-1 LLM refine pass
    /// (`stage1_model_used IS NULL AND sensitivity='normal' AND is_sent=0`),
    /// newest first. Rule-decided rows carry a non-NULL `stage1_model_used` and
    /// are excluded.
    fn stage1_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage1Queued>>;

    /// Apply a parsed Stage-1 LLM result onto a triage row IN ONE TRANSACTION:
    /// overwrite importance/tier/one_line/reason/field_reasons, stamp
    /// `stage1_model_used` (leaving the Stage-1 queue), set `needs_stage2`, and
    /// (re)write the `deadlines` row. Guarded by `sensitivity='normal'`; never
    /// touches `model_used` (the Stage-2 marker).
    ///
    /// `false` (the guarded UPDATE matched nothing) means the row was sealed
    /// between the queue SELECT and this apply: nothing was written, not even the
    /// deadline, and the caller must treat the verdict as NOT landed — in
    /// particular, no notification event.
    fn stage1_apply(&self, applied: &Stage1Applied) -> Result<bool>;

    /// Mark a Stage-1-queued row PROCESSED without changing its heuristic seed
    /// values — stamp `stage1_model_used` only, PRESERVING the `needs_stage2`
    /// seed written at ingest, so on the heuristic-only fallback (API down /
    /// refusal / permanent error) the seed's own confidence decides escalation.
    /// Guarded by `sensitivity='normal'`.
    fn stage1_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        stage1_model_used: &str,
    ) -> Result<()>;

    /// The row's CURRENT verdict as the heuristic seed left it, for the Stage-1
    /// fallback's notification decision. `None` when the row is missing or
    /// sealed. Read on the fallback path only, which is rare by construction.
    fn triage_seed_verdict(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<SeedVerdict>>;

    // ---- REVISITS (see `crate::triage::revisit`) --------------------------

    /// Replace a message's PENDING scheduled re-evaluations with `requests`.
    /// Already-fired rows are left as history. A sealed row stores nothing:
    /// firing a revisit would put sealed mail back in front of a model.
    fn revisits_schedule(
        &self,
        account_id: AccountId,
        message_id: i64,
        requests: &[crate::triage::revisit::RevisitRequest],
        now: DateTime<Utc>,
    ) -> Result<()>;

    /// Up to `limit` revisits that have come due, oldest first. Excludes sealed
    /// rows, sent mail, already-resolved rows, rows at or past `max_lifetime`
    /// re-evaluations, and any message the account owner has corrected by hand.
    fn revisit_queue(
        &self,
        account_id: AccountId,
        now: DateTime<Utc>,
        max_lifetime: u32,
        limit: usize,
    ) -> Result<Vec<RevisitQueued>>;

    /// Stamp a revisit fired and charge the message's lifetime counter. Called
    /// even when the re-classification failed, so a broken row cannot be retried
    /// every cycle forever. Idempotent: guarded on `fired_at IS NULL`.
    fn revisit_mark_fired(
        &self,
        account_id: AccountId,
        revisit_id: i64,
        now: DateTime<Utc>,
    ) -> Result<()>;

    /// Message ids in the standing band, not done, with no human correction and
    /// nothing that has looked at them since `older_than` — no pending revisit
    /// and none FIRED inside that window, which is what stops the sweep from
    /// re-asking every sync tick: the automatic staleness sweep's candidates.
    fn revisit_stale_standing(
        &self,
        account_id: AccountId,
        older_than: DateTime<Utc>,
        max_lifetime: u32,
        limit: usize,
    ) -> Result<Vec<i64>>;

    /// Apply a re-evaluated verdict: [`Store::stage1_apply`]'s write, plus
    /// clearing `model_used` so a newly escalated row can re-enter the Stage-2
    /// queue, and refusing any row carrying a human correction.
    ///
    /// `false` means the guarded UPDATE matched nothing — the row was sealed or
    /// corrected between the queue read and the apply — and the caller must
    /// treat the verdict as NOT landed.
    fn revisit_apply(&self, applied: &Stage1Applied) -> Result<bool>;

    /// Bump the Stage-1 usage ledger for `(account_id, day)`: +1 call plus the
    /// response's token counts. Kept separate from Stage-2's ledger.
    fn stage1_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        tokens: UsageTokens,
    ) -> Result<()>;

    /// Sum the Stage-1 usage ledger over every day `>= since_day`. Drives the
    /// trailing-window Stage-1 averages on `/client/triage-config`.
    fn stage1_usage_since(&self, account_id: AccountId, since_day: &str) -> Result<Stage2Usage>;

    /// Bump a SPECIALIST-EXTRACTOR usage line for `(account_id, day, category)`.
    /// `category` is the extractor's own ledger label (e.g. `extract_banking`),
    /// kept separate from `stage1`/`stage2` so per-specialist cost stays visible.
    #[allow(clippy::too_many_arguments)] // the parts of one usage ledger line
    fn extract_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        category: &str,
        tokens: UsageTokens,
    ) -> Result<()>;

    /// Stage-1 usage history: the most recent `days` rows, newest-first (sparse).
    fn list_usage_stage1(&self, account_id: AccountId, days: u32) -> Result<Vec<Stage2UsageDay>>;

    /// EVERY ledger category with its history, sorted by category name.
    ///
    /// The ledger is open-ended — each extractor writes its own category — so a
    /// caller that names the categories it wants silently drops every one added
    /// after it was written. That is exactly how extractor spend went unreported
    /// while it accrued daily. Enumerate, don't enumerate-by-hand.
    fn list_usage_by_category(
        &self,
        account_id: AccountId,
        days: u32,
    ) -> Result<Vec<(String, Vec<Stage2UsageDay>)>>;

    /// Up to `limit` queued Stage-2 rows (`stage1_model_used IS NOT NULL AND
    /// needs_stage2=1 AND model_used IS NULL AND sensitivity='normal'`) with
    /// message context and any matched Filtered rule's `want_text`. Newest-first,
    /// so the freshest ambiguous mail gets a look first.
    fn stage2_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage2Queued>>;

    /// Today's Stage-2 API-call count for a budget scope: `thread_id` is either a
    /// real Gmail thread id (per-thread cap) or the `'__global__'` sentinel
    /// (per-account cap). `day` is caller-provided so tests are deterministic.
    fn stage2_budget_used(&self, account_id: AccountId, thread_id: &str, day: &str) -> Result<u32>;

    /// Increment (and return the new value of) today's Stage-2 API-call count
    /// for a budget scope. Called BEFORE the API attempt so retries count and
    /// cannot exceed the cap. Upserts the `wake_budget` row.
    fn stage2_increment_budget(
        &self,
        account_id: AccountId,
        thread_id: &str,
        day: &str,
    ) -> Result<u32>;

    /// Apply a parsed Stage-2 result onto a triage row IN ONE TRANSACTION:
    /// overwrite importance/tier/one_line/reason, stamp `model_used` (leaving the
    /// queue), and (re)write the `deadlines` row when the model extracted one.
    /// Guarded by `sensitivity='normal'`.
    ///
    /// `false` is the sealed-mid-pass case, as on [`Store::stage1_apply`]:
    /// nothing was written and the caller must not emit for it.
    fn stage2_apply(&self, applied: &Stage2Applied) -> Result<bool>;

    /// Mark a queued row PROCESSED without changing its Stage-1 values — stamp
    /// `model_used` only, on refusal or a permanent API error, so the row does
    /// not loop forever. Guarded by `sensitivity='normal'`.
    fn stage2_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        model_used: &str,
    ) -> Result<()>;

    /// Bump the Stage-2 usage ledger for `(account_id, day)`: +1 call plus the
    /// response's token counts, after each classify that carried a usage block.
    fn stage2_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        tokens: UsageTokens,
    ) -> Result<()>;

    /// Read the Stage-2 usage totals for `(account_id, day)`. Returns a zeroed
    /// [`Stage2Usage`] when no row exists for that day.
    fn stage2_usage_today(&self, account_id: AccountId, day: &str) -> Result<Stage2Usage>;

    /// Stage-2 usage history: the most recent `days` rows, newest-first and
    /// sparse (only days that have a row; no zero-filling).
    fn list_usage(&self, account_id: AccountId, days: u32) -> Result<Vec<Stage2UsageDay>>;

    /// Sum the Stage-2 usage ledger over every day `>= since_day` (a `YYYY-MM-DD`
    /// UTC key — lexical compare is correct for that format). Drives the
    /// trailing-window averages on `/client/triage-config`.
    fn stage2_usage_since(&self, account_id: AccountId, since_day: &str) -> Result<Stage2Usage>;

    // RUNTIME APP SETTINGS (human door): a per-account key/value store for knobs
    // a client can change without a restart. The Stage-2 pass re-reads the
    // overrides at the START of each cycle. Precedence: override > config/env >
    // default.

    /// Read one `app_settings` value for `(account_id, key)`, or `None` if unset.
    fn get_app_setting(&self, account_id: AccountId, key: &str) -> Result<Option<String>>;

    /// Upsert one `app_settings` value for `(account_id, key)`.
    fn set_app_setting(&self, account_id: AccountId, key: &str, value: &str) -> Result<()>;

    /// Read the daily-cap overrides in ONE query. A cap is `None` when no row
    /// exists OR the stored value does not parse as an integer in `1..=100000`.
    fn stage2_cap_overrides(&self, account_id: AccountId) -> Result<Stage2CapOverrides>;

    // OUTBOUND READ TRACKING. Written by the human door's send path and by the
    // two open sinks (the direct `/t/:token` route and the opens poller); read
    // only by the human door. The agent door never touches any of it.

    /// Record a minted tracking token. `message_id` is `None` until the send's
    /// echo lands; `created_at` is unix seconds.
    fn insert_send_tracker(
        &self,
        account_id: AccountId,
        token: &str,
        message_id: Option<i64>,
        created_at: i64,
    ) -> Result<()>;

    /// Backfill the local message id of a tracker once the echo has ingested.
    /// `false` for a token this account did not mint.
    fn set_send_tracker_message(
        &self,
        account_id: AccountId,
        token: &str,
        message_id: i64,
    ) -> Result<bool>;

    /// Append one open. `Ok(false)` — no row written — when `token` names no
    /// tracker for this account: both open sinks accept arbitrary tokens from
    /// the outside, so an unknown one must leave nothing behind.
    fn record_open(
        &self,
        account_id: AccountId,
        token: &str,
        opened_at: i64,
        user_agent: Option<&str>,
        classification: &str,
    ) -> Result<bool>;

    /// Every open of one local message, oldest first, joined through the
    /// trackers minted for it. Empty for an untracked or unknown message.
    fn message_opens(&self, account_id: AccountId, message_id: i64) -> Result<Vec<MessageOpen>>;

    /// Resolve a tracking token to the message it was minted for, or `None` when
    /// the token is unknown to this account or its echo never backfilled an id.
    fn tracked_message(&self, account_id: AccountId, token: &str)
    -> Result<Option<TrackedMessage>>;

    /// Count NON-SENT (`is_sent=0`) messages received at or after `since`. Feeds
    /// the `avg_inbound_per_day` figure on `/client/triage-config`.
    fn count_inbound_since(&self, account_id: AccountId, since: DateTime<Utc>) -> Result<u64>;

    // SEMANTIC RECALL vector-index writes. The embedder lives in the caller, so
    // these take a precomputed vector / return the text to embed and never touch
    // a model. Query-side methods are inherent on `SqliteStore` because they need
    // the attached embedder.
    //
    // SECURITY: SEALED MESSAGES ARE NEVER EMBEDDED — the write callers gate on
    // `sensitivity='normal'` and `messages_missing_vectors` selects only normal
    // rows, so sealed content is structurally absent from the vector space.

    /// Insert (or replace) the embedding vector for one message; idempotent.
    /// `embedding.len()` MUST equal the vec0 table width (384). CALLER MUST
    /// ensure the message is non-sealed; this does not re-check.
    fn upsert_message_vector(
        &self,
        account_id: AccountId,
        message_id: i64,
        embedding: &[f32],
    ) -> Result<()>;

    /// Up to `limit` NON-SEALED messages with no vector yet, newest-first, for
    /// the startup backfill pass. Sealed rows are excluded in SQL.
    fn messages_missing_vectors(
        &self,
        account_id: AccountId,
        limit: usize,
    ) -> Result<Vec<MissingVector>>;

    /// The currently-attached embedder, letting the sync engine resolve a
    /// LATE-attached one (attached in the background after `squelchd serve` binds
    /// its port) without holding a second handle.
    fn embedder(&self) -> Option<std::sync::Arc<dyn crate::embed::Embedder>> {
        None
    }
}
