//! Storage abstraction.
//!
//! Design choice: rusqlite is synchronous, so the `Store` trait is kept SYNC
//! and `SqliteStore` wraps the `Connection` in a `Mutex`. This is the simplest
//! thing that compiles cleanly. Async callers (the MCP server) can wrap calls
//! in `tokio::task::spawn_blocking` if they need to. Keeping the trait sync
//! avoids dragging `async_trait` + `Send` bounds through every query.

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

/// A server-side convenience bucket for the sitrep chassis, selectable via the
/// `band` param on `/client/updates`. See [`Store::attention_updates`].
///
/// - `Standing`  — tier is `past_due`/`deadline` AND status != 'done'. Immune to
///   the surfacing clock; never rotates out until resolved.
/// - `New`       — `surfaced_at IS NULL`: never surfaced through ANY door.
/// - `Open`      — status = 'open', sorted by `age * importance` descending (the
///   aging/escalating band). See the SQL in `sqlite.rs` for the exact ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SitrepBand {
    Standing,
    New,
    Open,
}

/// The resolved bytes of one attachment: `(filename, mime, data)`, where `data`
/// is `None` when the bytes were not stored (over the ingest cap). Returned by
/// [`Store::attachment_bytes`]; factored out to keep that signature readable.
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

/// The Gmail sync cursor for one (account, mailbox-ish key). Persisted in
/// `sync_state`.
///
/// For the Gmail REST engine the only row is keyed `mailbox = 'history'`:
/// `uidvalidity` is unused (0) and `last_uid` holds the account's `historyId`
/// (a monotonically increasing u64 from `users.getProfile` / `history.list`).
/// The field names are retained from the IMAP era to avoid a schema migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncState {
    pub uidvalidity: u32,
    /// IMAP UID cursor OR (Gmail engine) the `historyId`.
    pub last_uid: u64,
}

/// A fully-triaged message ready to be committed in a single transaction.
///
/// SECURITY: the sync engine constructs this by running seal detection FIRST
/// (`sensitivity`), then — only for non-sealed mail — Stage-1. Passing this to
/// [`Store::ingest_message`] writes the message row and its triage row (and any
/// deadline) atomically, so a sealed message is never observable as normal mail.
#[derive(Debug, Clone)]
pub struct TriagedMessage {
    pub message: NewMessage,
    /// For Sent mail only: the To/Cc recipient addresses to seed the contacts
    /// table with (the account's OWN address is already filtered out at ingest).
    /// Empty for received mail — contacts are derived exclusively from the
    /// recipients of mail the user sent, never from senders of inbound mail.
    pub recipients: Vec<String>,
    pub sensitivity: Sensitivity,
    pub sealed_kind: Option<SealedKind>,
    pub importance: u8,
    pub tier: Tier,
    pub one_line: String,
    pub reason: String,
    /// Per-property Stage-1 justifications (importance / deadline / tier). Written
    /// to the triage row's `field_reasons` JSON column and served HUMAN-DOOR ONLY.
    /// Empty [`FieldReasons`] for sealed / sent mail (nothing to explain).
    pub field_reasons: FieldReasons,
    pub matched_rule: Option<i64>,
    /// The Stage-1 deadline hit, if any. Only ever `Some` for non-sealed mail.
    pub deadline: Option<DeadlineHit>,
    /// A detected shipment/package, if any. Runs INDEPENDENTLY of the triage
    /// tier (shipping mail is noise-tier but still feeds the tracker). Only ever
    /// `Some` for non-sealed mail — sealed content is never inspected for
    /// shipments, so a shipment can never carry sealed data.
    pub shipment: Option<ShipmentInfo>,
    /// A detected receipt (record of money already paid), if any. Runs
    /// INDEPENDENTLY of the triage tier AND of shipment detection — an order
    /// confirmation with a total AND tracking is both a receipt and a shipment.
    /// Only ever `Some` for non-sealed mail. When present, the ingest write also
    /// AUTO-RESOLVES the message's triage row (`status='done'`) so a receipt never
    /// surfaces as New/Attention/Aging clutter — it lives only in the Receipts
    /// category.
    pub receipt: Option<ReceiptInfo>,
    /// A detected calendar update (invite / update / cancellation / RSVP
    /// response), if any. Runs INDEPENDENTLY of the triage tier, exactly like
    /// receipts. Only ever `Some` for non-sealed mail. When present, the ingest
    /// write AUTO-RESOLVES the message's triage row (`status='done'`) so a
    /// calendar notification never surfaces as New/Attention/Aging clutter — it
    /// lives only in the Calendar category.
    pub calendar: Option<CalendarInfo>,
    /// Attachments extracted from the message's RFC822 (real attachments AND
    /// cid-inline parts), each already capped (over-cap parts carry `data: None`
    /// / metadata only). Written to the `attachments` table in the SAME ingest
    /// transaction as the message. Present for BOTH sealed and non-sealed mail —
    /// storage is fine; the byte-serving endpoint is what guards sealed parents.
    /// Empty when the message carried no attachment parts.
    pub attachments: Vec<AttachmentInfo>,
    /// `false` when Stage-1 was not confident: the row is left with
    /// `model_used IS NULL` so the Stage-2 queue predicate
    /// (`model_used IS NULL AND sensitivity = 'normal'`) picks it up.
    pub confident: bool,
}

