//! SQLite-backed [`Store`] implementation.
//!
//! rusqlite is synchronous, so the `Connection` is wrapped in a `Mutex` and the
//! trait is implemented synchronously. See `store/mod.rs` for rationale.

use std::path::Path;
use std::sync::{Mutex, Once, RwLock};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use zerocopy::AsBytes;

use crate::error::{CoreError, Result};
use crate::store::{
    TriageDebug,
    AttachmentBytes, BankingApplied, ExtractQueued, MarketingApplied, MarketingOffer,
    Device, MessageUnsub, MissingVector, NewAuditEntry, NewEvent,
    SealedBody, SealedMessage, SitrepBand, Stage1Applied, Stage1Queued, Stage2Applied,
    Stage2CapOverrides, Stage2Queued, Stage2Usage, Stage2UsageDay, Store, SyncState, TriagedMessage,
};
use crate::types::{
    AccountId, AttachmentInfo, AttentionStatus, AttentionUpdate, AuditEntry, BandCounts, Banking,
    CalendarUpdate, ClientAttachment, ClientMessage, ClientThreadView, Deadline, Disposition,
    Event, EventKind, NewMessage, Receipt, SanitizedMessage, SearchHit, SenderRule, Sensitivity,
    ShredCandidate, StoreStats, ThreadView, Tier, TriageAxis, TriageFeedback, Update,
    UnsubscribeRecord,
};

const SCHEMA: &str = include_str!("schema.sql");

/// The embedding dimension declared by the `message_vecs` vec0 table
/// (`FLOAT[384]`). The schema literal and this constant must move together;
/// attaching an embedder asserts they match.
pub const VEC_DIMS: usize = 384;

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
            ) -> std::os::raw::c_int = std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            );
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

    fn init(conn: Connection) -> Result<Self> {
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
        self.embedder
            .read()
            .ok()
            .and_then(|g| g.clone())
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

    // ON-BOX SEMANTIC RECALL. Inherent methods rather than `Store` ones because
    // they need the attached [`Embedder`] and the sqlite-vec `message_vecs`
    // table, which not every `Store` impl carries.
    //
    // SECURITY: SEALED MESSAGES ARE NEVER EMBEDDED — the write callers gate on
    // `sensitivity='normal'`, so sealed text is structurally absent from the
    // vector space; query-time methods re-exclude sealed rows anyway.

    /// SEMANTIC RECALL: embed `query_text` and return the `k` nearest messages as
    /// `(message_id, distance)`, smaller = closer, scoped to `account_id`.
    ///
    /// SECURITY: the KNN hit set is re-joined to `triage` to drop sealed rows
    /// (they should never be indexed at all). BOTH `is_sent` values are INCLUDED
    /// — recall wants the user's own sent mail ("did I say I'd send X").
    pub fn semantic_search(
        &self,
        account_id: AccountId,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(i64, f32)>> {
        let embedder = self
            .embedder()
            .ok_or_else(|| CoreError::InvalidInput("no embedder attached".into()))?;
        let qvec = embedder.embed(query_text)?;
        self.knn_by_vector(account_id, &qvec, k)
    }

    /// Lower-level KNN used by [`semantic_search`] (and reused by
    /// [`hybrid_search`]): given an already-computed query vector, return the `k`
    /// nearest non-sealed messages for the account as `(message_id, distance)`.
    fn knn_by_vector(
        &self,
        account_id: AccountId,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(i64, f32)>> {
        if query.len() != VEC_DIMS {
            return Err(CoreError::InvalidInput(format!(
                "query embedding len {} != vec0 width {VEC_DIMS}",
                query.len()
            )));
        }
        let conn = self.lock()?;
        // vec0 KNN: MATCH the embedding, constrain by the account_id metadata
        // column, cap with `k = ?`, then re-join triage to drop any sealed row
        // that should never have been indexed in the first place.
        let mut stmt = conn.prepare(
            "SELECT v.message_id, v.distance
             FROM message_vecs v
             JOIN messages m ON m.id = v.message_id
             LEFT JOIN triage t ON t.message_id = v.message_id
             WHERE v.embedding MATCH ?1
               AND v.account_id = ?2
               AND v.k = ?3
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
             ORDER BY v.distance",
        )?;
        let rows = stmt.query_map(
            params![query.as_bytes(), account_id, k as i64],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)? as f32)),
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// HYBRID RECALL: merge FTS5 keyword rank and vector distance with Reciprocal
    /// Rank Fusion — each candidate scores `sum(1 / (rrf_k + rank))` across the
    /// lists it appears in, `rrf_k` being the standard smoothing constant (60).
    /// Keyword catches exact tokens, vectors catch paraphrase, and either list
    /// alone still produces results. Both exclude sealed rows and include sent
    /// mail (recall).
    pub fn hybrid_search(
        &self,
        account_id: AccountId,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<SearchHit>> {
        const RRF_K: f32 = 60.0;

        // No embedder (e.g. before the background attach) => keyword-only.
        let vec_hits: Vec<(i64, f32)> = match self.embedder() {
            Some(embedder) => {
                let qvec = embedder.embed(query_text)?;
                self.knn_by_vector(account_id, &qvec, k)?
            }
            None => Vec::new(),
        };

        // FTS ranks over the SAME query text, sent mail included.
        let fts_ids = self.fts_recall_ids(account_id, query_text, k)?;

        use std::collections::HashMap;
        let mut score: HashMap<i64, f32> = HashMap::new();
        for (rank, (id, _dist)) in vec_hits.iter().enumerate() {
            *score.entry(*id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
        for (rank, id) in fts_ids.iter().enumerate() {
            *score.entry(*id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
        }

        let mut ranked: Vec<(i64, f32)> = score.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(k);

        let mut out = Vec::with_capacity(ranked.len());
        for (id, _s) in ranked {
            if let Some(hit) = self.search_hit_by_id(account_id, id)? {
                out.push(hit);
            }
        }
        Ok(out)
    }

    /// SEMANTIC-ONLY recall as hydrated [`SearchHit`]s, best-first by distance,
    /// for the human door's `mode=semantic` search. Empty without an attached
    /// embedder. Sealed rows are excluded in SQL; sent mail is included (recall).
    pub fn semantic_search_hits(
        &self,
        account_id: AccountId,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<SearchHit>> {
        let ids = self.semantic_search(account_id, query_text, k)?;
        let mut out = Vec::with_capacity(ids.len());
        for (id, _dist) in ids {
            if let Some(hit) = self.search_hit_by_id(account_id, id)? {
                out.push(hit);
            }
        }
        Ok(out)
    }

    /// FTS5 recall helper for [`hybrid_search`]: message ids in rank order.
    /// Unlike [`Store::search`] it INCLUDES sent mail, because recall wants the
    /// user's own outbound mail. Sealed rows are excluded in SQL, and a malformed
    /// FTS query yields an empty list rather than an error.
    fn fts_recall_ids(&self, account_id: AccountId, query: &str, limit: usize) -> Result<Vec<i64>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT m.id
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?1
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
               AND messages_fts MATCH ?2
             ORDER BY rank
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, query, limit as i64], |r| {
            r.get::<_, i64>(0)
        });
        let rows = match rows {
            Ok(r) => r,
            // A syntactically-invalid MATCH expression => no keyword hits.
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(id) => out.push(id),
                Err(_) => return Ok(out),
            }
        }
        Ok(out)
    }

    /// Hydrate a single non-sealed message id into a [`SearchHit`] (sealed rows
    /// return `None`, keeping them absent from hybrid results).
    fn search_hit_by_id(&self, account_id: AccountId, id: i64) -> Result<Option<SearchHit>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT m.id, m.thread_id, m.from_addr, m.from_name, m.subject,
                        m.received_at, m.snippet
                 FROM messages m
                 LEFT JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2
                   AND COALESCE(t.sensitivity, 'normal') != 'sealed'",
                params![account_id, id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, thread_id, from_addr, from_name, subject, received_at, snippet)) = row else {
            return Ok(None);
        };
        Ok(Some(SearchHit {
            id,
            thread_id,
            from_addr,
            from_name,
            subject,
            received_at: parse_dt(&received_at)?,
            snippet,
        }))
    }
}


/// Add `column` (`decl` = type + constraints) to `table` unless it is already
/// there. There is no schema-version counter, so presence is detected via
/// `PRAGMA table_info`, which keeps this idempotent across opens.
///
/// Returns `true` ONLY on the call that actually adds the column, which is what
/// lets a caller run a one-time backfill on the open that introduces it — never
/// on a fresh DB (schema.sql already carries it) and never again after.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt.query_map([], |r| r.get::<_, String>(1))?;
    let mut present = false;
    let mut any = false;
    for c in cols {
        any = true;
        if c? == column {
            present = true;
            break;
        }
    }
    // An empty `table_info` means the table does not exist yet; skipping then
    // keeps this seam per-table independent for tests that build partial schemas.
    if any && !present {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), [])?;
        return Ok(true);
    }
    Ok(false)
}

/// Additive, idempotent column migrations for pre-existing DBs. New tables and
/// indexes are handled by `CREATE ... IF NOT EXISTS` in `schema.sql`; only new
/// COLUMNS on an existing table need this seam.
fn migrate(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "messages", "list_unsubscribe", "TEXT")?;
    add_column_if_missing(
        conn,
        "messages",
        "list_unsub_one_click",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // Per-property triage reasons (JSON object). NULL on pre-existing rows.
    add_column_if_missing(conn, "triage", "field_reasons", "TEXT")?;

    // Two-stage triage markers: `stage1_model_used` gates the Stage-1 LLM queue
    // (NULL == still needs Stage-1), `needs_stage2` is the escalation flag.
    let added_stage1 = add_column_if_missing(conn, "triage", "stage1_model_used", "TEXT")?;
    add_column_if_missing(conn, "triage", "needs_stage2", "INTEGER NOT NULL DEFAULT 0")?;

    // stage2_usage grew `category` INSIDE ITS PRIMARY KEY. ALTER ADD COLUMN
    // cannot change a PK, and the bump upsert's ON CONFLICT(account_id, day,
    // category) needs that exact unique index, so an old table must be REBUILT —
    // otherwise every ledger bump fails and LLM spend goes unrecorded.
    let usage_has_category: bool = {
        let mut stmt = conn.prepare("PRAGMA table_info(stage2_usage)")?;
        let cols = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        cols.is_empty() || cols.iter().any(|c| c == "category")
    };
    if !usage_has_category {
        conn.execute_batch(
            "ALTER TABLE stage2_usage RENAME TO stage2_usage_old;
             CREATE TABLE stage2_usage (
                 account_id    INTEGER NOT NULL,
                 day           TEXT NOT NULL,
                 category      TEXT NOT NULL DEFAULT 'stage2',
                 calls         INTEGER NOT NULL DEFAULT 0,
                 input_tokens  INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY(account_id, day, category)
             );
             INSERT INTO stage2_usage(account_id, day, category, calls, input_tokens, output_tokens)
                 SELECT account_id, day, 'stage2', calls, input_tokens, output_tokens
                 FROM stage2_usage_old;
             DROP TABLE stage2_usage_old;",
        )?;
    }

    // CATEGORIZE-THEN-EXTRACT markers. NULL is the correct resting value for
    // both (no category => never queued for extraction), so no backfill.
    add_column_if_missing(conn, "triage", "category", "TEXT")?;
    add_column_if_missing(conn, "triage", "extractor_model_used", "TEXT")?;

    // Adding `stage1_model_used` leaves it NULL on every historical row — exactly
    // the Stage-1 queue predicate — so without this backfill the whole mailbox
    // re-classifies through the paid model. Rows already classified or already
    // seen are marked 'migrated' to stay out of the queue; the residual
    // (`status='new' AND model_used IS NULL`) correctly does re-enter Stage-1,
    // whose apply recomputes `needs_stage2` from model confidence.
    //
    // Guarded by `added_stage1` so it fires exactly once, at introduction: a
    // later run would wrongly 'migrate' rows legitimately queued for Stage-1 that
    // a read door had promoted past 'new'.
    if added_stage1 {
        conn.execute(
            "UPDATE triage SET stage1_model_used = 'migrated'
             WHERE stage1_model_used IS NULL
               AND (model_used IS NOT NULL OR status != 'new')",
            [],
        )?;
    }

    // Guarded on table existence — migration unit tests build partial schemas.
    let cleanup_tables_exist: bool = {
        let mut stmt = conn.prepare(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
             AND name IN ('triage','deadlines','messages')",
        )?;
        let n: i64 = stmt.query_row([], |r| r.get(0))?;
        n == 3
    };
    if !cleanup_tables_exist {
        return Ok(());
    }
    // A model-sourced deadline more than 45 days BEFORE its own message's
    // receipt is a hallucinated year, so clear it (the mail stays surfaced, just
    // dateless), demote a stranded past_due tier, and drop the deadlines row.
    // Deterministic parses (source not stage1/stage2) are untouched — explicit
    // year text in an email is data. Idempotent: the predicate cannot re-match
    // once cleared.
    conn.execute(
        "UPDATE triage SET deadline = NULL,
                tier = CASE WHEN tier = 'past_due' THEN 'deadline' ELSE tier END
         WHERE deadline IS NOT NULL
           AND message_id IN (
             SELECT d.message_id FROM deadlines d
             JOIN messages m ON m.id = d.message_id
             WHERE d.source IN ('stage1', 'stage2')
               AND julianday(m.received_at) - julianday(d.due_at) > 45
           )",
        [],
    )?;
    conn.execute(
        "DELETE FROM deadlines
         WHERE source IN ('stage1', 'stage2')
           AND id IN (
             SELECT d.id FROM deadlines d
             JOIN messages m ON m.id = d.message_id
             WHERE julianday(m.received_at) - julianday(d.due_at) > 45
           )",
        [],
    )?;
    Ok(())
}

/// Apply the unsubscribe VIOLATION bump for a just-stored inbound message, in
/// the caller's transaction: an unresolved `unsubscribes` row for
/// `(account_id, lower(from_addr))` whose request is more than 72h older than
/// this `received_at` gets `violation_count + 1` and `last_violation_at`. No
/// row, already resolved, or still within grace is a silent no-op.
fn bump_unsub_violation_conn(
    conn: &Connection,
    account_id: AccountId,
    from_addr: &str,
    received_at: DateTime<Utc>,
) -> Result<()> {
    let sender = from_addr.trim().to_ascii_lowercase();
    if sender.is_empty() {
        return Ok(());
    }
    // Read the outstanding request so the grace comparison runs on real
    // timestamps in Rust rather than as lexical string math in SQL.
    let row: Option<String> = conn
        .query_row(
            "SELECT requested_at FROM unsubscribes
             WHERE account_id = ?1 AND sender_addr = ?2 AND resolution IS NULL",
            params![account_id, sender],
            |r| r.get(0),
        )
        .optional()?;
    let Some(requested_s) = row else {
        return Ok(());
    };
    let requested_at = parse_dt(&requested_s)?;
    if received_at > requested_at + chrono::Duration::hours(72) {
        conn.execute(
            "UPDATE unsubscribes
             SET violation_count = violation_count + 1, last_violation_at = ?3
             WHERE account_id = ?1 AND sender_addr = ?2 AND resolution IS NULL",
            params![account_id, sender, received_at.to_rfc3339()],
        )?;
    }
    Ok(())
}

/// Upsert a message + FTS + Sent-derived contacts against an explicit
/// connection/transaction handle. Shared by [`SqliteStore::upsert_message`] and
/// the transactional [`Store::ingest_message`] path so both stay in sync.
fn upsert_message_conn(conn: &Connection, msg: &NewMessage) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages(account_id, gmail_msg_id, thread_id, from_addr, from_name,
             subject, received_at, snippet, body, body_html, is_sent,
             list_unsubscribe, list_unsub_one_click)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(account_id, gmail_msg_id) DO UPDATE SET
             thread_id=excluded.thread_id, from_addr=excluded.from_addr,
             from_name=excluded.from_name, subject=excluded.subject,
             received_at=excluded.received_at, snippet=excluded.snippet,
             body=excluded.body, body_html=excluded.body_html, is_sent=excluded.is_sent,
             list_unsubscribe=excluded.list_unsubscribe,
             list_unsub_one_click=excluded.list_unsub_one_click",
        params![
            msg.account_id,
            msg.gmail_msg_id,
            msg.thread_id,
            msg.from_addr,
            msg.from_name,
            msg.subject,
            msg.received_at.to_rfc3339(),
            msg.snippet,
            msg.body,
            msg.body_html,
            msg.is_sent as i64,
            msg.list_unsubscribe,
            msg.list_unsub_one_click as i64,
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM messages WHERE account_id=?1 AND gmail_msg_id=?2",
        params![msg.account_id, msg.gmail_msg_id],
        |r| r.get(0),
    )?;

    // Keep the FTS index in sync.
    conn.execute("DELETE FROM messages_fts WHERE rowid=?1", params![id])?;
    conn.execute(
        "INSERT INTO messages_fts(rowid, subject, body) VALUES(?1,?2,?3)",
        params![id, msg.subject, msg.body],
    )?;

    // Contacts are NOT seeded here: Sent mail's From header is the user's own
    // address. They come from the To/Cc recipients in `ingest_message`.
    Ok(id)
}

/// Seed the contacts table from a Sent message's recipients, each bumping its
/// `sent_count`. Addresses arrive de-duplicated and stripped of the account's own
/// address, so only empties are skipped. Received mail passes an empty list.
fn seed_contacts_conn(
    conn: &Connection,
    account_id: AccountId,
    recipients: &[String],
    first_seen: &str,
) -> Result<()> {
    for addr in recipients {
        if addr.trim().is_empty() {
            continue;
        }
        conn.execute(
            "INSERT INTO contacts(account_id, addr, sent_count, first_seen)
             VALUES(?1,?2,1,?3)
             ON CONFLICT(account_id, addr) DO UPDATE SET sent_count = sent_count + 1",
            params![account_id, addr, first_seen],
        )?;
    }
    Ok(())
}

/// Upsert a shipment keyed by `(account_id, tracking_number)` in the caller's
/// transaction. A repeat applies the no-regress status state machine
/// ([`crate::triage::ShipmentStatus::merge`]) — a delivered shipment is never
/// walked back — refreshes `last_update`/`last_message_id`, and adopts a more
/// informative `item_name` or a carrier more specific than "unknown".
///
/// SECURITY: callers gate on non-sealed mail; there is no sealed row to guard.
fn upsert_shipment_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
    s: &crate::triage::ShipmentInfo,
    seen_at: DateTime<Utc>,
) -> Result<i64> {
    use crate::triage::ShipmentStatus;

    let ts = seen_at.to_rfc3339();

    // Read any existing row so the merge runs in Rust rather than a SQL CASE.
    let existing: Option<(i64, String, String, String)> = conn
        .query_row(
            "SELECT id, status, item_name, carrier FROM shipments
             WHERE account_id=?1 AND tracking_number=?2",
            params![account_id, s.tracking_number],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;

    match existing {
        None => {
            conn.execute(
                "INSERT INTO shipments(account_id, tracking_number, carrier, item_name,
                     status, tracking_url, last_message_id, first_seen, last_update)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8)",
                params![
                    account_id,
                    s.tracking_number,
                    s.carrier,
                    s.item_name,
                    s.status.as_str(),
                    s.tracking_url,
                    message_id,
                    ts,
                ],
            )?;
            let id: i64 = conn.query_row(
                "SELECT id FROM shipments WHERE account_id=?1 AND tracking_number=?2",
                params![account_id, s.tracking_number],
                |r| r.get(0),
            )?;
            Ok(id)
        }
        Some((id, cur_status_s, cur_item, cur_carrier)) => {
            let cur_status =
                ShipmentStatus::parse(&cur_status_s).unwrap_or(ShipmentStatus::Shipped);
            let merged = ShipmentStatus::merge(cur_status, s.status);

            // Prefer a more informative item name.
            let item_name = if !s.item_name.is_empty()
                && (cur_item.is_empty() || s.item_name.len() > cur_item.len())
            {
                s.item_name.clone()
            } else {
                cur_item
            };
            // Prefer a concrete carrier over a prior "unknown".
            let (carrier, tracking_url) = if cur_carrier == "unknown" && s.carrier != "unknown" {
                (s.carrier.clone(), s.tracking_url.clone())
            } else {
                (cur_carrier, None) // tracking_url handled below (keep existing)
            };

            // When we kept the existing carrier, don't clobber a good tracking_url
            // with NULL — only update the url when we switched carrier.
            if carrier == s.carrier && s.carrier != "unknown" {
                conn.execute(
                    "UPDATE shipments SET status=?1, item_name=?2, carrier=?3,
                         tracking_url=?4, last_message_id=?5, last_update=?6
                     WHERE id=?7",
                    params![
                        merged.as_str(),
                        item_name,
                        carrier,
                        s.tracking_url,
                        message_id,
                        ts,
                        id,
                    ],
                )?;
            } else {
                let _ = tracking_url; // existing url retained
                conn.execute(
                    "UPDATE shipments SET status=?1, item_name=?2,
                         last_message_id=?3, last_update=?4
                     WHERE id=?5",
                    params![merged.as_str(), item_name, message_id, ts, id],
                )?;
            }
            Ok(id)
        }
    }
}

/// Upsert a receipt keyed by `(account_id, message_id)` in the caller's
/// transaction; a re-ingest overwrites in place.
///
/// SECURITY: callers gate on non-sealed mail; there is no sealed row to guard.
fn upsert_receipt_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
    from_addr: &str,
    from_name: Option<&str>,
    r: &crate::triage::ReceiptInfo,
    received_at: DateTime<Utc>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO receipts(account_id, message_id, from_addr, from_name,
             amount, currency, received_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
             from_addr=excluded.from_addr, from_name=excluded.from_name,
             amount=excluded.amount, currency=excluded.currency,
             received_at=excluded.received_at",
        params![
            account_id,
            message_id,
            from_addr,
            from_name,
            r.amount,
            r.currency,
            received_at.to_rfc3339(),
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM receipts WHERE account_id=?1 AND message_id=?2",
        params![account_id, message_id],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Replace this message's attachment rows, in the caller's transaction.
/// DELETE-then-INSERT keeps re-ingest idempotent; `data == None` writes a NULL
/// blob (over-cap, metadata only) while `size_bytes` stays the real decoded size.
/// Written for sealed mail too — the byte-serving path guards sealed parents.
///
/// INSERT OR IGNORE, not plain INSERT: a sender can attach the same file twice,
/// and a UNIQUE(account,message,filename,size) violation would roll back the
/// ENTIRE message ingest — a remote ingest DoS. Collapsing identical duplicates
/// to one row is the wanted outcome.
fn insert_attachments_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
    attachments: &[AttachmentInfo],
) -> Result<()> {
    conn.execute(
        "DELETE FROM attachments WHERE account_id=?1 AND message_id=?2",
        params![account_id, message_id],
    )?;
    for a in attachments {
        conn.execute(
            "INSERT OR IGNORE INTO attachments(account_id, message_id, filename, mime, size_bytes, data)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                account_id,
                message_id,
                a.filename,
                a.mime,
                a.size_bytes,
                a.data.as_deref(),
            ],
        )?;
    }
    Ok(())
}

