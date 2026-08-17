//! SQLite-backed [`Store`] implementation.
//!
//! rusqlite is synchronous, so the `Connection` is wrapped in a `Mutex` and the
//! trait is implemented synchronously. See `store/mod.rs` for rationale.
//!
//! This file holds the struct, the open/migrate/attach glue and the single
//! [`Store`] impl block; the query bodies live in the subject modules declared
//! below, as inherent methods that block delegates to. A trait impl cannot be
//! split across files, which is why the delegations exist.

mod attention;
mod audit;
mod contacts;
pub mod device_tokens;
mod drafts;
mod events;
mod feedback;
mod messages;
mod migrate;
mod rules;
mod search;
mod specialists;
#[cfg(test)]
mod tests;
mod tracking;
mod triage_stages;

use self::migrate::migrate;

use std::path::Path;
use std::sync::{Mutex, Once, RwLock};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{CoreError, Result};
use crate::store::{
    AttachmentBytes, BankingApplied, ContactEntry, Device, DeviceToken, Draft, ExtractQueued,
    InboxUnread, IssuedDeviceToken, MarketingApplied, MarketingOffer, MessageOpen, MessageUnsub,
    MintedPairingCode, MissingVector, NewAuditEntry, NewEvent, RevisitQueued, SealedBody,
    SealedMessage, SearchFilter, SenderHistory, SentMessage, SentMissingRecipients, SitrepBand,
    Stage1Applied, Stage1Queued, Stage2Applied, Stage2CapOverrides, Stage2Queued, Stage2Usage,
    Stage2UsageDay, Store, SyncState, ThreadSibling, TrackedMessage, TriageDebug, TriagedMessage,
    UsageTokens,
};
use crate::types::{
    AccountId, AttachmentInfo, AttentionStatus, AttentionUpdate, AuditEntry, BandCounts, Banking,
    CalendarUpdate, ClientAttachment, ClientMessage, ClientThreadView, Deadline, Disposition,
    Event, EventKind, NewMessage, Receipt, SanitizedMessage, SearchHit, SenderRule, Sensitivity,
    ShredCandidate, StoreStats, ThreadView, Tier, TriageAxis, TriageFeedback, UnsubscribeRecord,
    Update,
};

// schema.sql stays beside `store/mod.rs`; this file is `store/sqlite/mod.rs`.
const SCHEMA: &str = include_str!("../schema.sql");

/// The embedding dimension declared by the `message_vecs` vec0 table
/// (`FLOAT[384]`). The schema literal and this constant must move together;
/// attaching an embedder asserts they match.
pub const VEC_DIMS: usize = 384;

/// How long a connection waits out another process's write lock before giving
/// up. Generous next to the single transaction a `squelchd token`/`pair` command
/// runs, and short enough that a genuinely wedged writer still surfaces as an
/// error instead of a hang.
const BUSY_TIMEOUT_MS: u64 = 5_000;

static VEC_EXT_INIT: Once = Once::new();

/// Register the statically-linked sqlite-vec (`vec0`) extension on SQLite's
/// auto-extension hook, so every later connection has the virtual table. Runs
/// once per process and MUST run before the schema, which creates
/// `message_vecs USING vec0(...)`.
fn register_vec_extension() {
    VEC_EXT_INIT.call_once(|| {
        // SAFETY: `sqlite3_vec_init` is the C entrypoint sqlite-vec statically
        // links; transmuting it to the auto-extension fn pointer type is the
        // documented rusqlite integration pattern, spelled out explicitly here
        // for clippy::missing_transmute_annotations.
        unsafe {
            let init: unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int =
                std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
            rusqlite::ffi::sqlite3_auto_extension(Some(init));
        }
    });
}

pub struct SqliteStore {
    conn: Mutex<Connection>,
    /// The on-box embedder: embeds query text for the vector searches and message
    /// bodies for callers. `None` when semantic recall is not wired — the vector
    /// methods then return [`CoreError::InvalidInput`] and hybrid search degrades
    /// to keyword-only. `RwLock` because it can be attached LATE, while the store
    /// is already shared behind an `Arc`.
    embedder: RwLock<Option<std::sync::Arc<dyn crate::embed::Embedder>>>,
    /// In-process wake signal for newly-appended `events` rows, attachable late
    /// like `embedder`.
    ///
    /// THE PAYLOAD IS ONLY A HINT — the `events` TABLE is the source of truth. A
    /// reader wakes on any id and re-reads every row past its own cursor, so a
    /// lagged or missed broadcast costs only latency, which is why send errors
    /// are ignored.
    event_tx: RwLock<Option<tokio::sync::broadcast::Sender<i64>>>,
}

