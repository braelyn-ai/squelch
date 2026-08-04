//! Storage abstraction.
//!
//! rusqlite is synchronous, so `Store` is a SYNC trait and `SqliteStore` wraps
//! its `Connection` in a `Mutex`; async callers wrap calls in
//! `tokio::task::spawn_blocking`.

pub mod sqlite;

pub use sqlite::SqliteStore;

use crate::error::Result;
use crate::triage::{CalendarInfo, DeadlineHit, ReceiptInfo, ShipmentInfo};
use crate::types::{
    AccountId, AttachmentInfo, AttentionStatus, AttentionUpdate, AuditEntry, Banking,
    CalendarUpdate, Deadline, Disposition, Event, EventKind, FieldReasons, NewMessage, Receipt,
    SealedKind, SearchHit, SenderRule, Sensitivity, ShredCandidate, StoreStats, ThreadView, Tier,
    TriageAxis, TriageFeedback, Update, UnsubscribeRecord,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Server-side bucket for the sitrep chassis, selected by the `band` param on
/// `/client/updates`. See [`Store::attention_updates`].
///
/// - `Standing` — tier `past_due`/`deadline` AND status != 'done'; immune to the
///   surfacing clock, never rotates out until resolved.
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
    /// Always `'normal'` for queued rows; carried so the sealed guard can assert.
    pub sensitivity: Sensitivity,
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
    /// `true` when the model was not confident: sets `needs_stage2=1` so the
    /// Stage-2 queue predicate picks the row up.
    pub needs_stage2: bool,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stage2Usage {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
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
    fn deadlines(
        &self,
        account_id: AccountId,
        within_days: Option<u32>,
    ) -> Result<Vec<Deadline>>;

    /// Upsert a shipment keyed by `(account_id, tracking_number)`. A later mail
    /// about the same tracking number UPDATES the row through the no-regress
    /// status state machine (a delivered shipment is never walked back),
    /// refreshing `last_update`/`last_message_id` and adopting a longer
    /// `item_name`.
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
    fn list_shipments(
        &self,
        account_id: AccountId,
        include_delivered: bool,
    ) -> Result<Vec<crate::types::Shipment>>;

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

    /// DEV RE-TRIAGE: clear the LLM markers on non-sealed, non-sent inbound rows
    /// so they re-enter the Stage-1 queue, deleting their stale `banking` rows
    /// (extraction recreates them). Rule-decided rows (`stage1_model_used='rule'`)
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

    /// Read the sync cursor for a mailbox key, if one has been persisted.
    fn sync_state(&self, account_id: AccountId, mailbox: &str) -> Result<Option<SyncState>>;

    /// Upsert the sync cursor for a mailbox key.
    fn set_sync_state(
        &self,
        account_id: AccountId,
        mailbox: &str,
        state: &SyncState,
    ) -> Result<()>;

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

    /// Delete a sender rule by id (scoped to `account_id`). Returns whether a
    /// row was removed.
    fn delete_sender_rule(&self, account_id: AccountId, id: i64) -> Result<bool>;

    /// HUMAN-DOOR-ONLY: the full body of exactly one sealed message. Reachable
    /// only from the squelch-api reveal endpoint, which appends the audit row
    /// BEFORE calling this and answers `no-store`. `NotFound` when the message
    /// does not exist or is not sealed.
    fn sealed_body(&self, account_id: AccountId, message_id: i64) -> Result<SealedBody>;

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
    fn events_after(&self, account_id: AccountId, after_id: i64, limit: usize) -> Result<Vec<Event>>;

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
    fn triage_debug(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<TriageDebug>>;

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

    /// Per-tier / sealed / sync-cursor summary counts for the account.
    fn stats(&self, account_id: AccountId) -> Result<StoreStats>;

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

    /// Bump the Stage-1 usage ledger for `(account_id, day)`: +1 call plus the
    /// response's token counts. Kept separate from Stage-2's ledger.
    fn stage1_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()>;

    /// Sum the Stage-1 usage ledger over every day `>= since_day`. Drives the
    /// trailing-window Stage-1 averages on `/client/triage-config`.
    fn stage1_usage_since(&self, account_id: AccountId, since_day: &str) -> Result<Stage2Usage>;

    /// Bump a SPECIALIST-EXTRACTOR usage line for `(account_id, day, category)`.
    /// `category` is the extractor's own ledger label (e.g. `extract_banking`),
    /// kept separate from `stage1`/`stage2` so per-specialist cost stays visible.
    fn extract_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        category: &str,
        input_tokens: u64,
        output_tokens: u64,
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
    fn stage2_budget_used(&self, account_id: AccountId, thread_id: &str, day: &str)
    -> Result<u32>;

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
        input_tokens: u64,
        output_tokens: u64,
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
