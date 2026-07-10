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
use crate::triage::{DeadlineHit, ReceiptInfo, ShipmentInfo};
use crate::types::{
    AccountId, AttentionStatus, AttentionUpdate, AuditEntry, Deadline, Disposition, NewMessage,
    Receipt, SealedKind, SearchHit, SenderRule, Sensitivity, StoreStats, ThreadView, Tier, Update,
};
use chrono::{DateTime, Utc};

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

/// A row to append to the human-door audit log.
#[derive(Debug, Clone)]
pub struct NewAuditEntry {
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<String>,
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
    /// The model id string to stamp `model_used` with (marks the row processed
    /// so the queue predicate no longer selects it).
    pub model_used: String,
    /// A deadline to (re)write for this message, if the model extracted one.
    pub deadline: Option<DeadlineHit>,
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

    /// Upsert a sender rule. Returns the rule id.
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

    /// Read the most recent audit rows (newest first), capped at `limit`.
    fn list_audit(&self, account_id: AccountId, limit: u32) -> Result<Vec<AuditEntry>>;

    /// Per-tier / sealed / sync-cursor summary counts for the account.
    fn stats(&self, account_id: AccountId) -> Result<StoreStats>;

    // ---------------------------------------------------------------------
    // STAGE-2 additions. These support the LLM triage pass in the sync loop.
    // The queue predicate is `model_used IS NULL AND sensitivity='normal'`;
    // sealed rows are structurally excluded (never `model_used IS NULL AND
    // sensitivity='normal'` — sealed rows carry sensitivity='sealed').
    // ---------------------------------------------------------------------

    /// Fetch up to `limit` queued Stage-2 rows (non-confident Stage-1 output:
    /// `model_used IS NULL AND sensitivity='normal'`) with their message
    /// context and, when a Filtered sender rule matched, that rule's
    /// `want_text`. Ordered newest-first so the freshest ambiguous mail gets a
    /// look first. Sealed rows are excluded in SQL.
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
    fn stage2_apply(&self, applied: &Stage2Applied) -> Result<()>;

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
