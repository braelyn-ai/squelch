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
use crate::triage::DeadlineHit;
use crate::types::{
    AccountId, Deadline, Disposition, NewMessage, SealedKind, SenderRule, Sensitivity, ThreadView,
    Tier, Update,
};
use chrono::{DateTime, Utc};

/// The IMAP sync cursor for one (account, mailbox). Persisted in `sync_state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncState {
    pub uidvalidity: u32,
    pub last_uid: u32,
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
    pub sensitivity: Sensitivity,
    pub sealed_kind: Option<SealedKind>,
    pub importance: u8,
    pub tier: Tier,
    pub one_line: String,
    pub reason: String,
    pub matched_rule: Option<i64>,
    /// The Stage-1 deadline hit, if any. Only ever `Some` for non-sealed mail.
    pub deadline: Option<DeadlineHit>,
    /// `false` when Stage-1 was not confident: the row is left with
    /// `model_used IS NULL` so the Stage-2 queue predicate
    /// (`model_used IS NULL AND sensitivity = 'normal'`) picks it up.
    pub confident: bool,
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
    fn thread_view(&self, account_id: AccountId, thread_id: &str) -> Result<ThreadView>;

    /// MCP-facing deadlines within `within_days` (None = all). Sealed excluded.
    fn deadlines(
        &self,
        account_id: AccountId,
        within_days: Option<u32>,
    ) -> Result<Vec<Deadline>>;

    /// Upsert a sender rule. Returns the rule id.
    fn set_sender_rule(
        &self,
        account_id: AccountId,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<i64>;

    fn list_sender_rules(&self, account_id: AccountId) -> Result<Vec<SenderRule>>;

    /// Atomically store a message plus its triage (and any deadline) in ONE
    /// transaction. This is the ONLY ingest path the sync engine uses so that a
    /// sealed classification is committed in the same transaction as the row it
    /// seals — there is no window where a sealed message is queryable as normal
    /// mail. Returns the local message id.
    fn ingest_message(&self, triaged: &TriagedMessage) -> Result<i64>;

    /// True if `addr` appears in this account's Sent-derived contacts (the
    /// "people I know" signal the sync engine feeds to Stage-1).
    fn is_known_contact(&self, account_id: AccountId, addr: &str) -> Result<bool>;

    /// Read the IMAP sync cursor for a mailbox, if one has been persisted.
    fn sync_state(&self, account_id: AccountId, mailbox: &str) -> Result<Option<SyncState>>;

    /// Upsert the IMAP sync cursor for a mailbox.
    fn set_sync_state(
        &self,
        account_id: AccountId,
        mailbox: &str,
        state: &SyncState,
    ) -> Result<()>;

    /// LOCAL-ONLY (TUI): list sealed messages. This is the ONLY method that
    /// exposes sealed content and must never be reachable from MCP.
    fn sealed_messages(&self, account_id: AccountId) -> Result<Vec<SealedMessage>>;
}