impl SqliteStore {
    /// Open (or create) a store at `path`, applying the schema.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        register_vec_extension();
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an in-memory store (tests).
    pub fn open_in_memory() -> Result<Self> {
        register_vec_extension();
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    /// Put a fresh connection into the mode a SECOND PROCESS can share.
    ///
    /// `squelchd token issue`, `token revoke` and `pair` open this same file
    /// while `squelchd serve` holds it, so the store has real multi-process
    /// writers now, not just the daemon's one mutex:
    ///
    /// - WAL lets the CLI write while the daemon reads, instead of the rollback
    ///   journal's whole-file lock turning every mint into `database is locked`;
    /// - `busy_timeout` makes the two writers QUEUE for the few milliseconds a
    ///   token transaction takes rather than fail instantly on contention.
    ///
    /// Run on every open: `busy_timeout` is per-connection, and `journal_mode` is
    /// a persistent property of the FILE, so whichever process opens it first
    /// converts it and the rest simply confirm. The result is read and discarded
    /// because `:memory:` answers "memory" and there is nothing to assert there
    /// (the file-backed case is asserted in the store tests).
    ///
    /// A failure PROPAGATES rather than being swallowed. The conversion needs the
    /// file to itself, so it can lose to a process still holding the old mode
    /// open, and a store that quietly stayed on the rollback journal would
    /// reproduce the locking it is here to prevent with nothing to point at.
    fn set_pragmas(conn: &Connection) -> Result<()> {
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        Ok(())
    }

    fn init(conn: Connection) -> Result<Self> {
        Self::set_pragmas(&conn)?;
        conn.execute_batch(SCHEMA)?;
        // SCHEMA is all `CREATE TABLE IF NOT EXISTS`, so a pre-existing DB never
        // picks up freshly-added columns from the CREATE — `migrate` adds them.
        // New tables/indexes need no migration (IF NOT EXISTS covers them).
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            embedder: RwLock::new(None),
            event_tx: RwLock::new(None),
        })
    }

    /// Attach an [`Embedder`](crate::embed::Embedder) so the semantic-recall
    /// methods work. Fails loudly on a dimensionality mismatch with [`VEC_DIMS`],
    /// which would otherwise silently corrupt the index.
    pub fn with_embedder(
        self,
        embedder: std::sync::Arc<dyn crate::embed::Embedder>,
    ) -> Result<Self> {
        self.attach_embedder(embedder)?;
        Ok(self)
    }

    /// Swap in the embedder while the store may ALREADY be shared behind an `Arc`
    /// (`&self`): `squelchd serve` binds its HTTP port first and attaches in the
    /// background, so search runs keyword-only until this fires. Fails loudly on
    /// a [`VEC_DIMS`] mismatch. Returns the previous embedder.
    pub fn attach_embedder(
        &self,
        embedder: std::sync::Arc<dyn crate::embed::Embedder>,
    ) -> Result<Option<std::sync::Arc<dyn crate::embed::Embedder>>> {
        if embedder.dims() != VEC_DIMS {
            return Err(CoreError::InvalidInput(format!(
                "embedder dims {} != message_vecs vec0 width {VEC_DIMS}",
                embedder.dims()
            )));
        }
        let mut guard = self
            .embedder
            .write()
            .map_err(|_| CoreError::Other(anyhow::anyhow!("embedder lock poisoned")))?;
        Ok(guard.replace(embedder))
    }

    /// The attached embedder, if any — a cheap `Arc` clone.
    pub fn embedder(&self) -> Option<std::sync::Arc<dyn crate::embed::Embedder>> {
        self.embedder.read().ok().and_then(|g| g.clone())
    }

    /// Attach the in-process broadcast that every successful
    /// [`Store::append_event`] pokes with the new event id, late-attachable like
    /// [`SqliteStore::attach_embedder`]. Returns the previous sender.
    ///
    /// OPTIONAL and persists nothing: the `events` table is the source of truth,
    /// so a consumer that never attaches simply polls instead.
    pub fn attach_event_notifier(
        &self,
        tx: tokio::sync::broadcast::Sender<i64>,
    ) -> Result<Option<tokio::sync::broadcast::Sender<i64>>> {
        let mut guard = self
            .event_tx
            .write()
            .map_err(|_| CoreError::Other(anyhow::anyhow!("event notifier lock poisoned")))?;
        Ok(guard.replace(tx))
    }

    /// The attached event notifier, if any. Cheap clone of the sender.
    pub fn event_notifier(&self) -> Option<tokio::sync::broadcast::Sender<i64>> {
        self.event_tx.read().ok().and_then(|g| g.clone())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| CoreError::Other(anyhow::anyhow!("store mutex poisoned")))
    }

    /// Convenience for tests/other crates: create an account, return its id.
    pub fn ensure_account(&self, email: &str) -> Result<AccountId> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO accounts(email, created_at) VALUES(?1, ?2)
             ON CONFLICT(email) DO NOTHING",
            params![email, Utc::now().to_rfc3339()],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM accounts WHERE email = ?1",
            params![email],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// The account's own email address. [`CoreError::NotFound`] for an unknown
    /// id, like every other single-row getter here.
    pub fn account_email(&self, account_id: AccountId) -> Result<String> {
        let conn = self.lock()?;
        let email: Option<String> = conn
            .query_row(
                "SELECT email FROM accounts WHERE id = ?1",
                params![account_id],
                |r| r.get(0),
            )
            .optional()?;
        email.ok_or(CoreError::NotFound)
    }

    /// HUMAN-DOOR ACTION SUPPORT (squelch-api only): resolve a local message id
    /// to the Gmail ids + headers an action needs (archive/label/send).
    ///
    /// SECURITY: excludes `sensitivity = 'sealed'` in SQL, so an action can never
    /// target sealed mail — missing and sealed both return NotFound.
    pub fn action_message_ref(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<crate::store::ActionMessageRef> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT m.id, m.gmail_msg_id, m.thread_id, m.from_addr, m.from_name, m.subject
                 FROM messages m
                 JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2 AND t.sensitivity != 'sealed'",
                params![account_id, message_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let (id, gmail_msg_id, thread_id, from_addr, from_name, subject) =
            row.ok_or(CoreError::NotFound)?;
        Ok(crate::store::ActionMessageRef {
            id,
            account_id,
            gmail_msg_id,
            thread_id,
            from_addr,
            from_name,
            subject,
        })
    }

    /// Test/local helper: write a triage row for a message. Real triage is
    /// written by the triage pipeline; this keeps the store self-contained.
    #[allow(clippy::too_many_arguments)]
    pub fn set_triage(
        &self,
        message_id: i64,
        account_id: AccountId,
        importance: u8,
        tier: Tier,
        sensitivity: crate::types::Sensitivity,
        sealed_kind: Option<crate::types::SealedKind>,
        one_line: &str,
        reason: &str,
        deadline: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let conn = self.lock()?;
        // Rows created here are treated as Stage-1-finished and escalated
        // (`stage1_model_used='rule'`, `needs_stage2=1`) so they land in the
        // Stage-2 queue.
        conn.execute(
            "INSERT INTO triage(message_id, account_id, importance, tier, sensitivity,
                 sealed_kind, one_line, reason, deadline,
                 stage1_model_used, needs_stage2, created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'rule',1,?10)
             ON CONFLICT(message_id) DO UPDATE SET
                 importance=excluded.importance, tier=excluded.tier,
                 sensitivity=excluded.sensitivity, sealed_kind=excluded.sealed_kind,
                 one_line=excluded.one_line, reason=excluded.reason,
                 deadline=excluded.deadline",
            params![
                message_id,
                account_id,
                importance as i64,
                tier.as_str(),
                sensitivity.as_str(),
                sealed_kind.map(|k| k.as_str()),
                one_line,
                reason,
                deadline.map(|d| d.to_rfc3339()),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Test/local helper: set the per-property `field_reasons` JSON blob on an
    /// existing triage row. Real reasons are written by the triage pipeline;
    /// this lets tests seed the human-door insight column directly.
    pub fn set_field_reasons(
        &self,
        message_id: i64,
        account_id: AccountId,
        field_reasons: &crate::types::FieldReasons,
    ) -> Result<()> {
        let json = serde_json::to_string(field_reasons).ok();
        let conn = self.lock()?;
        conn.execute(
            "UPDATE triage SET field_reasons = ?3
             WHERE message_id = ?1 AND account_id = ?2",
            params![message_id, account_id, json],
        )?;
        Ok(())
    }

    /// Test/local helper: insert one attachment row directly (real ones come from
    /// [`Store::ingest_message`]), so tests can seed the byte-serving endpoint —
    /// including an over-cap `data == None` row — without an RFC822 fixture.
    pub fn insert_attachment(
        &self,
        account_id: AccountId,
        message_id: i64,
        filename: &str,
        mime: &str,
        size_bytes: i64,
        data: Option<&[u8]>,
    ) -> Result<i64> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO attachments(account_id, message_id, filename, mime, size_bytes, data)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![account_id, message_id, filename, mime, size_bytes, data],
        )?;
        Ok(conn.last_insert_rowid())
    }
}