/// Load one message's attachment metadata (NO bytes) as the human-door wire
/// shape, ordered by row id; `downloadable` is `data IS NOT NULL`. The CALLER
/// owns the sealed guard.
fn load_client_attachments_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
) -> Result<Vec<ClientAttachment>> {
    let mut stmt = conn.prepare(
        "SELECT id, filename, mime, size_bytes, data IS NOT NULL
         FROM attachments
         WHERE account_id=?1 AND message_id=?2
         ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map(params![account_id, message_id], |r| {
            Ok(ClientAttachment {
                id: r.get(0)?,
                filename: r.get(1)?,
                mime: r.get(2)?,
                size: r.get(3)?,
                downloadable: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Upsert a banking row keyed by `(account_id, message_id)` in the caller's
/// transaction; a re-run overwrites in place.
///
/// SECURITY: callers gate on non-sealed mail; there is no sealed row to guard.
fn upsert_banking_conn(
    conn: &Connection,
    applied: &BankingApplied,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO banking(account_id, message_id, kind, institution, amount,
             currency, account_hint, received_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
             kind=excluded.kind, institution=excluded.institution,
             amount=excluded.amount, currency=excluded.currency,
             account_hint=excluded.account_hint, received_at=excluded.received_at",
        params![
            applied.account_id,
            applied.message_id,
            applied.kind,
            applied.institution,
            applied.amount,
            applied.currency,
            applied.account_hint,
            applied.received_at.to_rfc3339(),
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM banking WHERE account_id=?1 AND message_id=?2",
        params![applied.account_id, applied.message_id],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Upsert a calendar update keyed by `(account_id, message_id)` in the caller's
/// transaction; a re-ingest overwrites in place.
///
/// SECURITY: callers gate on non-sealed mail; there is no sealed row to guard.
fn upsert_calendar_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
    c: &crate::triage::CalendarInfo,
    received_at: DateTime<Utc>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO calendar_updates(account_id, message_id, kind, event_title,
             starts_at, organizer, received_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
             kind=excluded.kind, event_title=excluded.event_title,
             starts_at=excluded.starts_at, organizer=excluded.organizer,
             received_at=excluded.received_at",
        params![
            account_id,
            message_id,
            c.kind.as_str(),
            c.event_title,
            c.starts_at.map(|d| d.to_rfc3339()),
            c.organizer,
            received_at.to_rfc3339(),
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM calendar_updates WHERE account_id=?1 AND message_id=?2",
        params![account_id, message_id],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// RECEIPT -> OPEN-BILL AUTO-CLOSE: resolve the one OPEN bill (a `deadlines` row
/// whose triage status != 'done') a just-ingested receipt plausibly settles,
/// inside the caller's ingest transaction so both land atomically.
///
/// Matching is the pure logic in [`crate::triage::receipt_match`], biased to
/// precision because a false auto-close hides an unpaid bill: merchant identity
/// by registrable domain or normalized display name; amounts must agree within
/// cents when both parse, and a parsed bill against an unparsed receipt refuses;
/// recency windows anchored on the two `received_at`s. At most ONE bill closes
/// per receipt, the EARLIEST-due match — recurring bills leave identical open
/// months, and closing both would hide an unpaid one.
///
/// The close appends an `audit_log` row (actor="ingest",
/// action="bill.auto_close") so the human door can answer "where did my bill
/// go?". Idempotent: a re-ingest finds the bill already 'done'.
fn auto_close_bill_for_receipt_conn(
    conn: &Connection,
    account_id: AccountId,
    receipt_message_id: i64,
    from_addr: &str,
    from_name: Option<&str>,
    r: &crate::triage::ReceiptInfo,
    received_at: DateTime<Utc>,
) -> Result<Option<i64>> {
    use crate::triage::receipt_match;

    // Candidate OPEN bills: every deadline whose triage row is not yet done, the
    // message join supplying biller identity + recency anchor. The open set is
    // small, so the pure rules filter it in Rust.
    let mut stmt = conn.prepare(
        "SELECT d.message_id, d.amount, d.currency, d.due_at,
                m.from_addr, m.from_name, m.received_at
         FROM deadlines d
         JOIN triage t ON t.message_id = d.message_id
         JOIN messages m ON m.id = d.message_id
         WHERE d.account_id = ?1
           AND t.status != 'done'
           AND t.sensitivity != 'sealed'
           AND d.message_id != ?2",
    )?;
    let rows = stmt.query_map(params![account_id, receipt_message_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<f64>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;

    // Best match = the EARLIEST-due bill that passes every rule.
    let mut best: Option<(i64, DateTime<Utc>, Option<f64>)> = None;
    for row in rows {
        let (bill_id, bill_amount, bill_currency, due_s, bill_addr, bill_name, bill_recv_s) = row?;

        // Currency sanity (v0 is USD-only, but never compare across currencies).
        if let (Some(rc), Some(bc)) = (r.currency.as_deref(), bill_currency.as_deref())
            && rc != bc
        {
            continue;
        }
        // Merchant identity is mandatory.
        if !receipt_match::merchant_matches(
            from_addr,
            from_name,
            &bill_addr,
            bill_name.as_deref(),
        ) {
            continue;
        }
        // Amount rule picks the recency window (or refuses outright).
        let Some(window_days) = receipt_match::amounts_permit_close(r.amount, bill_amount) else {
            continue;
        };
        // Recency: the bill must PRECEDE the receipt (a payment follows its
        // bill), within the rule's window.
        let bill_recv = parse_dt(&bill_recv_s)?;
        let age = received_at - bill_recv;
        if age < chrono::Duration::zero() || age > chrono::Duration::days(window_days) {
            continue;
        }

        let due_at = parse_dt(&due_s)?;
        if best.as_ref().is_none_or(|(_, best_due, _)| due_at < *best_due) {
            best = Some((bill_id, due_at, bill_amount));
        }
    }
    let Some((bill_id, _, bill_amount)) = best else {
        return Ok(None);
    };

    // 'done' stamps resolved_at, sealed is excluded, and the status guard makes
    // a re-run a no-op.
    let n = conn.execute(
        "UPDATE triage
         SET status = 'done', resolved_at = ?1
         WHERE account_id = ?2 AND message_id = ?3
           AND sensitivity != 'sealed' AND status != 'done'",
        params![Utc::now().to_rfc3339(), account_id, bill_id],
    )?;
    if n == 0 {
        return Ok(None); // raced/no-op — nothing closed, nothing to audit
    }

    // Record WHY in the audit log so the resolution is always explainable.
    let fmt_amt = |a: Option<f64>| a.map_or("unparsed".to_string(), |v| format!("${v:.2}"));
    conn.execute(
        "INSERT INTO audit_log(account_id, ts, actor, action, target, detail)
         VALUES(?1,?2,'ingest','bill.auto_close',?3,?4)",
        params![
            account_id,
            Utc::now().to_rfc3339(),
            bill_id.to_string(),
            format!(
                "receipt message {} from {} ({}) matched open bill (bill {})",
                receipt_message_id,
                from_addr,
                fmt_amt(r.amount),
                fmt_amt(bill_amount),
            ),
        ],
    )?;
    Ok(Some(bill_id))
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| CoreError::InvalidInput(format!("bad datetime {s:?}: {e}")))
}

/// Validate a sender-rule write, on the path of every door. A FILTERED rule with
/// an empty `want_text` is a contradiction ("filter everything except nothing"):
/// Stage-2 would get no instruction to evaluate, so the rule would silently
/// degrade while still reading as a rule in the UI.
fn validate_sender_rule(want_text: &str, disposition: Disposition) -> Result<()> {
    if disposition == Disposition::Filtered && want_text.trim().is_empty() {
        return Err(CoreError::InvalidInput(
            "a filtered rule requires a non-empty want_text (what SHOULD get through)".into(),
        ));
    }
    Ok(())
}

// ---- `events` row mapping (shared by events_after / event_by_id) -----------

/// One raw `events` row, columns in SELECT order. Split from [`finish_event`]
/// because a rusqlite mapper cannot fail with a [`CoreError`], so the timestamp
/// parse has to happen after it.
type EventRow = (i64, i64, String, String, String, i64, String, String, Option<String>, String);

fn map_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
    ))
}

/// Turn a raw [`EventRow`] into an [`Event`]. An unparseable `kind`/`tier` falls
/// back to the least-alarming value rather than erroring: refusing to serve a
/// stored row would stall a client's cursor at it forever.
fn finish_event(row: EventRow) -> Result<Event> {
    let (id, message_id, thread_id, kind, tier, importance, sender, one_line, deadline, created_at) =
        row;
    Ok(Event {
        id,
        kind: EventKind::parse(&kind).unwrap_or(EventKind::Surfaced),
        message_id,
        thread_id,
        tier: Tier::parse(&tier).unwrap_or(Tier::Noise),
        importance: importance.clamp(0, 255) as u8,
        sender,
        one_line,
        deadline,
        created_at: parse_dt(&created_at)?,
    })
}

// ---- `devices` row mapping -------------------------------------------------

/// One raw `devices` row, columns in SELECT order; split like [`EventRow`].
type DeviceRow = (i64, i64, String, String, String, String);

fn map_device(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
}

fn finish_device(row: DeviceRow) -> Result<Device> {
    let (id, account_id, token, platform, created_at, last_registered_at) = row;
    Ok(Device {
        id,
        account_id,
        token,
        platform,
        created_at: parse_dt(&created_at)?,
        last_registered_at: parse_dt(&last_registered_at)?,
    })
}

// ---- LLM usage ledger helpers (shared by the stage-1 and stage-2 categories) --

/// Bump the `stage2_usage` ledger for `(account, day, category)`: +1 call and add
/// the token counts. Both triage stages share this table keyed by `category`.
fn bump_usage_category(
    conn: &Connection,
    account_id: AccountId,
    day: &str,
    category: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO stage2_usage(account_id, day, category, calls, input_tokens, output_tokens)
         VALUES(?1, ?2, ?3, 1, ?4, ?5)
         ON CONFLICT(account_id, day, category) DO UPDATE SET
             calls = calls + 1,
             input_tokens = input_tokens + excluded.input_tokens,
             output_tokens = output_tokens + excluded.output_tokens",
        params![account_id, day, category, input_tokens as i64, output_tokens as i64],
    )?;
    Ok(())
}

/// Sum the ledger for `(account, category)` over every day `>= since_day`.
fn usage_since_category(
    conn: &Connection,
    account_id: AccountId,
    since_day: &str,
    category: &str,
) -> Result<Stage2Usage> {
    let row = conn.query_row(
        "SELECT COALESCE(SUM(calls), 0), COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0)
         FROM stage2_usage
         WHERE account_id = ?1 AND day >= ?2 AND category = ?3",
        params![account_id, since_day, category],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
    )?;
    Ok(Stage2Usage {
        calls: row.0.max(0) as u64,
        input_tokens: row.1.max(0) as u64,
        output_tokens: row.2.max(0) as u64,
    })
}

/// The most recent `days` rows for `(account, category)`, newest-first (sparse).
fn list_usage_category(
    conn: &Connection,
    account_id: AccountId,
    days: u32,
    category: &str,
) -> Result<Vec<Stage2UsageDay>> {
    let mut stmt = conn.prepare(
        "SELECT day, calls, input_tokens, output_tokens FROM stage2_usage
         WHERE account_id = ?1 AND category = ?3
         ORDER BY day DESC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![account_id, days as i64, category], |r| {
            Ok(Stage2UsageDay {
                day: r.get::<_, String>(0)?,
                calls: r.get::<_, i64>(1)?.max(0) as u64,
                input_tokens: r.get::<_, i64>(2)?.max(0) as u64,
                output_tokens: r.get::<_, i64>(3)?.max(0) as u64,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

impl Store for SqliteStore {
    fn upsert_message(&self, msg: &NewMessage) -> Result<i64> {
        let conn = self.lock()?;
        upsert_message_conn(&conn, msg)
    }

    fn ranked_updates(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        min_importance: Option<u8>,
    ) -> Result<Vec<Update>> {
        let conn = self.lock()?;
        let min = min_importance.unwrap_or(0) as i64;
        // SECURITY: sealed rows excluded in SQL. sensitivity != 'sealed'.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.thread_id, t.tier, t.importance, m.from_addr, t.one_line,
                    t.reason, t.deadline, t.matched_rule_id
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1
               AND t.sensitivity != 'sealed'
               AND m.is_sent = 0
               AND m.received_at >= ?2
               AND t.importance >= ?3
             ORDER BY t.importance DESC, m.received_at DESC",
        )?;
        let rows = stmt.query_map(
            params![account_id, since.to_rfc3339(), min],
            |r| {
                let tier_s: String = r.get(2)?;
                let deadline_s: Option<String> = r.get(7)?;
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    tier_s,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    deadline_s,
                    r.get::<_, Option<i64>>(8)?,
                ))
            },
        )?;

        let mut out = Vec::new();
        for row in rows {
            let (id, thread_id, tier_s, importance, sender, one_line, reason, deadline_s, rule) =
                row?;
            let deadline = match deadline_s {
                Some(s) => Some(parse_dt(&s)?),
                None => None,
            };
            out.push(Update {
                id,
                thread_id,
                tier: Tier::parse(&tier_s).unwrap_or(Tier::Noise),
                importance: importance.clamp(0, 255) as u8,
                sender,
                one_line,
                reason,
                deadline,
                matched_rule: rule,
                // AGENT DOOR: never carries field_reasons.
                field_reasons: None,
                has_attachments: None,
            });
        }
        Ok(out)
    }

    fn thread_view(&self, account_id: AccountId, thread_id: &str) -> Result<ThreadView> {
        let conn = self.lock()?;

        // SECURITY: if ANY message in this thread is sealed, the whole thread is
        // NotFound — as is a thread with no visible messages.
        let sealed_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM triage
             WHERE account_id=?1 AND sensitivity='sealed'
               AND message_id IN (SELECT id FROM messages WHERE account_id=?1 AND thread_id=?2)",
            params![account_id, thread_id],
            |r| r.get(0),
        )?;
        if sealed_count > 0 {
            return Err(CoreError::NotFound);
        }

        let subject: Option<String> = conn
            .query_row(
                "SELECT subject FROM messages
                 WHERE account_id=?1 AND thread_id=?2
                 ORDER BY received_at ASC LIMIT 1",
                params![account_id, thread_id],
                |r| r.get(0),
            )
            .optional()?;
        let subject = subject.ok_or(CoreError::NotFound)?;

        let mut stmt = conn.prepare(
            "SELECT id, from_addr, from_name, received_at, body
             FROM messages
             WHERE account_id=?1 AND thread_id=?2
             ORDER BY received_at ASC",
        )?;
        let rows = stmt.query_map(params![account_id, thread_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (id, from_addr, from_name, received_at, body) = row?;
            messages.push(SanitizedMessage {
                id,
                from_addr,
                from_name,
                received_at: parse_dt(&received_at)?,
                content: body,
            });
        }
        if messages.is_empty() {
            return Err(CoreError::NotFound);
        }

        Ok(ThreadView {
            thread_id: thread_id.to_string(),
            subject,
            messages,
        })
    }

    fn thread_id_for_message(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<String>> {
        let conn = self.lock()?;
        // SECURITY: a sealed message id resolves to `None` exactly like a
        // nonexistent one, so the `get_thread` fallback cannot confirm that a
        // sealed message exists. A message with no triage row COALESCEs to
        // non-sealed so plain mail still resolves.
        let thread_id: Option<String> = conn
            .query_row(
                "SELECT m.thread_id
                 FROM messages m
                 LEFT JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2
                   AND COALESCE(t.sensitivity, 'normal') != 'sealed'",
                params![account_id, message_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(thread_id)
    }

    fn thread_view_with_html(
        &self,
        account_id: AccountId,
        thread_id: &str,
    ) -> Result<ClientThreadView> {
        let conn = self.lock()?;

        // SECURITY: same sealed/nonexistent -> NotFound guard as `thread_view`,
        // so this human-door variant never reveals a sealed thread's html.
        let sealed_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM triage
             WHERE account_id=?1 AND sensitivity='sealed'
               AND message_id IN (SELECT id FROM messages WHERE account_id=?1 AND thread_id=?2)",
            params![account_id, thread_id],
            |r| r.get(0),
        )?;
        if sealed_count > 0 {
            return Err(CoreError::NotFound);
        }

        let subject: Option<String> = conn
            .query_row(
                "SELECT subject FROM messages
                 WHERE account_id=?1 AND thread_id=?2
                 ORDER BY received_at ASC LIMIT 1",
                params![account_id, thread_id],
                |r| r.get(0),
            )
            .optional()?;
        let subject = subject.ok_or(CoreError::NotFound)?;

        let mut stmt = conn.prepare(
            "SELECT id, from_addr, from_name, received_at, body, body_html
             FROM messages
             WHERE account_id=?1 AND thread_id=?2
             ORDER BY received_at ASC",
        )?;
        // Collect first, releasing `stmt`'s borrow of `conn`, so the per-message
        // attachment query below can run on the same connection.
        let rows = stmt
            .query_map(params![account_id, thread_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut messages = Vec::new();
        for (id, from_addr, from_name, received_at, body, body_html) in rows {
            // ALWAYS present on the wire ([] when none). No sealed guard needed:
            // the whole view 404s any thread containing a sealed message.
            let attachments = load_client_attachments_conn(&conn, account_id, id)?;
            messages.push(ClientMessage {
                id,
                from_addr,
                from_name,
                received_at: parse_dt(&received_at)?,
                content: body,
                html: body_html,
                attachments,
            });
        }
        if messages.is_empty() {
            return Err(CoreError::NotFound);
        }

        Ok(ClientThreadView {
            thread_id: thread_id.to_string(),
            subject,
            messages,
        })
    }

    fn attachment_bytes(
        &self,
        account_id: AccountId,
        attachment_id: i64,
    ) -> Result<Option<AttachmentBytes>> {
        let conn = self.lock()?;
        // SECURITY: the parent message must be non-sealed (a missing triage row
        // COALESCEs to 'normal'), so a sealed parent yields no row and the caller
        // 404s, indistinguishable from an unknown id. An existing row with NULL
        // `data` (over the ingest cap) flows out as `Some((.., None))` => 410.
        let row = conn
            .query_row(
                "SELECT a.filename, a.mime, a.data
                 FROM attachments a
                 JOIN messages m ON m.id = a.message_id AND m.account_id = a.account_id
                 LEFT JOIN triage t ON t.message_id = a.message_id
                 WHERE a.account_id = ?1 AND a.id = ?2
                   AND COALESCE(t.sensitivity, 'normal') = 'normal'",
                params![account_id, attachment_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    fn deadlines(
        &self,
        account_id: AccountId,
        within_days: Option<u32>,
    ) -> Result<Vec<Deadline>> {
        let conn = self.lock()?;
        // SECURITY: exclude deadlines whose source message is sealed.
        // within_days = None means "all".
        let cutoff = within_days
            .map(|d| (Utc::now() + chrono::Duration::days(d as i64)).to_rfc3339());
        let cutoff_ref: &dyn rusqlite::ToSql = match &cutoff {
            Some(s) => s,
            None => &"9999-12-31T23:59:59+00:00",
        };

        let mut stmt = conn.prepare(
            "SELECT d.id, d.account_id, d.message_id, d.kind, d.amount, d.currency,
                    d.due_at, d.past_due, d.source
             FROM deadlines d
             WHERE d.account_id = ?1
               AND d.due_at <= ?2
               AND NOT EXISTS (
                   SELECT 1 FROM triage t
                   WHERE t.message_id = d.message_id AND t.sensitivity = 'sealed'
               )
             ORDER BY d.due_at ASC",
        )?;
        let rows = stmt.query_map(params![account_id, cutoff_ref], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<f64>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, String>(8)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, acct, message_id, kind, amount, currency, due_at, past_due, source) = row?;
            out.push(Deadline {
                id,
                account_id: acct,
                message_id,
                kind,
                amount,
                currency,
                due_at: parse_dt(&due_at)?,
                past_due: past_due != 0,
                source,
            });
        }
        Ok(out)
    }

    fn upsert_shipment(
        &self,
        account_id: AccountId,
        message_id: i64,
        shipment: &crate::triage::ShipmentInfo,
        seen_at: DateTime<Utc>,
    ) -> Result<i64> {
        let conn = self.lock()?;
        upsert_shipment_conn(&conn, account_id, message_id, shipment, seen_at)
    }

    fn list_shipments(
        &self,
        account_id: AccountId,
        include_delivered: bool,
    ) -> Result<Vec<crate::types::Shipment>> {
        let conn = self.lock()?;
        // No sealed filter needed: detection never runs on sealed mail, so the
        // table holds no sealed rows by construction.
        let sql = if include_delivered {
            "SELECT id, account_id, tracking_number, carrier, item_name, status,
                    tracking_url, first_seen, last_update
             FROM shipments WHERE account_id=?1
             ORDER BY last_update DESC"
        } else {
            "SELECT id, account_id, tracking_number, carrier, item_name, status,
                    tracking_url, first_seen, last_update
             FROM shipments WHERE account_id=?1 AND status != 'delivered'
             ORDER BY last_update DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                acct,
                tracking_number,
                carrier,
                item_name,
                status,
                tracking_url,
                first_seen,
                last_update,
            ) = row?;
            out.push(crate::types::Shipment {
                id,
                account_id: acct,
                tracking_number,
                carrier,
                item_name,
                status,
                tracking_url,
                first_seen: parse_dt(&first_seen)?,
                last_update: parse_dt(&last_update)?,
            });
        }
        Ok(out)
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
        let conn = self.lock()?;
        upsert_receipt_conn(
            &conn,
            account_id,
            message_id,
            from_addr,
            from_name,
            receipt,
            received_at,
        )
    }

    fn list_receipts(&self, account_id: AccountId, days: u32) -> Result<Vec<Receipt>> {
        let conn = self.lock()?;
        // No sealed filter needed: detection never runs on sealed mail, so the
        // table holds no sealed rows by construction.
        let since = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT rc.id, rc.account_id, rc.message_id, m.thread_id, rc.from_addr,
                    rc.from_name, rc.amount, rc.currency, rc.received_at
             FROM receipts rc
             JOIN messages m ON m.id = rc.message_id
             WHERE rc.account_id=?1 AND rc.received_at >= ?2
             ORDER BY rc.received_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id, since], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<f64>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, String>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, acct, message_id, thread_id, from_addr, from_name, amount, currency, received_at) =
                row?;
            out.push(Receipt {
                id,
                account_id: acct,
                message_id,
                thread_id,
                from_addr,
                from_name,
                amount,
                currency,
                received_at: parse_dt(&received_at)?,
            });
        }
        Ok(out)
    }

    fn extract_queue(
        &self,
        account_id: AccountId,
        categories: &[&str],
        limit: usize,
    ) -> Result<Vec<ExtractQueued>> {
        if categories.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock()?;
        // A message that already produced a RECEIPT is excluded: a receipt and a
        // banking row must never double-create. The category IN (...) list is
        // built from bound params, never string-interpolated.
        let placeholders = (0..categories.len())
            .map(|i| format!("?{}", i + 3))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT m.id, m.thread_id, m.from_addr, m.from_name, m.subject, m.body,
                    t.category, t.sensitivity, m.received_at
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1
               AND t.category IN ({placeholders})
               AND t.extractor_model_used IS NULL
               AND t.sensitivity = 'normal'
               AND m.is_sent = 0
               AND NOT EXISTS(
                   SELECT 1 FROM receipts r
                   WHERE r.account_id = t.account_id AND r.message_id = t.message_id
               )
             ORDER BY m.received_at DESC
             LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        // Bind params: ?1 account_id, ?2 limit, ?3.. categories.
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(categories.len() + 2);
        binds.push(Box::new(account_id));
        binds.push(Box::new(limit as i64));
        for c in categories {
            binds.push(Box::new((*c).to_string()));
        }
        let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(bind_refs.as_slice(), |r| {
            let category: String = r.get(6)?;
            let sensitivity: String = r.get(7)?;
            let received_at: String = r.get(8)?;
            Ok((
                ExtractQueued {
                    message_id: r.get(0)?,
                    account_id,
                    thread_id: r.get(1)?,
                    from_addr: r.get(2)?,
                    from_name: r.get(3)?,
                    subject: r.get(4)?,
                    body: r.get(5)?,
                    category,
                    received_at: Utc::now(), // replaced below after parse
                    sensitivity: Sensitivity::parse(&sensitivity),
                },
                received_at,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (mut q, received_at) = row?;
            q.received_at = parse_dt(&received_at)?;
            out.push(q);
        }
        Ok(out)
    }

    fn retriage_reset(
        &self,
        account_id: AccountId,
        message_id: Option<i64>,
        days: u32,
    ) -> Result<u64> {
        let conn = self.lock()?;
        let cutoff = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        // Scope: one message, or the trailing-days inbound window. Normal
        // sensitivity only, and never a rule-decided ('rule'), human-corrected
        // ('human') or sealed/sent ('n/a') marker — rules are authoritative, a
        // human is more authoritative still, and sealed mail re-enters no queue.
        // Re-running the model over a row someone fixed would undo their work and
        // record feedback corrections that never stuck.
        let scope_sql = match message_id {
            Some(_) => "m.id = ?2",
            None => "m.received_at >= ?2",
        };
        let scope_param: String = match message_id {
            Some(id) => id.to_string(),
            None => cutoff,
        };
        let update = format!(
            "UPDATE triage SET stage1_model_used = NULL, model_used = NULL,
                    needs_stage2 = 0, extractor_model_used = NULL
             WHERE account_id = ?1
               AND COALESCE(sensitivity, 'normal') = 'normal'
               AND COALESCE(stage1_model_used, '') NOT IN ('rule', 'n/a', 'human')
               AND message_id IN (
                   SELECT m.id FROM messages m
                   WHERE m.account_id = ?1 AND m.is_sent = 0 AND {scope_sql}
               )"
        );
        let n = conn.execute(&update, rusqlite::params![account_id, scope_param])?;
        // Drop stale specialist rows; re-extraction recreates them, possibly
        // under a different category verdict.
        let del = format!(
            "DELETE FROM banking
             WHERE account_id = ?1
               AND message_id IN (
                   SELECT t.message_id FROM triage t
                   JOIN messages m ON m.id = t.message_id
                   WHERE t.account_id = ?1 AND t.stage1_model_used IS NULL
                     AND m.is_sent = 0 AND {scope_sql}
               )"
        );
        conn.execute(&del, rusqlite::params![account_id, scope_param])?;
        Ok(n as u64)
    }

    fn extract_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        extractor_model_used: &str,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE triage SET extractor_model_used = ?3
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![message_id, account_id, extractor_model_used],
        )?;
        Ok(())
    }

    fn banking_apply(&self, applied: &BankingApplied) -> Result<i64> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let id = upsert_banking_conn(&tx, applied)?;
        // Stamp the extractor marker (leaving the extract queue) and, for a
        // RECORD (statement/alert), resolve the row to 'done' so it leaves the
        // attention bands. The sensitivity='normal' guard keeps a sealed row from
        // ever being mutated here.
        let now_s = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE triage SET
                 extractor_model_used = ?3,
                 status = CASE WHEN ?4 = 1 THEN 'done' ELSE status END,
                 resolved_at = CASE WHEN ?4 = 1 THEN ?5 ELSE resolved_at END
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![
                applied.message_id,
                applied.account_id,
                applied.extractor_model_used,
                applied.auto_resolve as i64,
                now_s,
            ],
        )?;
        tx.commit()?;
        Ok(id)
    }

    fn marketing_apply(&self, applied: &MarketingApplied) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO marketing(account_id, message_id, brand, offer, discount, code,
                                   expires_at, received_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(account_id, message_id) DO UPDATE SET
                 brand=excluded.brand,
                 offer=excluded.offer,
                 discount=excluded.discount,
                 code=excluded.code,
                 expires_at=excluded.expires_at",
            params![
                applied.account_id,
                applied.message_id,
                applied.brand,
                applied.offer,
                applied.discount,
                applied.code,
                applied.expires_at,
                applied.received_at.to_rfc3339(),
            ],
        )?;
        // Stamp the extractor marker so the row leaves the queue. NO status
        // change: marketing does not auto-resolve. The sensitivity='normal' guard
        // keeps a sealed row from ever being mutated here.
        tx.execute(
            "UPDATE triage SET extractor_model_used = ?3
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![
                applied.message_id,
                applied.account_id,
                applied.extractor_model_used
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn marketing_offers(
        &self,
        account_id: AccountId,
        days: u32,
        limit: u32,
    ) -> Result<Vec<MarketingOffer>> {
        let conn = self.lock()?;
        let since = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT k.message_id, m.thread_id, m.from_addr, m.subject, k.brand, k.offer,
                    k.discount, k.code, k.expires_at, k.received_at
             FROM marketing k
             JOIN messages m ON m.id = k.message_id AND m.account_id = k.account_id
             WHERE k.account_id = ?1 AND k.received_at >= ?2
             ORDER BY k.received_at DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, since, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, String>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (message_id, thread_id, sender, subject, brand, offer, discount, code, expires_at, received_at) = row?;
            out.push(MarketingOffer {
                message_id,
                thread_id,
                sender,
                subject,
                brand,
                offer,
                discount,
                code,
                expires_at,
                received_at: parse_dt(&received_at)?,
            });
        }
        Ok(out)
    }

    fn list_banking(&self, account_id: AccountId) -> Result<Vec<Banking>> {
        let conn = self.lock()?;
        // No sealed filter needed: extraction never runs on sealed mail, so the
        // table holds no sealed rows by construction.
        let mut stmt = conn.prepare(
            "SELECT b.id, b.message_id, m.thread_id, m.from_addr, b.kind, b.institution,
                    b.amount, b.currency, b.account_hint, b.received_at
             FROM banking b
             JOIN messages m ON m.id = b.message_id
             WHERE b.account_id=?1
             ORDER BY b.received_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<f64>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, String>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                message_id,
                thread_id,
                from_addr,
                kind,
                institution,
                amount,
                currency,
                account_hint,
                received_at,
            ) = row?;
            out.push(Banking {
                id,
                message_id,
                thread_id,
                from_addr,
                kind,
                institution,
                amount,
                currency,
                account_hint,
                received_at: parse_dt(&received_at)?,
            });
        }
        Ok(out)
    }

    fn upsert_calendar_update(
        &self,
        account_id: AccountId,
        message_id: i64,
        calendar: &crate::triage::CalendarInfo,
        received_at: DateTime<Utc>,
    ) -> Result<i64> {
        let conn = self.lock()?;
        upsert_calendar_conn(&conn, account_id, message_id, calendar, received_at)
    }

    fn list_calendar_updates(
        &self,
        account_id: AccountId,
        hours: u32,
    ) -> Result<Vec<CalendarUpdate>> {
        let conn = self.lock()?;
        // The window is on received_at, NOT the event's starts_at. No sealed
        // filter needed: detection never runs on sealed mail.
        let since = (Utc::now() - chrono::Duration::hours(hours as i64)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.message_id, m.thread_id, c.kind, c.event_title, c.starts_at,
                    c.organizer, c.received_at
             FROM calendar_updates c
             JOIN messages m ON m.id = c.message_id
             WHERE c.account_id=?1 AND c.received_at >= ?2
             ORDER BY c.received_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id, since], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, message_id, thread_id, kind, event_title, starts_at, organizer, received_at) =
                row?;
            out.push(CalendarUpdate {
                id,
                message_id,
                thread_id,
                kind,
                event_title,
                starts_at: starts_at.as_deref().map(parse_dt).transpose()?,
                organizer,
                received_at: parse_dt(&received_at)?,
            });
        }
        Ok(out)
    }

    fn set_sender_rule(
        &self,
        account_id: AccountId,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<i64> {
        validate_sender_rule(want_text, disposition)?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sender_rules(account_id, match_pattern, want_text, disposition, updated_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(account_id, match_pattern) DO UPDATE SET
                 want_text=excluded.want_text, disposition=excluded.disposition,
                 updated_at=excluded.updated_at",
            params![
                account_id,
                match_pattern,
                want_text,
                disposition.as_str(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM sender_rules WHERE account_id=?1 AND match_pattern=?2",
            params![account_id, match_pattern],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    fn set_sender_rule_audited(
        &self,
        account_id: AccountId,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
        audit: &NewAuditEntry,
    ) -> Result<i64> {
        validate_sender_rule(want_text, disposition)?;
        // FAIL-CLOSED: the rule write and its audit row share ONE transaction, so
        // a failed audit INSERT rolls the rule back and an agent-door write can
        // never land untraced.
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO sender_rules(account_id, match_pattern, want_text, disposition, updated_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(account_id, match_pattern) DO UPDATE SET
                 want_text=excluded.want_text, disposition=excluded.disposition,
                 updated_at=excluded.updated_at",
            params![
                account_id,
                match_pattern,
                want_text,
                disposition.as_str(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        let id: i64 = tx.query_row(
            "SELECT id FROM sender_rules WHERE account_id=?1 AND match_pattern=?2",
            params![account_id, match_pattern],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO audit_log(account_id, ts, actor, action, target, detail)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                account_id,
                Utc::now().to_rfc3339(),
                audit.actor,
                audit.action,
                audit.target,
                audit.detail,
            ],
        )?;
        tx.commit()?;
        Ok(id)
    }

    fn update_sender_rule(
        &self,
        account_id: AccountId,
        id: i64,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<bool> {
        validate_sender_rule(want_text, disposition)?;
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE sender_rules SET
                 match_pattern = ?3, want_text = ?4, disposition = ?5, updated_at = ?6
             WHERE account_id = ?1 AND id = ?2",
            params![
                account_id,
                id,
                match_pattern,
                want_text,
                disposition.as_str(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(n > 0)
    }

    fn list_sender_rules(&self, account_id: AccountId) -> Result<Vec<SenderRule>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, match_pattern, want_text, disposition, updated_at
             FROM sender_rules WHERE account_id=?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, acct, match_pattern, want_text, disposition, updated_at) = row?;
            out.push(SenderRule {
                id,
                account_id: acct,
                match_pattern,
                want_text,
                disposition: Disposition::parse(&disposition).unwrap_or(Disposition::Surface),
                updated_at: parse_dt(&updated_at)?,
            });
        }
        Ok(out)
    }

    fn ingest_message(&self, triaged: &TriagedMessage) -> Result<i64> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;

        // 1. Upsert the message row (+ FTS).
        let id = upsert_message_conn(&tx, &triaged.message)?;

        // 1b. Contacts from Sent-mail To/Cc, in the SAME transaction.
        seed_contacts_conn(
            &tx,
            triaged.message.account_id,
            &triaged.recipients,
            &triaged.message.received_at.to_rfc3339(),
        )?;

        // 1c. UNSUBSCRIBE VIOLATION LEDGER: inbound mail from a sender the user
        //     unsubscribed from, past the 72h grace, bumps that sender's
        //     violation_count — in the SAME transaction as the message insert, so
        //     the ledger cannot drift from the mail that drives it.
        if !triaged.message.is_sent {
            bump_unsub_violation_conn(
                &tx,
                triaged.message.account_id,
                &triaged.message.from_addr,
                triaged.message.received_at,
            )?;
        }

        // 2. Write the triage row IN THE SAME TRANSACTION. That is the whole
        //    point for sealed mail: sensitivity='sealed' commits atomically with
        //    the message, so there is no window in which it is queryable as
        //    normal mail. `model_used` stays NULL, which with
        //    sensitivity='normal' is the Stage-2 queue predicate.
        let deadline_dt = triaged.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
        // AUTO-RESOLVE receipts and calendar updates: both are RECORDS, not
        // things to act on, so they start terminal ('done' + resolved_at), stay
        // out of the New/Attention/Aging bands, and live only in their category.
        // Other rows start 'new'.
        let now_s = Utc::now().to_rfc3339();
        let auto_resolved = triaged.sensitivity != Sensitivity::Sealed
            && (triaged.receipt.is_some() || triaged.calendar.is_some());
        let (status, resolved_at) = if auto_resolved {
            ("done", Some(now_s.clone()))
        } else {
            ("new", None)
        };
        // Re-ingest PRESERVES the existing attention lifecycle: a re-sync must not
        // reopen an item the user dismissed. Receipt/calendar rows are the
        // exception, force-resolved on every ingest — the CASE keys off
        // `excluded.status`, and only auto-resolved rows pass 'done' in.
        //
        // Per-property Stage-1 reasons as JSON (NULL when empty, as for sealed /
        // sent mail). HUMAN-DOOR ONLY on read.
        let field_reasons_json = if triaged.field_reasons.is_empty() {
            None
        } else {
            serde_json::to_string(&triaged.field_reasons).ok()
        };
        // STAGE-1/STAGE-2 QUEUE MARKERS: `stage1_model_used` decides whether the
        // Stage-1 pass looks at this row, `needs_stage2` is the escalation seed.
        //   * Sealed / Sent: never queued for any LLM ('n/a').
        //   * Explicit Squelch/Surface rule: the user already ruled on this
        //     sender, so no model spend at all ('rule', needs_stage2=0).
        //   * Filtered rule: skip Stage-1, straight to Stage-2 for the want_text
        //     ('rule', needs_stage2=1).
        //   * Everything else: enter the Stage-1 queue (NULL), seeding
        //     `needs_stage2` from heuristic confidence.
        let (stage1_model_used, needs_stage2): (Option<&str>, i64) =
            if triaged.sensitivity != Sensitivity::Normal || triaged.message.is_sent {
                (Some("n/a"), 0)
            } else if triaged.matched_rule.is_some() {
                (Some("rule"), if triaged.confident { 0 } else { 1 })
            } else {
                (None, if triaged.confident { 0 } else { 1 })
            };
        // RE-INGEST CLASSIFICATION GUARD. A re-ingest carries only HEURISTIC SEED
        // values, so for a row an LLM already classified (`model_used` set, or a
        // `stage1_model_used` other than the 'rule'/'n/a' sentinels) writing the
        // seed back would discard paid classification while the model markers
        // stay put — the row would never re-queue to recover it. This predicate
        // keeps those columns on conflict; still-seed rows refresh normally.
        const PROCESSED: &str = "(triage.model_used IS NOT NULL \
             OR (triage.stage1_model_used IS NOT NULL \
                 AND triage.stage1_model_used NOT IN ('rule', 'n/a')))";
        let triage_upsert = format!(
            "INSERT INTO triage(message_id, account_id, importance, tier, sensitivity,
                 sealed_kind, one_line, reason, deadline, matched_rule_id,
                 stage1_model_used, needs_stage2, model_used,
                 status, resolved_at, created_at, field_reasons)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,NULL,?13,?14,?15,?16)
             ON CONFLICT(message_id) DO UPDATE SET
                 importance=CASE WHEN {PROCESSED} THEN triage.importance ELSE excluded.importance END,
                 tier=CASE WHEN {PROCESSED} THEN triage.tier ELSE excluded.tier END,
                 sensitivity=excluded.sensitivity, sealed_kind=excluded.sealed_kind,
                 one_line=CASE WHEN {PROCESSED} THEN triage.one_line ELSE excluded.one_line END,
                 reason=CASE WHEN {PROCESSED} THEN triage.reason ELSE excluded.reason END,
                 field_reasons=CASE WHEN {PROCESSED} THEN triage.field_reasons ELSE excluded.field_reasons END,
                 deadline=CASE WHEN {PROCESSED} THEN triage.deadline ELSE excluded.deadline END,
                 matched_rule_id=excluded.matched_rule_id,
                 status=CASE WHEN excluded.status='done' THEN 'done' ELSE triage.status END,
                 resolved_at=CASE WHEN excluded.status='done'
                     THEN excluded.resolved_at ELSE triage.resolved_at END"
        );
        tx.execute(
            &triage_upsert,
            params![
                id,
                triaged.message.account_id,
                triaged.importance as i64,
                triaged.tier.as_str(),
                triaged.sensitivity.as_str(),
                triaged.sealed_kind.map(|k| k.as_str()),
                triaged.one_line,
                triaged.reason,
                deadline_dt,
                triaged.matched_rule,
                stage1_model_used,
                needs_stage2,
                status,
                resolved_at,
                now_s,
                field_reasons_json,
            ],
        )?;

        // 3. Deadlines: non-sealed mail only (Stage-1 never runs on sealed
        //    content). Replace any prior row so re-ingest is idempotent.
        tx.execute(
            "DELETE FROM deadlines WHERE message_id=?1",
            params![id],
        )?;
        if triaged.sensitivity != Sensitivity::Sealed
            && let Some(d) = &triaged.deadline
        {
                tx.execute(
                    "INSERT INTO deadlines(account_id, message_id, kind, amount, currency,
                         due_at, past_due, source)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        triaged.message.account_id,
                        id,
                        d.kind,
                        d.amount,
                        d.currency,
                        d.due_at.to_rfc3339(),
                        d.past_due as i64,
                        d.source,
                    ],
                )?;
        }

        // 4. Shipment: NON-SEALED mail only, so `shipments` is sealed-free by
        //    construction. Upserted in the SAME transaction so a package's state
        //    and its source message land atomically.
        if triaged.sensitivity != Sensitivity::Sealed
            && let Some(s) = &triaged.shipment
        {
            upsert_shipment_conn(
                &tx,
                triaged.message.account_id,
                id,
                s,
                triaged.message.received_at,
            )?;
        }

        // 5. Receipt: NON-SEALED mail only, so `receipts` is sealed-free by
        //    construction. Independent of shipment detection — an order
        //    confirmation with a total AND tracking lands in BOTH tables.
        if triaged.sensitivity != Sensitivity::Sealed
            && let Some(r) = &triaged.receipt
        {
            upsert_receipt_conn(
                &tx,
                triaged.message.account_id,
                id,
                &triaged.message.from_addr,
                triaged.message.from_name.as_deref(),
                r,
                triaged.message.received_at,
            )?;

            // 5b. RECEIPT -> OPEN-BILL AUTO-CLOSE, in the SAME transaction:
            //     resolve the bill this payment settles and audit why. A missed
            //     match is fine; a false close would hide an unpaid bill.
            auto_close_bill_for_receipt_conn(
                &tx,
                triaged.message.account_id,
                id,
                &triaged.message.from_addr,
                triaged.message.from_name.as_deref(),
                r,
                triaged.message.received_at,
            )?;
        }

        // 6. Calendar update: NON-SEALED mail only, so `calendar_updates` is
        //    sealed-free by construction. Independent of the other detectors,
        //    exactly like receipts. Nothing is written back to Gmail — "resolved"
        //    is squelch-internal.
        if triaged.sensitivity != Sensitivity::Sealed
            && let Some(c) = &triaged.calendar
        {
            upsert_calendar_conn(
                &tx,
                triaged.message.account_id,
                id,
                c,
                triaged.message.received_at,
            )?;
        }

        // 7. Attachments: written for sealed mail too — the byte-serving endpoint
        //    guards sealed parents. Replaces prior rows so re-ingest is
        //    idempotent; over-cap parts store a NULL blob.
        insert_attachments_conn(&tx, triaged.message.account_id, id, &triaged.attachments)?;

        tx.commit()?;
        Ok(id)
    }

    fn is_known_contact(&self, account_id: AccountId, addr: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM contacts
             WHERE account_id=?1 AND addr=?2 COLLATE NOCASE AND sent_count > 0",
            params![account_id, addr],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    fn sync_state(&self, account_id: AccountId, mailbox: &str) -> Result<Option<SyncState>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT uidvalidity, last_uid FROM sync_state
                 WHERE account_id=?1 AND mailbox=?2",
                params![account_id, mailbox],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(row.map(|(uv, lu)| SyncState {
            uidvalidity: uv as u32,
            last_uid: lu as u64,
        }))
    }

    fn set_sync_state(
        &self,
        account_id: AccountId,
        mailbox: &str,
        state: &SyncState,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sync_state(account_id, mailbox, uidvalidity, last_uid)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(account_id, mailbox) DO UPDATE SET
                 uidvalidity=excluded.uidvalidity, last_uid=excluded.last_uid",
            params![
                account_id,
                mailbox,
                state.uidvalidity as i64,
                state.last_uid as i64,
            ],
        )?;
        Ok(())
    }

    fn sealed_messages(&self, account_id: AccountId) -> Result<Vec<SealedMessage>> {
        // LOCAL-ONLY: the only method that returns sealed rows. TUI use only.
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT m.id, m.account_id, m.thread_id, m.from_addr, m.subject,
                    m.received_at, t.sealed_kind
             FROM messages m
             JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?1 AND t.sensitivity = 'sealed'
             ORDER BY m.received_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, acct, thread_id, from_addr, subject, received_at, sealed_kind) = row?;
            out.push(SealedMessage {
                id,
                account_id: acct,
                thread_id,
                from_addr,
                subject,
                received_at: parse_dt(&received_at)?,
                sealed_kind,
            });
        }
        Ok(out)
    }

    fn search(
        &self,
        account_id: AccountId,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>> {
        let conn = self.lock()?;
        // SECURITY: sealed rows excluded in SQL. An untriaged message COALESCEs
        // to non-sealed so freshly-ingested mail is still findable, but a sealed
        // classification always hides the row.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.thread_id, m.from_addr, m.from_name, m.subject,
                    m.received_at, m.snippet
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?1
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
               AND m.is_sent = 0
               AND messages_fts MATCH ?2
             ORDER BY rank
             LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(
            params![account_id, query, limit as i64, offset as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )?;

        let mut out = Vec::new();
        for row in rows {
            let (id, thread_id, from_addr, from_name, subject, received_at, snippet) = row?;
            out.push(SearchHit {
                id,
                thread_id,
                from_addr,
                from_name,
                subject,
                received_at: parse_dt(&received_at)?,
                snippet,
            });
        }
        Ok(out)
    }

    fn attention_updates(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        min_importance: Option<u8>,
        status: Option<AttentionStatus>,
        band: Option<SitrepBand>,
    ) -> Result<Vec<AttentionUpdate>> {
        let conn = self.lock()?;
        let min = min_importance.unwrap_or(0) as i64;

        // Base predicate: sealed excluded, sent excluded, since/importance window.
        // Bands:
        //   standing = tier IN ('past_due','deadline') AND status != 'done'
        //   new      = surfaced_at IS NULL AND status != 'done'
        //   open     = status = 'open'
        // The `status != 'done'` on `new` keeps AUTO-RESOLVED receipts out of the
        // band — a receipt is a record, not new inbox clutter.
        let mut sql = String::from(
            "SELECT m.id, m.thread_id, t.tier, t.importance, m.from_addr, t.one_line,
                    t.reason, t.deadline, t.matched_rule_id,
                    t.status, t.surfaced_at, t.resolved_at, t.field_reasons,
                    EXISTS(SELECT 1 FROM attachments a
                           WHERE a.account_id = m.account_id
                             AND a.message_id = m.id) AS has_atts
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1
               AND t.sensitivity != 'sealed'
               AND m.is_sent = 0
               AND m.received_at >= ?2
               AND t.importance >= ?3",
        );
        if let Some(s) = status {
            sql.push_str(match s {
                AttentionStatus::New => " AND t.status = 'new'",
                AttentionStatus::Open => " AND t.status = 'open'",
                AttentionStatus::Done => " AND t.status = 'done'",
            });
        }
        match band {
            Some(SitrepBand::Standing) => {
                sql.push_str(" AND t.tier IN ('past_due','deadline') AND t.status != 'done'");
            }
            Some(SitrepBand::New) => {
                sql.push_str(" AND t.surfaced_at IS NULL AND t.status != 'done'")
            }
            Some(SitrepBand::Open) => sql.push_str(" AND t.status = 'open'"),
            None => {}
        }
        // The `open` band is the aging one: age*importance floats
        // long-unresolved-and-important items, computed in SQL via julianday so
        // the ordering stays server-side. Other bands sort by importance.
        if band == Some(SitrepBand::Open) {
            sql.push_str(
                " ORDER BY (julianday(?4) - julianday(m.received_at)) * t.importance DESC,
                          m.received_at DESC",
            );
        } else {
            sql.push_str(" ORDER BY t.importance DESC, m.received_at DESC");
        }

        let now = Utc::now().to_rfc3339();
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &rusqlite::Row| {
            let tier_s: String = r.get(2)?;
            let deadline_s: Option<String> = r.get(7)?;
            let status_s: String = r.get(9)?;
            let surfaced_s: Option<String> = r.get(10)?;
            let resolved_s: Option<String> = r.get(11)?;
            let field_reasons_s: Option<String> = r.get(12)?;
            let has_atts: bool = r.get::<_, i64>(13)? != 0;
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                tier_s,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                deadline_s,
                r.get::<_, Option<i64>>(8)?,
                status_s,
                surfaced_s,
                resolved_s,
                field_reasons_s,
                has_atts,
            ))
        };
        let rows = if band == Some(SitrepBand::Open) {
            stmt.query_map(params![account_id, since.to_rfc3339(), min, now], map_row)?
        } else {
            stmt.query_map(params![account_id, since.to_rfc3339(), min], map_row)?
        };

        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                thread_id,
                tier_s,
                importance,
                sender,
                one_line,
                reason,
                deadline_s,
                rule,
                status_s,
                surfaced_s,
                resolved_s,
                field_reasons_s,
                has_atts,
            ) = row?;
            let deadline = match deadline_s {
                Some(s) => Some(parse_dt(&s)?),
                None => None,
            };
            // A NULL or malformed reasons blob yields None: one bad row must
            // never fail the whole updates read.
            let field_reasons: Option<crate::types::FieldReasons> = field_reasons_s
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            let surfaced_at = match surfaced_s {
                Some(s) => Some(parse_dt(&s)?),
                None => None,
            };
            let resolved_at = match resolved_s {
                Some(s) => Some(parse_dt(&s)?),
                None => None,
            };
            out.push(AttentionUpdate {
                update: Update {
                    id,
                    thread_id,
                    tier: Tier::parse(&tier_s).unwrap_or(Tier::Noise),
                    importance: importance.clamp(0, 255) as u8,
                    sender,
                    one_line,
                    reason,
                    deadline,
                    matched_rule: rule,
                    field_reasons,
                    has_attachments: Some(has_atts),
                },
                status: AttentionStatus::parse(&status_s).unwrap_or(AttentionStatus::New),
                surfaced_at,
                resolved_at,
            });
        }
        Ok(out)
    }

    fn mark_surfaced(&self, account_id: AccountId, message_ids: &[i64]) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        let tx = conn.transaction()?;
        let mut first_surfaced = 0usize;
        {
            // Stamp surfaced_at only if NULL and promote new->open. The
            // sensitivity guard means a sealed row is NEVER stamped, so it cannot
            // leak into a "new since last check" delta. Idempotent.
            let mut stmt = tx.prepare(
                "UPDATE triage
                 SET surfaced_at = COALESCE(surfaced_at, ?1),
                     status = CASE WHEN status = 'new' THEN 'open' ELSE status END
                 WHERE account_id = ?2 AND message_id = ?3
                   AND sensitivity != 'sealed'
                   AND surfaced_at IS NULL",
            )?;
            for &id in message_ids {
                first_surfaced += stmt.execute(params![now, account_id, id])?;
            }
        }
        tx.commit()?;
        Ok(first_surfaced)
    }

    fn set_attention_status(
        &self,
        account_id: AccountId,
        message_id: i64,
        status: AttentionStatus,
    ) -> Result<bool> {
        let conn = self.lock()?;
        // Done stamps resolved_at; reopening (open/new) clears it. Sealed rows are
        // excluded so this can never touch a sealed message.
        let resolved_at = match status {
            AttentionStatus::Done => Some(Utc::now().to_rfc3339()),
            _ => None,
        };
        let n = conn.execute(
            "UPDATE triage
             SET status = ?1, resolved_at = ?2
             WHERE account_id = ?3 AND message_id = ?4 AND sensitivity != 'sealed'",
            params![status.as_str(), resolved_at, account_id, message_id],
        )?;
        Ok(n > 0)
    }

    fn delete_sender_rule(&self, account_id: AccountId, id: i64) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM sender_rules WHERE account_id=?1 AND id=?2",
            params![account_id, id],
        )?;
        Ok(n > 0)
    }

    fn sealed_body(&self, account_id: AccountId, message_id: i64) -> Result<SealedBody> {
        // HUMAN-DOOR-ONLY. Returns NotFound for a missing OR non-sealed message.
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT m.id, m.account_id, m.thread_id, m.from_addr, m.from_name,
                        m.subject, m.received_at, t.sealed_kind, m.body, m.body_html
                 FROM messages m
                 JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2 AND t.sensitivity = 'sealed'",
                params![account_id, message_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, Option<String>>(7)?,
                        r.get::<_, String>(8)?,
                        r.get::<_, Option<String>>(9)?,
                    ))
                },
            )
            .optional()?;
        let (
            id,
            acct,
            thread_id,
            from_addr,
            from_name,
            subject,
            received_at,
            sealed_kind,
            body,
            body_html,
        ) = row.ok_or(CoreError::NotFound)?;
        Ok(SealedBody {
            id,
            account_id: acct,
            thread_id,
            from_addr,
            from_name,
            subject,
            received_at: parse_dt(&received_at)?,
            sealed_kind,
            body,
            body_html,
        })
    }

    fn append_audit(&self, account_id: AccountId, entry: &NewAuditEntry) -> Result<i64> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO audit_log(account_id, ts, actor, action, target, detail)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                account_id,
                Utc::now().to_rfc3339(),
                entry.actor,
                entry.action,
                entry.target,
                entry.detail,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn append_event(&self, ev: &NewEvent) -> Result<Option<i64>> {
        let inserted = {
            let conn = self.lock()?;
            // INSERT OR IGNORE on UNIQUE(message_id): one event per message ever,
            // so a re-ingest or a second refined verdict stays silent (0 rows).
            let n = conn.execute(
                "INSERT OR IGNORE INTO events(account_id, message_id, thread_id, kind, tier,
                     importance, sender, one_line, deadline, created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    ev.account_id,
                    ev.message_id,
                    ev.thread_id,
                    ev.kind.as_str(),
                    ev.tier.as_str(),
                    ev.importance as i64,
                    ev.sender,
                    ev.one_line,
                    ev.deadline,
                    Utc::now().to_rfc3339(),
                ],
            )?;
            if n == 0 {
                return Ok(None);
            }
            conn.last_insert_rowid()
        };
        // Poke the broadcast AFTER dropping the connection lock. The payload is
        // only a hint, so a send error (nobody listening) is not a failure.
        if let Some(tx) = self.event_notifier() {
            let _ = tx.send(inserted);
        }
        Ok(Some(inserted))
    }

    fn events_after(
        &self,
        account_id: AccountId,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, message_id, thread_id, kind, tier, importance, sender, one_line,
                    deadline, created_at
             FROM events
             WHERE account_id = ?1 AND id > ?2
             ORDER BY id ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, after_id, limit as i64], map_event)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(finish_event(row?)?);
        }
        Ok(out)
    }

    fn event_by_id(&self, account_id: AccountId, id: i64) -> Result<Option<Event>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT id, message_id, thread_id, kind, tier, importance, sender, one_line,
                        deadline, created_at
                 FROM events
                 WHERE account_id = ?1 AND id = ?2",
                params![account_id, id],
                map_event,
            )
            .optional()?;
        match row {
            Some(r) => Ok(Some(finish_event(r)?)),
            None => Ok(None),
        }
    }

    fn latest_event_id(&self, account_id: AccountId) -> Result<i64> {
        let conn = self.lock()?;
        // COALESCE, so an account with no events reports the 0 cursor.
        let id: i64 = conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM events WHERE account_id = ?1",
            params![account_id],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    fn upsert_device(&self, account_id: AccountId, token: &str, platform: &str) -> Result<Device> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        // UPSERT on UNIQUE(token); `created_at` is deliberately NOT touched on
        // conflict — first sight is a fact worth keeping.
        //
        // The conflict update's account-scoped WHERE is the security property:
        // adopting `excluded.account_id` would let a re-registration silently
        // repoint another account's device. A cross-account collision therefore
        // changes 0 rows, and that is reported rather than swallowed — returning
        // the other account's row would be worse than the rebind.
        let changed = conn.execute(
            "INSERT INTO devices(account_id, token, platform, created_at, last_registered_at)
             VALUES(?1,?2,?3,?4,?4)
             ON CONFLICT(token) DO UPDATE SET
                 platform=excluded.platform,
                 last_registered_at=excluded.last_registered_at
             WHERE devices.account_id = excluded.account_id",
            params![account_id, token, platform, now],
        )?;
        if changed == 0 {
            // States the RULE, never the token: this string reaches the human
            // door as a 400 body.
            return Err(CoreError::InvalidInput(
                "device token is already registered to another account".to_string(),
            ));
        }
        let row = conn.query_row(
            "SELECT id, account_id, token, platform, created_at, last_registered_at
             FROM devices WHERE token = ?1",
            params![token],
            map_device,
        )?;
        finish_device(row)
    }

    fn list_devices(&self, account_id: AccountId) -> Result<Vec<Device>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, token, platform, created_at, last_registered_at
             FROM devices WHERE account_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![account_id], map_device)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(finish_device(row?)?);
        }
        Ok(out)
    }

    fn delete_device_by_token(&self, account_id: AccountId, token: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM devices WHERE account_id = ?1 AND token = ?2",
            params![account_id, token],
        )?;
        Ok(n > 0)
    }

    fn triage_debug(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<TriageDebug>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT t.message_id, m.subject, t.importance, t.tier, t.category,
                        t.one_line, t.reason, t.field_reasons, t.deadline,
                        t.matched_rule_id, t.status, t.surfaced_at, t.resolved_at,
                        t.stage1_model_used, t.model_used, t.needs_stage2,
                        t.extractor_model_used, t.created_at
                 FROM triage t
                 JOIN messages m ON m.id = t.message_id
                 WHERE t.account_id = ?1 AND t.message_id = ?2
                   AND COALESCE(t.sensitivity, 'normal') = 'normal'",
                params![account_id, message_id],
                |r| {
                    Ok(TriageDebug {
                        message_id: r.get(0)?,
                        subject: r.get(1)?,
                        importance: r.get(2)?,
                        tier: r.get(3)?,
                        category: r.get(4)?,
                        one_line: r.get(5)?,
                        reason: r.get(6)?,
                        field_reasons: r
                            .get::<_, Option<String>>(7)?
                            .as_deref()
                            .and_then(|s| serde_json::from_str(s).ok()),
                        deadline: r.get(8)?,
                        matched_rule_id: r.get(9)?,
                        status: r.get(10)?,
                        surfaced_at: r.get(11)?,
                        resolved_at: r.get(12)?,
                        stage1_model_used: r.get(13)?,
                        model_used: r.get(14)?,
                        needs_stage2: r.get::<_, i64>(15)? != 0,
                        extractor_model_used: r.get(16)?,
                        created_at: r.get(17)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    fn message_unsub_fields(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<MessageUnsub>> {
        let conn = self.lock()?;
        // SECURITY: sealed rows excluded in SQL, so an unsubscribe against sealed
        // mail resolves to `None` (=> 404) exactly like an unknown id.
        let row = conn
            .query_row(
                "SELECT m.from_addr, m.list_unsubscribe, m.list_unsub_one_click
                 FROM messages m
                 LEFT JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2
                   AND COALESCE(t.sensitivity, 'normal') != 'sealed'",
                params![account_id, message_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(|(from_addr, list_unsubscribe, one_click)| MessageUnsub {
            from_addr,
            list_unsubscribe,
            list_unsub_one_click: one_click != 0,
        }))
    }

    fn upsert_unsubscribe(
        &self,
        account_id: AccountId,
        sender: &str,
        method: &str,
        source_message_id: Option<i64>,
        requested_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.lock()?;
        // A fresh request RESETS the ledger — the user re-asked, so the 72h grace
        // clock restarts from this `requested_at`.
        conn.execute(
            "INSERT INTO unsubscribes(account_id, sender_addr, requested_at, method,
                 source_message_id, violation_count, last_violation_at, resolution)
             VALUES(?1,?2,?3,?4,?5,0,NULL,NULL)
             ON CONFLICT(account_id, sender_addr) DO UPDATE SET
                 requested_at=excluded.requested_at, method=excluded.method,
                 source_message_id=excluded.source_message_id,
                 violation_count=0, last_violation_at=NULL, resolution=NULL",
            params![
                account_id,
                sender,
                requested_at.to_rfc3339(),
                method,
                source_message_id,
            ],
        )?;
        Ok(())
    }

    fn list_unsubscribes(&self, account_id: AccountId) -> Result<Vec<UnsubscribeRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT sender_addr, requested_at, method, violation_count,
                    last_violation_at, resolution
             FROM unsubscribes
             WHERE account_id = ?1
             ORDER BY requested_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (sender, requested_at, method, violation_count, last_violation_at, resolution) =
                row?;
            out.push(UnsubscribeRecord {
                sender,
                requested_at: parse_dt(&requested_at)?,
                method,
                violation_count,
                last_violation_at: last_violation_at.as_deref().map(parse_dt).transpose()?,
                resolution,
            });
        }
        Ok(out)
    }

    fn set_unsubscribe_resolution(
        &self,
        account_id: AccountId,
        sender: &str,
        resolution: &str,
    ) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE unsubscribes SET resolution = ?3
             WHERE account_id = ?1 AND sender_addr = ?2",
            params![account_id, sender, resolution],
        )?;
        Ok(n > 0)
    }

    // ---- triage feedback (human corrections) -----------------------------

    fn correct_triage(
        &self,
        account_id: AccountId,
        message_id: i64,
        axis: TriageAxis,
        to_value: &str,
        note: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<TriageFeedback>> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;

        // Snapshot the row BEFORE touching it. Sealed rows are included on
        // purpose — a human correcting a misclassified auth message is exactly
        // the signal wanted — and no body is read, only verdict and envelope.
        let row = tx
            .query_row(
                "SELECT m.from_addr, m.subject, t.tier, t.category, t.importance,
                        t.one_line, t.reason, t.sensitivity, t.sealed_kind,
                        t.stage1_model_used, t.model_used, t.matched_rule_id
                 FROM messages m
                 JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2",
                params![account_id, message_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, Option<String>>(8)?,
                        r.get::<_, Option<String>>(9)?,
                        r.get::<_, Option<String>>(10)?,
                        r.get::<_, Option<i64>>(11)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            sender,
            subject,
            tier,
            category,
            importance,
            one_line,
            reason,
            sensitivity,
            sealed_kind,
            stage1_model,
            model,
            matched_rule_id,
        )) = row
        else {
            return Ok(None);
        };

        let from_value = match axis {
            TriageAxis::Tier => Some(tier.clone()),
            TriageAxis::Category => category.clone(),
            TriageAxis::Sensitivity => Some(sensitivity.clone()),
        };

        // The features that produced the verdict, alongside the verdict itself —
        // refinement needs both.
        let original = serde_json::json!({
            "tier": tier,
            "category": category,
            "importance": importance,
            "one_line": one_line,
            "reason": reason,
            "sensitivity": sensitivity,
            "sealed_kind": sealed_kind,
            "stage1_model_used": stage1_model,
            "model_used": model,
            "matched_rule_id": matched_rule_id,
        });

        // Apply the correction and stamp the row HUMAN-DECIDED: 'human' in the
        // model columns takes it out of both LLM queue predicates, so a later
        // pass cannot overwrite the person who corrected it and make the
        // feedback dataset record corrections that never stuck.
        let column = match axis {
            TriageAxis::Tier => "tier",
            TriageAxis::Category => "category",
            TriageAxis::Sensitivity => "sensitivity",
        };
        tx.execute(
            &format!(
                "UPDATE triage
                 SET {column} = ?3,
                     stage1_model_used = 'human',
                     model_used = 'human',
                     needs_stage2 = 0
                 WHERE account_id = ?1 AND message_id = ?2"
            ),
            params![account_id, message_id, to_value],
        )?;

        // SEALING has consequences beyond the one column: sealed rows carry a
        // NULL category and the specialist tables hold no sealed rows BY
        // CONSTRUCTION. Sealing a row that was already categorized and extracted
        // would falsify both and leave the message in the Marketing/Banking zones
        // while the Auth page also claims it, so drop the category and the
        // extracted rows.
        if matches!(axis, TriageAxis::Sensitivity) && to_value == "sealed" {
            tx.execute(
                "UPDATE triage SET category = NULL, extractor_model_used = NULL
                 WHERE account_id = ?1 AND message_id = ?2",
                params![account_id, message_id],
            )?;
            tx.execute(
                "DELETE FROM marketing WHERE account_id = ?1 AND message_id = ?2",
                params![account_id, message_id],
            )?;
            tx.execute(
                "DELETE FROM banking WHERE account_id = ?1 AND message_id = ?2",
                params![account_id, message_id],
            )?;
            // And the NOTIFICATION EVENT, if one was already emitted: its
            // sender + one_line snapshot replays to every client cursor forever,
            // including onto a lock screen, and sealed content must not reach a
            // notification surface.
            //
            // REDACT, NEVER DELETE. `events.id` is `INTEGER PRIMARY KEY` without
            // AUTOINCREMENT, so it is the rowid and SQLite hands the largest free
            // one to the next insert — deleting the newest event would let the
            // next `append_event` REUSE that id, and every durable cursor already
            // past it would skip that event permanently. The row stays and only
            // its CONTENT goes; a replaying client renders its generic fallback.
            tx.execute(
                "UPDATE events SET sender = '', one_line = '', deadline = NULL
                 WHERE account_id = ?1 AND message_id = ?2",
                params![account_id, message_id],
            )?;
        }

        let original_s = original.to_string();
        tx.execute(
            "INSERT INTO triage_feedback(account_id, message_id, corrected_at, dimension,
                                         from_value, to_value, original, sender, subject, note)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                account_id,
                message_id,
                now.to_rfc3339(),
                axis.as_str(),
                from_value,
                to_value,
                original_s,
                sender,
                subject,
                note,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;

        Ok(Some(TriageFeedback {
            id,
            message_id,
            corrected_at: now,
            dimension: axis.as_str().to_string(),
            from_value,
            to_value: to_value.to_string(),
            original,
            sender,
            subject,
            note: note.map(|s| s.to_string()),
        }))
    }

    fn list_triage_feedback(
        &self,
        account_id: AccountId,
        limit: u32,
    ) -> Result<Vec<TriageFeedback>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, message_id, corrected_at, dimension, from_value, to_value,
                    original, sender, subject, note
             FROM triage_feedback
             WHERE account_id = ?1
             ORDER BY corrected_at DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account_id, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, Option<String>>(9)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, message_id, at, dimension, from_value, to_value, original, sender, subject, note) =
                row?;
            out.push(TriageFeedback {
                id,
                message_id,
                corrected_at: parse_dt(&at)?,
                dimension,
                from_value,
                to_value,
                // One unparseable snapshot must not take the whole list down.
                original: serde_json::from_str(&original).unwrap_or(serde_json::Value::Null),
                sender,
                subject,
                note,
            });
        }
        Ok(out)
    }

    // ---- auth-mail shredder (retention) ----------------------------------

    fn shred_candidates(
        &self,
        account_id: AccountId,
        cutoff: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ShredCandidate>> {
        let conn = self.lock()?;
        // Sealed rows older than the cutoff and not already shredded. OLDEST
        // FIRST so a capped pass makes monotonic progress across runs, and
        // `gmail_msg_id <> ''` because an unaddressable row cannot be trashed.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.gmail_msg_id, m.from_addr, t.sealed_kind, m.received_at
             FROM messages m
             JOIN triage t ON t.message_id = m.id
             LEFT JOIN shred_log s
               ON s.message_id = m.id AND s.account_id = m.account_id
             WHERE m.account_id = ?1
               AND t.sensitivity = 'sealed'
               AND m.received_at <= ?2
               AND m.gmail_msg_id IS NOT NULL AND m.gmail_msg_id <> ''
               AND s.id IS NULL
             ORDER BY m.received_at ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, cutoff.to_rfc3339(), limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (message_id, gmail_msg_id, sender, kind, received_at) = row?;
            out.push(ShredCandidate {
                message_id,
                gmail_msg_id,
                sender,
                kind,
                received_at: parse_dt(&received_at)?,
            });
        }
        Ok(out)
    }

    fn shred_pending_count(&self, account_id: AccountId, cutoff: DateTime<Utc>) -> Result<i64> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*)
             FROM messages m
             JOIN triage t ON t.message_id = m.id
             LEFT JOIN shred_log s
               ON s.message_id = m.id AND s.account_id = m.account_id
             WHERE m.account_id = ?1
               AND t.sensitivity = 'sealed'
               AND m.received_at <= ?2
               AND m.gmail_msg_id IS NOT NULL AND m.gmail_msg_id <> ''
               AND s.id IS NULL",
            params![account_id, cutoff.to_rfc3339()],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    fn record_shred(
        &self,
        account_id: AccountId,
        candidate: &ShredCandidate,
        shredded_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.lock()?;
        // DO NOTHING on conflict: a message already in the ledger has already
        // been trashed, and re-running the pass must never inflate the count.
        conn.execute(
            "INSERT INTO shred_log(account_id, message_id, gmail_msg_id, sender, kind,
                                   received_at, shredded_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(account_id, message_id) DO NOTHING",
            params![
                account_id,
                candidate.message_id,
                candidate.gmail_msg_id,
                candidate.sender,
                candidate.kind,
                candidate.received_at.to_rfc3339(),
                shredded_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn shred_counts(
        &self,
        account_id: AccountId,
        recent_since: DateTime<Utc>,
    ) -> Result<(i64, i64, Option<DateTime<Utc>>)> {
        let conn = self.lock()?;
        let (recent, total, last): (i64, i64, Option<String>) = conn.query_row(
            "SELECT
               COALESCE(SUM(CASE WHEN shredded_at >= ?2 THEN 1 ELSE 0 END), 0),
               COUNT(*),
               MAX(shredded_at)
             FROM shred_log WHERE account_id = ?1",
            params![account_id, recent_since.to_rfc3339()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let last_at = match last {
            Some(s) => Some(parse_dt(&s)?),
            None => None,
        };
        Ok((recent, total, last_at))
    }

    fn list_audit(&self, account_id: AccountId, limit: u32) -> Result<Vec<AuditEntry>> {
        let conn = self.lock()?;
        // Enrich each row with the targeted message's sender/subject. `target` is
        // TEXT and often non-numeric (rule patterns, senders); SQLite CASTs those
        // to 0, which cannot match a real id, so the LEFT JOIN yields NULLs
        // instead of erroring. Sealed messages ARE joined (human door) — their
        // sender/subject already show on the Auth tab, and no CONTENT is selected.
        let mut stmt = conn.prepare(
            "SELECT a.id, a.account_id, a.ts, a.actor, a.action, a.target, a.detail,
                    m.from_addr, m.from_name, m.subject
             FROM audit_log a
             LEFT JOIN messages m
               ON m.id = CAST(a.target AS INTEGER) AND m.account_id = a.account_id
             WHERE a.account_id=?1 ORDER BY a.id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account_id, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, acct, ts, actor, action, target, detail, from_addr, from_name, subject) = row?;
            // from_name if present, else from_addr; both None when the join found
            // no message.
            let target_sender = from_name.filter(|s| !s.is_empty()).or(from_addr);
            out.push(AuditEntry {
                id,
                account_id: acct,
                ts: parse_dt(&ts)?,
                actor,
                action,
                target,
                detail,
                target_sender,
                target_subject: subject,
            });
        }
        Ok(out)
    }

    fn stats(&self, account_id: AccountId) -> Result<StoreStats> {
        let conn = self.lock()?;

        let mut tier_counts = std::collections::BTreeMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT tier, COUNT(*) FROM triage
                 WHERE account_id=?1 AND sensitivity != 'sealed'
                 GROUP BY tier",
            )?;
            let rows = stmt.query_map(params![account_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (tier, n) = row?;
                tier_counts.insert(tier, n);
            }
        }
        let total: i64 = tier_counts.values().sum();

        let sealed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM triage WHERE account_id=?1 AND sensitivity='sealed'",
            params![account_id],
            |r| r.get(0),
        )?;

        let last_history_id: Option<i64> = conn
            .query_row(
                "SELECT last_uid FROM sync_state WHERE account_id=?1 AND mailbox='history'",
                params![account_id],
                |r| r.get(0),
            )
            .optional()?;

        // Sitrep band counts over non-sealed rows. These definitions MUST match
        // the `band` query on attention_updates, or header and list disagree.
        let (standing, new_count, open_count): (i64, i64, i64) = conn.query_row(
            "SELECT
                 COUNT(*) FILTER (
                     WHERE tier IN ('past_due','deadline') AND status != 'done'),
                 COUNT(*) FILTER (WHERE surfaced_at IS NULL AND status != 'done'),
                 COUNT(*) FILTER (WHERE status = 'open')
             FROM triage
             WHERE account_id = ?1 AND sensitivity != 'sealed'",
            params![account_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        let last_surfaced_s: Option<String> = conn.query_row(
            "SELECT MAX(surfaced_at) FROM triage
             WHERE account_id = ?1 AND sensitivity != 'sealed'",
            params![account_id],
            |r| r.get(0),
        )?;
        let last_surfaced_at = match last_surfaced_s {
            Some(s) => Some(parse_dt(&s)?),
            None => None,
        };

        Ok(StoreStats {
            tier_counts,
            total,
            sealed,
            last_history_id: last_history_id.map(|v| v as u64),
            bands: BandCounts {
                standing,
                new: new_count,
                open: open_count,
            },
            last_surfaced_at,
        })
    }

    // ---- STAGE-2 ----------------------------------------------------------

    fn stage1_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage1Queued>> {
        let conn = self.lock()?;
        // Rows still needing Stage-1: heuristic seed values in place
        // (stage1_model_used IS NULL), non-sealed, non-sent. Rule-decided rows
        // carry stage1_model_used='rule' and are excluded.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.thread_id, m.from_addr, m.subject, m.body, t.sensitivity,
                    m.received_at,
                    EXISTS(
                        SELECT 1 FROM contacts c
                        WHERE c.account_id = m.account_id
                          AND c.addr = m.from_addr COLLATE NOCASE
                          AND c.sent_count > 0
                    ) AS is_known
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1
               AND t.stage1_model_used IS NULL
               AND t.sensitivity = 'normal'
               AND m.is_sent = 0
             ORDER BY m.received_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account_id, limit as i64], |r| {
            let sensitivity: String = r.get(5)?;
            let received_at: String = r.get(6)?;
            let is_known: i64 = r.get(7)?;
            Ok((
                Stage1Queued {
                    message_id: r.get(0)?,
                    account_id,
                    thread_id: r.get(1)?,
                    from_addr: r.get(2)?,
                    subject: r.get(3)?,
                    body: r.get(4)?,
                    received_at: Utc::now(), // replaced below after parse
                    is_known_contact: is_known != 0,
                    sensitivity: Sensitivity::parse(&sensitivity),
                },
                received_at,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (mut q, received_at) = row?;
            q.received_at = parse_dt(&received_at)?;
            out.push(q);
        }
        Ok(out)
    }

    fn stage1_apply(&self, applied: &Stage1Applied) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let deadline_dt = applied.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
        let field_reasons_json = if applied.field_reasons.is_empty() {
            None
        } else {
            serde_json::to_string(&applied.field_reasons).ok()
        };
        // Overwrite the seed values, stamp stage1_model_used (leaving the queue),
        // and set the escalation flag, leaving `model_used` (the Stage-2 marker)
        // untouched. Guarded by sensitivity='normal'.
        let n = tx.execute(
            "UPDATE triage SET
                 importance = ?3,
                 tier = ?4,
                 one_line = ?5,
                 reason = ?6,
                 deadline = ?7,
                 stage1_model_used = ?8,
                 needs_stage2 = ?9,
                 field_reasons = ?10,
                 category = COALESCE(?11, category)
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![
                applied.message_id,
                applied.account_id,
                applied.importance as i64,
                applied.tier.as_str(),
                applied.one_line,
                applied.reason,
                deadline_dt,
                applied.stage1_model_used,
                applied.needs_stage2 as i64,
                field_reasons_json,
                applied.category,
            ],
        )?;
        // TOCTOU: a row sealed by hand between the queue SELECT and this apply
        // makes the guard match nothing. The verdict did NOT land, so skip the
        // deadlines rewrite too — a sealed message must not grow a fresh deadline
        // row — and report `false` so the caller emits no event.
        if n > 0 {
            tx.execute(
                "DELETE FROM deadlines WHERE message_id=?1",
                params![applied.message_id],
            )?;
            if let Some(d) = &applied.deadline {
                tx.execute(
                    "INSERT INTO deadlines(account_id, message_id, kind, amount, currency,
                         due_at, past_due, source)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        applied.account_id,
                        applied.message_id,
                        d.kind,
                        d.amount,
                        d.currency,
                        d.due_at.to_rfc3339(),
                        d.past_due as i64,
                        d.source,
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(n > 0)
    }

    fn stage1_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        stage1_model_used: &str,
    ) -> Result<()> {
        let conn = self.lock()?;
        // Stamp only the Stage-1 marker; PRESERVE the needs_stage2 seed so the
        // heuristic-confidence decision still drives escalation.
        conn.execute(
            "UPDATE triage SET stage1_model_used = ?3
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![message_id, account_id, stage1_model_used],
        )?;
        Ok(())
    }

    fn stage1_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        let conn = self.lock()?;
        bump_usage_category(&conn, account_id, day, "stage1", input_tokens, output_tokens)
    }

    fn stage1_usage_since(&self, account_id: AccountId, since_day: &str) -> Result<Stage2Usage> {
        let conn = self.lock()?;
        usage_since_category(&conn, account_id, since_day, "stage1")
    }

    fn extract_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        category: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        let conn = self.lock()?;
        bump_usage_category(&conn, account_id, day, category, input_tokens, output_tokens)
    }

    fn list_usage_stage1(&self, account_id: AccountId, days: u32) -> Result<Vec<Stage2UsageDay>> {
        let conn = self.lock()?;
        list_usage_category(&conn, account_id, days, "stage1")
    }

    fn stage2_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage2Queued>> {
        let conn = self.lock()?;
        // Queue predicate: Stage-1 finished the row, flagged it for escalation,
        // and Stage-2 has not processed it. Sealed rows are structurally
        // excluded. The LEFT JOIN carries a matched Filtered rule's want_text.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.thread_id, m.from_addr, m.subject, m.body, t.sensitivity,
                    sr.want_text, m.received_at,
                    EXISTS(
                        SELECT 1 FROM contacts c
                        WHERE c.account_id = m.account_id
                          AND c.addr = m.from_addr COLLATE NOCASE
                          AND c.sent_count > 0
                    ) AS is_known
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             LEFT JOIN sender_rules sr ON sr.id = t.matched_rule_id
             WHERE t.account_id = ?1
               AND t.stage1_model_used IS NOT NULL
               AND t.needs_stage2 = 1
               AND t.model_used IS NULL
               AND t.sensitivity = 'normal'
               AND m.is_sent = 0
             ORDER BY m.received_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account_id, limit as i64], |r| {
            let sensitivity: String = r.get(5)?;
            let want_text: Option<String> = r.get(6)?;
            let received_at: String = r.get(7)?;
            let is_known: i64 = r.get(8)?;
            Ok((
                Stage2Queued {
                    message_id: r.get(0)?,
                    account_id,
                    thread_id: r.get(1)?,
                    from_addr: r.get(2)?,
                    subject: r.get(3)?,
                    body: r.get(4)?,
                    received_at: Utc::now(), // replaced below after parse
                    is_known_contact: is_known != 0,
                    rule_want_text: want_text.filter(|s| !s.is_empty()),
                    sensitivity: Sensitivity::parse(&sensitivity),
                },
                received_at,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (mut q, received_at) = row?;
            q.received_at = parse_dt(&received_at)?;
            out.push(q);
        }
        Ok(out)
    }

    fn stage2_budget_used(
        &self,
        account_id: AccountId,
        thread_id: &str,
        day: &str,
    ) -> Result<u32> {
        let conn = self.lock()?;
        let n: i64 = conn
            .query_row(
                "SELECT model_calls FROM wake_budget
                 WHERE account_id=?1 AND thread_id=?2 AND day=?3",
                params![account_id, thread_id, day],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(n.max(0) as u32)
    }

    fn stage2_increment_budget(
        &self,
        account_id: AccountId,
        thread_id: &str,
        day: &str,
    ) -> Result<u32> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO wake_budget(account_id, thread_id, day, model_calls)
             VALUES(?1, ?2, ?3, 1)
             ON CONFLICT(account_id, thread_id, day)
             DO UPDATE SET model_calls = model_calls + 1",
            params![account_id, thread_id, day],
        )?;
        let n: i64 = conn.query_row(
            "SELECT model_calls FROM wake_budget
             WHERE account_id=?1 AND thread_id=?2 AND day=?3",
            params![account_id, thread_id, day],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u32)
    }

    fn stage2_apply(&self, applied: &Stage2Applied) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        // Overwrite triage fields and stamp model_used, guarded by
        // sensitivity='normal' so a mis-targeted sealed row is never mutated.
        let deadline_dt = applied.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
        // Stage-2 owns all three properties on apply, so its reasons fully
        // replace any Stage-1 blob.
        let field_reasons_json = if applied.field_reasons.is_empty() {
            None
        } else {
            serde_json::to_string(&applied.field_reasons).ok()
        };
        let n = tx.execute(
            "UPDATE triage SET
                 importance = ?3,
                 tier = ?4,
                 one_line = ?5,
                 reason = ?6,
                 deadline = ?7,
                 model_used = ?8,
                 field_reasons = ?9,
                 category = COALESCE(?10, category)
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![
                applied.message_id,
                applied.account_id,
                applied.importance as i64,
                applied.tier.as_str(),
                applied.one_line,
                applied.reason,
                deadline_dt,
                applied.model_used,
                field_reasons_json,
                applied.category,
            ],
        )?;
        // TOCTOU, as in `stage1_apply`: a row sealed mid-pass makes the guard
        // match nothing, so no verdict landed, no deadline row is written, and
        // `false` keeps the caller from emitting an event.
        if n > 0 {
            tx.execute(
                "DELETE FROM deadlines WHERE message_id=?1",
                params![applied.message_id],
            )?;
            if let Some(d) = &applied.deadline {
                tx.execute(
                    "INSERT INTO deadlines(account_id, message_id, kind, amount, currency,
                         due_at, past_due, source)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        applied.account_id,
                        applied.message_id,
                        d.kind,
                        d.amount,
                        d.currency,
                        d.due_at.to_rfc3339(),
                        d.past_due as i64,
                        d.source,
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(n > 0)
    }

    fn stage2_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        model_used: &str,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE triage SET model_used = ?3
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![message_id, account_id, model_used],
        )?;
        Ok(())
    }

    fn stage2_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        let conn = self.lock()?;
        bump_usage_category(&conn, account_id, day, "stage2", input_tokens, output_tokens)
    }

    fn stage2_usage_today(&self, account_id: AccountId, day: &str) -> Result<Stage2Usage> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT calls, input_tokens, output_tokens FROM stage2_usage
                 WHERE account_id = ?1 AND day = ?2 AND category = 'stage2'",
                params![account_id, day],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row
            .map(|(calls, in_tok, out_tok)| Stage2Usage {
                calls: calls.max(0) as u64,
                input_tokens: in_tok.max(0) as u64,
                output_tokens: out_tok.max(0) as u64,
            })
            .unwrap_or_default())
    }

    fn list_usage(&self, account_id: AccountId, days: u32) -> Result<Vec<Stage2UsageDay>> {
        let conn = self.lock()?;
        list_usage_category(&conn, account_id, days, "stage2")
    }

    fn stage2_usage_since(&self, account_id: AccountId, since_day: &str) -> Result<Stage2Usage> {
        let conn = self.lock()?;
        usage_since_category(&conn, account_id, since_day, "stage2")
    }

    fn get_app_setting(&self, account_id: AccountId, key: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE account_id = ?1 AND key = ?2",
                params![account_id, key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v)
    }

    fn set_app_setting(&self, account_id: AccountId, key: &str, value: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO app_settings(account_id, key, value)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(account_id, key) DO UPDATE SET value = excluded.value",
            params![account_id, key, value],
        )?;
        Ok(())
    }

    fn stage2_cap_overrides(&self, account_id: AccountId) -> Result<Stage2CapOverrides> {
        let conn = self.lock()?;
        // One SELECT pulls all four cap rows (only those that exist come back).
        let mut stmt = conn.prepare(
            "SELECT key, value FROM app_settings
             WHERE account_id = ?1 AND key IN (?2, ?3, ?4, ?5)",
        )?;
        // A stored value only counts if it parses as an integer in the valid
        // range; anything else is treated as absent (fall back to config/default).
        let valid = |s: String| -> Option<u32> {
            s.trim()
                .parse::<u32>()
                .ok()
                .filter(|n| (crate::config::STAGE2_CAP_MIN..=crate::config::STAGE2_CAP_MAX).contains(n))
        };
        let mut out = Stage2CapOverrides::default();
        let rows = stmt.query_map(
            params![
                account_id,
                crate::config::APP_SETTING_THREAD_DAILY_CAP,
                crate::config::APP_SETTING_SENDER_DAILY_CAP,
                crate::config::APP_SETTING_GLOBAL_DAILY_CAP,
                crate::config::APP_SETTING_STAGE1_GLOBAL_DAILY_CAP,
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        for row in rows {
            let (key, value) = row?;
            match key.as_str() {
                k if k == crate::config::APP_SETTING_THREAD_DAILY_CAP => {
                    out.thread_daily_cap = valid(value)
                }
                k if k == crate::config::APP_SETTING_SENDER_DAILY_CAP => {
                    out.sender_daily_cap = valid(value)
                }
                k if k == crate::config::APP_SETTING_GLOBAL_DAILY_CAP => {
                    out.global_daily_cap = valid(value)
                }
                k if k == crate::config::APP_SETTING_STAGE1_GLOBAL_DAILY_CAP => {
                    out.stage1_global_daily_cap = valid(value)
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn count_inbound_since(&self, account_id: AccountId, since: DateTime<Utc>) -> Result<u64> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages
             WHERE account_id = ?1 AND is_sent = 0 AND received_at >= ?2",
            params![account_id, since.to_rfc3339()],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    fn upsert_message_vector(
        &self,
        account_id: AccountId,
        message_id: i64,
        embedding: &[f32],
    ) -> Result<()> {
        if embedding.len() != VEC_DIMS {
            return Err(CoreError::InvalidInput(format!(
                "embedding len {} != vec0 width {VEC_DIMS}",
                embedding.len()
            )));
        }
        let conn = self.lock()?;
        // vec0 rejects a re-INSERT on an existing rowid, so delete-then-insert
        // keeps re-embed idempotent.
        conn.execute(
            "DELETE FROM message_vecs WHERE message_id = ?1",
            params![message_id],
        )?;
        conn.execute(
            "INSERT INTO message_vecs(message_id, embedding, account_id)
             VALUES (?1, ?2, ?3)",
            params![message_id, embedding.as_bytes(), account_id],
        )?;
        Ok(())
    }

    fn messages_missing_vectors(
        &self,
        account_id: AccountId,
        limit: usize,
    ) -> Result<Vec<MissingVector>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT m.id, m.subject, m.body
             FROM messages m
             JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?1
               AND t.sensitivity = 'normal'
               AND NOT EXISTS (
                   SELECT 1 FROM message_vecs v WHERE v.message_id = m.id
               )
             ORDER BY m.received_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account_id, limit as i64], |r| {
            Ok(MissingVector {
                message_id: r.get(0)?,
                subject: r.get(1)?,
                body: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Trait override so a generic `S: Store` caller resolves the CURRENT
    /// embedder, including one attached late in the background.
    fn embedder(&self) -> Option<std::sync::Arc<dyn crate::embed::Embedder>> {
        SqliteStore::embedder(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SealedKind, Sensitivity, Tier};
    use chrono::TimeZone;

    #[test]
    fn ingest_message_persists_attachments_and_thread_view_carries_them() {
        use crate::config::Stage1Config;
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let eml = "From: S <s@ex.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: files\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/mixed; boundary=\"B\"\r\n\
                   \r\n\
                   --B\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
                   --B\r\nContent-Type: application/pdf\r\n\
                   Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n\
                   --B--\r\n";
        let fetched = crate::sync::ingest::RawFetched {
            account_id: acct,
            gmail_msg_id: "g1".into(),
            gmail_thread_id: Some("t1".into()),
            raw: eml.as_bytes().to_vec(),
            internal_date: Some(Utc::now()),
            is_sent: false,
            account_addr: "me@example.com".into(),
        };
        let t = crate::sync::ingest::ingest(&fetched, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.attachments.len(), 1, "one pdf attachment extracted");
        let mid = store.ingest_message(&t).unwrap();

        // Thread view carries the attachment metadata (downloadable = true).
        let view = store.thread_view_with_html(acct, "t1").unwrap();
        assert_eq!(view.messages.len(), 1);
        let atts = &view.messages[0].attachments;
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "doc.pdf");
        assert_eq!(atts[0].mime, "application/pdf");
        assert_eq!(atts[0].size, 5);
        assert!(atts[0].downloadable);

        // Bytes come back through attachment_bytes.
        let got = store.attachment_bytes(acct, atts[0].id).unwrap().expect("bytes");
        assert_eq!(got.0, "doc.pdf");
        assert_eq!(got.2.as_deref(), Some(&b"Hello"[..]));

        // Re-ingest is idempotent: still exactly one attachment row.
        let mid2 = store.ingest_message(&t).unwrap();
        assert_eq!(mid, mid2);
        let view2 = store.thread_view_with_html(acct, "t1").unwrap();
        assert_eq!(view2.messages[0].attachments.len(), 1, "re-ingest must not duplicate");
    }

    #[test]
    fn double_attached_identical_file_cannot_kill_ingest() {
        // REMOTE INGEST DoS: two parts with the SAME filename and size violate
        // the UNIQUE key, which must collapse to one row rather than roll back
        // the whole message ingest.
        use crate::config::Stage1Config;
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let eml = "From: S <s@ex.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: dup\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/mixed; boundary=\"B\"\r\n\
                   \r\n\
                   --B\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
                   --B\r\nContent-Type: application/pdf\r\n\
                   Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n\
                   --B\r\nContent-Type: application/pdf\r\n\
                   Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n\
                   --B--\r\n";
        let fetched = crate::sync::ingest::RawFetched {
            account_id: acct,
            gmail_msg_id: "g-dup".into(),
            gmail_thread_id: Some("t-dup".into()),
            raw: eml.as_bytes().to_vec(),
            internal_date: Some(Utc::now()),
            is_sent: false,
            account_addr: "me@example.com".into(),
        };
        let t = crate::sync::ingest::ingest(&fetched, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.attachments.len(), 2, "both parts extracted");
        // The ingest MUST NOT error — identical duplicates collapse to one row.
        store.ingest_message(&t).expect("duplicate attachments must not fail ingest");
        let view = store.thread_view_with_html(acct, "t-dup").unwrap();
        assert_eq!(view.messages[0].attachments.len(), 1);
    }

    #[test]
    fn attachment_bytes_guards_sealed_overcap_and_unknown() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // Normal parent with a stored attachment and an over-cap (NULL data) one.
        let mid = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(mid, acct, 10, Tier::Noise, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        let a_ok = store
            .insert_attachment(acct, mid, "doc.pdf", "application/pdf", 5, Some(b"Hello"))
            .unwrap();
        let a_over = store
            .insert_attachment(acct, mid, "big.bin", "application/octet-stream", 11_000_000, None)
            .unwrap();

        // Normal parent, bytes present.
        let ok = store.attachment_bytes(acct, a_ok).unwrap().expect("row");
        assert_eq!(ok.2.as_deref(), Some(&b"Hello"[..]));

        // Over-cap: row resolves but data is None (endpoint -> 410).
        let over = store.attachment_bytes(acct, a_over).unwrap().expect("metadata row exists");
        assert!(over.2.is_none(), "over-cap attachment carries no bytes");

        // Unknown id -> None (endpoint -> 404).
        assert!(store.attachment_bytes(acct, 999_999).unwrap().is_none());

        // Sealed parent: attachment is stored, but attachment_bytes hides it
        // (returns None, indistinguishable from unknown -> 404).
        let sid = store.upsert_message(&sample_msg(acct, "g2", "t2")).unwrap();
        store
            .set_triage(
                sid,
                acct,
                0,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "",
                "",
                None,
            )
            .unwrap();
        let sealed_att = store
            .insert_attachment(acct, sid, "secret.pdf", "application/pdf", 6, Some(b"secret"))
            .unwrap();
        assert!(
            store.attachment_bytes(acct, sealed_att).unwrap().is_none(),
            "sealed parent must hide its attachment bytes"
        );
    }

    fn sample_msg(account_id: AccountId, gmail_id: &str, thread: &str) -> NewMessage {
        NewMessage {
            account_id,
            gmail_msg_id: gmail_id.to_string(),
            thread_id: thread.to_string(),
            from_addr: "alice@example.com".to_string(),
            from_name: Some("Alice".to_string()),
            subject: "Lunch?".to_string(),
            received_at: Utc::now(),
            snippet: "want to grab lunch".to_string(),
            body: "Hey, want to grab lunch tomorrow?".to_string(),
            body_html: None,
            is_sent: false,
            list_unsubscribe: None,
            list_unsub_one_click: false,
        }
    }

    /// Build a store-ready TriagedMessage carrying a receipt (noise tier), for
    /// the auto-resolve tests.
    fn receipt_triaged(
        acct: AccountId,
        gmail: &str,
        thread: &str,
        amount: Option<f64>,
    ) -> TriagedMessage {
        let mut m = sample_msg(acct, gmail, thread);
        m.from_addr = "no-reply@baywheels.com".into();
        m.from_name = Some("Bay Wheels".into());
        m.subject = "Your Bay Wheels ride receipt".into();
        TriagedMessage {
            message: m,
            recipients: vec![],
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
            importance: 15,
            tier: Tier::Noise,
            one_line: "Your Bay Wheels ride receipt".into(),
            reason: "receipt".into(),
            matched_rule: None,
            field_reasons: crate::types::FieldReasons::default(),
            deadline: None,
            shipment: None,
            receipt: Some(crate::triage::ReceiptInfo {
                amount,
                currency: amount.map(|_| "USD".into()),
            }),
            calendar: None,
            attachments: vec![],
            confident: true,
        }
    }

    /// A plain, normal, non-receipt inbound (or sent) TriagedMessage with an
    /// explicit sender + received_at, for the unsubscribe violation tests.
    fn inbound_triaged(
        acct: AccountId,
        gmail: &str,
        thread: &str,
        from: &str,
        received_at: DateTime<Utc>,
        is_sent: bool,
    ) -> TriagedMessage {
        let mut m = sample_msg(acct, gmail, thread);
        m.from_addr = from.into();
        m.from_name = None;
        m.received_at = received_at;
        m.is_sent = is_sent;
        TriagedMessage {
            message: m,
            recipients: vec![],
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
            importance: 10,
            tier: Tier::Noise,
            one_line: String::new(),
            reason: String::new(),
            matched_rule: None,
            field_reasons: crate::types::FieldReasons::default(),
            deadline: None,
            shipment: None,
            receipt: None,
            calendar: None,
            attachments: vec![],
            confident: true,
        }
    }

    #[test]
    fn migrate_adds_unsub_columns_to_a_preexisting_messages_table() {
        // Simulate an existing install whose `messages` predates the unsubscribe
        // columns. The migration must add them, and be idempotent on re-open.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages(
                 id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
                 gmail_msg_id TEXT NOT NULL, body TEXT);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // idempotent
        let mut stmt = conn.prepare("PRAGMA table_info(messages)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(cols.iter().any(|c| c == "list_unsubscribe"));
        assert!(cols.iter().any(|c| c == "list_unsub_one_click"));
    }

    #[test]
    fn migrate_adds_field_reasons_to_a_preexisting_triage_table() {
        // An install whose `triage` has no field_reasons. `model_used`/`status`
        // are present because the two-stage backfill in migrate() reads them.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE triage(
                 message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
                 importance INTEGER NOT NULL DEFAULT 0, reason TEXT,
                 model_used TEXT, status TEXT NOT NULL DEFAULT 'new');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // idempotent
        let mut stmt = conn.prepare("PRAGMA table_info(triage)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(cols.iter().any(|c| c == "field_reasons"));
    }

    #[test]
    fn migrate_adds_two_stage_columns_so_ingest_and_both_queues_work() {
        // A `triage` table with no stage1_model_used / needs_stage2 /
        // field_reasons. The migration must add them so ingest and both queue
        // SELECTs, which NAME those columns, don't fail with "no such column".
        register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE triage(
                 message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
                 importance INTEGER NOT NULL DEFAULT 0, tier TEXT NOT NULL DEFAULT 'noise',
                 sensitivity TEXT NOT NULL DEFAULT 'normal', sealed_kind TEXT,
                 one_line TEXT NOT NULL DEFAULT '', reason TEXT NOT NULL DEFAULT '',
                 deadline TEXT, matched_rule_id INTEGER, model_used TEXT,
                 status TEXT NOT NULL DEFAULT 'new', surfaced_at TEXT, resolved_at TEXT,
                 created_at TEXT NOT NULL);",
        )
        .unwrap();
        // init applies SCHEMA (IF NOT EXISTS preserves the existing triage
        // shape) then migrate(), which adds the two columns + backfill.
        let store = SqliteStore::init(conn).unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let id = store
            .ingest_message(&triaged_row(acct, "g-x", "t-x", None, false, Sensitivity::Normal))
            .unwrap();
        // Both queue SELECTs run cleanly on the upgraded table.
        let s1 = store.stage1_queue(acct, 10).unwrap();
        assert_eq!(s1.len(), 1, "the fresh normal row enters Stage-1");
        assert_eq!(s1[0].message_id, id);
        assert!(store.stage2_queue(acct, 10).unwrap().is_empty());
    }

    #[test]
    fn migrate_rebuilds_stage2_usage_with_category_pk() {
        // A pre-category stage2_usage table (PK account_id+day) must be rebuilt
        // — ALTER ADD COLUMN can't extend a PK, and the bump upsert's
        // ON CONFLICT(account_id, day, category) needs the new unique index.
        // Existing rows land under category='stage2'; bumps work after.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE stage2_usage (
                 account_id INTEGER NOT NULL, day TEXT NOT NULL,
                 calls INTEGER NOT NULL DEFAULT 0,
                 input_tokens INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY(account_id, day));
             INSERT INTO stage2_usage VALUES (1, '2026-07-01', 3, 100, 50);
             CREATE TABLE triage(message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
                 model_used TEXT, status TEXT NOT NULL DEFAULT 'new',
                 tier TEXT NOT NULL DEFAULT 'noise', deadline TEXT);
             CREATE TABLE deadlines(id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
                 message_id INTEGER NOT NULL, kind TEXT NOT NULL, due_at TEXT NOT NULL,
                 past_due INTEGER NOT NULL DEFAULT 0, source TEXT NOT NULL);
             CREATE TABLE messages(id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
                 received_at TEXT NOT NULL);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        // Old row preserved under the default category.
        let (cat, calls): (String, i64) = conn
            .query_row(
                "SELECT category, calls FROM stage2_usage WHERE account_id=1 AND day='2026-07-01'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cat, "stage2");
        assert_eq!(calls, 3);
        // The category-keyed upsert now works (this was the failing bump).
        conn.execute(
            "INSERT INTO stage2_usage(account_id, day, category, calls, input_tokens, output_tokens)
             VALUES (1, '2026-07-01', 'stage1', 1, 10, 5)
             ON CONFLICT(account_id, day, category) DO UPDATE SET calls = calls + 1",
            [],
        )
        .unwrap();
        // Idempotent: migrate again is a no-op.
        migrate(&conn).unwrap();
    }

    #[test]
    fn migrate_cleans_year_slipped_stage_deadlines() {
        // A stage-sourced deadline far before its message's receipt is nulled
        // (tier demoted from past_due) and its deadlines row deleted;
        // deterministic-source rows are untouched.
        let store = SqliteStore::open_in_memory().unwrap();
        let conn = store.lock().unwrap();
        conn.execute_batch(
            "INSERT INTO accounts(id, email, created_at) VALUES (1, 'a@b', '2026-01-01T00:00:00Z');
             INSERT INTO messages(id, account_id, gmail_msg_id, thread_id, from_addr, subject,
                                  received_at, snippet)
                 VALUES (1, 1, 'g1', 't1', 'x@y', 's', '2026-07-23T00:00:00Z', ''),
                        (2, 1, 'g2', 't2', 'x@y', 's', '2026-07-23T00:00:00Z', '');
             INSERT INTO triage(message_id, account_id, tier, deadline, created_at, status)
                 VALUES (1, 1, 'past_due', '2025-07-24T00:00:00Z', '2026-07-23T00:00:00Z', 'new'),
                        (2, 1, 'past_due', '2025-07-24T00:00:00Z', '2026-07-23T00:00:00Z', 'new');
             INSERT INTO deadlines(account_id, message_id, kind, due_at, past_due, source)
                 VALUES (1, 1, 'bill', '2025-07-24T00:00:00Z', 1, 'stage1'),
                        (1, 2, 'bill', '2025-07-24T00:00:00Z', 1, 'subject');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        // Stage-sourced year-slip: deadline nulled, tier demoted, row gone.
        let (dl1, tier1): (Option<String>, String) = conn
            .query_row(
                "SELECT deadline, tier FROM triage WHERE message_id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(dl1, None);
        assert_eq!(tier1, "deadline");
        let n1: i64 = conn
            .query_row("SELECT COUNT(*) FROM deadlines WHERE message_id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n1, 0);
        // Deterministic source ('subject'): untouched (explicit text is data).
        let dl2: Option<String> = conn
            .query_row("SELECT deadline FROM triage WHERE message_id=2", [], |r| r.get(0))
            .unwrap();
        assert!(dl2.is_some(), "deterministic-source deadline must survive");
    }

    #[test]
    fn migrate_backfill_keeps_only_processed_history_out_of_stage1() {
        // Old-semantics rows on a pre-two-stage `triage` table; the backfill must
        // mark ONLY genuinely-processed history 'migrated' (out of the Stage-1
        // queue) and leave genuinely-unprocessed recent rows to re-queue.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE triage(
                 message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
                 model_used TEXT, status TEXT NOT NULL DEFAULT 'new');
             INSERT INTO triage(message_id, account_id, model_used, status) VALUES
                 (1, 1, NULL, 'open'),       -- finalized/seen (past 'new')
                 (2, 1, 'claude-x', 'new'),  -- old stage-2 processed
                 (3, 1, NULL, 'new');        -- genuinely new & unprocessed",
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // idempotent: backfill fires ONCE at column add

        let get = |mid: i64| -> (Option<String>, i64) {
            conn.query_row(
                "SELECT stage1_model_used, needs_stage2 FROM triage WHERE message_id=?1",
                [mid],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)),
            )
            .unwrap()
        };
        assert_eq!(get(1).0.as_deref(), Some("migrated"), "seen/finalized row out of Stage-1");
        assert_eq!(get(2).0.as_deref(), Some("migrated"), "old stage-2 processed row out");
        assert_eq!(get(3).0, None, "genuinely-new unprocessed row re-enters Stage-1");
        assert_eq!(get(3).1, 0, "residual row's needs_stage2 rests at 0 (Stage-1 recomputes)");
    }

    // ---- categorize-then-extract: schema migration + extractor queue -------

    #[test]
    fn migrate_adds_category_and_extractor_columns_to_preexisting_triage() {
        // A `triage` table predating the categorize-then-extract feature. The
        // additive migration must add both columns so the queue/apply SQL that
        // NAMES them doesn't fail with "no such column". Idempotent on re-open.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE triage(
                 message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
                 model_used TEXT, status TEXT NOT NULL DEFAULT 'new');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // idempotent
        let mut stmt = conn.prepare("PRAGMA table_info(triage)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(cols.iter().any(|c| c == "category"), "category column added");
        assert!(
            cols.iter().any(|c| c == "extractor_model_used"),
            "extractor_model_used column added"
        );
    }

    #[test]
    fn init_upgrades_old_db_with_banking_table_and_extract_flow() {
        // OLD-shape triage (no category / extractor_model_used) and NO banking
        // table. init() applies SCHEMA (creates the banking table IF NOT EXISTS)
        // then migrate() (adds the two triage columns), so the whole extract flow
        // works on the upgraded DB.
        register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE triage(
                 message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
                 importance INTEGER NOT NULL DEFAULT 0, tier TEXT NOT NULL DEFAULT 'noise',
                 sensitivity TEXT NOT NULL DEFAULT 'normal', sealed_kind TEXT,
                 one_line TEXT NOT NULL DEFAULT '', reason TEXT NOT NULL DEFAULT '',
                 deadline TEXT, matched_rule_id INTEGER, model_used TEXT,
                 status TEXT NOT NULL DEFAULT 'new', surfaced_at TEXT, resolved_at TEXT,
                 created_at TEXT NOT NULL);",
        )
        .unwrap();
        let store = SqliteStore::init(conn).unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let id = store
            .ingest_message(&triaged_row(acct, "g-b", "t-b", None, false, Sensitivity::Normal))
            .unwrap();
        apply_category(&store, acct, id, "banking_statement", false);

        // The banking table exists and the extract queue + apply work end to end.
        let cats = ["banking_statement", "transaction_alert"];
        let q = store.extract_queue(acct, &cats, 10).unwrap();
        assert_eq!(q.len(), 1);
        let applied = BankingApplied {
            message_id: id,
            account_id: acct,
            kind: "statement".into(),
            institution: Some("Chase".into()),
            amount: Some(1234.56),
            currency: Some("USD".into()),
            account_hint: Some("…1234".into()),
            received_at: Utc::now(),
            extractor_model_used: "claude-haiku-4-5".into(),
            auto_resolve: true,
        };
        store.banking_apply(&applied).unwrap();
        assert_eq!(store.list_banking(acct).unwrap().len(), 1);
    }

    /// Set a triage row's LLM category via the real Stage-1 apply path (the
    /// categorizer). `needs_stage2` is irrelevant to extraction but honored.
    fn apply_category(
        store: &SqliteStore,
        acct: AccountId,
        message_id: i64,
        category: &str,
        needs_stage2: bool,
    ) {
        store
            .stage1_apply(&Stage1Applied {
                message_id,
                account_id: acct,
                importance: 40,
                tier: Tier::Noise,
                one_line: "categorized".into(),
                reason: "stage-1".into(),
                field_reasons: crate::types::FieldReasons::default(),
                stage1_model_used: "claude-haiku-4-5".into(),
                needs_stage2,
                deadline: None,
                category: Some(category.into()),
            })
            .unwrap();
    }

    #[test]
    fn extract_queue_selects_banking_only_excludes_others_sealed_and_receipts() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // banking_statement + transaction_alert -> queued.
        let stmt = store
            .ingest_message(&triaged_row(acct, "g-stmt", "t1", None, false, Sensitivity::Normal))
            .unwrap();
        apply_category(&store, acct, stmt, "banking_statement", false);
        let alert = store
            .ingest_message(&triaged_row(acct, "g-alert", "t2", None, false, Sensitivity::Normal))
            .unwrap();
        apply_category(&store, acct, alert, "transaction_alert", false);

        // invoice + general -> NOT queued (no extractor).
        let inv = store
            .ingest_message(&triaged_row(acct, "g-inv", "t3", None, false, Sensitivity::Normal))
            .unwrap();
        apply_category(&store, acct, inv, "invoice", false);
        let genr = store
            .ingest_message(&triaged_row(acct, "g-gen", "t4", None, false, Sensitivity::Normal))
            .unwrap();
        apply_category(&store, acct, genr, "general", false);

        // A sealed row: category stays NULL (stage1_apply is guarded), excluded.
        let sealed = store
            .ingest_message(&triaged_row(acct, "g-seal", "t5", None, false, Sensitivity::Sealed))
            .unwrap();
        apply_category(&store, acct, sealed, "banking_statement", false); // no-op on sealed

        // A banking-categorized message that ALSO produced a receipt: excluded
        // (a receipt and a banking row must never double-create).
        let dual = store
            .ingest_message(&triaged_row(acct, "g-dual", "t6", None, false, Sensitivity::Normal))
            .unwrap();
        apply_category(&store, acct, dual, "banking_statement", false);
        store
            .upsert_receipt(
                acct,
                dual,
                "orders@shop.com",
                None,
                &crate::triage::ReceiptInfo { amount: Some(5.0), currency: Some("USD".into()) },
                Utc::now(),
            )
            .unwrap();

        let cats = ["banking_statement", "transaction_alert"];
        let q = store.extract_queue(acct, &cats, 20).unwrap();
        let ids: Vec<i64> = q.iter().map(|r| r.message_id).collect();
        assert_eq!(q.len(), 2, "only the two banking rows without a receipt: {ids:?}");
        assert!(ids.contains(&stmt));
        assert!(ids.contains(&alert));
        assert!(!ids.contains(&inv), "invoice has no extractor");
        assert!(!ids.contains(&genr), "general has no extractor");
        assert!(!ids.contains(&sealed), "sealed row carries a NULL category");
        assert!(!ids.contains(&dual), "receipt-bearing row is excluded");
    }

    fn triage_extract_status(store: &SqliteStore, message_id: i64) -> (String, Option<String>, Option<String>) {
        let conn = store.lock().unwrap();
        conn.query_row(
            "SELECT status, resolved_at, extractor_model_used FROM triage WHERE message_id=?1",
            params![message_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn banking_apply_writes_row_stamps_marker_and_auto_resolves() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = store
            .ingest_message(&triaged_row(acct, "g-stmt", "t1", None, false, Sensitivity::Normal))
            .unwrap();
        apply_category(&store, acct, id, "banking_statement", false);

        store
            .banking_apply(&BankingApplied {
                message_id: id,
                account_id: acct,
                kind: "statement".into(),
                institution: Some("Chase".into()),
                amount: Some(1234.56),
                currency: Some("USD".into()),
                account_hint: Some("…1234".into()),
                received_at: Utc::now(),
                extractor_model_used: "claude-haiku-4-5".into(),
                auto_resolve: true,
            })
            .unwrap();

        // The banking row landed with the extracted fields.
        let b = store.list_banking(acct).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].kind, "statement");
        assert_eq!(b[0].institution.as_deref(), Some("Chase"));
        assert_eq!(b[0].amount, Some(1234.56));
        assert_eq!(b[0].account_hint.as_deref(), Some("…1234"));

        // The triage row was stamped (leaves the queue) AND auto-resolved.
        let (status, resolved_at, marker) = triage_extract_status(&store, id);
        assert_eq!(status, "done", "banking statement auto-resolves");
        assert!(resolved_at.is_some(), "resolved_at stamped");
        assert_eq!(marker.as_deref(), Some("claude-haiku-4-5"));
        assert!(store.extract_queue(acct, &["banking_statement"], 10).unwrap().is_empty());
    }

    #[test]
    fn invoice_row_is_not_auto_resolved_and_stays_standing() {
        // Parity guard: an invoice is categorized but has NO extractor, so it is
        // never queued and never auto-resolved — it stays 'new' (standing).
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = store
            .ingest_message(&triaged_row(acct, "g-inv", "t1", None, false, Sensitivity::Normal))
            .unwrap();
        apply_category(&store, acct, id, "invoice", false);

        assert!(
            store.extract_queue(acct, &["banking_statement", "transaction_alert"], 10).unwrap().is_empty(),
            "invoice is never in the extract queue"
        );
        let (status, resolved_at, _) = triage_extract_status(&store, id);
        assert_eq!(status, "new", "invoice stays standing (not auto-resolved)");
        assert!(resolved_at.is_none());
    }

    #[test]
    fn extract_bump_usage_records_its_own_ledger_category() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        store
            .extract_bump_usage(acct, "2026-07-23", "extract_banking", 500, 20)
            .unwrap();
        store
            .extract_bump_usage(acct, "2026-07-23", "extract_banking", 300, 10)
            .unwrap();
        let conn = store.lock().unwrap();
        let (calls, in_tok, out_tok): (i64, i64, i64) = conn
            .query_row(
                "SELECT calls, input_tokens, output_tokens FROM stage2_usage
                 WHERE account_id=?1 AND day=?2 AND category='extract_banking'",
                params![acct, "2026-07-23"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(in_tok, 800);
        assert_eq!(out_tok, 30);
    }

    #[test]
    fn field_reasons_roundtrip_through_ingest_and_attention_updates() {
        use crate::types::FieldReasons;
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // Build a normal inbound TriagedMessage carrying per-property reasons.
        let mut t = inbound_triaged(acct, "g1", "t1", "boss@work.com", Utc::now(), false);
        t.importance = 72;
        t.tier = Tier::Signal;
        t.reason = "known contact".into();
        t.field_reasons = FieldReasons {
            importance: Some("known contact -> signal importance 72".into()),
            deadline: None,
            tier: Some("known contact -> signal".into()),
        };
        let id = store.ingest_message(&t).unwrap();

        // HUMAN DOOR: attention_updates carries the parsed field_reasons.
        let ups = store
            .attention_updates(acct, Utc::now() - chrono::Duration::days(1), None, None, None)
            .unwrap();
        let u = ups.iter().find(|u| u.update.id == id).expect("row present");
        let fr = u.update.field_reasons.as_ref().expect("field_reasons present");
        assert_eq!(fr.importance.as_deref(), Some("known contact -> signal importance 72"));
        assert_eq!(fr.tier.as_deref(), Some("known contact -> signal"));
        assert!(fr.deadline.is_none());
        // And it serializes into the /client/updates JSON as an object.
        let v = serde_json::to_value(&u.update).unwrap();
        assert_eq!(v["field_reasons"]["tier"], serde_json::json!("known contact -> signal"));

        // AGENT DOOR: ranked_updates (MCP) never carries field_reasons — the key
        // is absent from the serialized Update.
        let ranked = store
            .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), None)
            .unwrap();
        let r = ranked.iter().find(|u| u.id == id).expect("row present");
        assert!(r.field_reasons.is_none());
        let rv = serde_json::to_value(r).unwrap();
        assert!(rv.get("field_reasons").is_none(), "MCP payload must omit field_reasons: {rv}");
        // Same byte-absence discipline for the paperclip flag.
        assert!(r.has_attachments.is_none());
        assert!(
            rv.get("has_attachments").is_none(),
            "MCP payload must omit has_attachments: {rv}"
        );
    }

    #[test]
    fn predating_triage_row_reads_back_as_none() {
        // A row written with no field_reasons (NULL column) reads back as None.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let mid = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(mid, acct, 60, Tier::Signal, Sensitivity::Normal, None, "x", "y", None)
            .unwrap();
        let ups = store
            .attention_updates(acct, Utc::now() - chrono::Duration::days(1), None, None, None)
            .unwrap();
        let u = ups.iter().find(|u| u.update.id == mid).unwrap();
        assert!(u.update.field_reasons.is_none());
    }

    #[test]
    fn correcting_triage_applies_records_and_survives_retriage() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let id = store
            .ingest_message(&inbound_triaged(acct, "g1", "t1", "acme@x.com", t0, false))
            .unwrap();

        // The pipeline called it noise; the human says it is a deadline.
        let fb = store
            .correct_triage(acct, id, TriageAxis::Tier, "deadline", Some("this is a bill"), t0)
            .unwrap()
            .expect("message exists");
        assert_eq!(fb.dimension, "tier");
        assert_eq!(fb.from_value.as_deref(), Some("noise"));
        assert_eq!(fb.to_value, "deadline");
        assert_eq!(fb.sender, "acme@x.com");
        assert_eq!(fb.note.as_deref(), Some("this is a bill"));
        // The snapshot carries the FEATURES, not just the label — without them
        // the row is near-useless for refining anything.
        assert_eq!(fb.original["tier"], "noise");
        assert!(fb.original.get("importance").is_some());
        assert!(fb.original.get("reason").is_some());

        // The correction actually moved the row.
        let listed = store.list_triage_feedback(acct, 10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].to_value, "deadline");

        // THE INVARIANT THAT MATTERS: a later re-triage must not silently
        // overwrite the human. retriage_reset requeues rows for the LLM passes;
        // a human-stamped row must not come back into the queue.
        let reset = store.retriage_reset(acct, Some(id), 90).unwrap();
        assert_eq!(reset, 0, "a human-corrected row must not be requeued");
    }

    #[test]
    fn sealing_by_hand_clears_the_category_and_specialist_rows() {
        // Sealing a message the pipeline already categorized and extracted must
        // keep both invariants true — sealed rows carry a NULL category, and the
        // specialist tables hold no sealed rows.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let t0 = Utc::now();
        let id = store
            .ingest_message(&inbound_triaged(acct, "g1", "t1", "deals@shop.com", t0, false))
            .unwrap();

        // Pretend the pipeline categorized + extracted it as marketing.
        store
            .correct_triage(acct, id, TriageAxis::Category, "marketing", None, t0)
            .unwrap()
            .unwrap();
        store
            .marketing_apply(&crate::store::MarketingApplied {
                message_id: id,
                account_id: acct,
                brand: Some("Shop".into()),
                offer: Some("30% off".into()),
                discount: Some("30% off".into()),
                code: Some("SAVE30".into()),
                expires_at: None,
                received_at: t0,
                extractor_model_used: "m".into(),
            })
            .unwrap();
        assert_eq!(store.marketing_offers(acct, 30, 10).unwrap().len(), 1);

        // Now the human says: actually this is auth.
        store
            .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
            .unwrap()
            .unwrap();

        // The extracted row is gone, so it cannot show in the Marketing zone.
        assert_eq!(store.marketing_offers(acct, 30, 10).unwrap().len(), 0);
        // And it now reads as sealed auth mail.
        let sealed = store.sealed_messages(acct).unwrap();
        assert_eq!(sealed.len(), 1);
        assert_eq!(sealed[0].id, id);
    }

    #[test]
    fn sealing_by_hand_retracts_the_notification_event() {
        // A message can notify FIRST and be sealed by hand after, so sealing has
        // to retract the pre-seal snapshot every client cursor replays forever.
        //
        // Retraction is REDACTION, not deletion: `events.id` is the rowid, so
        // deleting the newest row would free that id for the next append, and
        // every durable cursor past it would skip the reused event forever.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let t0 = Utc::now();
        let id = store
            .ingest_message(&inbound_triaged(acct, "g1", "t1", "noreply@bank.com", t0, false))
            .unwrap();
        let other = store
            .ingest_message(&inbound_triaged(acct, "g2", "t2", "alice@x.com", t0, false))
            .unwrap();
        let sealed_ev = store
            .append_event(&NewEvent {
                deadline: Some("2026-08-01T17:00:00Z".to_string()),
                ..new_event(acct, id)
            })
            .unwrap()
            .unwrap();
        let keep = store.append_event(&new_event(acct, other)).unwrap().unwrap();
        let before = store.event_by_id(acct, sealed_ev).unwrap().unwrap();
        assert!(!before.sender.is_empty() && !before.one_line.is_empty());
        assert!(before.deadline.is_some());
        let keep_before = store.event_by_id(acct, keep).unwrap().unwrap();

        store
            .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
            .unwrap()
            .unwrap();

        // The row SURVIVES — the id must never be recyclable...
        let after = store
            .event_by_id(acct, sealed_ev)
            .unwrap()
            .expect("the id must stay taken; a freed rowid is a skipped notification later");
        // ...carrying nothing about the mail any more.
        assert_eq!(after.sender, "", "sealed content must not survive the seal");
        assert_eq!(after.one_line, "");
        assert_eq!(after.deadline, None);
        // Structure is untouched: the row is still addressable and orderable.
        assert_eq!(after.id, before.id);
        assert_eq!(after.message_id, before.message_id);
        assert_eq!(after.thread_id, before.thread_id);
        assert_eq!(after.created_at, before.created_at);

        // Replay sees both rows, in order, and only the sealed one is blanked.
        let left = store.events_after(acct, 0, 100).unwrap();
        assert_eq!(
            left.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![sealed_ev, keep],
            "redaction must not renumber or drop the log"
        );
        assert_eq!(left[0].one_line, "");
        assert_eq!(
            (left[1].sender.as_str(), left[1].one_line.as_str()),
            (keep_before.sender.as_str(), keep_before.one_line.as_str()),
            "and only THAT message's event is redacted"
        );

        // The next append gets a FRESH id rather than the sealed row's — which is
        // the whole reason this is an UPDATE.
        let third = store
            .ingest_message(&inbound_triaged(acct, "g3", "t3", "bob@x.com", t0, false))
            .unwrap();
        let next = store.append_event(&new_event(acct, third)).unwrap().unwrap();
        assert!(next > keep, "ids must keep moving forward, got {next}");

        assert_eq!(store.sealed_messages(acct).unwrap().len(), 1, "it WAS sealed");
    }

    #[test]
    fn unsealing_records_the_correction_and_frees_the_message() {
        // The other direction: over-sealing is a real failure mode (seal.rs
        // carries explicit guards against it), so it must be correctable.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let t0 = Utc::now();
        let id = store
            .ingest_message(&inbound_triaged(acct, "g1", "t1", "news@x.com", t0, false))
            .unwrap();
        store
            .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
            .unwrap()
            .unwrap();
        assert_eq!(store.sealed_messages(acct).unwrap().len(), 1);

        let fb = store
            .correct_triage(acct, id, TriageAxis::Sensitivity, "normal", None, t0)
            .unwrap()
            .unwrap();
        assert_eq!(fb.dimension, "sensitivity");
        assert_eq!(fb.from_value.as_deref(), Some("sealed"));
        assert_eq!(fb.to_value, "normal");
        assert_eq!(store.sealed_messages(acct).unwrap().len(), 0);
    }

    #[test]
    fn correcting_an_unknown_message_is_none_not_an_error() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let t0 = Utc::now();
        assert!(store
            .correct_triage(acct, 9999, TriageAxis::Category, "invoice", None, t0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn unsub_violation_bumps_only_after_grace_and_resets_on_rerequest() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .upsert_unsubscribe(acct, "news@x.com", "browser", None, t0)
            .unwrap();

        // Within the 72h grace => no violation.
        store
            .ingest_message(&inbound_triaged(
                acct,
                "g1",
                "t1",
                "news@x.com",
                t0 + chrono::Duration::hours(1),
                false,
            ))
            .unwrap();
        assert_eq!(store.list_unsubscribes(acct).unwrap()[0].violation_count, 0);

        // Past the grace => first violation, last_violation_at stamped.
        let v1_at = t0 + chrono::Duration::hours(80);
        store
            .ingest_message(&inbound_triaged(acct, "g2", "t2", "news@x.com", v1_at, false))
            .unwrap();
        let rec = &store.list_unsubscribes(acct).unwrap()[0];
        assert_eq!(rec.violation_count, 1);
        assert_eq!(rec.last_violation_at, Some(v1_at));

        // Another past-grace message => second violation.
        store
            .ingest_message(&inbound_triaged(
                acct,
                "g3",
                "t3",
                "news@x.com",
                t0 + chrono::Duration::hours(100),
                false,
            ))
            .unwrap();
        assert_eq!(store.list_unsubscribes(acct).unwrap()[0].violation_count, 2);

        // A FRESH request resets the ledger (clock restarts).
        let t_re = t0 + chrono::Duration::hours(200);
        store
            .upsert_unsubscribe(acct, "news@x.com", "browser", None, t_re)
            .unwrap();
        let rec = &store.list_unsubscribes(acct).unwrap()[0];
        assert_eq!(rec.violation_count, 0);
        assert!(rec.last_violation_at.is_none());
        assert!(rec.resolution.is_none());
    }

    #[test]
    fn unsub_violation_ignores_resolved_and_sent_and_is_case_insensitive() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .upsert_unsubscribe(acct, "news@x.com", "browser", None, t0)
            .unwrap();

        // A SENT message past the grace never counts as a violation.
        store
            .ingest_message(&inbound_triaged(
                acct,
                "gs",
                "ts",
                "news@x.com",
                t0 + chrono::Duration::hours(80),
                true,
            ))
            .unwrap();
        assert_eq!(store.list_unsubscribes(acct).unwrap()[0].violation_count, 0);

        // Mixed-case sender still matches the lowercased ledger key.
        store
            .ingest_message(&inbound_triaged(
                acct,
                "g1",
                "t1",
                "News@X.com",
                t0 + chrono::Duration::hours(80),
                false,
            ))
            .unwrap();
        assert_eq!(store.list_unsubscribes(acct).unwrap()[0].violation_count, 1);

        // Once resolved, the detector is disarmed.
        assert!(store.set_unsubscribe_resolution(acct, "news@x.com", "blocked").unwrap());
        store
            .ingest_message(&inbound_triaged(
                acct,
                "g2",
                "t2",
                "news@x.com",
                t0 + chrono::Duration::hours(100),
                false,
            ))
            .unwrap();
        assert_eq!(store.list_unsubscribes(acct).unwrap()[0].violation_count, 1);
    }

    #[test]
    fn message_unsub_fields_reads_stored_headers_and_hides_sealed() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // Normal message carrying unsubscribe headers.
        let mut normal = sample_msg(acct, "g1", "t1");
        normal.from_addr = "News@Sub.com".into();
        normal.list_unsubscribe = Some("<https://sub.com/u/1>".into());
        normal.list_unsub_one_click = true;
        let nid = store.upsert_message(&normal).unwrap();
        store
            .set_triage(nid, acct, 10, Tier::Noise, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        let f = store.message_unsub_fields(acct, nid).unwrap().expect("present");
        assert_eq!(f.from_addr, "News@Sub.com");
        assert_eq!(f.list_unsubscribe.as_deref(), Some("<https://sub.com/u/1>"));
        assert!(f.list_unsub_one_click);

        // Sealed message => None (indistinguishable from unknown).
        let mut sealed = sample_msg(acct, "g2", "t2");
        sealed.list_unsubscribe = Some("<https://sub.com/u/2>".into());
        let sid = store.upsert_message(&sealed).unwrap();
        store
            .set_triage(
                sid, acct, 90, Tier::Noise, Sensitivity::Sealed,
                Some(crate::types::SealedKind::Otp), "", "", None,
            )
            .unwrap();
        assert!(store.message_unsub_fields(acct, sid).unwrap().is_none());

        // Unknown id => None.
        assert!(store.message_unsub_fields(acct, 999_999).unwrap().is_none());
    }

    #[test]
    fn receipt_ingest_auto_resolves_and_lists_and_stays_out_of_bands() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(30);

        let id = store
            .ingest_message(&receipt_triaged(acct, "g-r1", "t-r1", Some(3.49)))
            .unwrap();

        // 1. The receipt row exists with its amount + clean sender.
        let receipts = store.list_receipts(acct, 30).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].amount, Some(3.49));
        assert_eq!(receipts[0].currency.as_deref(), Some("USD"));
        assert_eq!(receipts[0].from_addr, "no-reply@baywheels.com");
        assert_eq!(receipts[0].from_name.as_deref(), Some("Bay Wheels"));

        // 2. AUTO-RESOLVE: the triage row is status='done' with resolved_at set.
        let done = store
            .attention_updates(acct, since, None, Some(AttentionStatus::Done), None)
            .unwrap();
        assert_eq!(done.len(), 1, "receipt is auto-resolved to done");
        assert_eq!(done[0].update.id, id);
        assert!(done[0].resolved_at.is_some());

        // 3. It is ABSENT from the New band (never inbox clutter) even though it
        //    was never surfaced (surfaced_at IS NULL).
        let fresh = store
            .attention_updates(acct, since, None, None, Some(SitrepBand::New))
            .unwrap();
        assert!(fresh.is_empty(), "auto-done receipt must not be in the New band");

        // 4. Bands counts agree: new == 0, standing == 0.
        let stats = store.stats(acct).unwrap();
        assert_eq!(stats.bands.new, 0, "receipt excluded from new count");
        assert_eq!(stats.bands.standing, 0);
    }

    #[test]
    fn receipt_with_no_amount_still_lists() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        store
            .ingest_message(&receipt_triaged(acct, "g-r2", "t-r2", None))
            .unwrap();
        let receipts = store.list_receipts(acct, 30).unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].amount, None, "a receipt with no total is still a receipt");
    }

    // ---- calendar updates --------------------------------------------------

    /// Build a store-ready TriagedMessage carrying a calendar update (noise
    /// tier), for the auto-resolve/listing tests.
    fn calendar_triaged(
        acct: AccountId,
        gmail: &str,
        kind: crate::triage::CalendarKind,
        received_at: DateTime<Utc>,
    ) -> TriagedMessage {
        let mut m = sample_msg(acct, gmail, &format!("t-{gmail}"));
        m.from_addr = "sam@gmail.com".into();
        m.from_name = Some("Sam Doe".into());
        m.subject = "Invitation: Design review @ Wed Jul 22, 2026 10am".into();
        m.received_at = received_at;
        TriagedMessage {
            message: m,
            recipients: vec![],
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
            importance: 15,
            tier: Tier::Noise,
            one_line: "Invitation: Design review".into(),
            reason: "calendar".into(),
            matched_rule: None,
            field_reasons: crate::types::FieldReasons::default(),
            deadline: None,
            shipment: None,
            receipt: None,
            calendar: Some(crate::triage::CalendarInfo {
                kind,
                event_title: Some("Design review".into()),
                starts_at: Some(Utc.with_ymd_and_hms(2026, 7, 22, 10, 0, 0).unwrap()),
                organizer: Some("Sam Doe".into()),
            }),
            attachments: vec![],
            confident: true,
        }
    }

    #[test]
    fn calendar_ingest_auto_resolves_and_lists_and_stays_out_of_bands() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(30);

        let id = store
            .ingest_message(&calendar_triaged(
                acct,
                "g-cal1",
                crate::triage::CalendarKind::Invite,
                Utc::now(),
            ))
            .unwrap();

        // 1. The calendar row exists with its extracted fields.
        let items = store.list_calendar_updates(acct, 24).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message_id, id);
        // The joined thread is what the rail clicks through to; without it the
        // row can only jump to the mail page.
        assert_eq!(items[0].thread_id, "t-g-cal1");
        assert_eq!(items[0].kind, "invite");
        assert_eq!(items[0].event_title.as_deref(), Some("Design review"));
        assert_eq!(
            items[0].starts_at,
            Some(Utc.with_ymd_and_hms(2026, 7, 22, 10, 0, 0).unwrap())
        );
        assert_eq!(items[0].organizer.as_deref(), Some("Sam Doe"));

        // 2. AUTO-RESOLVE: the triage row is status='done' with resolved_at set
        //    (same mechanism as receipts — squelch-internal only; nothing is
        //    written back to Gmail).
        let done = store
            .attention_updates(acct, since, None, Some(AttentionStatus::Done), None)
            .unwrap();
        assert_eq!(done.len(), 1, "calendar update is auto-resolved to done");
        assert_eq!(done[0].update.id, id);
        assert!(done[0].resolved_at.is_some());

        // 3. ABSENT from the New band (never inbox clutter).
        let fresh = store
            .attention_updates(acct, since, None, None, Some(SitrepBand::New))
            .unwrap();
        assert!(fresh.is_empty(), "auto-done calendar update must not be in New");
        let stats = store.stats(acct).unwrap();
        assert_eq!(stats.bands.new, 0);
        assert_eq!(stats.bands.standing, 0);
    }

    #[test]
    fn calendar_list_windows_on_received_at_hours() {
        // The window is mail-ARRIVAL time (received_at), not event start.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        store
            .ingest_message(&calendar_triaged(
                acct,
                "g-cal-new",
                crate::triage::CalendarKind::Update,
                now - chrono::Duration::hours(2),
            ))
            .unwrap();
        store
            .ingest_message(&calendar_triaged(
                acct,
                "g-cal-old",
                crate::triage::CalendarKind::Cancellation,
                now - chrono::Duration::hours(30),
            ))
            .unwrap();

        // Default-ish 24h window: only the 2h-old row.
        let items = store.list_calendar_updates(acct, 24).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "update");
        // Wider window: both, newest-received first.
        let items = store.list_calendar_updates(acct, 48).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "update", "newest-received first");
        assert_eq!(items[1].kind, "cancellation");
    }

    #[test]
    fn calendar_upsert_is_idempotent_per_message() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let t = calendar_triaged(acct, "g-cal-i", crate::triage::CalendarKind::Invite, Utc::now());
        let id1 = store.ingest_message(&t).unwrap();
        let id2 = store.ingest_message(&t).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(
            store.list_calendar_updates(acct, 24).unwrap().len(),
            1,
            "re-ingest updates the same row"
        );
    }

    // ---- receipt -> open-bill auto-close ----------------------------------

    /// Build a store-ready TriagedMessage carrying a BILL (deadline tier) from
    /// the given sender, for the receipt->bill auto-close tests.
    fn bill_triaged(
        acct: AccountId,
        gmail: &str,
        from_addr: &str,
        from_name: Option<&str>,
        amount: Option<f64>,
        received_at: DateTime<Utc>,
        due_at: DateTime<Utc>,
    ) -> TriagedMessage {
        let mut m = sample_msg(acct, gmail, &format!("t-{gmail}"));
        m.from_addr = from_addr.into();
        m.from_name = from_name.map(Into::into);
        m.subject = "Your statement is ready".into();
        m.received_at = received_at;
        TriagedMessage {
            message: m,
            recipients: vec![],
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
            importance: 200,
            tier: Tier::Deadline,
            one_line: "Payment due".into(),
            reason: "bill".into(),
            matched_rule: None,
            field_reasons: crate::types::FieldReasons::default(),
            deadline: Some(crate::triage::DeadlineHit {
                kind: "payment_due".into(),
                amount,
                currency: amount.map(|_| "USD".into()),
                due_at,
                past_due: false,
                source: "test".into(),
            }),
            shipment: None,
            receipt: None,
            calendar: None,
            attachments: vec![],
            confident: true,
        }
    }

    /// Build a store-ready receipt TriagedMessage from the given sender.
    fn receipt_from(
        acct: AccountId,
        gmail: &str,
        from_addr: &str,
        from_name: Option<&str>,
        amount: Option<f64>,
        received_at: DateTime<Utc>,
    ) -> TriagedMessage {
        let mut t = receipt_triaged(acct, gmail, &format!("t-{gmail}"), amount);
        t.message.from_addr = from_addr.into();
        t.message.from_name = from_name.map(Into::into);
        t.message.received_at = received_at;
        t
    }

    /// Read (status, resolved_at) straight off a triage row.
    fn triage_status(store: &SqliteStore, acct: AccountId, id: i64) -> (String, Option<String>) {
        let conn = store.lock().unwrap();
        conn.query_row(
            "SELECT status, resolved_at FROM triage WHERE account_id=?1 AND message_id=?2",
            params![acct, id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    fn auto_close_audits(store: &SqliteStore, acct: AccountId) -> Vec<AuditEntry> {
        store
            .list_audit(acct, 50)
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "bill.auto_close")
            .collect()
    }

    #[test]
    fn receipt_matching_merchant_and_amount_closes_open_bill() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        // An open PG&E bill for $84.20, received 10 days ago.
        let bill_id = store
            .ingest_message(&bill_triaged(
                acct,
                "g-bill1",
                "billing@pge.com",
                Some("PG&E"),
                Some(84.20),
                now - chrono::Duration::days(10),
                now + chrono::Duration::days(5),
            ))
            .unwrap();

        // The payment receipt: different mailbox + subdomain, name spelled
        // "PGE", same amount.
        store
            .ingest_message(&receipt_from(
                acct,
                "g-pay1",
                "receipts@billing.pge.com",
                Some("PGE"),
                Some(84.20),
                now,
            ))
            .unwrap();

        // The bill's triage row is resolved through the standard transition
        // (done + resolved_at), so it leaves the standing/obligations band.
        let (status, resolved_at) = triage_status(&store, acct, bill_id);
        assert_eq!(status, "done", "matched bill auto-closes");
        assert!(resolved_at.is_some(), "done stamps resolved_at");
        assert_eq!(store.stats(acct).unwrap().bands.standing, 0);

        // The WHY is on the audit trail, targeting the bill's message id.
        let audits = auto_close_audits(&store, acct);
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].actor, "ingest");
        assert_eq!(audits[0].target.as_deref(), Some(bill_id.to_string().as_str()));
    }

    #[test]
    fn receipt_amount_mismatch_does_not_close_bill() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let bill_id = store
            .ingest_message(&bill_triaged(
                acct,
                "g-bill2",
                "billing@pge.com",
                Some("PG&E"),
                Some(84.20),
                now - chrono::Duration::days(10),
                now + chrono::Duration::days(5),
            ))
            .unwrap();
        // Same merchant, WRONG amount (a small partial charge, not the bill).
        store
            .ingest_message(&receipt_from(
                acct,
                "g-pay2",
                "receipts@pge.com",
                Some("PG&E"),
                Some(12.00),
                now,
            ))
            .unwrap();

        let (status, _) = triage_status(&store, acct, bill_id);
        assert_eq!(status, "new", "amount mismatch must NOT close the bill");
        assert!(auto_close_audits(&store, acct).is_empty());
    }

    #[test]
    fn receipt_without_amount_never_closes_an_amounted_bill() {
        // The bill has a verifiable amount but the receipt parsed none: the one
        // number we could check is missing — refuse (a false close hides an
        // unpaid bill).
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let bill_id = store
            .ingest_message(&bill_triaged(
                acct,
                "g-bill3",
                "billing@pge.com",
                Some("PG&E"),
                Some(84.20),
                now - chrono::Duration::days(3),
                now + chrono::Duration::days(12),
            ))
            .unwrap();
        store
            .ingest_message(&receipt_from(
                acct,
                "g-pay3",
                "receipts@pge.com",
                Some("PG&E"),
                None,
                now,
            ))
            .unwrap();

        let (status, _) = triage_status(&store, acct, bill_id);
        assert_eq!(status, "new");
        assert!(auto_close_audits(&store, acct).is_empty());
    }

    #[test]
    fn merchant_name_normalization_matches_across_domains() {
        // Different domains entirely; identity carried by the normalized
        // display name ("PG&E" == "PGE" after case/punctuation folding).
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let bill_id = store
            .ingest_message(&bill_triaged(
                acct,
                "g-bill4",
                "billing@pacificgas.com",
                Some("PG&E"),
                Some(84.20),
                now - chrono::Duration::days(7),
                now + chrono::Duration::days(7),
            ))
            .unwrap();
        store
            .ingest_message(&receipt_from(
                acct,
                "g-pay4",
                "no-reply@pge.com",
                Some("pge"),
                Some(84.20),
                now,
            ))
            .unwrap();

        let (status, _) = triage_status(&store, acct, bill_id);
        assert_eq!(status, "done", "normalized names establish the merchant");
    }

    #[test]
    fn already_done_bill_is_not_touched() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let bill_id = store
            .ingest_message(&bill_triaged(
                acct,
                "g-bill5",
                "billing@pge.com",
                Some("PG&E"),
                Some(84.20),
                now - chrono::Duration::days(10),
                now + chrono::Duration::days(5),
            ))
            .unwrap();
        // The user already dismissed it.
        assert!(store
            .set_attention_status(acct, bill_id, AttentionStatus::Done)
            .unwrap());

        store
            .ingest_message(&receipt_from(
                acct,
                "g-pay5",
                "receipts@pge.com",
                Some("PG&E"),
                Some(84.20),
                now,
            ))
            .unwrap();

        // Still done, and the auto-closer left no audit row (it never fired —
        // a done bill is not an open candidate, so no double-resolution).
        let (status, resolved_at) = triage_status(&store, acct, bill_id);
        assert_eq!(status, "done");
        assert!(resolved_at.is_some());
        assert!(auto_close_audits(&store, acct).is_empty());
    }

    #[test]
    fn receipt_with_no_matching_bill_does_nothing() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        // An open Comcast bill; the receipt is from an unrelated merchant.
        let bill_id = store
            .ingest_message(&bill_triaged(
                acct,
                "g-bill6",
                "billing@comcast.com",
                Some("Comcast"),
                Some(89.99),
                now - chrono::Duration::days(5),
                now + chrono::Duration::days(10),
            ))
            .unwrap();
        let receipt_id = store
            .ingest_message(&receipt_from(
                acct,
                "g-pay6",
                "no-reply@baywheels.com",
                Some("Bay Wheels"),
                Some(3.49),
                now,
            ))
            .unwrap();

        let (status, _) = triage_status(&store, acct, bill_id);
        assert_eq!(status, "new", "unrelated bill stays open");
        assert!(auto_close_audits(&store, acct).is_empty());
        // The receipt itself is still auto-resolved + listed as usual.
        let (rstatus, _) = triage_status(&store, acct, receipt_id);
        assert_eq!(rstatus, "done");
        assert_eq!(store.list_receipts(acct, 30).unwrap().len(), 1);
    }

    #[test]
    fn amountless_bill_closes_on_merchant_match_within_tight_window() {
        // The bill parsed no amount: merchant identity + the tight recency
        // window carry the match alone.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let bill_id = store
            .ingest_message(&bill_triaged(
                acct,
                "g-bill7",
                "billing@pge.com",
                Some("PG&E"),
                None,
                now - chrono::Duration::days(10),
                now + chrono::Duration::days(5),
            ))
            .unwrap();
        store
            .ingest_message(&receipt_from(
                acct,
                "g-pay7",
                "receipts@pge.com",
                Some("PG&E"),
                Some(84.20),
                now,
            ))
            .unwrap();

        let (status, _) = triage_status(&store, acct, bill_id);
        assert_eq!(status, "done");
        assert_eq!(auto_close_audits(&store, acct).len(), 1);
    }

    #[test]
    fn stale_bill_outside_recency_window_is_not_closed() {
        // Same merchant + amount, but the bill is 90 days old — outside even
        // the wide amount-verified window, so it stays open (stale history must
        // not be silently swept by a coincidental amount).
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let bill_id = store
            .ingest_message(&bill_triaged(
                acct,
                "g-bill8",
                "billing@pge.com",
                Some("PG&E"),
                Some(84.20),
                now - chrono::Duration::days(90),
                now - chrono::Duration::days(75),
            ))
            .unwrap();
        store
            .ingest_message(&receipt_from(
                acct,
                "g-pay8",
                "receipts@pge.com",
                Some("PG&E"),
                Some(84.20),
                now,
            ))
            .unwrap();

        let (status, _) = triage_status(&store, acct, bill_id);
        assert_eq!(status, "new");
        assert!(auto_close_audits(&store, acct).is_empty());
    }

    #[test]
    fn one_receipt_closes_only_the_earliest_due_of_identical_bills() {
        // Two open months of the same $15.49 subscription: one payment settles
        // ONE month — the earliest due. Closing both would hide the unpaid one.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let june = store
            .ingest_message(&bill_triaged(
                acct,
                "g-bill-jun",
                "billing@streamco.com",
                Some("StreamCo"),
                Some(15.49),
                now - chrono::Duration::days(40),
                now - chrono::Duration::days(25),
            ))
            .unwrap();
        let july = store
            .ingest_message(&bill_triaged(
                acct,
                "g-bill-jul",
                "billing@streamco.com",
                Some("StreamCo"),
                Some(15.49),
                now - chrono::Duration::days(10),
                now + chrono::Duration::days(5),
            ))
            .unwrap();
        store
            .ingest_message(&receipt_from(
                acct,
                "g-pay-jun",
                "receipts@streamco.com",
                Some("StreamCo"),
                Some(15.49),
                now,
            ))
            .unwrap();

        let (june_status, _) = triage_status(&store, acct, june);
        let (july_status, _) = triage_status(&store, acct, july);
        assert_eq!(june_status, "done", "earliest-due month is the one paid");
        assert_eq!(july_status, "new", "the newer month must stay open");
        assert_eq!(auto_close_audits(&store, acct).len(), 1);
    }

    #[test]
    fn round_trips_a_message() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(
                id, acct, 80, Tier::Signal, Sensitivity::Normal, None, "Lunch invite",
                "known contact", None,
            )
            .unwrap();

        let updates = store
            .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), Some(1))
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].sender, "alice@example.com");
        assert_eq!(updates[0].tier, Tier::Signal);

        let tv = store.thread_view(acct, "t1").unwrap();
        assert_eq!(tv.messages.len(), 1);
        assert_eq!(tv.subject, "Lunch?");
    }

    #[test]
    fn shipment_upsert_dedupes_and_state_machine_no_regress() {
        use crate::triage::{ShipmentInfo, ShipmentStatus};
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let mid = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();

        let ship = |status, item: &str| ShipmentInfo {
            carrier: "ups".into(),
            tracking_number: "1Z999AA10123456784".into(),
            item_name: item.into(),
            status,
            tracking_url: Some("https://www.ups.com/track?tracknum=1Z999AA10123456784".into()),
        };

        // First sight: shipped.
        let t0 = Utc::now();
        let id1 = store
            .upsert_shipment(acct, mid, &ship(ShipmentStatus::Shipped, ""), t0)
            .unwrap();
        // Second email, same tracking number: out_for_delivery + a better item
        // name. Must UPDATE the same row (dedupe), advance status, adopt name.
        let id2 = store
            .upsert_shipment(
                acct,
                mid,
                &ship(ShipmentStatus::OutForDelivery, "Wireless Headphones"),
                t0 + chrono::Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(id1, id2, "same tracking number dedupes to one row");

        let en_route = store.list_shipments(acct, false).unwrap();
        assert_eq!(en_route.len(), 1);
        assert_eq!(en_route[0].status, "out_for_delivery");
        assert_eq!(en_route[0].item_name, "Wireless Headphones");

        // Deliver it.
        store
            .upsert_shipment(
                acct,
                mid,
                &ship(ShipmentStatus::Delivered, ""),
                t0 + chrono::Duration::minutes(2),
            )
            .unwrap();
        // A LATE stale "shipped" email must NOT regress the delivered shipment.
        store
            .upsert_shipment(
                acct,
                mid,
                &ship(ShipmentStatus::Shipped, ""),
                t0 + chrono::Duration::minutes(3),
            )
            .unwrap();

        // En-route list now excludes it (delivered).
        assert!(store.list_shipments(acct, false).unwrap().is_empty());
        // include_delivered surfaces it, still delivered (no regress).
        let all = store.list_shipments(acct, true).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, "delivered", "delivered never regresses");
    }

    /// `thread_id_for_message` (the get_thread forgiveness fallback) resolves a
    /// normal message id to its thread, returns None for an unknown id, and
    /// returns None for a SEALED message id — so a sealed id is indistinguishable
    /// from a nonexistent one and never leaks thread existence.
    #[test]
    fn thread_id_for_message_resolves_normal_and_hides_sealed() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let normal = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(normal, acct, 80, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        let sealed = store.upsert_message(&sample_msg(acct, "g2", "t2")).unwrap();
        store
            .set_triage(
                sealed, acct, 90, Tier::Noise, Sensitivity::Sealed, Some(SealedKind::Otp), "", "",
                None,
            )
            .unwrap();

        assert_eq!(
            store.thread_id_for_message(acct, normal).unwrap().as_deref(),
            Some("t1")
        );
        assert_eq!(store.thread_id_for_message(acct, 999_999).unwrap(), None);
        assert_eq!(
            store.thread_id_for_message(acct, sealed).unwrap(),
            None,
            "sealed message id must not resolve (no thread-existence leak)"
        );
    }

    #[test]
    fn sealed_rows_absent_from_updates_but_present_in_sealed_messages() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // A normal message.
        let normal = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(
                normal, acct, 80, Tier::Signal, Sensitivity::Normal, None, "Lunch", "", None,
            )
            .unwrap();

        // A sealed OTP message in a different thread.
        let mut otp = sample_msg(acct, "g2", "t2");
        otp.subject = "Your verification code".to_string();
        otp.from_addr = "noreply@bank.com".to_string();
        let sealed_id = store.upsert_message(&otp).unwrap();
        store
            .set_triage(
                sealed_id,
                acct,
                90,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "code",
                "otp",
                None,
            )
            .unwrap();

        // ranked_updates must NOT include the sealed row.
        let updates = store
            .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), None)
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert!(updates.iter().all(|u| u.thread_id != "t2"));

        // thread_view on the sealed thread => NotFound.
        let err = store.thread_view(acct, "t2").unwrap_err();
        assert!(matches!(err, CoreError::NotFound));

        // Nonexistent thread also => NotFound (indistinguishable).
        let err2 = store.thread_view(acct, "does-not-exist").unwrap_err();
        assert!(matches!(err2, CoreError::NotFound));

        // The human-door html variant enforces the SAME guard: a sealed thread
        // (and a nonexistent one) are both NotFound, so html never leaks a
        // sealed thread either.
        assert!(matches!(
            store.thread_view_with_html(acct, "t2").unwrap_err(),
            CoreError::NotFound
        ));
        assert!(matches!(
            store
                .thread_view_with_html(acct, "does-not-exist")
                .unwrap_err(),
            CoreError::NotFound
        ));

        // sealed_messages (local-only) DOES surface it.
        let sealed = store.sealed_messages(acct).unwrap();
        assert_eq!(sealed.len(), 1);
        assert_eq!(sealed[0].thread_id, "t2");
        assert_eq!(sealed[0].sealed_kind.as_deref(), Some("otp"));
    }

    #[test]
    fn deadlines_exclude_sealed_source() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let mid = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(
                mid, acct, 50, Tier::Deadline, Sensitivity::Sealed, None, "", "", None,
            )
            .unwrap();

        {
            let conn = store.lock().unwrap();
            conn.execute(
                "INSERT INTO deadlines(account_id, message_id, kind, due_at, past_due, source)
                 VALUES(?1,?2,'bill',?3,0,'regex')",
                params![acct, mid, (Utc::now() + chrono::Duration::days(2)).to_rfc3339()],
            )
            .unwrap();
        }

        let ds = store.deadlines(acct, Some(30)).unwrap();
        assert!(ds.is_empty(), "sealed-source deadline must be hidden");
    }

    #[test]
    fn search_excludes_sealed_and_delete_rule_works() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let mut normal = sample_msg(acct, "g1", "t1");
        normal.subject = "verification steps".to_string();
        normal.body = "how to verify your account".to_string();
        let n = store.upsert_message(&normal).unwrap();
        store
            .set_triage(n, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        let mut sealed = sample_msg(acct, "g2", "t2");
        sealed.subject = "verification code".to_string();
        sealed.body = "code 999".to_string();
        let s = store.upsert_message(&sealed).unwrap();
        store
            .set_triage(
                s, acct, 90, Tier::Noise, Sensitivity::Sealed, Some(SealedKind::Otp), "", "", None,
            )
            .unwrap();

        let hits = store.search(acct, "verification", 10, 0).unwrap();
        assert_eq!(hits.len(), 1, "sealed row must be excluded from search");
        assert_eq!(hits[0].thread_id, "t1");

        // delete_sender_rule
        let rid = store
            .set_sender_rule(acct, "*@x.com", "no", Disposition::Squelch)
            .unwrap();
        assert!(store.delete_sender_rule(acct, rid).unwrap());
        assert!(!store.delete_sender_rule(acct, rid).unwrap());
        assert!(store.list_sender_rules(acct).unwrap().is_empty());
    }

    #[test]
    fn sealed_body_reveal_audit_and_stats() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let mut sealed = sample_msg(acct, "g1", "t1");
        sealed.body = "secret 123456".to_string();
        let s = store.upsert_message(&sealed).unwrap();
        store
            .set_triage(
                s, acct, 90, Tier::Noise, Sensitivity::Sealed, Some(SealedKind::Otp), "", "", None,
            )
            .unwrap();

        let mut normal = sample_msg(acct, "g2", "t2");
        normal.thread_id = "t2".to_string();
        let nid = store.upsert_message(&normal).unwrap();
        store
            .set_triage(nid, acct, 80, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        // sealed_body returns only for the sealed message.
        let body = store.sealed_body(acct, s).unwrap();
        assert_eq!(body.body, "secret 123456");
        assert!(matches!(
            store.sealed_body(acct, nid).unwrap_err(),
            CoreError::NotFound
        ));

        // audit append + list
        let aid = store
            .append_audit(
                acct,
                &crate::store::NewAuditEntry {
                    actor: "human".into(),
                    action: "reveal_sealed".into(),
                    target: Some(s.to_string()),
                    detail: None,
                },
            )
            .unwrap();
        assert!(aid > 0);
        let audit = store.list_audit(acct, 10).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].action, "reveal_sealed");

        // stats: 1 signal (t2), 1 sealed.
        let stats = store.stats(acct).unwrap();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.tier_counts.get("signal").copied(), Some(1));
        assert_eq!(stats.sealed, 1);
    }

    // --- sitrep seen-ledger --------------------------------------------------

    /// Helper: a non-sealed triaged message with a chosen tier/importance.
    fn ingest_normal(
        store: &SqliteStore,
        acct: AccountId,
        gmail: &str,
        thread: &str,
        tier: Tier,
        importance: u8,
        received: DateTime<Utc>,
    ) -> i64 {
        let mut m = sample_msg(acct, gmail, thread);
        m.received_at = received;
        let id = store.upsert_message(&m).unwrap();
        store
            .set_triage(id, acct, importance, tier, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        id
    }

    #[test]
    fn mark_surfaced_is_stamp_once_and_promotes_new_to_open() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(1);
        let id = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, Utc::now());

        // Pre-stamp: status new, surfaced_at NULL.
        let before = store
            .attention_updates(acct, since, None, None, None)
            .unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].status, AttentionStatus::New);
        assert!(before[0].surfaced_at.is_none());

        // First surface: stamps + promotes.
        let n = store.mark_surfaced(acct, &[id]).unwrap();
        assert_eq!(n, 1, "first surface counts as a transition");
        let after = store
            .attention_updates(acct, since, None, None, None)
            .unwrap();
        assert_eq!(after[0].status, AttentionStatus::Open);
        let stamp = after[0].surfaced_at.expect("surfaced_at set");

        // Second surface: idempotent, surfaced_at unchanged, no transition.
        let n2 = store.mark_surfaced(acct, &[id]).unwrap();
        assert_eq!(n2, 0, "second surface transitions nothing");
        let after2 = store
            .attention_updates(acct, since, None, None, None)
            .unwrap();
        assert_eq!(after2[0].surfaced_at, Some(stamp));
        assert_eq!(after2[0].status, AttentionStatus::Open);
    }

    #[test]
    fn band_queries_bucket_correctly() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(30);

        // A past_due bill (standing), a fresh signal (new), an aged signal.
        let bill = ingest_normal(&store, acct, "g1", "t1", Tier::PastDue, 90, Utc::now());
        let fresh = ingest_normal(&store, acct, "g2", "t2", Tier::Signal, 70, Utc::now());
        let aged = ingest_normal(
            &store,
            acct,
            "g3",
            "t3",
            Tier::Signal,
            60,
            Utc::now() - chrono::Duration::days(14),
        );

        // STANDING: only the bill (tier past_due/deadline, not done).
        let standing = store
            .attention_updates(acct, since, None, None, Some(SitrepBand::Standing))
            .unwrap();
        assert_eq!(standing.len(), 1);
        assert_eq!(standing[0].update.id, bill);

        // NEW: everything (nothing surfaced yet).
        let new = store
            .attention_updates(acct, since, None, None, Some(SitrepBand::New))
            .unwrap();
        assert_eq!(new.len(), 3);

        // Surface fresh + aged -> they become 'open'; bill stays new.
        store.mark_surfaced(acct, &[fresh, aged]).unwrap();

        // NEW now only the bill.
        let new2 = store
            .attention_updates(acct, since, None, None, Some(SitrepBand::New))
            .unwrap();
        assert_eq!(new2.len(), 1);
        assert_eq!(new2[0].update.id, bill);

        // OPEN band sorted by age*importance: aged (14d*60) before fresh (0d*70).
        let open = store
            .attention_updates(acct, since, None, None, Some(SitrepBand::Open))
            .unwrap();
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].update.id, aged, "older*importance floats to top");
        assert_eq!(open[1].update.id, fresh);
    }

    #[test]
    fn set_attention_status_resolves_and_reopens() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(1);
        let id = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, Utc::now());

        assert!(store
            .set_attention_status(acct, id, AttentionStatus::Done)
            .unwrap());
        let done = store
            .attention_updates(acct, since, None, Some(AttentionStatus::Done), None)
            .unwrap();
        assert_eq!(done.len(), 1);
        assert!(done[0].resolved_at.is_some(), "done stamps resolved_at");

        // Reopen clears resolved_at.
        assert!(store
            .set_attention_status(acct, id, AttentionStatus::Open)
            .unwrap());
        let open = store
            .attention_updates(acct, since, None, Some(AttentionStatus::Open), None)
            .unwrap();
        assert_eq!(open.len(), 1);
        assert!(open[0].resolved_at.is_none(), "reopen clears resolved_at");

        // Unknown id => false.
        assert!(!store
            .set_attention_status(acct, 999, AttentionStatus::Done)
            .unwrap());
    }

    #[test]
    fn sealed_rows_never_surface_through_the_ledger() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(1);

        let mut otp = sample_msg(acct, "g1", "t1");
        otp.subject = "Your verification code".to_string();
        let sealed = store.upsert_message(&otp).unwrap();
        store
            .set_triage(
                sealed,
                acct,
                90,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "",
                "",
                None,
            )
            .unwrap();

        // Never appears in attention_updates (any band).
        assert!(store
            .attention_updates(acct, since, None, None, None)
            .unwrap()
            .is_empty());
        assert!(store
            .attention_updates(acct, since, None, None, Some(SitrepBand::New))
            .unwrap()
            .is_empty());

        // mark_surfaced refuses to stamp a sealed row.
        let n = store.mark_surfaced(acct, &[sealed]).unwrap();
        assert_eq!(n, 0);
        // set_attention_status refuses a sealed row.
        assert!(!store
            .set_attention_status(acct, sealed, AttentionStatus::Done)
            .unwrap());

        // Stats: sealed row contributes to `sealed`, never to any band, and
        // never advances last_surfaced_at.
        let stats = store.stats(acct).unwrap();
        assert_eq!(stats.sealed, 1);
        assert_eq!(stats.bands.new, 0);
        assert_eq!(stats.bands.standing, 0);
        assert_eq!(stats.bands.open, 0);
        assert!(stats.last_surfaced_at.is_none());
    }

    #[test]
    fn stats_bands_and_last_surfaced_at() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let bill = ingest_normal(&store, acct, "g1", "t1", Tier::Deadline, 90, Utc::now());
        let sig = ingest_normal(&store, acct, "g2", "t2", Tier::Signal, 70, Utc::now());

        let s0 = store.stats(acct).unwrap();
        assert_eq!(s0.bands.standing, 1, "deadline tier counts as standing");
        assert_eq!(s0.bands.new, 2);
        assert_eq!(s0.bands.open, 0);
        assert!(s0.last_surfaced_at.is_none());

        store.mark_surfaced(acct, &[bill, sig]).unwrap();
        let s1 = store.stats(acct).unwrap();
        assert_eq!(s1.bands.new, 0, "both surfaced");
        assert_eq!(s1.bands.open, 2);
        assert_eq!(s1.bands.standing, 1, "surfacing doesn't change standing");
        assert!(s1.last_surfaced_at.is_some());
    }

    #[test]
    fn sender_rules_round_trip() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = store
            .set_sender_rule(acct, "*@newsletter.com", "no marketing", Disposition::Squelch)
            .unwrap();
        assert!(id > 0);
        let rules = store.list_sender_rules(acct).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].disposition, Disposition::Squelch);
    }

    // ---- Stage-1 LLM queue / markers -------------------------------------

    /// Build a store-ready TriagedMessage with controllable rule/confidence so
    /// the queue-marker logic in `ingest_message` is testable directly.
    fn triaged_row(
        acct: AccountId,
        gmail: &str,
        thread: &str,
        matched_rule: Option<i64>,
        confident: bool,
        sensitivity: Sensitivity,
    ) -> TriagedMessage {
        TriagedMessage {
            message: sample_msg(acct, gmail, thread),
            recipients: vec![],
            sensitivity,
            sealed_kind: if sensitivity == Sensitivity::Sealed {
                Some(crate::types::SealedKind::Otp)
            } else {
                None
            },
            importance: 40,
            tier: Tier::Noise,
            one_line: "seed".into(),
            reason: "seed".into(),
            field_reasons: crate::types::FieldReasons::default(),
            matched_rule,
            deadline: None,
            shipment: None,
            receipt: None,
            calendar: None,
            attachments: vec![],
            confident,
        }
    }

    #[test]
    fn stage1_queue_selects_normal_unrefined_excludes_rule_and_sealed() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // Normal, non-rule row -> enters the Stage-1 LLM queue.
        let normal = store
            .ingest_message(&triaged_row(acct, "g-n", "t-n", None, false, Sensitivity::Normal))
            .unwrap();
        // Explicit rule (confident) -> decided; NO Stage-1 model spend.
        store
            .ingest_message(&triaged_row(acct, "g-r", "t-r", Some(7), true, Sensitivity::Normal))
            .unwrap();
        // Sealed -> never queued for any LLM.
        store
            .ingest_message(&triaged_row(acct, "g-s", "t-s", None, false, Sensitivity::Sealed))
            .unwrap();

        let q = store.stage1_queue(acct, 10).unwrap();
        assert_eq!(q.len(), 1, "only the normal, non-rule row needs Stage-1");
        assert_eq!(q[0].message_id, normal);
        assert_eq!(q[0].sensitivity, Sensitivity::Normal);
    }

    #[test]
    fn retriage_reset_requeues_llm_rows_but_never_rule_or_sealed() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let normal = store
            .ingest_message(&triaged_row(acct, "g-n", "t-n", None, false, Sensitivity::Normal))
            .unwrap();
        store
            .ingest_message(&triaged_row(acct, "g-r", "t-r", Some(7), true, Sensitivity::Normal))
            .unwrap();
        store
            .ingest_message(&triaged_row(acct, "g-s", "t-s", None, false, Sensitivity::Sealed))
            .unwrap();

        // Simulate the LLM having classified the normal row (leaves the queue).
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "UPDATE triage SET stage1_model_used='claude-x', needs_stage2=1,
                        extractor_model_used='claude-x'
                 WHERE message_id=?1",
                rusqlite::params![normal],
            )
            .unwrap();
        }
        assert_eq!(store.stage1_queue(acct, 10).unwrap().len(), 0);

        // Window re-triage: only the LLM-classified normal row resets.
        let n = store.retriage_reset(acct, None, 7).unwrap();
        assert_eq!(n, 1, "rule + sealed rows must never reset");
        let q = store.stage1_queue(acct, 10).unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].message_id, normal);
        // The escalation + extractor markers were cleared too.
        {
            let conn = store.lock().unwrap();
            let (needs, ext): (i64, Option<String>) = conn
                .query_row(
                    "SELECT needs_stage2, extractor_model_used FROM triage WHERE message_id=?1",
                    rusqlite::params![normal],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(needs, 0);
            assert_eq!(ext, None);
        }

        // Single-message scope on a sealed message: resets nothing.
        let sealed_reset = store.retriage_reset(acct, Some(999_999), 7).unwrap();
        assert_eq!(sealed_reset, 0);
    }

    #[test]
    fn explicit_rule_row_skips_both_llm_queues() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // A Squelch/Surface rule row is final: not in Stage-1, not in Stage-2.
        store
            .ingest_message(&triaged_row(acct, "g-r", "t-r", Some(9), true, Sensitivity::Normal))
            .unwrap();
        assert!(store.stage1_queue(acct, 10).unwrap().is_empty());
        assert!(store.stage2_queue(acct, 10).unwrap().is_empty());
    }

    #[test]
    fn filtered_rule_row_goes_straight_to_stage2() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // A Filtered rule (matched_rule set, NOT confident) skips Stage-1 and
        // escalates directly to Stage-2 for want_text evaluation.
        let id = store
            .ingest_message(&triaged_row(acct, "g-f", "t-f", Some(3), false, Sensitivity::Normal))
            .unwrap();
        assert!(store.stage1_queue(acct, 10).unwrap().is_empty(), "no Stage-1 spend");
        let s2 = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].message_id, id);
    }

    #[test]
    fn stage1_apply_confident_false_escalates_true_does_not() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let a = store
            .ingest_message(&triaged_row(acct, "g-a", "t-a", None, false, Sensitivity::Normal))
            .unwrap();
        let b = store
            .ingest_message(&triaged_row(acct, "g-b", "t-b", None, false, Sensitivity::Normal))
            .unwrap();

        let applied = |mid: i64, needs_stage2: bool| Stage1Applied {
            message_id: mid,
            account_id: acct,
            importance: 60,
            tier: Tier::Noise,
            one_line: "refined".into(),
            reason: "stage-1".into(),
            field_reasons: crate::types::FieldReasons::default(),
            stage1_model_used: "claude-haiku-4-5".into(),
            needs_stage2,
            deadline: None,
            category: Some("general".into()),
        };
        store.stage1_apply(&applied(a, false)).unwrap(); // confident -> final
        store.stage1_apply(&applied(b, true)).unwrap(); // not confident -> escalate

        // Both left the Stage-1 queue.
        assert!(store.stage1_queue(acct, 10).unwrap().is_empty());
        // Only `b` is now in the Stage-2 queue.
        let s2 = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].message_id, b);
    }

    #[test]
    fn stage1_mark_processed_preserves_needs_stage2_seed() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        // Ambiguous seed (confident=false => needs_stage2 seed = 1).
        let amb = store
            .ingest_message(&triaged_row(acct, "g-amb", "t-amb", None, false, Sensitivity::Normal))
            .unwrap();
        // Confident seed (confident=true => needs_stage2 seed = 0).
        let sure = store
            .ingest_message(&triaged_row(acct, "g-sure", "t-sure", None, true, Sensitivity::Normal))
            .unwrap();

        // Heuristic-only fallback stamps the marker but PRESERVES the seed.
        store.stage1_mark_processed(acct, amb, HEURISTIC_ONLY_MARKER).unwrap();
        store.stage1_mark_processed(acct, sure, HEURISTIC_ONLY_MARKER).unwrap();

        assert!(store.stage1_queue(acct, 10).unwrap().is_empty());
        let s2 = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(s2.len(), 1, "only the ambiguous seed escalates");
        assert_eq!(s2[0].message_id, amb);
    }

    #[test]
    fn stage1_usage_ledger_is_a_separate_category() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        store.stage1_bump_usage(acct, "2026-07-09", 100, 20).unwrap();
        store.stage2_bump_usage(acct, "2026-07-09", 500, 90).unwrap();

        let s1 = store.stage1_usage_since(acct, "2026-07-01").unwrap();
        assert_eq!(s1.calls, 1);
        assert_eq!(s1.input_tokens, 100);
        assert_eq!(s1.output_tokens, 20);
        let s2 = store.stage2_usage_since(acct, "2026-07-01").unwrap();
        assert_eq!(s2.calls, 1);
        assert_eq!(s2.input_tokens, 500);

        let rows1 = store.list_usage_stage1(acct, 30).unwrap();
        assert_eq!(rows1.len(), 1);
        assert_eq!(rows1[0].input_tokens, 100);
        // The stage-2 list is unaffected by the stage-1 row.
        let rows2 = store.list_usage(acct, 30).unwrap();
        assert_eq!(rows2.len(), 1);
        assert_eq!(rows2[0].input_tokens, 500);
    }

    const HEURISTIC_ONLY_MARKER: &str = "heuristic-only";

    #[test]
    fn reingest_preserves_llm_classification_but_refreshes_heuristic_rows() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(3650);

        // --- Row A: LLM-classified, then re-delivered. ---
        let a = store
            .ingest_message(&triaged_row(acct, "g-a", "t-a", None, false, Sensitivity::Normal))
            .unwrap();
        // Stage-1 refines it with a REAL model id + distinctive values.
        store
            .stage1_apply(&Stage1Applied {
                message_id: a,
                account_id: acct,
                importance: 88,
                tier: Tier::Signal,
                one_line: "LLM verdict".into(),
                reason: "stage-1 refined".into(),
                field_reasons: crate::types::FieldReasons::default(),
                stage1_model_used: "claude-haiku-4-5".into(),
                needs_stage2: false,
                deadline: None,
                category: Some("general".into()),
            })
            .unwrap();
        // Re-deliver the SAME message (heuristic seed carries importance 40).
        store
            .ingest_message(&triaged_row(acct, "g-a", "t-a", None, false, Sensitivity::Normal))
            .unwrap();
        let ups = store.ranked_updates(acct, since, None).unwrap();
        let ua = ups.iter().find(|u| u.id == a).expect("row A present");
        assert_eq!(ua.importance, 88, "paid LLM importance preserved on re-ingest");
        assert_eq!(ua.one_line, "LLM verdict", "paid LLM one_line preserved");
        assert_eq!(ua.tier, Tier::Signal, "paid LLM tier preserved");

        // --- Row B: still heuristic-only -> re-ingest refreshes the seed. ---
        let b = store
            .ingest_message(&triaged_row(acct, "g-b", "t-b", None, false, Sensitivity::Normal))
            .unwrap();
        let mut refreshed = triaged_row(acct, "g-b", "t-b", None, false, Sensitivity::Normal);
        refreshed.importance = 71;
        refreshed.tier = Tier::Signal;
        refreshed.one_line = "fresh seed".into();
        store.ingest_message(&refreshed).unwrap();
        let ups = store.ranked_updates(acct, since, None).unwrap();
        let ub = ups.iter().find(|u| u.id == b).expect("row B present");
        assert_eq!(ub.importance, 71, "still-heuristic row adopts the new seed");
        assert_eq!(ub.one_line, "fresh seed");
    }

    // ---- Stage-2 store methods -------------------------------------------

    /// Insert a message + a triage row with model_used NULL (queued) or set
    /// (processed), controlling sensitivity so the sealed-exclusion is testable.
    fn seed_triage_row(
        store: &SqliteStore,
        acct: AccountId,
        gmail_id: &str,
        thread: &str,
        sensitivity: Sensitivity,
    ) -> i64 {
        let id = store
            .upsert_message(&sample_msg(acct, gmail_id, thread))
            .unwrap();
        store
            .set_triage(
                id, acct, 40, Tier::Noise, sensitivity, None, "ambiguous",
                "no rule matched", None,
            )
            .unwrap();
        id
    }

    #[test]
    fn stage2_queue_selects_only_normal_unprocessed_rows() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // A queued (normal, model_used NULL) row.
        let q1 = seed_triage_row(&store, acct, "g-normal", "t-1", Sensitivity::Normal);
        // A sealed row must be excluded.
        seed_triage_row(&store, acct, "g-sealed", "t-2", Sensitivity::Sealed);
        // A processed row (model_used set) must be excluded.
        let done = seed_triage_row(&store, acct, "g-done", "t-3", Sensitivity::Normal);
        store
            .stage2_mark_processed(acct, done, "claude-haiku-4-5")
            .unwrap();

        let rows = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(rows.len(), 1, "only the normal, unprocessed row is queued");
        assert_eq!(rows[0].message_id, q1);
        assert_eq!(rows[0].sensitivity, Sensitivity::Normal);
        assert!(rows[0].rule_want_text.is_none());
    }

    #[test]
    fn stage2_queue_surfaces_matched_rule_want_text() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let rule_id = store
            .set_sender_rule(
                acct,
                "*@shop.com",
                "only discounts, clearance, new collections",
                Disposition::Filtered,
            )
            .unwrap();
        let id = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(
                id, acct, 30, Tier::Noise, Sensitivity::Normal, None, "filtered",
                "matched filtered rule", None,
            )
            .unwrap();
        // Attach the matched rule id (set_triage leaves matched_rule_id NULL).
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "UPDATE triage SET matched_rule_id=?2 WHERE message_id=?1",
                params![id, rule_id],
            )
            .unwrap();
        }

        let rows = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].rule_want_text.as_deref(),
            Some("only discounts, clearance, new collections")
        );
    }

    #[test]
    fn stage2_prompt_carries_only_the_matched_rules_want_text() {
        // DETERMINISM: a Stage-2 prompt carries AT MOST the one want_text whose
        // rule id equals the row's matched_rule_id, never the others'. The queue
        // LEFT JOINs exactly `sr.id = t.matched_rule_id`, so the full rule list
        // is never fed to the prompt.
        use crate::triage::stage2::{RowContext, build_user_message};

        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // Three distinct Filtered rules, each with a unique, greppable want_text.
        let wants = [
            "WANT_ALPHA only closures",
            "WANT_BRAVO only invoices",
            "WANT_CHARLIE only shipments",
        ];
        let patterns = ["*@alpha.com", "*@bravo.com", "*@charlie.com"];
        let mut rule_ids = Vec::new();
        for (pat, want) in patterns.iter().zip(wants.iter()) {
            rule_ids.push(
                store
                    .set_sender_rule(acct, pat, want, Disposition::Filtered)
                    .unwrap(),
            );
        }

        // A queued row whose Stage-1 match landed on rule #2 (bravo). We stamp
        // matched_rule_id exactly as Stage-1 would (it selects a single rule id).
        let matched_id = rule_ids[1];
        let id = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(
                id, acct, 30, Tier::Noise, Sensitivity::Normal, None, "filtered",
                "matched filtered rule", None,
            )
            .unwrap();
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "UPDATE triage SET matched_rule_id=?2 WHERE message_id=?1",
                params![id, matched_id],
            )
            .unwrap();
        }

        let rows = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(rows.len(), 1);
        // Only the matched rule's want_text surfaces from the store.
        assert_eq!(rows[0].rule_want_text.as_deref(), Some("WANT_BRAVO only invoices"));

        // And the BUILT prompt contains exactly that one rule's text — none of
        // the other two rules leak in.
        let ctx = RowContext::from_queued(&rows[0], 4000);
        let prompt = build_user_message(&ctx);
        assert!(prompt.contains("WANT_BRAVO only invoices"), "matched want must appear");
        assert!(!prompt.contains("WANT_ALPHA"), "non-matched rule must not leak");
        assert!(!prompt.contains("WANT_CHARLIE"), "non-matched rule must not leak");
        assert_eq!(
            prompt.matches("WANT_").count(),
            1,
            "exactly one rule's want_text in the prompt"
        );

        // NO-MATCH case: a row with matched_rule_id NULL carries zero rule text.
        let id2 = store.upsert_message(&sample_msg(acct, "g2", "t2")).unwrap();
        store
            .set_triage(
                id2, acct, 40, Tier::Noise, Sensitivity::Normal, None, "ambiguous",
                "no rule matched", None,
            )
            .unwrap();
        let rows2 = store.stage2_queue(acct, 10).unwrap();
        let unmatched = rows2.iter().find(|r| r.message_id == id2).unwrap();
        assert!(unmatched.rule_want_text.is_none(), "no rule => no want_text");
        let prompt2 = build_user_message(&RowContext::from_queued(unmatched, 4000));
        assert!(!prompt2.contains("WANT_"), "unmatched row prompt has zero rule text");
        assert!(prompt2.contains("standing_instruction_for_this_sender: none"));
    }

    #[test]
    fn stage2_budget_increment_and_exhaustion() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let day = "2026-07-09";

        assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 0);
        assert_eq!(store.stage2_increment_budget(acct, "t-abc", day).unwrap(), 1);
        assert_eq!(store.stage2_increment_budget(acct, "t-abc", day).unwrap(), 2);
        assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 2);

        // A different thread and a different day are independent counters.
        assert_eq!(store.stage2_budget_used(acct, "t-other", day).unwrap(), 0);
        assert_eq!(store.stage2_budget_used(acct, "t-abc", "2026-07-10").unwrap(), 0);

        // The global sentinel is a separate scope in the same table.
        assert_eq!(store.stage2_increment_budget(acct, "__global__", day).unwrap(), 1);
        assert_eq!(store.stage2_budget_used(acct, "__global__", day).unwrap(), 1);
        // The per-thread counter is unaffected by the global increment.
        assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 2);
    }

    #[test]
    fn mailing_list_storm_capped_at_thread_daily_cap() {
        // A mailing-list storm — 30 messages in ONE thread — must cost AT MOST
        // `thread_daily_cap` API calls. Models the check-BEFORE-increment
        // discipline stage2_pass runs per row, with the global cap set high so
        // the per-thread cap binds.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let day = "2026-07-09";
        let thread = "t-listserv";
        let thread_daily_cap: u32 = 3; // matches Stage2Config default

        let mut calls = 0u32;
        for _ in 0..30 {
            let used = store.stage2_budget_used(acct, thread, day).unwrap();
            if used >= thread_daily_cap {
                continue; // capped: row stays queued, no call
            }
            // "Make the call": increment BEFORE the attempt.
            store.stage2_increment_budget(acct, thread, day).unwrap();
            calls += 1;
        }

        assert_eq!(
            calls, thread_daily_cap,
            "30-message storm on one thread must cost at most thread_daily_cap calls"
        );
        assert_eq!(
            store.stage2_budget_used(acct, thread, day).unwrap(),
            thread_daily_cap,
            "counter must not exceed the cap"
        );
    }

    #[test]
    fn one_sender_across_many_threads_capped_at_sender_daily_cap() {
        // A chatty sender fanning 10 messages across 10 DIFFERENT threads must
        // cost AT MOST `sender_daily_cap` calls. Models the per-sender
        // check-BEFORE-increment the pass runs (keyed by sender:<addr>), with the
        // per-thread and global caps set high so the per-sender cap binds.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let day = "2026-07-09";
        let sender_key = "sender:chatty@example.com";
        let sender_daily_cap: u32 = 5; // matches Stage2Config default

        let mut calls = 0u32;
        for i in 0..10 {
            // Each message is in its OWN thread — the per-thread cap never binds.
            let _thread = format!("t-{i}");
            let used = store.stage2_budget_used(acct, sender_key, day).unwrap();
            if used >= sender_daily_cap {
                continue; // sender capped: row stays queued, no call
            }
            store.stage2_increment_budget(acct, sender_key, day).unwrap();
            calls += 1;
        }

        assert_eq!(
            calls, sender_daily_cap,
            "10 messages from one sender across 10 threads cost at most sender_daily_cap"
        );
        assert_eq!(
            store.stage2_budget_used(acct, sender_key, day).unwrap(),
            sender_daily_cap
        );
    }

    #[test]
    fn stage2_usage_ledger_bumps_and_reads() {
        // Bumping the ledger accumulates calls + tokens per day, and reading
        // returns the running totals (zeroed for an untouched day).
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let day = "2026-07-09";

        // Untouched day reads as zeros.
        let z = store.stage2_usage_today(acct, day).unwrap();
        assert_eq!(z, Stage2Usage::default());

        store.stage2_bump_usage(acct, day, 1200, 60).unwrap();
        store.stage2_bump_usage(acct, day, 800, 40).unwrap();
        let u = store.stage2_usage_today(acct, day).unwrap();
        assert_eq!(u.calls, 2);
        assert_eq!(u.input_tokens, 2000);
        assert_eq!(u.output_tokens, 100);

        // A different day is an independent row.
        assert_eq!(
            store.stage2_usage_today(acct, "2026-07-10").unwrap(),
            Stage2Usage::default()
        );
    }

    #[test]
    fn list_usage_returns_recent_days_newest_first() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // Empty ledger => no rows.
        assert!(store.list_usage(acct, 30).unwrap().is_empty());

        store.stage2_bump_usage(acct, "2026-07-07", 100, 10).unwrap();
        store.stage2_bump_usage(acct, "2026-07-08", 200, 20).unwrap();
        store.stage2_bump_usage(acct, "2026-07-09", 300, 30).unwrap();
        store.stage2_bump_usage(acct, "2026-07-09", 100, 10).unwrap();

        // Newest-first, sparse (only days with a row).
        let rows = store.list_usage(acct, 30).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].day, "2026-07-09");
        assert_eq!(rows[0].calls, 2);
        assert_eq!(rows[0].input_tokens, 400);
        assert_eq!(rows[0].output_tokens, 40);
        assert_eq!(rows[2].day, "2026-07-07");

        // `days` caps the row count (still newest-first).
        let capped = store.list_usage(acct, 2).unwrap();
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].day, "2026-07-09");
        assert_eq!(capped[1].day, "2026-07-08");
    }

    #[test]
    fn app_settings_get_set_roundtrip_and_scoping() {
        let store = SqliteStore::open_in_memory().unwrap();
        let a = store.ensure_account("a@example.com").unwrap();
        let b = store.ensure_account("b@example.com").unwrap();

        // Unset key reads None.
        assert!(store.get_app_setting(a, "k").unwrap().is_none());

        // Set, read back, and overwrite (upsert).
        store.set_app_setting(a, "k", "v1").unwrap();
        assert_eq!(store.get_app_setting(a, "k").unwrap().as_deref(), Some("v1"));
        store.set_app_setting(a, "k", "v2").unwrap();
        assert_eq!(store.get_app_setting(a, "k").unwrap().as_deref(), Some("v2"));

        // Per-account scoped: b's key is independent.
        assert!(store.get_app_setting(b, "k").unwrap().is_none());
    }

    #[test]
    fn stage2_cap_overrides_reads_and_precedence() {
        use crate::config::{
            APP_SETTING_GLOBAL_DAILY_CAP, APP_SETTING_SENDER_DAILY_CAP,
            APP_SETTING_THREAD_DAILY_CAP,
        };
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // No rows => all None (caller falls back to config/env then default).
        assert_eq!(store.stage2_cap_overrides(acct).unwrap(), Default::default());

        // A set thread cap surfaces; the others stay None (so the effective cap
        // is the override where present, config/default elsewhere — precedence).
        store.set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "5").unwrap();
        let o = store.stage2_cap_overrides(acct).unwrap();
        assert_eq!(o.thread_daily_cap, Some(5));
        assert_eq!(o.sender_daily_cap, None);
        assert_eq!(o.global_daily_cap, None);

        // Set the remaining two.
        store.set_app_setting(acct, APP_SETTING_SENDER_DAILY_CAP, "9").unwrap();
        store.set_app_setting(acct, APP_SETTING_GLOBAL_DAILY_CAP, "300").unwrap();
        let o = store.stage2_cap_overrides(acct).unwrap();
        assert_eq!(o.thread_daily_cap, Some(5));
        assert_eq!(o.sender_daily_cap, Some(9));
        assert_eq!(o.global_daily_cap, Some(300));

        // A malformed OR out-of-range stored value is ignored (treated as absent),
        // so a corrupt row can never remove the cap entirely.
        store.set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "not-a-number").unwrap();
        assert_eq!(store.stage2_cap_overrides(acct).unwrap().thread_daily_cap, None);
        store.set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "0").unwrap();
        assert_eq!(store.stage2_cap_overrides(acct).unwrap().thread_daily_cap, None);
        store.set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "100001").unwrap();
        assert_eq!(store.stage2_cap_overrides(acct).unwrap().thread_daily_cap, None);
    }

    #[test]
    fn override_cap_binds_below_config_default() {
        // Precedence is override > config/env > default: a runtime override of 1
        // caps a thread the config default (3) would have allowed 3 calls on.
        use crate::config::APP_SETTING_THREAD_DAILY_CAP;
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let day = "2026-07-09";
        let thread = "t-override";
        let config_default_cap: u32 = 3; // Stage2Config default

        // Client lowers the per-thread cap to 1 at runtime.
        store.set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "1").unwrap();

        // Effective cap = override (1), NOT the config default (3) — precedence.
        let overrides = store.stage2_cap_overrides(acct).unwrap();
        let effective = overrides.thread_daily_cap.unwrap_or(config_default_cap);
        assert_eq!(effective, 1);

        // Same check-before-increment loop the pass runs, using the effective cap.
        let mut calls = 0u32;
        for _ in 0..10 {
            let used = store.stage2_budget_used(acct, thread, day).unwrap();
            if used >= effective {
                continue;
            }
            store.stage2_increment_budget(acct, thread, day).unwrap();
            calls += 1;
        }
        assert_eq!(calls, 1, "override cap of 1 must bind below the config default of 3");
    }

    #[test]
    fn stage2_usage_since_sums_window_inclusively() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // Empty ledger => zeros.
        assert_eq!(
            store.stage2_usage_since(acct, "2026-07-01").unwrap(),
            Stage2Usage::default()
        );

        store.stage2_bump_usage(acct, "2026-07-05", 100, 10).unwrap();
        store.stage2_bump_usage(acct, "2026-07-08", 200, 20).unwrap();
        store.stage2_bump_usage(acct, "2026-07-08", 300, 30).unwrap();

        // since_day <= earliest => everything summed (2 days, 3 calls).
        let all = store.stage2_usage_since(acct, "2026-07-05").unwrap();
        assert_eq!(all.calls, 3);
        assert_eq!(all.input_tokens, 600);
        assert_eq!(all.output_tokens, 60);

        // Window boundary is inclusive on since_day and excludes older rows.
        let recent = store.stage2_usage_since(acct, "2026-07-08").unwrap();
        assert_eq!(recent.calls, 2);
        assert_eq!(recent.input_tokens, 500);
        assert_eq!(recent.output_tokens, 50);
    }

    #[test]
    fn count_inbound_since_counts_only_received_in_window() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let now = Utc::now();

        let inbound = |gmail: &str, sent: bool, received: DateTime<Utc>| {
            let m = NewMessage {
                account_id: acct,
                gmail_msg_id: gmail.to_string(),
                thread_id: gmail.to_string(),
                from_addr: "x@y.com".to_string(),
                from_name: None,
                subject: "s".to_string(),
                received_at: received,
                snippet: String::new(),
                body: String::new(),
                body_html: None,
                is_sent: sent,
                list_unsubscribe: None,
                list_unsub_one_click: false,
            };
            store.upsert_message(&m).unwrap();
        };

        // Two recent inbound, one old inbound, one recent SENT (excluded).
        inbound("m1", false, now - chrono::Duration::days(1));
        inbound("m2", false, now - chrono::Duration::days(10));
        inbound("m3", false, now - chrono::Duration::days(30));
        inbound("m4", true, now - chrono::Duration::days(1));

        let since = now - chrono::Duration::days(14);
        assert_eq!(store.count_inbound_since(acct, since).unwrap(), 2);
    }

    #[test]
    fn update_sender_rule_edits_by_id_and_404s_unknown() {
        // Overwrites pattern/want/disposition by id; false for an unknown id.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = store
            .set_sender_rule(acct, "*@old.com", "old want", Disposition::Squelch)
            .unwrap();

        let updated = store
            .update_sender_rule(acct, id, "*@new.com", "new want", Disposition::Surface)
            .unwrap();
        assert!(updated);
        let rules = store.list_sender_rules(acct).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_pattern, "*@new.com");
        assert_eq!(rules[0].want_text, "new want");
        assert_eq!(rules[0].disposition, Disposition::Surface);

        // Unknown id => false (handler turns this into 404).
        assert!(!store
            .update_sender_rule(acct, 9999, "*@x.com", "", Disposition::Squelch)
            .unwrap());
    }

    #[test]
    fn stale_skip_marks_processed_without_budget() {
        // A row older than the cutoff is stale-skipped: marked processed with
        // model_used='stale-skip' (keeping Stage-1 values), leaving the queue,
        // and spending no budget.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let max_age_days: i64 = 7;
        let now = Utc::now();
        let cutoff = now - chrono::Duration::days(max_age_days);

        // A stale row (received 30d ago) and a fresh row (now).
        let mut stale = sample_msg(acct, "g-stale", "t-stale");
        stale.received_at = now - chrono::Duration::days(30);
        let stale_id = store.upsert_message(&stale).unwrap();
        store
            .set_triage(stale_id, acct, 40, Tier::Noise, Sensitivity::Normal, None, "amb", "", None)
            .unwrap();
        let mut fresh = sample_msg(acct, "g-fresh", "t-fresh");
        fresh.received_at = now;
        let fresh_id = store.upsert_message(&fresh).unwrap();
        store
            .set_triage(fresh_id, acct, 40, Tier::Noise, Sensitivity::Normal, None, "amb", "", None)
            .unwrap();

        // Apply the pass-loop decision: stale-skip old rows, keep fresh queued.
        let day = "2026-07-09";
        for row in store.stage2_queue(acct, 10).unwrap() {
            if row.received_at < cutoff {
                store
                    .stage2_mark_processed(acct, row.message_id, "stale-skip")
                    .unwrap();
            }
        }

        // Only the fresh row remains queued.
        let remaining = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].message_id, fresh_id);

        // No budget was spent on the stale skip.
        assert_eq!(store.stage2_budget_used(acct, "t-stale", day).unwrap(), 0);
        assert_eq!(
            store.stage2_budget_used(acct, "__global__", day).unwrap(),
            0
        );

        // The stale row's triage is stamped 'stale-skip' with Stage-1 values kept.
        let conn = store.lock().unwrap();
        let (imp, model): (i64, Option<String>) = conn
            .query_row(
                "SELECT importance, model_used FROM triage WHERE message_id=?1",
                params![stale_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(imp, 40, "stale-skip keeps Stage-1 importance");
        assert_eq!(model.as_deref(), Some("stale-skip"));
    }

    #[test]
    fn stage2_queue_carries_received_at() {
        // The queue surfaces received_at so the pass can skip stale rows.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let mut m = sample_msg(acct, "g1", "t1");
        let when = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        m.received_at = when;
        let id = store.upsert_message(&m).unwrap();
        store
            .set_triage(
                id, acct, 40, Tier::Noise, Sensitivity::Normal, None, "amb", "", None,
            )
            .unwrap();
        let rows = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].received_at, when);
    }

    #[test]
    fn stage2_apply_updates_row_stamps_model_and_writes_deadline() {
        use crate::triage::DeadlineHit;
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = seed_triage_row(&store, acct, "g1", "t1", Sensitivity::Normal);

        let due = DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let applied = Stage2Applied {
            message_id: id,
            account_id: acct,
            importance: 88,
            tier: Tier::Deadline,
            one_line: "invoice due sep 1".into(),
            reason: "stage-2 (m): real bill".into(),
            field_reasons: crate::types::FieldReasons {
                importance: Some("stage-2: real bill".into()),
                deadline: Some("stage-2: invoice due sep 1".into()),
                tier: Some("stage-2: future deadline -> deadline".into()),
            },
            model_used: "claude-haiku-4-5".into(),
            deadline: Some(DeadlineHit {
                kind: "invoice".into(),
                amount: None,
                currency: None,
                due_at: due,
                past_due: false,
                source: "stage2".into(),
            }),
            category: Some("invoice".into()),
        };
        assert!(store.stage2_apply(&applied).unwrap(), "the guard matched the normal row");

        // Row left the queue (model_used stamped).
        assert!(store.stage2_queue(acct, 10).unwrap().is_empty());
        // A deadlines row was written.
        let ds = store.deadlines(acct, Some(365)).unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].kind, "invoice");
        // The ranked update reflects the new tier/importance.
        let ups = store
            .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), None)
            .unwrap();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].tier, Tier::Deadline);
        assert_eq!(ups[0].importance, 88);
    }

    #[test]
    fn stage2_apply_never_touches_sealed_row() {
        use crate::triage::DeadlineHit;
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = seed_triage_row(&store, acct, "g-sealed", "t1", Sensitivity::Sealed);
        let applied = Stage2Applied {
            message_id: id,
            account_id: acct,
            importance: 99,
            tier: Tier::Signal,
            one_line: "leak".into(),
            reason: "should not apply".into(),
            field_reasons: crate::types::FieldReasons::default(),
            model_used: "m".into(),
            deadline: Some(DeadlineHit {
                kind: "invoice".into(),
                amount: None,
                currency: None,
                due_at: Utc::now() + chrono::Duration::days(3),
                past_due: false,
                source: "stage2".into(),
            }),
            category: Some("general".into()),
        };
        // TOCTOU report: the guard matched nothing and the caller must know —
        // a bare Ok would let a message sealed mid-pass emit an event anyway.
        assert!(!store.stage2_apply(&applied).unwrap(), "sealed row: apply reports false");
        // The sealed row's triage must be unchanged (guarded by sensitivity),
        // and the verdict's deadline must NOT have been written either.
        assert!(store.deadlines(acct, Some(365)).unwrap().is_empty(), "no deadline row");
        let conn = store.lock().unwrap();
        let (imp, model): (i64, Option<String>) = conn
            .query_row(
                "SELECT importance, model_used FROM triage WHERE message_id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(imp, 40, "sealed row importance unchanged");
        assert!(model.is_none(), "sealed row model_used untouched");
    }

    #[test]
    fn stage1_apply_reports_false_when_the_row_was_sealed_mid_pass() {
        use crate::triage::DeadlineHit;
        // TOCTOU: a human seals a row between the Stage-1 queue SELECT and its
        // apply. The guarded UPDATE matches nothing, and the apply must say so or
        // the engine emits a notification event for a now-sealed message.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = seed_triage_row(&store, acct, "g-race", "t1", Sensitivity::Normal);
        let applied = Stage1Applied {
            message_id: id,
            account_id: acct,
            importance: 90,
            tier: Tier::PastDue,
            one_line: "loud verdict".into(),
            reason: "stage-1".into(),
            field_reasons: crate::types::FieldReasons::default(),
            stage1_model_used: "claude-haiku-4-5".into(),
            needs_stage2: false,
            deadline: Some(DeadlineHit {
                kind: "bill".into(),
                amount: Some(10.0),
                currency: Some("USD".into()),
                due_at: Utc::now() - chrono::Duration::days(1),
                past_due: true,
                source: "stage1".into(),
            }),
            category: None,
        };

        // Sealed between queue and apply.
        store
            .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, Utc::now())
            .unwrap()
            .unwrap();

        assert!(!store.stage1_apply(&applied).unwrap(), "sealed mid-pass: apply reports false");
        assert!(store.deadlines(acct, Some(365)).unwrap().is_empty(), "no deadline row");

        // Control: the same apply on a live row reports true.
        let live = seed_triage_row(&store, acct, "g-live", "t2", Sensitivity::Normal);
        let mut ok = applied.clone();
        ok.message_id = live;
        assert!(store.stage1_apply(&ok).unwrap(), "normal row: apply reports true");
    }

    #[test]
    fn set_sender_rule_audited_writes_both_rows() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let audit = NewAuditEntry {
            actor: "agent".into(),
            action: "rule.set".into(),
            target: Some("*@spam.com".into()),
            detail: Some("squelch: kill it".into()),
        };
        let id = store
            .set_sender_rule_audited(acct, "*@spam.com", "kill it", Disposition::Squelch, &audit)
            .unwrap();
        assert!(id > 0);

        let rules = store.list_sender_rules(acct).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_pattern, "*@spam.com");

        let log = store.list_audit(acct, 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].actor, "agent");
        assert_eq!(log[0].action, "rule.set");
        assert_eq!(log[0].target.as_deref(), Some("*@spam.com"));
    }

    #[test]
    fn filtered_rules_reject_an_empty_want_text_on_every_write_path() {
        // A Filtered rule's want_text IS the rule ("filter everything except
        // <this>"). Empty, it degrades silently: `stage2_queue` maps it to None
        // and Stage-2 gets no instruction, while the UI still shows a rule. So
        // all three write paths reject it up front (`InvalidInput`).
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let audit = NewAuditEntry {
            actor: "agent".into(),
            action: "rule.set".into(),
            target: Some("*@vendor.com".into()),
            detail: None,
        };

        for want in ["", "   ", "\t\n"] {
            let e = store
                .set_sender_rule(acct, "*@vendor.com", want, Disposition::Filtered)
                .unwrap_err();
            assert!(matches!(e, CoreError::InvalidInput(_)), "set: {want:?}");
            let e = store
                .set_sender_rule_audited(acct, "*@vendor.com", want, Disposition::Filtered, &audit)
                .unwrap_err();
            assert!(matches!(e, CoreError::InvalidInput(_)), "set_audited: {want:?}");
        }
        // Nothing landed — no rule, and (fail-closed) no orphan audit row either.
        assert!(store.list_sender_rules(acct).unwrap().is_empty());
        assert!(store.list_audit(acct, 10).unwrap().is_empty());

        // The other dispositions don't require want_text; empty stays legal.
        let id = store
            .set_sender_rule(acct, "*@vendor.com", "", Disposition::Squelch)
            .unwrap();
        // ...but an UPDATE cannot smuggle the empty-want Filtered shape in.
        let e = store
            .update_sender_rule(acct, id, "*@vendor.com", " ", Disposition::Filtered)
            .unwrap_err();
        assert!(matches!(e, CoreError::InvalidInput(_)), "update path validates too");
        assert_eq!(store.list_sender_rules(acct).unwrap()[0].disposition, Disposition::Squelch);

        // With a real want_text, Filtered writes fine on both mutating paths.
        assert!(
            store
                .update_sender_rule(acct, id, "*@vendor.com", "only invoices", Disposition::Filtered)
                .unwrap()
        );
        store
            .set_sender_rule(acct, "*@other.com", "only receipts", Disposition::Filtered)
            .unwrap();
    }

    #[test]
    fn list_audit_enriches_message_target_and_nulls_non_numeric() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // A stored message: from_name present => sender is the name; subject verbatim.
        let mut m = sample_msg(acct, "g-audit", "t-audit");
        m.from_addr = "news@sub.com".into();
        m.from_name = Some("Newsletter Co".into());
        m.subject = "Weekly digest".into();
        let mid = store.upsert_message(&m).unwrap();

        // Row 1: target is the message id -> enriched.
        store
            .append_audit(
                acct,
                &NewAuditEntry {
                    actor: "client-api".into(),
                    action: "unsubscribe".into(),
                    target: Some(mid.to_string()),
                    detail: Some("browser:news@sub.com".into()),
                },
            )
            .unwrap();
        // Row 2: non-numeric target (a rule pattern) -> nulls, no error.
        store
            .append_audit(
                acct,
                &NewAuditEntry {
                    actor: "client-api".into(),
                    action: "rule.create".into(),
                    target: Some("*@spam.com".into()),
                    detail: Some("42".into()),
                },
            )
            .unwrap();
        // Row 3: numeric target that is NOT a known message id -> nulls.
        store
            .append_audit(
                acct,
                &NewAuditEntry {
                    actor: "client-api".into(),
                    action: "archive".into(),
                    target: Some("999999".into()),
                    detail: Some("ok".into()),
                },
            )
            .unwrap();

        let log = store.list_audit(acct, 10).unwrap();
        assert_eq!(log.len(), 3);

        let unsub = log.iter().find(|a| a.action == "unsubscribe").unwrap();
        assert_eq!(unsub.target_sender.as_deref(), Some("Newsletter Co"));
        assert_eq!(unsub.target_subject.as_deref(), Some("Weekly digest"));

        let rule = log.iter().find(|a| a.action == "rule.create").unwrap();
        assert!(rule.target_sender.is_none(), "non-numeric target yields no enrichment");
        assert!(rule.target_subject.is_none());

        let arch = log.iter().find(|a| a.action == "archive").unwrap();
        assert!(arch.target_sender.is_none(), "unknown message id yields no enrichment");
        assert!(arch.target_subject.is_none());
    }

    #[test]
    fn set_sender_rule_audited_rolls_back_rule_when_audit_fails() {
        // FAIL-CLOSED: force the audit INSERT to error (drop the audit_log table)
        // and assert the rule write did NOT land — the whole tx rolled back.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        {
            let conn = store.lock().unwrap();
            conn.execute_batch("DROP TABLE audit_log").unwrap();
        }
        let audit = NewAuditEntry {
            actor: "agent".into(),
            action: "rule.set".into(),
            target: Some("*@spam.com".into()),
            detail: None,
        };
        let res =
            store.set_sender_rule_audited(acct, "*@spam.com", "kill it", Disposition::Squelch, &audit);
        assert!(res.is_err(), "audit failure must fail the whole call");
        // The rule write must have been rolled back.
        assert_eq!(store.list_sender_rules(acct).unwrap().len(), 0);
    }

    // ---- NOTIFICATION EVENTS ---------------------------------------------

    /// A worthy `NewEvent` for `message_id`, distinct per id so ordering and
    /// snapshotting are visible in assertions.
    fn new_event(acct: AccountId, message_id: i64) -> NewEvent {
        NewEvent {
            account_id: acct,
            message_id,
            thread_id: format!("t{message_id}"),
            kind: EventKind::Surfaced,
            tier: Tier::Signal,
            importance: 70,
            sender: "alice@example.com".to_string(),
            one_line: format!("line {message_id}"),
            deadline: None,
        }
    }

    #[test]
    fn append_event_is_once_per_message_ever() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let first = store.append_event(&new_event(acct, 1)).unwrap();
        assert_eq!(first, Some(1), "first append inserts and returns the new id");

        // A SECOND append for the same message — a re-ingest, or a Stage-2 verdict
        // landing on a row that already notified at ingest — is a silent no-op.
        let mut again = new_event(acct, 1);
        again.kind = EventKind::Urgent;
        again.one_line = "a louder verdict".into();
        assert_eq!(store.append_event(&again).unwrap(), None, "dedup on message_id");

        let all = store.events_after(acct, 0, 100).unwrap();
        assert_eq!(all.len(), 1, "still exactly one row");
        assert_eq!(all[0].kind, EventKind::Surfaced, "the FIRST verdict is the one kept");
        assert_eq!(all[0].one_line, "line 1");
    }

    #[test]
    fn events_after_pages_in_id_order_and_scopes_by_account() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let other = store.ensure_account("other@example.com").unwrap();

        let mut ids = Vec::new();
        for m in 1..=5 {
            ids.push(store.append_event(&new_event(acct, m)).unwrap().unwrap());
        }
        // Another account's event must never appear in this account's replay.
        store.append_event(&new_event(other, 99)).unwrap().unwrap();

        // From the zero cursor: everything, oldest first.
        let all = store.events_after(acct, 0, 100).unwrap();
        assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), ids);
        assert_eq!(all.iter().map(|e| e.message_id).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);

        // Limit truncates from the FRONT (the oldest unseen), so a client that
        // pages never skips a row.
        let page = store.events_after(acct, 0, 2).unwrap();
        assert_eq!(page.iter().map(|e| e.id).collect::<Vec<_>>(), ids[..2].to_vec());

        // Resuming from a cursor is exclusive of the cursor itself.
        let rest = store.events_after(acct, ids[2], 100).unwrap();
        assert_eq!(rest.iter().map(|e| e.id).collect::<Vec<_>>(), ids[3..].to_vec());

        // Caught up.
        assert!(store.events_after(acct, *ids.last().unwrap(), 100).unwrap().is_empty());
        // Account scoping.
        assert_eq!(store.events_after(other, 0, 100).unwrap().len(), 1);
    }

    #[test]
    fn event_by_id_round_trips_the_snapshot_and_scopes_by_account() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let other = store.ensure_account("other@example.com").unwrap();

        let due = Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap();
        let mut ev = new_event(acct, 42);
        ev.kind = EventKind::Urgent;
        ev.tier = Tier::PastDue;
        ev.importance = 95;
        ev.deadline = Some(due.to_rfc3339());
        let id = store.append_event(&ev).unwrap().unwrap();

        // Every field a client renders from comes back verbatim — the row alone
        // is enough to build the notification (the iOS NSE has no second call).
        let got = store.event_by_id(acct, id).unwrap().expect("event");
        assert_eq!(got.id, id);
        assert_eq!(got.kind, EventKind::Urgent);
        assert_eq!(got.message_id, 42);
        assert_eq!(got.thread_id, "t42");
        assert_eq!(got.tier, Tier::PastDue);
        assert_eq!(got.importance, 95);
        assert_eq!(got.sender, "alice@example.com");
        assert_eq!(got.one_line, "line 42");
        assert_eq!(got.deadline.as_deref(), Some(due.to_rfc3339().as_str()));

        // Unknown id, and another account's id, are both indistinguishable misses.
        assert!(store.event_by_id(acct, id + 1).unwrap().is_none());
        assert!(store.event_by_id(other, id).unwrap().is_none());
    }

    #[test]
    fn latest_event_id_is_zero_until_something_happens() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let other = store.ensure_account("other@example.com").unwrap();

        assert_eq!(store.latest_event_id(acct).unwrap(), 0, "empty => the 0 cursor");

        let a = store.append_event(&new_event(acct, 1)).unwrap().unwrap();
        let b = store.append_event(&new_event(acct, 2)).unwrap().unwrap();
        assert_eq!(store.latest_event_id(acct).unwrap(), b);
        assert!(b > a, "ids are monotonic");
        // A deduped append does not move the cursor.
        assert_eq!(store.append_event(&new_event(acct, 2)).unwrap(), None);
        assert_eq!(store.latest_event_id(acct).unwrap(), b);
        // Per-account, so a busy second account cannot skip this one's replay.
        assert_eq!(store.latest_event_id(other).unwrap(), 0);
    }

    #[test]
    fn append_event_pokes_the_attached_notifier_only_on_a_real_insert() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // No notifier attached: appending must still work (a consumer that never
        // attaches simply polls the table instead).
        store.append_event(&new_event(acct, 1)).unwrap().unwrap();

        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        assert!(store.attach_event_notifier(tx).unwrap().is_none());

        let id = store.append_event(&new_event(acct, 2)).unwrap().unwrap();
        assert_eq!(rx.try_recv().unwrap(), id, "the new id is broadcast on insert");

        // A deduped append broadcasts nothing — no phantom wake for a no-op.
        assert_eq!(store.append_event(&new_event(acct, 2)).unwrap(), None);
        assert!(rx.try_recv().is_err(), "no broadcast for a deduped append");
    }

    #[test]
    fn append_event_survives_having_no_receivers() {
        // The broadcast payload is only a hint; the table is the source of truth.
        // A sender with every receiver dropped errors on send, and that must be
        // invisible to the caller.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let (tx, rx) = tokio::sync::broadcast::channel::<i64>(8);
        drop(rx);
        store.attach_event_notifier(tx).unwrap();
        assert_eq!(store.append_event(&new_event(acct, 1)).unwrap(), Some(1));
    }

    // ---- REGISTERED PUSH DEVICES -----------------------------------------

    const TOK_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
    const TOK_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2";

    /// Registration is IDEMPOTENT: iOS re-registers on every launch, so a second
    /// call must refresh the same row rather than fork a new one.
    #[test]
    fn upsert_device_is_idempotent_and_refreshes_liveness() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let first = store.upsert_device(acct, TOK_A, "ios").unwrap();
        assert_eq!(first.token, TOK_A);
        assert_eq!(first.platform, "ios");
        assert_eq!(first.account_id, acct);

        // Same token again: same row id, `created_at` preserved (first sight is a
        // fact), `last_registered_at` moved forward.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let again = store.upsert_device(acct, TOK_A, "ios").unwrap();
        assert_eq!(again.id, first.id, "a re-register must not fork a row");
        assert_eq!(again.created_at, first.created_at);
        assert!(again.last_registered_at >= first.last_registered_at);
        assert_eq!(store.list_devices(acct).unwrap().len(), 1);

        // A distinct token is a distinct device.
        store.upsert_device(acct, TOK_B, "ios").unwrap();
        let all = store.list_devices(acct).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, first.id, "listed oldest-first");
    }

    /// A token belonging to another account CANNOT be taken over by
    /// re-registering it: the collision is an error and the original row is
    /// untouched, or account B could repoint account A's phone at itself.
    #[test]
    fn a_cross_account_token_collision_is_refused_not_rebound() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let other = store.ensure_account("other@example.com").unwrap();

        let a = store.upsert_device(acct, TOK_A, "ios").unwrap();
        assert!(store.list_devices(other).unwrap().is_empty());

        let err = store
            .upsert_device(other, TOK_A, "macos")
            .expect_err("another account must not be able to adopt this token");
        assert!(
            matches!(err, CoreError::InvalidInput(ref m) if !m.contains(TOK_A)),
            "expected InvalidInput that does not echo the token, got {err:?}"
        );

        // A's row is exactly as it was: same id, same account, same platform.
        let still = store.list_devices(acct).unwrap();
        assert_eq!(still.len(), 1);
        assert_eq!(still[0].id, a.id);
        assert_eq!(still[0].account_id, acct);
        assert_eq!(still[0].platform, "ios");
        assert_eq!(still[0].last_registered_at, a.last_registered_at);
        // And B gained nothing at all.
        assert!(store.list_devices(other).unwrap().is_empty());

        // The owner can still re-register it, of course.
        let refreshed = store.upsert_device(acct, TOK_A, "ios").unwrap();
        assert_eq!(refreshed.id, a.id);
    }

    /// Delete-by-token is the shape both the human door's DELETE and the pusher's
    /// APNs-410 cleanup use: scoped, boolean, and idempotent.
    #[test]
    fn delete_device_by_token_is_scoped_and_idempotent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let other = store.ensure_account("other@example.com").unwrap();
        store.upsert_device(acct, TOK_A, "ios").unwrap();
        store.upsert_device(acct, TOK_B, "ios").unwrap();

        // Another account cannot delete this account's device.
        assert!(!store.delete_device_by_token(other, TOK_A).unwrap());
        assert_eq!(store.list_devices(acct).unwrap().len(), 2);

        assert!(store.delete_device_by_token(acct, TOK_A).unwrap());
        let left = store.list_devices(acct).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].token, TOK_B);

        // A second delete is a no-op, not an error.
        assert!(!store.delete_device_by_token(acct, TOK_A).unwrap());
        // An unknown token likewise.
        assert!(!store.delete_device_by_token(acct, "ffff0000ffff0000").unwrap());
    }

    // ---- SEMANTIC RECALL (v1) --------------------------------------------
    //
    // These exercise the vec0 index + gating with a deterministic, download-free
    // `StubEmbedder`, so the SQL/gating/ranking are covered offline. The e2e test
    // against the real fastembed model is feature-gated behind an env var
    // (SQUELCH_EMBED_E2E) so CI never downloads weights.

    use crate::embed::{Embedder, StubEmbedder, message_embed_text};
    use std::sync::Arc;

    /// Embed a message's subject+body with `embedder` and write its vector, exactly
    /// as the sync ingest/backfill path does. CALLER ensures the row is non-sealed
    /// (mirrors the structural gate: sealed mail never reaches this).
    fn embed_and_store(
        store: &SqliteStore,
        embedder: &dyn Embedder,
        acct: AccountId,
        message_id: i64,
        subject: &str,
        body: &str,
    ) {
        let text = message_embed_text(subject, body, 2000);
        let v = embedder.embed(&text).unwrap();
        store
            .upsert_message_vector(acct, message_id, &v)
            .unwrap();
    }

    /// Count vectors present for a given message id (0 or 1). Used to assert a
    /// sealed message is structurally absent from the vector space.
    fn vec_count_for(store: &SqliteStore, message_id: i64) -> i64 {
        let conn = store.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM message_vecs WHERE message_id = ?1",
            params![message_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn sealed_message_is_never_embedded() {
        // The structural gate lives at the CALLER, so `messages_missing_vectors`
        // — the backfill's source — must NEVER return a sealed row. Both halves
        // are asserted: absent from the list, and its vec slot stays empty.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // A normal message and a sealed OTP.
        let normal = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(normal, acct, 70, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        let mut otp = sample_msg(acct, "g2", "t2");
        otp.subject = "Your verification code".to_string();
        otp.body = "code 123456".to_string();
        let sealed = store.upsert_message(&otp).unwrap();
        store
            .set_triage(
                sealed, acct, 90, Tier::Noise, Sensitivity::Sealed, Some(SealedKind::Otp),
                "", "", None,
            )
            .unwrap();

        // messages_missing_vectors returns the normal row, NEVER the sealed one.
        let missing = store.messages_missing_vectors(acct, 10).unwrap();
        assert!(missing.iter().any(|m| m.message_id == normal));
        assert!(
            !missing.iter().any(|m| m.message_id == sealed),
            "sealed message must be structurally absent from the backfill source"
        );

        // Simulate the backfill embedding only what it was handed: the sealed row
        // gets no vector.
        let embedder = StubEmbedder::new(VEC_DIMS);
        for m in &missing {
            embed_and_store(&store, &embedder, acct, m.message_id, &m.subject, &m.body);
        }
        assert_eq!(vec_count_for(&store, sealed), 0, "sealed row has no vector");
        assert_eq!(vec_count_for(&store, normal), 1, "normal row was embedded");
    }

    #[test]
    fn sent_raw_body_is_stored_and_embeddable() {
        // A SENT message stores its full body (recall covers what the USER
        // wrote), and that body flows through the missing-vector backfill so it
        // becomes embeddable — even though sent mail is excluded from triage.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let mut sent = sample_msg(acct, "g-sent", "t-sent");
        sent.is_sent = true;
        sent.subject = "re: the design doc".to_string();
        sent.body = "I'll send you the revised design doc by Friday.".to_string();
        let id = store.upsert_message(&sent).unwrap();
        // Sent mail ingests with a neutral normal-sensitivity triage row.
        store
            .set_triage(id, acct, 0, Tier::Noise, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        // The raw body is stored verbatim.
        {
            let conn = store.lock().unwrap();
            let body: String = conn
                .query_row("SELECT body FROM messages WHERE id=?1", params![id], |r| r.get(0))
                .unwrap();
            assert!(body.contains("revised design doc by Friday"));
        }

        // And it is a backfill candidate (sent mail is embeddable for recall).
        let missing = store.messages_missing_vectors(acct, 10).unwrap();
        let row = missing
            .iter()
            .find(|m| m.message_id == id)
            .expect("sent message is a missing-vector candidate");
        assert!(row.body.contains("revised design doc"));

        let embedder = StubEmbedder::new(VEC_DIMS);
        embed_and_store(&store, &embedder, acct, id, &row.subject, &row.body);
        assert_eq!(vec_count_for(&store, id), 1);
    }

    #[test]
    fn semantic_search_ranks_relevant_above_decoy_and_includes_sent() {
        // Plant a relevant SENT doc and an unrelated decoy; the query about what
        // the user said they'd send must rank the relevant doc first. Sent mail is
        // INCLUDED (recall wants it) — unlike keyword `search`, which excludes it.
        let embedder = Arc::new(StubEmbedder::new(VEC_DIMS));
        let store = SqliteStore::open_in_memory()
            .unwrap()
            .with_embedder(embedder.clone())
            .unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // Relevant: the user promised to send an invoice.
        let mut relevant = sample_msg(acct, "g-rel", "t-rel");
        relevant.is_sent = true;
        relevant.subject = "invoice".to_string();
        relevant.body =
            "Hi Dana, I will send you the invoice for the consulting work tomorrow.".to_string();
        let rel = store.upsert_message(&relevant).unwrap();
        store
            .set_triage(rel, acct, 0, Tier::Noise, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        // Decoy: completely unrelated received mail.
        let mut decoy = sample_msg(acct, "g-dec", "t-dec");
        decoy.subject = "weekend hiking trip".to_string();
        decoy.body = "The mountain trail was gorgeous and the weather held up nicely.".to_string();
        let dec = store.upsert_message(&decoy).unwrap();
        store
            .set_triage(dec, acct, 20, Tier::Noise, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        // Embed both through the missing-vector path (mirrors backfill).
        for m in store.messages_missing_vectors(acct, 10).unwrap() {
            embed_and_store(&store, &*embedder, acct, m.message_id, &m.subject, &m.body);
        }

        let hits = store
            .semantic_search(acct, "did I say I would send the invoice", 5)
            .unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        assert_eq!(hits[0].0, rel, "the relevant sent doc must rank first");
        // The decoy, if present, ranks strictly worse (larger distance).
        if let Some(d) = hits.iter().find(|(id, _)| *id == dec) {
            assert!(d.1 >= hits[0].1, "decoy must not beat the relevant doc");
        }
    }

    #[test]
    fn semantic_search_excludes_sealed_even_if_a_vector_leaked() {
        // BELT-AND-SUSPENDERS: vectors are never written for sealed mail, but if a
        // vector somehow existed, semantic_search's re-join to triage must still
        // drop it. We force the pathological case by inserting a vector directly.
        let embedder = Arc::new(StubEmbedder::new(VEC_DIMS));
        let store = SqliteStore::open_in_memory()
            .unwrap()
            .with_embedder(embedder.clone())
            .unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let mut otp = sample_msg(acct, "g-seal", "t-seal");
        otp.subject = "verification code".to_string();
        otp.body = "your one time passcode is 999111".to_string();
        let sealed = store.upsert_message(&otp).unwrap();
        store
            .set_triage(
                sealed, acct, 90, Tier::Noise, Sensitivity::Sealed, Some(SealedKind::Otp),
                "", "", None,
            )
            .unwrap();

        // Pathological: write a vector for the sealed row anyway (bypassing the gate).
        embed_and_store(&store, &*embedder, acct, sealed, "verification code",
            "your one time passcode is 999111");
        assert_eq!(vec_count_for(&store, sealed), 1, "vector was forced in");

        // semantic_search must STILL never return it (re-join drops sealed).
        let hits = store
            .semantic_search(acct, "verification code passcode", 5)
            .unwrap();
        assert!(
            !hits.iter().any(|(id, _)| *id == sealed),
            "sealed row must be excluded by the query-time re-join"
        );
    }

    #[test]
    fn hybrid_search_fuses_keyword_and_vector_and_includes_sent() {
        // RRF hybrid: a sent doc that both keyword-matches and vector-matches the
        // query should surface. Confirms hybrid_search returns SearchHits and
        // includes sent mail (recall).
        let embedder = Arc::new(StubEmbedder::new(VEC_DIMS));
        let store = SqliteStore::open_in_memory()
            .unwrap()
            .with_embedder(embedder.clone())
            .unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let mut sent = sample_msg(acct, "g-h", "t-h");
        sent.is_sent = true;
        sent.subject = "contract".to_string();
        sent.body = "I promised to send the signed contract to the vendor.".to_string();
        let id = store.upsert_message(&sent).unwrap();
        store
            .set_triage(id, acct, 0, Tier::Noise, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        embed_and_store(&store, &*embedder, acct, id, &sent.subject, &sent.body);

        let hits = store.hybrid_search(acct, "signed contract vendor", 5).unwrap();
        assert!(
            hits.iter().any(|h| h.id == id),
            "hybrid search must surface the matching sent doc (recall includes sent mail)"
        );
    }

    #[test]
    fn embedder_dims_mismatch_is_rejected_at_attach() {
        // The store asserts the embedder width matches the vec0 table at attach.
        let wrong = Arc::new(StubEmbedder::new(VEC_DIMS + 1));
        // `SqliteStore` is not `Debug`, so match on the Result rather than
        // `unwrap_err()` (which would require `Ok: Debug`).
        match SqliteStore::open_in_memory().unwrap().with_embedder(wrong) {
            Ok(_) => panic!("dims mismatch must be rejected at attach"),
            Err(e) => assert!(matches!(e, CoreError::InvalidInput(_))),
        }
    }

    #[test]
    fn keyword_search_works_before_embedder_then_attaches_live() {
        // The serve-bind model: the store is already SHARED behind an Arc and
        // serving with NO embedder yet, so hybrid_search must work keyword-only,
        // semantic_search must fail gracefully, and an attach on &self must swap
        // the embedder in live with no restart.
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let acct = store.ensure_account("me@example.com").unwrap();

        let mut msg = sample_msg(acct, "g-kw", "t-kw");
        msg.subject = "quarterly invoice".to_string();
        msg.body = "The quarterly invoice from Acme is attached.".to_string();
        let id = store.upsert_message(&msg).unwrap();
        store
            .set_triage(id, acct, 0, Tier::Noise, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        // 1) Keyword-only hybrid search returns the doc with no embedder attached.
        assert!(store.embedder().is_none(), "no embedder before background attach");
        let hits = store.hybrid_search(acct, "quarterly invoice", 5).unwrap();
        assert!(
            hits.iter().any(|h| h.id == id),
            "hybrid_search must return keyword hits before the embedder is ready"
        );

        // 2) Semantic search has nothing to embed against yet.
        assert!(store.semantic_search(acct, "quarterly invoice", 5).is_err());

        // 3) Background attach (post-Arc, &self) — the serve-bind mechanism.
        let embedder = Arc::new(StubEmbedder::new(VEC_DIMS));
        let prev = store.attach_embedder(embedder.clone()).unwrap();
        assert!(prev.is_none(), "no previous embedder");
        assert!(store.embedder().is_some(), "embedder attached live");

        // 4) Now embed the row and prove semantic recall works without any restart.
        embed_and_store(&store, &*embedder, acct, id, &msg.subject, &msg.body);
        let sem = store.semantic_search(acct, "quarterly invoice", 5).unwrap();
        assert!(
            sem.iter().any(|(hid, _)| *hid == id),
            "semantic_search must work once the embedder attaches — no rebind/restart"
        );
    }

    /// E2E against the REAL fastembed model. Gated behind SQUELCH_EMBED_E2E so CI
    /// never downloads ONNX weights. Run with:
    ///   SQUELCH_EMBED_E2E=1 cargo test -p squelch-core embed_e2e
    #[test]
    fn embed_e2e_real_model_ranks_relevant_first() {
        if std::env::var("SQUELCH_EMBED_E2E").ok().as_deref() != Some("1") {
            eprintln!("skipping embed_e2e (set SQUELCH_EMBED_E2E=1 to run)");
            return;
        }
        use crate::config::EmbedConfig;
        use crate::embed::FastEmbedder;

        let embedder: Arc<dyn Embedder> =
            Arc::new(FastEmbedder::new(&EmbedConfig::default().settings()).unwrap());
        let store = SqliteStore::open_in_memory()
            .unwrap()
            .with_embedder(embedder.clone())
            .unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let mut relevant = sample_msg(acct, "g-rel", "t-rel");
        relevant.is_sent = true;
        relevant.subject = "invoice".to_string();
        relevant.body =
            "I will send over the invoice for last month's work by end of day.".to_string();
        let rel = store.upsert_message(&relevant).unwrap();
        store
            .set_triage(rel, acct, 0, Tier::Noise, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        let mut decoy = sample_msg(acct, "g-dec", "t-dec");
        decoy.subject = "lunch".to_string();
        decoy.body = "Want to grab tacos on Thursday?".to_string();
        let dec = store.upsert_message(&decoy).unwrap();
        store
            .set_triage(dec, acct, 0, Tier::Noise, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        for m in store.messages_missing_vectors(acct, 10).unwrap() {
            embed_and_store(&store, &*embedder, acct, m.message_id, &m.subject, &m.body);
        }

        let hits = store
            .semantic_search(acct, "when did I promise to send the invoice?", 5)
            .unwrap();
        assert_eq!(hits[0].0, rel, "real model must rank the invoice doc first");
        assert!(hits.iter().any(|(id, _)| *id == dec), "decoy present but lower");
    }
}