/// The full body of a single sealed message. HUMAN-DOOR-ONLY: returned solely
/// by [`Store::sealed_body`], which is reachable only from the squelch-api
/// per-message reveal endpoint (never MCP, sync, or triage). Every reveal is
/// audited by the caller before this value leaves the process.
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
    /// The server-side-sanitized (ammonia) HTML body stored at ingest, when the
    /// mail had one. Serving it here keeps the single audited reveal door — the
    /// client renders it in the SAME hard-sandboxed EmailFrame as normal mail.
    pub body_html: Option<String>,
}

/// The Gmail ids + header source fields an action endpoint needs to act on a
/// message. HUMAN-DOOR-ONLY: produced solely by
/// [`SqliteStore::action_message_ref`](sqlite::SqliteStore::action_message_ref),
/// which excludes sealed rows in SQL so an action can never target sealed mail.
/// Carries no message body.
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

/// The stored unsubscribe intent for one NON-SEALED message, resolved by
/// [`Store::message_unsub_fields`] for `POST /client/unsubscribe`. Sealed rows
/// are excluded in SQL so this is returned `None` for a missing OR sealed
/// message (indistinguishable), mirroring [`Store::thread_view`].
/// The FULL triage state of one message, for the developer-mode inspector.
/// Serialized verbatim to the client; carries verdicts/markers/reasons but
/// never body content. Human door only; sealed rows are never returned.
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

/// A notification-worthy event to append to the `events` log.
///
/// Produced ONLY by [`crate::triage::events`] (the pure emission decision) and
/// passed to [`Store::append_event`]. Every field besides the ids is a
/// denormalized snapshot of the verdict at emission time — see the `events`
/// block in schema.sql for why. Sealed mail can never produce one.
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

/// One non-confident triage row queued for the Stage-2 LLM pass, plus the
/// message context and the matched Filtered-rule's `want_text` (when a rule
/// fired). Produced by [`Store::stage2_queue`].
///
/// SECURITY: the query that produces these EXCLUDES sealed rows in SQL (the
/// queue predicate is `model_used IS NULL AND sensitivity='normal'`), so a
/// `Stage2Queued` never represents sealed mail. The Stage-2 pass additionally
/// re-checks the sealed guard defensively before every classify call.
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
    /// When the message was received. Used by the pass loop's SKIP-STALE check:
    /// rows older than `stage2_max_age_days` are marked processed
    /// (`model_used='stale-skip'`) without spending a model call.
    pub received_at: DateTime<Utc>,
    /// `true` if the sender is in the account's Sent-derived contacts. Feeds the
    /// TRUSTED CONTEXT block and gates unknown-sender deadline capping.
    pub is_known_contact: bool,
    /// The matched sender rule's `want_text`, present only when a Filtered rule
    /// fired. Presented in the TRUSTED CONTEXT block as the account owner's
    /// standing instruction for this sender.
    pub rule_want_text: Option<String>,
    /// The row's current sensitivity as stored — always `'normal'` for queued
    /// rows (sealed is excluded in SQL). Carried so the sealed guard can assert.
    pub sensitivity: Sensitivity,
}

/// The store-facing outcome of applying a parsed Stage-2 result onto a triage
/// row. Pure mapping lives in `triage::stage2::apply_result`; this is what the
/// store persists. When `deadline` is `Some`, a `deadlines` row is (re)written.
#[derive(Debug, Clone)]
pub struct Stage2Applied {
    pub message_id: i64,
    pub account_id: AccountId,
    pub importance: u8,
    pub tier: Tier,
    pub one_line: String,
    pub reason: String,
    /// Per-property Stage-2 justifications (importance / deadline / tier),
    /// synthesized in [`crate::triage::stage2::apply_result`] to describe the
    /// values THIS apply actually stores. Written to the `field_reasons` column,
    /// fully replacing any Stage-1 reasons (Stage-2 owns all three properties on
    /// apply). Served HUMAN-DOOR ONLY.
    pub field_reasons: FieldReasons,
    /// The model id string to stamp `model_used` with (marks the row processed
    /// so the queue predicate no longer selects it).
    pub model_used: String,
    /// A deadline to (re)write for this message, if the model extracted one.
    pub deadline: Option<DeadlineHit>,
    /// The coarse routing category to stamp `triage.category` with (parity with
    /// Stage-1). `None` leaves the column untouched.
    pub category: Option<String>,
}

/// One row queued for the Stage-1 LLM refine pass: an ingested, NON-SEALED,
/// non-rule-decided message still carrying its heuristic seed values
/// (`stage1_model_used IS NULL AND sensitivity='normal'`). Produced by
/// [`Store::stage1_queue`].
///
/// SECURITY: the query EXCLUDES sealed rows in SQL, so a `Stage1Queued` never
/// represents sealed mail. The Stage-1 pass additionally re-checks
/// [`crate::triage::stage1_sealed_guard`] before every classify call.
#[derive(Debug, Clone)]
pub struct Stage1Queued {
    pub message_id: i64,
    pub account_id: AccountId,
    /// Gmail thread id (carried for context/logging; Stage-1 uses only a GLOBAL
    /// budget scope, never per-thread).
    pub thread_id: String,
    pub from_addr: String,
    pub subject: String,
    pub body: String,
    /// When the message was received (drives the deadline sanity bounds and the
    /// stale-skip check).
    pub received_at: DateTime<Utc>,
    /// `true` if the sender is a Sent-derived contact. Feeds the TRUSTED CONTEXT
    /// block and gates the unknown-sender deadline cap.
    pub is_known_contact: bool,
    /// Always `'normal'` for queued rows (sealed is excluded in SQL). Carried so
    /// the sealed guard can assert.
    pub sensitivity: Sensitivity,
}