/// Shared by every submodule: a stored RFC3339 timestamp back into a `DateTime`.
fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| CoreError::InvalidInput(format!("bad datetime {s:?}: {e}")))
}

/// Read column `i` as a stored RFC3339 timestamp INSIDE a rusqlite mapper, so a
/// list method can build its struct in one pass instead of yielding a tuple and
/// parsing after. A mapper cannot fail with a [`CoreError`], so a malformed
/// stored timestamp surfaces as `CoreError::Sqlite` rather than the
/// `CoreError::InvalidInput` [`parse_dt`] gives; both abort the same read.
fn dt(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<DateTime<Utc>> {
    let s: String = r.get(i)?;
    parse_col_dt(i, &s)
}

/// [`dt`] for a nullable column: SQL NULL is `None`, a present value must parse.
fn dt_opt(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    match r.get::<_, Option<String>>(i)? {
        Some(s) => parse_col_dt(i, &s).map(Some),
        None => Ok(None),
    }
}

fn parse_col_dt(i: usize, s: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
        })
}

/// Every method here is a one-line delegation to the inherent method of the
/// same name, which lives in the subject module named above. A trait impl
/// cannot be split across files, so the bodies moved and this block stayed.
impl Store for SqliteStore {
    fn upsert_message(&self, msg: &NewMessage) -> Result<i64> {
        self.upsert_message(msg)
    }

    fn ranked_updates(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        min_importance: Option<u8>,
    ) -> Result<Vec<Update>> {
        self.ranked_updates(account_id, since, min_importance)
    }

    fn thread_view(&self, account_id: AccountId, thread_id: &str) -> Result<ThreadView> {
        self.thread_view(account_id, thread_id)
    }

    fn thread_id_for_message(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<String>> {
        self.thread_id_for_message(account_id, message_id)
    }

    fn thread_view_with_html(
        &self,
        account_id: AccountId,
        thread_id: &str,
    ) -> Result<ClientThreadView> {
        self.thread_view_with_html(account_id, thread_id)
    }

    fn attachment_bytes(
        &self,
        account_id: AccountId,
        attachment_id: i64,
    ) -> Result<Option<AttachmentBytes>> {
        self.attachment_bytes(account_id, attachment_id)
    }

    fn deadlines(&self, account_id: AccountId, within_days: Option<u32>) -> Result<Vec<Deadline>> {
        self.deadlines(account_id, within_days)
    }

    fn upsert_shipment(
        &self,
        account_id: AccountId,
        message_id: i64,
        shipment: &crate::triage::ShipmentInfo,
        seen_at: DateTime<Utc>,
    ) -> Result<i64> {
        self.upsert_shipment(account_id, message_id, shipment, seen_at)
    }

    fn list_shipments(
        &self,
        account_id: AccountId,
        include_delivered: bool,
        policy: crate::config::ShipmentListPolicy,
    ) -> Result<Vec<crate::types::Shipment>> {
        self.list_shipments(account_id, include_delivered, policy)
    }

    fn clear_shipment(
        &self,
        account_id: AccountId,
        shipment_id: i64,
        at: DateTime<Utc>,
    ) -> Result<bool> {
        self.clear_shipment(account_id, shipment_id, at)
    }

    fn shipments_redetect_cleanup(&self, account_id: AccountId) -> Result<u64> {
        self.shipments_redetect_cleanup(account_id)
    }

    fn list_pollable_shipments(
        &self,
        account_id: AccountId,
        min_first_seen: DateTime<Utc>,
        max_failures: u32,
    ) -> Result<Vec<crate::types::Shipment>> {
        self.list_pollable_shipments(account_id, min_first_seen, max_failures)
    }

    fn apply_carrier_track(
        &self,
        account_id: AccountId,
        shipment_id: i64,
        track: &crate::triage::CarrierTrack,
        polled_at: DateTime<Utc>,
    ) -> Result<bool> {
        self.apply_carrier_track(account_id, shipment_id, track, polled_at)
    }

    fn record_poll_outcome(
        &self,
        account_id: AccountId,
        shipment_id: i64,
        polled_at: DateTime<Utc>,
        permanent_failure: bool,
    ) -> Result<()> {
        self.record_poll_outcome(account_id, shipment_id, polled_at, permanent_failure)
    }

    fn upsert_receipt(
        &self,
        account_id: AccountId,
        message_id: i64,
        from_addr: &str,
        from_name: Option<&str>,
        receipt: &crate::triage::ReceiptInfo,
        received_at: DateTime<Utc>,
    ) -> Result<i64> {
        self.upsert_receipt(
            account_id,
            message_id,
            from_addr,
            from_name,
            receipt,
            received_at,
        )
    }

    fn list_receipts(&self, account_id: AccountId, days: u32) -> Result<Vec<Receipt>> {
        self.list_receipts(account_id, days)
    }

    fn extract_queue(
        &self,
        account_id: AccountId,
        categories: &[&str],
        limit: usize,
    ) -> Result<Vec<ExtractQueued>> {
        self.extract_queue(account_id, categories, limit)
    }

    fn ship_extract_queue(
        &self,
        account_id: AccountId,
        limit: usize,
    ) -> Result<Vec<ExtractQueued>> {
        self.ship_extract_queue(account_id, limit)
    }

    fn ship_extract_mark(
        &self,
        account_id: AccountId,
        message_id: i64,
        marker: &str,
    ) -> Result<()> {
        self.ship_extract_mark(account_id, message_id, marker)
    }

    fn shipments_extract_apply(
        &self,
        applied: &crate::triage::extract::shipments::ShipmentsApplied,
    ) -> Result<bool> {
        self.shipments_extract_apply(applied)
    }

    fn retriage_reset(
        &self,
        account_id: AccountId,
        message_id: Option<i64>,
        days: u32,
    ) -> Result<u64> {
        self.retriage_reset(account_id, message_id, days)
    }

    fn extract_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        extractor_model_used: &str,
    ) -> Result<()> {
        self.extract_mark_processed(account_id, message_id, extractor_model_used)
    }