/// The store-facing outcome of applying a parsed Stage-1 LLM result onto a triage
/// row. Pure mapping lives in [`crate::triage::stage1_llm::apply_result`]. Stamps
/// `stage1_model_used` (removing the row from the Stage-1 queue) and sets
/// `needs_stage2` (whether the row escalates to the Stage-2 queue).
#[derive(Debug, Clone)]
pub struct Stage1Applied {
    pub message_id: i64,
    pub account_id: AccountId,
    pub importance: u8,
    pub tier: Tier,
    pub one_line: String,
    pub reason: String,
    /// Per-property Stage-1 justifications describing the STORED values. Written
    /// to `field_reasons`, replacing the heuristic seed reasons. Human-door only.
    pub field_reasons: FieldReasons,
    /// The Stage-1 model id to stamp `stage1_model_used` with.
    pub stage1_model_used: String,
    /// `true` when the model was not confident: sets `needs_stage2=1` so the
    /// Stage-2 queue predicate picks the row up.
    pub needs_stage2: bool,
    /// A deadline to (re)write for this message, if the model extracted one.
    pub deadline: Option<DeadlineHit>,
    /// The coarse routing category to stamp `triage.category` with (`general` |
    /// `invoice` | `banking_statement` | `transaction_alert`). `None` leaves the
    /// column untouched (heuristic-only rows never reach this apply path).
    pub category: Option<String>,
}

/// One row queued for a specialist EXTRACTOR pass: a NON-SEALED triage row whose
/// LLM-assigned `category` has a registered extractor and that has not yet been
/// extracted (`extractor_model_used IS NULL`). Produced by [`Store::extract_queue`].
///
/// SECURITY: the query EXCLUDES sealed rows in SQL (`sensitivity='normal'`) and
/// rows are only ever selected when a real LLM category is present (sealed rows
/// carry `category=NULL`), so an `ExtractQueued` never represents sealed mail. The
/// extract pass additionally re-checks [`crate::triage::extract::extract_sealed_guard`]
/// before every classify call.
#[derive(Debug, Clone)]
pub struct ExtractQueued {
    pub message_id: i64,
    pub account_id: AccountId,
    /// Gmail thread id (carried for context/logging; the extract pass uses only
    /// the shared Stage-1 GLOBAL budget scope).
    pub thread_id: String,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub subject: String,
    pub body: String,
    /// The LLM-assigned routing category that selected this row (one of the
    /// extractable categories). Drives which specialist extractor runs.
    pub category: String,
    /// When the message was received (drives the stored `banking.received_at` and
    /// the stale-skip check).
    pub received_at: DateTime<Utc>,
    /// Always `'normal'` for queued rows (sealed is excluded in SQL). Carried so
    /// the sealed guard can assert.
    pub sensitivity: Sensitivity,
}

/// The store-facing outcome of running the banking specialist extractor on a row.
/// Pure mapping lives in [`crate::triage::extract::banking::apply_result`]; this
/// is what the store persists: a `banking` row upsert PLUS the extractor marker
/// and (for records) an auto-resolve of the triage row.
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
    /// the row. NOTE: unlike banking, this write does NOT resolve the triage
    /// row — see the extractor's module header.
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
    /// The extractor model id to stamp `triage.extractor_model_used` with (marks
    /// the row done with extraction so the queue no longer selects it).
    pub extractor_model_used: String,
    /// `true` => also resolve the triage row to `status='done'` (banking
    /// statements/alerts are RECORDS and must leave the attention bands). Invoice
    /// rows are never handled by this extractor, so they are never auto-resolved.
    pub auto_resolve: bool,
}

/// A day's Stage-2 API usage for one account, read from the `stage2_usage`
/// ledger. Cost is NOT stored — the human door computes `est_cost_usd_today`
/// from the config-driven per-MTok prices at read time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stage2Usage {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One day's Stage-2 usage row carrying its `day` key, returned by
/// [`Store::list_usage`] for the human-door usage history. Newest-first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stage2UsageDay {
    /// UTC date key, `YYYY-MM-DD`.
    pub day: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The runtime Stage-2 daily-cap overrides read from the `app_settings` table
/// (one row per set cap). `None` for a cap means no override row exists, so the
/// caller falls back to its config/env value (then the built-in default). Only
/// values that parse as an integer in `1..=100000` are surfaced; a malformed or
/// out-of-range stored value is treated as absent. Returned by
/// [`Store::stage2_cap_overrides`] in a single query so the Stage-2 pass can
/// re-read all three caps cheaply at the start of each cycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stage2CapOverrides {
    pub thread_daily_cap: Option<u32>,
    pub sender_daily_cap: Option<u32>,
    pub global_daily_cap: Option<u32>,
    /// The Stage-1 GLOBAL daily-cap override (Stage-1 has only a global cap).
    pub stage1_global_daily_cap: Option<u32>,
}