    fn banking_apply(&self, applied: &BankingApplied) -> Result<i64> {
        self.banking_apply(applied)
    }

    fn marketing_apply(&self, applied: &MarketingApplied) -> Result<()> {
        self.marketing_apply(applied)
    }

    fn marketing_offers(
        &self,
        account_id: AccountId,
        days: u32,
        limit: u32,
    ) -> Result<Vec<MarketingOffer>> {
        self.marketing_offers(account_id, days, limit)
    }

    fn list_banking(&self, account_id: AccountId) -> Result<Vec<Banking>> {
        self.list_banking(account_id)
    }

    fn upsert_calendar_update(
        &self,
        account_id: AccountId,
        message_id: i64,
        calendar: &crate::triage::CalendarInfo,
        received_at: DateTime<Utc>,
    ) -> Result<i64> {
        self.upsert_calendar_update(account_id, message_id, calendar, received_at)
    }

    fn list_calendar_updates(
        &self,
        account_id: AccountId,
        hours: u32,
    ) -> Result<Vec<CalendarUpdate>> {
        self.list_calendar_updates(account_id, hours)
    }

    fn set_sender_rule(
        &self,
        account_id: AccountId,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<i64> {
        self.set_sender_rule(account_id, match_pattern, want_text, disposition)
    }

    fn set_sender_rule_audited(
        &self,
        account_id: AccountId,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
        audit: &NewAuditEntry,
    ) -> Result<i64> {
        self.set_sender_rule_audited(account_id, match_pattern, want_text, disposition, audit)
    }

    fn update_sender_rule(
        &self,
        account_id: AccountId,
        id: i64,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<bool> {
        self.update_sender_rule(account_id, id, match_pattern, want_text, disposition)
    }

    fn list_sender_rules(&self, account_id: AccountId) -> Result<Vec<SenderRule>> {
        self.list_sender_rules(account_id)
    }

    fn ingest_message(&self, triaged: &TriagedMessage) -> Result<i64> {
        self.ingest_message(triaged)
    }

    fn is_known_contact(&self, account_id: AccountId, addr: &str) -> Result<bool> {
        self.is_known_contact(account_id, addr)
    }

    fn search_contacts(
        &self,
        account_id: AccountId,
        q: &str,
        limit: u32,
    ) -> Result<Vec<ContactEntry>> {
        self.search_contacts(account_id, q, limit)
    }

    fn merge_harvested_contacts(
        &self,
        account_id: AccountId,
        batch: &[ContactEntry],
    ) -> Result<()> {
        self.merge_harvested_contacts(account_id, batch)
    }

    fn sync_state(&self, account_id: AccountId, mailbox: &str) -> Result<Option<SyncState>> {
        self.sync_state(account_id, mailbox)
    }

    fn set_sync_state(
        &self,
        account_id: AccountId,
        mailbox: &str,
        state: &SyncState,
    ) -> Result<()> {
        self.set_sync_state(account_id, mailbox, state)
    }

    fn inbox_unread(&self, account_id: AccountId) -> Result<Option<InboxUnread>> {
        self.inbox_unread(account_id)
    }

    fn set_inbox_unread(&self, account_id: AccountId, messages: i64, threads: i64) -> Result<()> {
        self.set_inbox_unread(account_id, messages, threads)
    }

    fn sealed_messages(&self, account_id: AccountId) -> Result<Vec<SealedMessage>> {
        self.sealed_messages(account_id)
    }

    fn search(
        &self,
        account_id: AccountId,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>> {
        self.search(account_id, query, limit, offset)
    }

    fn search_filtered(
        &self,
        account_id: AccountId,
        text: &str,
        filter: &SearchFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>> {
        self.search_filtered(account_id, text, filter, limit, offset)
    }

    fn attention_updates(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        min_importance: Option<u8>,
        status: Option<AttentionStatus>,
        band: Option<SitrepBand>,
    ) -> Result<Vec<AttentionUpdate>> {
        self.attention_updates(account_id, since, min_importance, status, band)
    }

    fn mark_surfaced(&self, account_id: AccountId, message_ids: &[i64]) -> Result<usize> {
        self.mark_surfaced(account_id, message_ids)
    }

    fn set_attention_status(
        &self,
        account_id: AccountId,
        message_id: i64,
        status: AttentionStatus,
    ) -> Result<bool> {
        self.set_attention_status(account_id, message_id, status)
    }

    fn resolve_sender(&self, account_id: AccountId, sender_addr: &str) -> Result<usize> {
        self.resolve_sender(account_id, sender_addr)
    }

    fn delete_sender_rule(&self, account_id: AccountId, id: i64) -> Result<bool> {
        self.delete_sender_rule(account_id, id)
    }

    fn sealed_body(&self, account_id: AccountId, message_id: i64) -> Result<SealedBody> {
        self.sealed_body(account_id, message_id)
    }

    fn sent_listing(
        &self,
        account_id: AccountId,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SentMessage>> {
        self.sent_listing(account_id, limit, offset)
    }

    fn sent_missing_recipients(
        &self,
        account_id: AccountId,
        limit: u32,
    ) -> Result<Vec<SentMissingRecipients>> {
        self.sent_missing_recipients(account_id, limit)
    }

    fn set_message_to_addrs(
        &self,
        account_id: AccountId,
        message_id: i64,
        to_addrs: &str,
    ) -> Result<bool> {
        self.set_message_to_addrs(account_id, message_id, to_addrs)
    }

    fn append_audit(&self, account_id: AccountId, entry: &NewAuditEntry) -> Result<i64> {
        self.append_audit(account_id, entry)
    }

    fn append_event(&self, ev: &NewEvent) -> Result<Option<i64>> {
        self.append_event(ev)
    }

    fn events_after(
        &self,
        account_id: AccountId,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.events_after(account_id, after_id, limit)
    }

    fn event_by_id(&self, account_id: AccountId, id: i64) -> Result<Option<Event>> {
        self.event_by_id(account_id, id)
    }

    fn latest_event_id(&self, account_id: AccountId) -> Result<i64> {
        self.latest_event_id(account_id)
    }

    fn upsert_device(&self, account_id: AccountId, token: &str, platform: &str) -> Result<Device> {
        self.upsert_device(account_id, token, platform)
    }

    fn list_devices(&self, account_id: AccountId) -> Result<Vec<Device>> {
        self.list_devices(account_id)
    }

    fn delete_device_by_token(&self, account_id: AccountId, token: &str) -> Result<bool> {
        self.delete_device_by_token(account_id, token)
    }

    fn triage_debug(&self, account_id: AccountId, message_id: i64) -> Result<Option<TriageDebug>> {
        self.triage_debug(account_id, message_id)
    }

    fn message_unsub_fields(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<MessageUnsub>> {
        self.message_unsub_fields(account_id, message_id)
    }

    fn upsert_unsubscribe(
        &self,
        account_id: AccountId,
        sender: &str,
        method: &str,
        source_message_id: Option<i64>,
        requested_at: DateTime<Utc>,
    ) -> Result<()> {
        self.upsert_unsubscribe(account_id, sender, method, source_message_id, requested_at)
    }

    fn list_unsubscribes(&self, account_id: AccountId) -> Result<Vec<UnsubscribeRecord>> {
        self.list_unsubscribes(account_id)
    }

    fn set_unsubscribe_resolution(
        &self,
        account_id: AccountId,
        sender: &str,
        resolution: &str,
    ) -> Result<bool> {
        self.set_unsubscribe_resolution(account_id, sender, resolution)
    }

    fn correct_triage(
        &self,
        account_id: AccountId,
        message_id: i64,
        axis: TriageAxis,
        to_value: &str,
        note: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<TriageFeedback>> {
        self.correct_triage(account_id, message_id, axis, to_value, note, now)
    }

    fn list_triage_feedback(
        &self,
        account_id: AccountId,
        limit: u32,
    ) -> Result<Vec<TriageFeedback>> {
        self.list_triage_feedback(account_id, limit)
    }

    fn shred_candidates(
        &self,
        account_id: AccountId,
        cutoff: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ShredCandidate>> {
        self.shred_candidates(account_id, cutoff, limit)
    }

    fn shred_pending_count(&self, account_id: AccountId, cutoff: DateTime<Utc>) -> Result<i64> {
        self.shred_pending_count(account_id, cutoff)
    }

    fn record_shred(
        &self,
        account_id: AccountId,
        candidate: &ShredCandidate,
        shredded_at: DateTime<Utc>,
    ) -> Result<()> {
        self.record_shred(account_id, candidate, shredded_at)
    }

    fn shred_counts(
        &self,
        account_id: AccountId,
        recent_since: DateTime<Utc>,
    ) -> Result<(i64, i64, Option<DateTime<Utc>>)> {
        self.shred_counts(account_id, recent_since)
    }

    fn list_audit(&self, account_id: AccountId, limit: u32) -> Result<Vec<AuditEntry>> {
        self.list_audit(account_id, limit)
    }

    fn stats(&self, account_id: AccountId, bands_since: DateTime<Utc>) -> Result<StoreStats> {
        self.stats(account_id, bands_since)
    }

    fn stage1_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage1Queued>> {
        self.stage1_queue(account_id, limit)
    }

    fn stage1_apply(&self, applied: &Stage1Applied) -> Result<bool> {
        self.stage1_apply(applied)
    }

    fn stage1_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        stage1_model_used: &str,
    ) -> Result<()> {
        self.stage1_mark_processed(account_id, message_id, stage1_model_used)
    }

    fn revisits_schedule(
        &self,
        account_id: AccountId,
        message_id: i64,
        requests: &[crate::triage::revisit::RevisitRequest],
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.revisits_schedule(account_id, message_id, requests, now)
    }

    fn revisit_queue(
        &self,
        account_id: AccountId,
        now: DateTime<Utc>,
        max_lifetime: u32,
        limit: usize,
    ) -> Result<Vec<RevisitQueued>> {
        self.revisit_queue(account_id, now, max_lifetime, limit)
    }

    fn revisit_mark_fired(
        &self,
        account_id: AccountId,
        revisit_id: i64,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.revisit_mark_fired(account_id, revisit_id, now)
    }

    fn revisit_stale_standing(
        &self,
        account_id: AccountId,
        older_than: DateTime<Utc>,
        max_lifetime: u32,
        limit: usize,
    ) -> Result<Vec<i64>> {
        self.revisit_stale_standing(account_id, older_than, max_lifetime, limit)
    }

    fn revisit_apply(&self, applied: &Stage1Applied) -> Result<bool> {
        self.revisit_apply(applied)
    }

    fn stage1_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        tokens: UsageTokens,
    ) -> Result<()> {
        self.stage1_bump_usage(account_id, day, tokens)
    }

    fn stage1_usage_since(&self, account_id: AccountId, since_day: &str) -> Result<Stage2Usage> {
        self.stage1_usage_since(account_id, since_day)
    }

    #[allow(clippy::too_many_arguments)]
    fn extract_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        category: &str,
        tokens: UsageTokens,
    ) -> Result<()> {
        self.extract_bump_usage(account_id, day, category, tokens)
    }

    fn list_usage_stage1(&self, account_id: AccountId, days: u32) -> Result<Vec<Stage2UsageDay>> {
        self.list_usage_stage1(account_id, days)
    }

    fn list_usage_by_category(
        &self,
        account_id: AccountId,
        days: u32,
    ) -> Result<Vec<(String, Vec<Stage2UsageDay>)>> {
        self.list_usage_by_category(account_id, days)
    }

    fn stage2_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage2Queued>> {
        self.stage2_queue(account_id, limit)
    }

    fn stage2_budget_used(&self, account_id: AccountId, thread_id: &str, day: &str) -> Result<u32> {
        self.stage2_budget_used(account_id, thread_id, day)
    }

    fn stage2_increment_budget(
        &self,
        account_id: AccountId,
        thread_id: &str,
        day: &str,
    ) -> Result<u32> {
        self.stage2_increment_budget(account_id, thread_id, day)
    }

    fn stage2_apply(&self, applied: &Stage2Applied) -> Result<bool> {
        self.stage2_apply(applied)
    }

    fn stage2_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        model_used: &str,
    ) -> Result<()> {
        self.stage2_mark_processed(account_id, message_id, model_used)
    }

    fn stage2_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        tokens: UsageTokens,
    ) -> Result<()> {
        self.stage2_bump_usage(account_id, day, tokens)
    }

    fn stage2_usage_today(&self, account_id: AccountId, day: &str) -> Result<Stage2Usage> {
        self.stage2_usage_today(account_id, day)
    }

    fn list_usage(&self, account_id: AccountId, days: u32) -> Result<Vec<Stage2UsageDay>> {
        self.list_usage(account_id, days)
    }

    fn stage2_usage_since(&self, account_id: AccountId, since_day: &str) -> Result<Stage2Usage> {
        self.stage2_usage_since(account_id, since_day)
    }

    fn get_app_setting(&self, account_id: AccountId, key: &str) -> Result<Option<String>> {
        self.get_app_setting(account_id, key)
    }

    fn set_app_setting(&self, account_id: AccountId, key: &str, value: &str) -> Result<()> {
        self.set_app_setting(account_id, key, value)
    }

    fn stage2_cap_overrides(&self, account_id: AccountId) -> Result<Stage2CapOverrides> {
        self.stage2_cap_overrides(account_id)
    }

    fn insert_send_tracker(
        &self,
        account_id: AccountId,
        token: &str,
        message_id: Option<i64>,
        created_at: i64,
    ) -> Result<()> {
        self.insert_send_tracker(account_id, token, message_id, created_at)
    }

    fn set_send_tracker_message(
        &self,
        account_id: AccountId,
        token: &str,
        message_id: i64,
    ) -> Result<bool> {
        self.set_send_tracker_message(account_id, token, message_id)
    }

    fn record_open(
        &self,
        account_id: AccountId,
        token: &str,
        opened_at: i64,
        user_agent: Option<&str>,
        classification: &str,
    ) -> Result<bool> {
        self.record_open(account_id, token, opened_at, user_agent, classification)
    }

    fn message_opens(&self, account_id: AccountId, message_id: i64) -> Result<Vec<MessageOpen>> {
        self.message_opens(account_id, message_id)
    }

    fn tracked_message(
        &self,
        account_id: AccountId,
        token: &str,
    ) -> Result<Option<TrackedMessage>> {
        self.tracked_message(account_id, token)
    }

    fn count_inbound_since(&self, account_id: AccountId, since: DateTime<Utc>) -> Result<u64> {
        self.count_inbound_since(account_id, since)
    }

    fn upsert_message_vector(
        &self,
        account_id: AccountId,
        message_id: i64,
        embedding: &[f32],
    ) -> Result<()> {
        self.upsert_message_vector(account_id, message_id, embedding)
    }

    fn messages_missing_vectors(
        &self,
        account_id: AccountId,
        limit: usize,
    ) -> Result<Vec<MissingVector>> {
        self.messages_missing_vectors(account_id, limit)
    }

    /// Trait override so a generic `S: Store` caller resolves the CURRENT
    /// embedder, including one attached late in the background.
    fn embedder(&self) -> Option<std::sync::Arc<dyn crate::embed::Embedder>> {
        SqliteStore::embedder(self)
    }
}