/// A NON-SEALED message that still needs an embedding vector, returned by
/// [`Store::messages_missing_vectors`] for the startup backfill pass. Carries
/// only the text the embedder consumes (subject + body).
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
/// SECURITY: every method that can feed the MCP surface
/// (`ranked_updates`, `thread_view`, `deadlines`) MUST exclude
/// `sensitivity = 'sealed'` in the SQL itself. `sealed_messages` is the sole
/// local-only escape hatch and is documented as TUI-only.
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
    /// SECURITY: the returned [`ThreadView`] carries text ONLY — never HTML. The
    /// html-bearing variant is [`Store::thread_view_with_html`], reachable ONLY
    /// from the human door. Keeping them as two methods returning two types is
    /// the structural guarantee that html never crosses /mcp.
    fn thread_view(&self, account_id: AccountId, thread_id: &str) -> Result<ThreadView>;

    /// Resolve a LOCAL MESSAGE id to its `thread_id`, for the `get_thread`
    /// forgiveness path (caller passed a message id where a thread id was
    /// expected). Returns `NotFound` when the id is unknown OR the message is
    /// sealed — sealed rows must never leak thread existence, so the two are
    /// indistinguishable exactly as in [`Store::thread_view`]. The returned
    /// thread id may still contain sealed messages; the caller re-runs the full
    /// sealed guard via `thread_view`, so this method does not itself vouch for
    /// the whole thread being unsealed.
    fn thread_id_for_message(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<String>>;

    /// HUMAN-DOOR-ONLY thread view: same sealed/nonexistent -> `NotFound`
    /// behavior as [`Store::thread_view`], but each message additionally carries
    /// its server-side-sanitized `html` (`None` when the mail was
    /// plain-text-only). Used solely by squelch-api `GET /client/thread/{id}`;
    /// MUST NOT be called from MCP, sync, or triage.
    fn thread_view_with_html(
        &self,
        account_id: AccountId,
        thread_id: &str,
    ) -> Result<crate::types::ClientThreadView>;

    /// HUMAN-DOOR-ONLY attachment byte fetch for `GET /client/attachments/{id}`.
    /// Resolves one attachment by its `attachments.id`, returning
    /// `(filename, mime, data)` where `data` is `None` when the bytes were not
    /// stored (the part was over the ingest cap — the endpoint answers 410).
    ///
    /// SECURITY: the query JOINs `triage` and requires the PARENT message's
    /// `sensitivity='normal'`, so an attachment on a sealed message is returned
    /// as `Ok(None)` — indistinguishable from an unknown id (both 404). Sealed
    /// attachment bytes therefore never leave the process through this door.
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

    /// Upsert a shipment keyed by `(account_id, tracking_number)`. A first sight
    /// inserts; a subsequent email about the same tracking number UPDATES the
    /// row via the no-regress status state machine (a delivered shipment is never
    /// walked back), refreshing `last_update`, `last_message_id`, and adopting a
    /// better (non-empty, longer) `item_name`. Returns the shipment row id.
    ///
    /// SECURITY: the caller runs this ONLY for non-sealed mail; the `shipments`
    /// table therefore holds no sealed rows by construction (no sealed join is
    /// needed on read).
    fn upsert_shipment(
        &self,
        account_id: AccountId,
        message_id: i64,
        shipment: &ShipmentInfo,
        seen_at: DateTime<Utc>,
    ) -> Result<i64>;

    /// List shipments for the account. When `include_delivered` is false, only
    /// en-route shipments (status != 'delivered') are returned; when true, all
    /// shipments including delivered ones. Ordered by `last_update` descending
    /// (most-recently-updated first). Sealed rows are structurally absent (never
    /// inserted), so no sealed filter is required.
    fn list_shipments(
        &self,
        account_id: AccountId,
        include_delivered: bool,
    ) -> Result<Vec<crate::types::Shipment>>;

    /// Upsert a receipt keyed by `(account_id, message_id)`. A first sight
    /// inserts; a re-ingest of the same message UPDATES the row (idempotent).
    /// Returns the receipt row id.
    ///
    /// SECURITY: the caller runs this ONLY for non-sealed mail; the `receipts`
    /// table therefore holds no sealed rows by construction (no sealed join is
    /// needed on read).
    fn upsert_receipt(
        &self,
        account_id: AccountId,
        message_id: i64,
        from_addr: &str,
        from_name: Option<&str>,
        receipt: &ReceiptInfo,
        received_at: DateTime<Utc>,
    ) -> Result<i64>;

    /// List receipts for the account received within the last `days`, newest
    /// first. Sealed rows are structurally absent (never inserted), so no sealed
    /// filter is required.
    fn list_receipts(&self, account_id: AccountId, days: u32) -> Result<Vec<Receipt>>;

    /// Upsert a calendar update keyed by `(account_id, message_id)`. A first
    /// sight inserts; a re-ingest of the same message UPDATES the row
    /// (idempotent). Returns the calendar row id.
    ///
    /// SECURITY: the caller runs this ONLY for non-sealed mail; the
    /// `calendar_updates` table therefore holds no sealed rows by construction
    /// (no sealed join is needed on read).
    fn upsert_calendar_update(
        &self,
        account_id: AccountId,
        message_id: i64,
        calendar: &CalendarInfo,
        received_at: DateTime<Utc>,
    ) -> Result<i64>;

    /// List calendar updates for the account RECEIVED within the last `hours`
    /// (mail arrival window, NOT event start time), newest-received first,
    /// each carrying its message's `thread_id` (joined) so the client can open
    /// the mail. Sealed rows are structurally absent (never inserted), so no
    /// sealed filter is required.
    fn list_calendar_updates(
        &self,
        account_id: AccountId,
        hours: u32,
    ) -> Result<Vec<CalendarUpdate>>;

    // ---------------------------------------------------------------------
    // SPECIALIST EXTRACTORS (categorize-then-extract). The stage-1/stage-2 LLM
    // assigns a `category`; a category with a registered extractor queues the
    // row for a structured second pass. Sealed rows carry `category=NULL` and
    // are structurally excluded from every extractor queue.
    // ---------------------------------------------------------------------

    /// Fetch up to `limit` rows queued for a specialist extractor: NON-SEALED,
    /// non-sent rows whose `category` is in `categories` (the set of categories
    /// with a registered extractor) and whose `extractor_model_used IS NULL`.
    /// Rows that already produced a RECEIPT are excluded (a receipt and a banking
    /// row must never double-create). Newest-first. Sealed rows are excluded in
    /// SQL.
    fn extract_queue(
        &self,
        account_id: AccountId,
        categories: &[&str],
        limit: usize,
    ) -> Result<Vec<ExtractQueued>>;

    /// DEV RE-TRIAGE: reset the LLM markers on non-sealed, non-sent inbound rows
    /// so they re-enter the Stage-1 queue (and re-escalate / re-extract from
    /// scratch): `stage1_model_used=NULL`, `model_used=NULL`, `needs_stage2=0`,
    /// `extractor_model_used=NULL`, plus stale `banking` rows for the affected
    /// messages are deleted (extraction recreates them). Rows a sender rule
    /// decided (`stage1_model_used='rule'`) and sealed/sent rows (`'n/a'`) are
    /// NEVER touched — rules are authoritative and sealed mail re-enters no
    /// queue. `message_id=None` scopes to inbound mail received in the trailing
    /// `days`; `Some(id)` scopes to that one message. Returns rows reset.
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

    /// Apply a parsed banking extraction onto the store IN ONE TRANSACTION:
    /// upsert the `banking` row (keyed by `(account_id, message_id)`), stamp
    /// `triage.extractor_model_used` (leaving the extract queue), and — when
    /// `auto_resolve` is set — resolve the triage row to `status='done'`
    /// (stamping `resolved_at`) so the record leaves the attention bands. Guarded
    /// by `sensitivity='normal'`. Returns the banking row id.
    fn banking_apply(&self, applied: &BankingApplied) -> Result<i64>;

    /// List banking records for the account, newest-received first. Sealed rows
    /// are structurally absent (never inserted), so no sealed filter is required.
    fn list_banking(&self, account_id: AccountId) -> Result<Vec<Banking>>;

    /// Upsert a sender rule. Returns the rule id.
    ///
    /// Rejects (`InvalidInput`) a `Filtered` rule whose `want_text` is empty or
    /// whitespace: the want text IS a filtered rule's instruction, and without
    /// one the rule silently degrades (Stage-2 receives nothing to evaluate).
    /// The same validation applies to [`Store::set_sender_rule_audited`] and
    /// [`Store::update_sender_rule`].
    fn set_sender_rule(
        &self,
        account_id: AccountId,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<i64>;

    /// AGENT-DOOR upsert: identical to [`Store::set_sender_rule`] but appends the
    /// given audit row IN THE SAME TRANSACTION as the rule write. FAIL-CLOSED: if
    /// the audit insert fails, the whole transaction rolls back and the rule write
    /// is NOT committed — an untrusted-adjacent agent write must never land
    /// untraced. Returns the rule id. `entry.action`/`actor`/`target`/`detail` are
    /// written verbatim (the MCP door supplies actor="agent", action="rule.set").
    fn set_sender_rule_audited(
        &self,
        account_id: AccountId,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
        audit: &NewAuditEntry,
    ) -> Result<i64>;

    fn list_sender_rules(&self, account_id: AccountId) -> Result<Vec<SenderRule>>;

    /// Update an existing sender rule by id (scoped to `account_id`): overwrite
    /// `match_pattern`, `want_text`, and `disposition`, restamping `updated_at`.
    /// Returns whether a row was updated (`false` => unknown id => the caller
    /// returns 404). Mirrors [`Store::set_sender_rule`]'s shapes but keys on id
    /// so the desktop's old delete+recreate dance is unnecessary.
    fn update_sender_rule(
        &self,
        account_id: AccountId,
        id: i64,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<bool>;

    /// Atomically store a message plus its triage (and any deadline) in ONE
    /// transaction. This is the ONLY ingest path the sync engine uses so that a
    /// sealed classification is committed in the same transaction as the row it
    /// seals — there is no window where a sealed message is queryable as normal
    /// mail. Returns the local message id.
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

    // ---------------------------------------------------------------------
    // HUMAN-DOOR additions (squelch-api /client/*). These MUST NOT be called
    // from MCP, sync, or triage. `search` still excludes sealed rows; the
    // sealed_* / audit methods are the human door's privileged surface.
    // ---------------------------------------------------------------------

    /// HUMAN-DOOR-ONLY: ranked updates carrying attention-lifecycle fields
    /// (`status`/`surfaced_at`/`resolved_at`) for the sitrep chassis. Sealed rows
    /// are excluded in SQL exactly like [`Store::ranked_updates`].
    ///
    /// `since`/`min_importance` behave as in `ranked_updates`. `status` filters
    /// to a single lifecycle value. `band` applies a server-side sitrep bucket
    /// (see [`SitrepBand`]). The returned `surfaced_at` is the PRE-stamp value —
    /// this method never mutates the ledger; the caller stamps with
    /// [`Store::mark_surfaced`] AFTER the serialization set is computed.
    fn attention_updates(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        min_importance: Option<u8>,
        status: Option<AttentionStatus>,
        band: Option<SitrepBand>,
    ) -> Result<Vec<AttentionUpdate>>;

    /// SEEN-LEDGER stamp. For each non-sealed message id: set `surfaced_at=now`
    /// only if currently NULL, and promote `status` `new`->`open`. Applied in ONE
    /// transaction after a read door has computed the rows it is about to return.
    /// Sealed rows are never affected (`sensitivity != 'sealed'` guard in SQL),
    /// upholding "sealed never surfaces through any of this". Returns the count of
    /// rows whose `surfaced_at` transitioned from NULL (i.e. first-surface count).
    fn mark_surfaced(&self, account_id: AccountId, message_ids: &[i64]) -> Result<usize>;

    /// Set the attention status of one message's triage row. `Done` stamps
    /// `resolved_at=now`; `Open`/`New` clear it. Sealed rows are excluded in SQL
    /// (returns `false` for a missing OR sealed message, keeping them
    /// indistinguishable). Returns whether a row was updated.
    fn set_attention_status(
        &self,
        account_id: AccountId,
        message_id: i64,
        status: AttentionStatus,
    ) -> Result<bool>;

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

    /// HUMAN-DOOR-ONLY: fetch the full body of exactly one sealed message.
    /// Reachable only from the squelch-api reveal endpoint, which appends an
    /// audit row (see [`Store::append_audit`]) BEFORE calling this. Returns
    /// `NotFound` if the message does not exist or is not sealed. Never cached.
    fn sealed_body(&self, account_id: AccountId, message_id: i64) -> Result<SealedBody>;

    /// Append a row to the human-door audit log. Returns the new row id.
    fn append_audit(&self, account_id: AccountId, entry: &NewAuditEntry) -> Result<i64>;

    // ---------------------------------------------------------------------
    // NOTIFICATION EVENTS. The durable, monotonic log the delivery adapters
    // read (SSE today, an APNs pusher later). WRITTEN ONLY BY THE SYNC ENGINE
    // via `triage::events` — never from a store write method, because only the
    // engine knows which sync path it is on and whether the mail is fresh.
    // Every reader carries its OWN cursor; there is deliberately no global
    // 'delivered' flag. See the `events` block in schema.sql.
    // ---------------------------------------------------------------------

    /// Append one event, at most once per message ever (INSERT OR IGNORE on the
    /// `UNIQUE(message_id)` key). Returns the new id, or `None` when the message
    /// already has an event — which is what makes re-ingest and the Stage-1/
    /// Stage-2 refine passes idempotent.
    ///
    /// On a real insert the implementation ALSO pokes the in-process broadcast
    /// (see [`SqliteStore::attach_event_notifier`]) so an SSE reader wakes
    /// without polling. That send is best-effort: no receivers is normal.
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

    // ---------------------------------------------------------------------
    // UNSUBSCRIBE (human door). All four are `/client/*`-only. Sealed mail is
    // invisible: `message_unsub_fields` excludes sealed rows in SQL so an
    // unsubscribe against a sealed message is `None` (=> 404) exactly like an
    // unknown id.
    // ---------------------------------------------------------------------

    /// Load the stored unsubscribe fields for a NON-SEALED message. Returns
    /// `None` for a missing OR sealed message (indistinguishable), so the caller
    /// maps `None` to 404. Never returns sealed content.
    fn message_unsub_fields(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<MessageUnsub>>;

    /// DEV DEBUG: the full triage row for one NON-SEALED message — every model
    /// marker, verdict, and reason, for the developer-mode triage inspector.
    /// `None` for missing OR sealed (indistinguishable). Human door only.
    fn triage_debug(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<TriageDebug>>;

    /// Upsert the unsubscribe row for `(account_id, sender)`. On conflict this
    /// RESETS the violation ledger: `requested_at`/`method`/`source_message_id`
    /// are overwritten and `violation_count -> 0`, `last_violation_at -> NULL`,
    /// `resolution -> NULL` (the user re-asked; the 72h grace clock restarts).
    /// `sender` is stored verbatim — the caller passes it already lowercased.
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

    /// Set the `resolution` (blocked|dismissed) on an existing unsubscribe row.
    /// Returns `false` when no row exists for that sender (=> caller returns
    /// 404). Resolving disarms the violation detector for that sender.
    fn set_unsubscribe_resolution(
        &self,
        account_id: AccountId,
        sender: &str,
        resolution: &str,
    ) -> Result<bool>;

    /// Upsert one extracted promotion + stamp `triage.extractor_model_used`.
    /// Unlike [`Store::banking_apply`] this does NOT resolve the triage row; see
    /// the marketing extractor's module header for why.
    fn marketing_apply(&self, applied: &MarketingApplied) -> Result<()>;

    /// Extracted promotions, newest first, received within the last `days`,
    /// capped at `limit`. Structurally sealed-free (see the `marketing` block
    /// in schema.sql).
    fn marketing_offers(
        &self,
        account_id: AccountId,
        days: u32,
        limit: u32,
    ) -> Result<Vec<MarketingOffer>>;

    /// Apply a human's triage correction and record it as feedback, in ONE
    /// transaction — the training row and the state it describes must never
    /// disagree.
    ///
    /// Two effects. The triage row's `axis` column is set to `to_value`, and the
    /// row is stamped as human-decided (`stage1_model_used`/`model_used` =
    /// 'human', `needs_stage2` = 0) so the LLM queue predicates skip it and a
    /// later pass cannot silently overwrite the human. And a `triage_feedback`
    /// row is appended carrying the full prior snapshot.
    ///
    /// Returns `None` when the message does not exist for this account. The
    /// caller validates `to_value` against [`TriageAxis::allowed`] first.
    fn correct_triage(
        &self,
        account_id: AccountId,
        message_id: i64,
        axis: TriageAxis,
        to_value: &str,
        note: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<TriageFeedback>>;

    /// Recorded corrections, newest first, capped at `limit`. This is the
    /// refinement dataset — read it to see where triage actually goes wrong.
    fn list_triage_feedback(
        &self,
        account_id: AccountId,
        limit: u32,
    ) -> Result<Vec<TriageFeedback>>;

    // ---------------------------------------------------------------------
    // AUTH-MAIL SHREDDER (retention). See the `shred_log` block in schema.sql
    // for the policy and why this is human-door-only.
    // ---------------------------------------------------------------------

    /// Auth mail (`triage.sensitivity = 'sealed'` — the same set the Auth page
    /// lists) received at or before `cutoff` and not already in `shred_log`,
    /// oldest first, capped at `limit`. Rows without a `gmail_msg_id` are
    /// skipped: there is nothing for the trash call to address.
    fn shred_candidates(
        &self,
        account_id: AccountId,
        cutoff: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ShredCandidate>>;

    /// How many candidates [`Store::shred_candidates`] would return, unbounded.
    /// Drives the "N waiting" figure without materializing the rows.
    fn shred_pending_count(&self, account_id: AccountId, cutoff: DateTime<Utc>) -> Result<i64>;

    /// Record one shredded message. Called ONLY after Gmail has confirmed the
    /// trash, so the ledger never claims a deletion that did not happen. The
    /// UNIQUE(account_id, message_id) constraint makes a re-run a no-op.
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

    /// Read the most recent audit rows (newest first), capped at `limit`. Each row
    /// is enriched with `target_sender`/`target_subject` when its `target` parses
    /// as a message id that exists for the account (otherwise those are `None`).
    fn list_audit(&self, account_id: AccountId, limit: u32) -> Result<Vec<AuditEntry>>;

    /// Per-tier / sealed / sync-cursor summary counts for the account.
    fn stats(&self, account_id: AccountId) -> Result<StoreStats>;

    // ---------------------------------------------------------------------
    // STAGE-2 additions. These support the LLM triage pass in the sync loop.
    // The queue predicate is `model_used IS NULL AND sensitivity='normal'`;
    // sealed rows are structurally excluded (never `model_used IS NULL AND
    // sensitivity='normal'` — sealed rows carry sensitivity='sealed').
    // ---------------------------------------------------------------------

    /// Fetch up to `limit` rows queued for the Stage-1 LLM refine pass
    /// (`stage1_model_used IS NULL AND sensitivity='normal' AND is_sent=0`).
    /// Rows decided by an explicit sender rule (or a Filtered rule that skips to
    /// Stage-2) carry a non-NULL `stage1_model_used` and are excluded. Newest
    /// first. Sealed rows are excluded in SQL.
    fn stage1_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage1Queued>>;

    /// Apply a parsed Stage-1 LLM result onto a triage row IN ONE TRANSACTION:
    /// overwrite importance/tier/one_line/reason/field_reasons, stamp
    /// `stage1_model_used` (leaving the Stage-1 queue), set `needs_stage2`, and
    /// (re)write the message's `deadlines` row. Guarded by `sensitivity='normal'`.
    /// Does NOT touch `model_used` (the Stage-2 marker).
    ///
    /// Returns whether the guarded UPDATE matched a row. `false` means the row
    /// was sealed BY HAND between the queue SELECT and this apply (TOCTOU):
    /// nothing was written — not even the deadline — and the caller must treat
    /// the verdict as NOT landed (in particular: no notification event).
    fn stage1_apply(&self, applied: &Stage1Applied) -> Result<bool>;

    /// Mark a Stage-1-queued row PROCESSED without changing its heuristic seed
    /// values — stamp `stage1_model_used` only, PRESERVING the `needs_stage2`
    /// seed written at ingest (`= !heuristic-confident`). Used on the
    /// heuristic-only fallback (API down / refusal / permanent error): the row
    /// keeps its seed values and the seed's own confidence decides escalation.
    /// Guarded by `sensitivity='normal'`.
    fn stage1_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        stage1_model_used: &str,
    ) -> Result<()>;

    /// Bump the Stage-1 usage ledger for `(account_id, day)`: +1 call and add the
    /// response's input/output token counts. Its own category, separate from
    /// Stage-2's ledger.
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

    /// Bump a SPECIALIST-EXTRACTOR usage ledger line for `(account_id, day,
    /// category)`: +1 call and add the response's input/output token counts.
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

    /// Fetch up to `limit` queued Stage-2 rows (Stage-1 escalated them:
    /// `stage1_model_used IS NOT NULL AND needs_stage2=1 AND model_used IS NULL
    /// AND sensitivity='normal'`) with their message context and, when a Filtered
    /// sender rule matched, that rule's `want_text`. Ordered newest-first so the
    /// freshest ambiguous mail gets a look first. Sealed rows are excluded in SQL.
    fn stage2_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage2Queued>>;

    /// Read today's Stage-2 API-call count for a budget scope. `thread_id` is
    /// either a real Gmail thread id (per-thread cap) or the `'__global__'`
    /// sentinel (per-account cap). `day` is the caller-provided UTC date key
    /// (e.g. `2026-07-09`) so tests are deterministic.
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
    /// overwrite importance/tier/one_line/reason, stamp `model_used` (marking
    /// the row processed so it leaves the queue), and (re)write the message's
    /// `deadlines` row when the model extracted a deadline. Never touches sealed
    /// rows (guarded by `sensitivity='normal'` in the UPDATE).
    ///
    /// Returns whether the guarded UPDATE matched a row — `false` is the
    /// sealed-mid-pass TOCTOU case, exactly as on [`Store::stage1_apply`]:
    /// nothing was written and the caller must not emit for it.
    fn stage2_apply(&self, applied: &Stage2Applied) -> Result<bool>;

    /// Mark a queued row PROCESSED without changing its Stage-1 values — stamp
    /// `model_used` only. Used when the model refused (keep Stage-1 output) or a
    /// permanent (non-retryable) API error was hit, so the row does not loop
    /// forever. Guarded by `sensitivity='normal'`.
    fn stage2_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        model_used: &str,
    ) -> Result<()>;

    /// Bump the Stage-2 usage ledger for `(account_id, day)`: +1 call and add the
    /// response's input/output token counts. Upserts the `stage2_usage` row.
    /// Called after each successful classify that carried a usage block. `day` is
    /// the caller-provided UTC date key (e.g. `2026-07-09`) for determinism.
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

    /// Stage-2 usage history for `account_id`: the most recent `days` rows from
    /// the `stage2_usage` ledger, newest-first. Only days that actually have a
    /// row are returned (sparse — no zero-filling). `days` caps the row count.
    fn list_usage(&self, account_id: AccountId, days: u32) -> Result<Vec<Stage2UsageDay>>;

    /// Sum the Stage-2 usage ledger for `account_id` over every day `>= since_day`
    /// (a `YYYY-MM-DD` UTC key; lexical compare is correct for that format).
    /// Zeroed when no rows fall in the window. Drives the trailing-window
    /// averages on `/client/triage-config`.
    fn stage2_usage_since(&self, account_id: AccountId, since_day: &str) -> Result<Stage2Usage>;

    // ---------------------------------------------------------------------
    // RUNTIME APP SETTINGS (human door). A tiny per-account key/value store for
    // operator knobs a client can change at runtime without editing config or
    // restarting — currently the Stage-2 daily-cap overrides. The Stage-2 pass
    // re-reads the overrides at the START of each cycle so a change applies
    // without a restart. Precedence: override > config/env > default.
    // ---------------------------------------------------------------------

    /// Read one `app_settings` value for `(account_id, key)`, or `None` if unset.
    fn get_app_setting(&self, account_id: AccountId, key: &str) -> Result<Option<String>>;

    /// Upsert one `app_settings` value for `(account_id, key)`.
    fn set_app_setting(&self, account_id: AccountId, key: &str, value: &str) -> Result<()>;

    /// Read the three Stage-2 daily-cap overrides in ONE query. A cap is `None`
    /// when no row exists OR the stored value does not parse as an integer in
    /// `1..=100000` (a malformed value is ignored, not surfaced). The Stage-2
    /// pass calls this once per cycle; the human door uses it to report sources.
    fn stage2_cap_overrides(&self, account_id: AccountId) -> Result<Stage2CapOverrides>;

    /// Count NON-SENT (`is_sent=0`) messages received at or after `since`. Feeds
    /// the `avg_inbound_per_day` figure on `/client/triage-config`.
    fn count_inbound_since(&self, account_id: AccountId, since: DateTime<Utc>) -> Result<u64>;

    // ---------------------------------------------------------------------
    // SEMANTIC RECALL (v1) vector-index writes. The embedder itself lives in
    // the caller (sync engine), so these take a precomputed vector / return the
    // text to embed — they never touch a model. QUERY-side methods
    // (`semantic_search`/`hybrid_search`) are inherent on `SqliteStore` because
    // they need the attached embedder.
    //
    // SECURITY: SEALED MESSAGES ARE NEVER EMBEDDED. `upsert_message_vector`'s
    // only callers gate on `sensitivity='normal'`, and
    // `messages_missing_vectors` selects ONLY normal rows, so sealed content is
    // structurally absent from the vector space.
    // ---------------------------------------------------------------------

    /// Insert (or replace) the embedding vector for one message. `embedding.len()`
    /// MUST equal the vec0 table width (384). CALLER MUST ensure the message is
    /// non-sealed; this does not re-check (ingest/backfill gate structurally).
    /// Idempotent — re-embedding overwrites.
    fn upsert_message_vector(
        &self,
        account_id: AccountId,
        message_id: i64,
        embedding: &[f32],
    ) -> Result<()>;

    /// Fetch up to `limit` NON-SEALED messages that have no vector yet (subject +
    /// body to embed). Drives the startup backfill pass (pre-existing rows +
    /// ingest-time embed failures). Sealed rows are excluded in SQL. Newest-first.
    fn messages_missing_vectors(
        &self,
        account_id: AccountId,
        limit: usize,
    ) -> Result<Vec<MissingVector>>;

    /// The currently-attached embedder, if any. Lets the sync engine resolve a
    /// LATE-attached embedder (e.g. one attached in the background after
    /// `squelchd serve` binds its port) without holding a second handle. Default
    /// `None` for stores that don't wire semantic recall.
    fn embedder(&self) -> Option<std::sync::Arc<dyn crate::embed::Embedder>> {
        None
    }
}
