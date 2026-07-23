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
    MessageUnsub, MissingVector, NewAuditEntry, SealedBody, SealedMessage, SitrepBand,
    Stage1Applied, Stage1Queued, Stage2Applied, Stage2CapOverrides, Stage2Queued, Stage2Usage,
    Stage2UsageDay, Store, SyncState, TriagedMessage,
};
use crate::types::{
    AccountId, AttentionStatus, AttentionUpdate, AuditEntry, BandCounts, CalendarUpdate,
    ClientMessage, ClientThreadView, Deadline, Disposition, NewMessage, Receipt, SanitizedMessage,
    SearchHit, SenderRule, Sensitivity, StoreStats, ThreadView, Tier, Update, UnsubscribeRecord,
};

const SCHEMA: &str = include_str!("schema.sql");

/// The embedding dimension declared by the `message_vecs` vec0 table
/// (`FLOAT[384]`). The store asserts the configured embedder matches this at
/// registration time; the schema literal and this constant must move together.
pub const VEC_DIMS: usize = 384;

static VEC_EXT_INIT: Once = Once::new();

/// Register the statically-linked sqlite-vec (`vec0`) extension with SQLite's
/// auto-extension hook so EVERY connection opened afterwards has the `vec0`
/// virtual table available. This is a process-global, one-time registration
/// (guarded by [`Once`]); it must run BEFORE the schema (which creates a
/// `message_vecs USING vec0(...)` table) is applied.
fn register_vec_extension() {
    VEC_EXT_INIT.call_once(|| {
        // SAFETY: `sqlite3_vec_init` is the C entrypoint the sqlite-vec crate
        // statically links; transmuting it to the auto-extension fn pointer type
        // is the documented rusqlite integration pattern.
        unsafe {
            // Explicit transmute annotation (clippy::missing_transmute_annotations):
            // the source is the C init fn as a bare pointer, the target is the
            // auto-extension entrypoint signature rusqlite expects.
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
    /// The on-box embedder used by [`SqliteStore::semantic_search`] /
    /// [`SqliteStore::hybrid_search`] to embed the QUERY text, and available to
    /// callers for embedding message bodies at ingest/backfill. `None` when
    /// semantic recall is not wired (e.g. plain unit tests) — the vector methods
    /// then return [`CoreError::InvalidInput`] and hybrid search degrades to
    /// keyword-only. Set at construction via [`SqliteStore::with_embedder`], OR
    /// SWAPPED IN LATER via [`SqliteStore::attach_embedder`] while the store is
    /// already shared behind an `Arc` — that is what lets `squelchd serve` bind
    /// the HTTP port immediately and attach the embedder in the background once
    /// the model has finished downloading. `RwLock` keeps concurrent readers
    /// (every search) cheap; the single background write is rare.
    embedder: RwLock<Option<std::sync::Arc<dyn crate::embed::Embedder>>>,
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
        // SCHEMA is all `CREATE TABLE IF NOT EXISTS`, so a pre-existing DB keeps
        // its old `messages` shape and never picks up freshly-added columns from
        // the CREATE. Run the additive column migrations so existing installs
        // upgrade cleanly (new tables/indexes are covered by IF NOT EXISTS).
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            embedder: RwLock::new(None),
        })
    }

    /// Attach an [`Embedder`](crate::embed::Embedder) so the semantic-recall
    /// methods work. Asserts the embedder's dimensionality matches the
    /// `message_vecs` vec0 table ([`VEC_DIMS`]); a mismatch is a config/schema
    /// error that would silently corrupt the index, so it fails loudly here.
    pub fn with_embedder(
        self,
        embedder: std::sync::Arc<dyn crate::embed::Embedder>,
    ) -> Result<Self> {
        self.attach_embedder(embedder)?;
        Ok(self)
    }

    /// Swap in (or replace) the embedder while the store may ALREADY be shared
    /// behind an `Arc` (`&self`, not `self`). This is the hook `squelchd serve`
    /// uses to attach the embedder in the BACKGROUND after binding the HTTP port:
    /// search runs keyword-only until this fires, then upgrades to hybrid/semantic
    /// live. Asserts the dimensionality matches [`VEC_DIMS`] (a mismatch would
    /// silently corrupt the index, so it fails loudly). Returns the previous
    /// embedder, if any.
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

    /// The attached embedder, if any. Used by the sync engine to embed message
    /// bodies at ingest/backfill without holding a second handle, and by the
    /// vector-search paths to embed the query text. Cheap clone of the `Arc`.
    pub fn embedder(&self) -> Option<std::sync::Arc<dyn crate::embed::Embedder>> {
        self.embedder
            .read()
            .ok()
            .and_then(|g| g.clone())
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
    /// SECURITY: this INTENTIONALLY excludes `sensitivity = 'sealed'` rows in
    /// SQL, so an action can never target a sealed message (NotFound is returned
    /// for a missing OR sealed message, keeping the two indistinguishable). It is
    /// read-only and is never called by sync/triage/MCP. It does not touch bodies.
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
        // TEST HELPER: rows created here are treated as Stage-1-finished and
        // escalated (`stage1_model_used='rule'`, `needs_stage2=1`) so they land in
        // the Stage-2 queue exactly as the old `model_used IS NULL` rows did —
        // this is the migration of the pre-two-stage queue semantics.
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

    // =====================================================================
    // ON-BOX SEMANTIC RECALL (v1). Inherent methods (not on the `Store`
    // trait) because they depend on the attached [`Embedder`] and the
    // sqlite-vec `message_vecs` table, which not every `Store` impl carries.
    //
    // SECURITY: SEALED MESSAGES ARE NEVER EMBEDDED. Vector inserts here are
    // callable for any id, but the ONLY caller (the sync ingest/backfill path)
    // gates on `sensitivity='normal'`, and [`messages_missing_vectors`] selects
    // ONLY normal rows, so sealed text is structurally absent from the vector
    // space. Query-time methods additionally re-exclude sealed rows in SQL.
    // =====================================================================

    /// SEMANTIC RECALL. Embed `query_text` with the attached embedder and return
    /// the `k` nearest messages as `(message_id, distance)` (smaller distance =
    /// closer), scoped to `account_id`.
    ///
    /// SECURITY: the KNN hit set is JOINed back to `triage` and sealed rows are
    /// re-excluded in SQL (belt: vectors were never written for sealed mail;
    /// suspenders: this join re-checks). BOTH `is_sent` values are INCLUDED —
    /// recall wants the user's own sent mail ("did I say I'd send X").
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
        // column, and cap with `k = ?`. We over-fetch (k rows from the index)
        // then re-join triage to drop any sealed row defensively; sealed rows
        // should never be in the index, so this rarely trims anything.
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
    /// Rank Fusion (RRF). Each candidate's score is `sum(1 / (rrf_k + rank))`
    /// across the two lists it appears in; results are returned best-first as
    /// [`SearchHit`]s. `rrf_k` is the standard smoothing constant (60). Both
    /// lists exclude sealed rows; both `is_sent` values are INCLUDED (recall).
    ///
    /// This is the cheap "belt-and-suspenders" retrieval: keyword catches exact
    /// tokens, vectors catch paraphrase. Falls back to whichever list is
    /// available (e.g. FTS-only if the query embeds empty).
    pub fn hybrid_search(
        &self,
        account_id: AccountId,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<SearchHit>> {
        const RRF_K: f32 = 60.0;

        // Vector ranks (if an embedder is attached; degrade gracefully to
        // keyword-only if not — e.g. before the background embedder attaches under
        // `squelchd serve`).
        let vec_hits: Vec<(i64, f32)> = match self.embedder() {
            Some(embedder) => {
                let qvec = embedder.embed(query_text)?;
                self.knn_by_vector(account_id, &qvec, k)?
            }
            None => Vec::new(),
        };

        // FTS ranks over the SAME query text. `fts_recall` mirrors `search` but
        // INCLUDES sent mail (recall) and returns bare ids in rank order.
        let fts_ids = self.fts_recall_ids(account_id, query_text, k)?;

        // Fuse: accumulate RRF score per message id.
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

        // Hydrate the winners into SearchHits in fused order.
        let mut out = Vec::with_capacity(ranked.len());
        for (id, _s) in ranked {
            if let Some(hit) = self.search_hit_by_id(account_id, id)? {
                out.push(hit);
            }
        }
        Ok(out)
    }

    /// SEMANTIC-ONLY recall as hydrated [`SearchHit`]s (vector KNN, no keyword
    /// leg), best-first by distance. Used by the human door's
    /// `mode=semantic` search. Requires an attached embedder; returns an empty
    /// list when none is attached (nothing to embed against). Sealed rows are
    /// excluded in SQL by [`knn_by_vector`] and re-checked by
    /// [`search_hit_by_id`]; both `is_sent` values are included (recall).
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

    /// FTS5 recall helper for [`hybrid_search`]: keyword search returning bare
    /// message ids in rank order. Unlike [`Store::search`] this INCLUDES sent
    /// mail (`is_sent` not constrained) because recall wants the user's own
    /// outbound mail. Sealed rows are excluded in SQL. A malformed FTS query
    /// yields an empty list rather than an error (recall degrades to vectors).
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


/// Add `column` (`decl` = its type + constraints) to `table` if the table does
/// not already have it. Idempotent: the codebase has no schema-version counter,
/// so we detect the existing columns via `PRAGMA table_info` and only `ALTER
/// TABLE ADD COLUMN` when the column is genuinely missing. This upgrades
/// pre-existing installs (whose `CREATE TABLE IF NOT EXISTS` left the old shape
/// in place) without disturbing fresh DBs that already carry the column.
///
/// Returns `true` iff the column was actually added this call (i.e. the DB was a
/// pre-existing install missing it). Callers use that to run a ONE-TIME backfill
/// only on the open that first introduces the column — never on fresh DBs (where
/// `schema.sql` already carries it) and never on subsequent opens.
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
    // An empty `table_info` means the table does not exist yet (every real table
    // has >= 1 column). The real open path applies `schema.sql` before `migrate`,
    // so the table is always present there; skipping when absent just keeps this
    // seam robust to partial schemas (and independent per-table in tests).
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
    // Unsubscribe capture (added with the unsubscribe feature).
    add_column_if_missing(conn, "messages", "list_unsubscribe", "TEXT")?;
    add_column_if_missing(
        conn,
        "messages",
        "list_unsub_one_click",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // Per-property triage reasons (JSON object). NULL on pre-existing rows.
    add_column_if_missing(conn, "triage", "field_reasons", "TEXT")?;

    // Two-stage triage split markers. `stage1_model_used` gates the Stage-1 LLM
    // queue (NULL == still needs Stage-1); `needs_stage2` is the escalation flag.
    // A pre-existing `triage` table predates BOTH, so the first INSERT/queue SQL
    // that names them would fail with "no such column" until they are added.
    let added_stage1 = add_column_if_missing(conn, "triage", "stage1_model_used", "TEXT")?;
    add_column_if_missing(conn, "triage", "needs_stage2", "INTEGER NOT NULL DEFAULT 0")?;

    // BACKFILL (runs ONCE, the open that first introduces `stage1_model_used`).
    // The column add leaves `stage1_model_used = NULL` on EVERY historical row —
    // which is exactly the Stage-1 LLM queue predicate — so without this the whole
    // mailbox history would re-classify through the (paid) Stage-1 model. Mark
    // every row the OLD single-stage pipeline already classified (`model_used`
    // set) OR that the user has already seen/acted on (`status != 'new'`) as
    // 'migrated' so it stays OUT of the Stage-1 queue.
    //
    // The residual set — `status='new' AND model_used IS NULL` — keeps
    // `stage1_model_used = NULL` and DOES re-enter Stage-1. That is correct: these
    // are genuinely-unprocessed recent rows that were awaiting the OLD stage-2
    // anyway. Their `needs_stage2` stays at the column-add default (0); Stage-1's
    // apply recomputes it from model confidence when the row is classified, so the
    // default is merely the pre-Stage-1 resting value, not a lost escalation.
    //
    // Guarded by `added_stage1` so it fires exactly once at introduction — NOT on
    // fresh DBs (schema.sql already carries the column) and NOT on later opens
    // (which would wrongly 'migrate' rows legitimately queued for Stage-1 that a
    // read door had promoted past 'new').
    if added_stage1 {
        conn.execute(
            "UPDATE triage SET stage1_model_used = 'migrated'
             WHERE stage1_model_used IS NULL
               AND (model_used IS NOT NULL OR status != 'new')",
            [],
        )?;
    }
    Ok(())
}

/// Apply the unsubscribe VIOLATION bump for a just-stored inbound message, in
/// the caller's transaction. Contract: for a NON-SENT message, if an
/// `unsubscribes` row exists for `(account_id, lower(from_addr))` with
/// `resolution IS NULL` and the message's `received_at` is more than 72h after
/// the request, increment `violation_count` and set `last_violation_at` to that
/// `received_at`. A no-match (no row / already resolved / still within grace)
/// is a silent no-op.
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
    // Read the outstanding request (if any) to run the grace comparison in Rust
    // against real timestamps rather than lexical string math.
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

    // NOTE: contacts are NOT seeded here. Sent mail's From header is the user's
    // OWN address, so seeding from it produced exactly one bogus self-contact.
    // Contacts are instead seeded from the To/Cc recipients of Sent mail in
    // `ingest_message` (which carries the pre-filtered recipient list).
    Ok(id)
}

/// Seed the contacts table from the recipients of a Sent message. Each recipient
/// increments its `sent_count`. Addresses are already de-duplicated and stripped
/// of the account's own address at ingest, so no self-guard is needed here — but
/// we defensively skip empties. Received mail passes an empty list (no-op).
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

/// Upsert a shipment against an explicit connection/transaction handle, keyed by
/// `(account_id, tracking_number)`. On first sight it inserts; on a repeat it
/// applies the no-regress status state machine
/// ([`crate::triage::ShipmentStatus::merge`]) — a delivered shipment is never
/// walked back — refreshes `last_update`/`last_message_id`, and adopts a better
/// `item_name` (a non-empty incoming name replaces an empty stored one, or a
/// strictly longer one replaces a shorter one). `carrier`/`tracking_url` are also
/// refreshed when the incoming carrier is more specific (not "unknown").
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

    // Read any existing row to run the merge (status state machine + item-name
    // preference) in Rust rather than a gnarly SQL CASE.
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

/// Upsert a receipt keyed by `(account_id, message_id)`. Idempotent: a re-ingest
/// of the same message overwrites amount/currency/sender/received_at. Runs in the
/// caller's connection/transaction.
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

/// Upsert a calendar update keyed by `(account_id, message_id)`. Idempotent: a
/// re-ingest of the same message overwrites kind/title/start/organizer/
/// received_at. Runs in the caller's connection/transaction.
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

/// RECEIPT -> OPEN-BILL AUTO-CLOSE. Given a just-ingested receipt, find the one
/// OPEN bill (a `deadlines` row whose triage status != 'done') this payment
/// plausibly settles and resolve it to 'done'. Runs in the caller's ingest
/// transaction so the receipt and the bill closure land atomically.
///
/// The matching rules are the PURE logic in [`crate::triage::receipt_match`]
/// (precision over recall — a false auto-close hides an unpaid bill):
///   * merchant identity: same registrable domain or same normalized display
///     name ([`receipt_match::merchant_matches`]);
///   * amounts: both parsed => must agree within a couple cents; bill parsed
///     but receipt not => refuse; bill unparsed => merchant + recency alone
///     ([`receipt_match::amounts_permit_close`]), each with its recency window
///     anchored on the two messages' `received_at` (bill before receipt);
///   * at most ONE bill closes per receipt — the EARLIEST-due match. Recurring
///     bills can leave two identical open months; one payment settles one, and
///     you pay the oldest first. Closing both would hide an unpaid month.
///
/// The close uses the same status-transition shape as `set_attention_status`
/// ('done' stamps `resolved_at`, sealed rows excluded in SQL) and appends an
/// `audit_log` row (actor="ingest", action="bill.auto_close") recording WHY, so
/// the human door can always answer "where did my bill go?". Idempotent: a
/// re-ingest finds the bill already 'done' and does nothing (no audit spam).
///
/// SECURITY: receipts and deadlines are both structurally sealed-free, and the
/// UPDATE re-excludes `sensitivity='sealed'` anyway (belt-and-suspenders).
/// This is internal ingest logic — nothing here crosses the MCP surface.
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

    // Candidate OPEN bills: every deadline whose triage row is not yet done.
    // The message join supplies the biller identity + recency anchor. The open
    // set is small, so filtering happens in Rust against the pure rules.
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

    // Same transition shape as set_attention_status: 'done' stamps resolved_at;
    // sealed excluded; the status guard makes a re-run a no-op.
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
                // AGENT DOOR: never carries field_reasons — the human-door insight
                // feature stays absent from the MCP payload (skip_serializing_if).
                field_reasons: None,
            });
        }
        Ok(out)
    }

    fn thread_view(&self, account_id: AccountId, thread_id: &str) -> Result<ThreadView> {
        let conn = self.lock()?;

        // SECURITY: if ANY message in this thread is sealed, treat the whole
        // thread as NotFound (indistinguishable from nonexistent). Also, a
        // thread with no visible messages is NotFound.
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
        // SECURITY: exclude sealed rows in SQL. A sealed message id resolves to
        // `None` exactly like a nonexistent one, so the `get_thread` message-id
        // fallback can never confirm that a sealed message (or its thread)
        // exists. A message with no triage row is treated as non-sealed
        // (COALESCE) so plain mail still resolves.
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

        // SECURITY: identical sealed/nonexistent -> NotFound guard as
        // `thread_view`. If ANY message in this thread is sealed, the whole
        // thread is NotFound (indistinguishable from nonexistent), so this
        // human-door variant never reveals a sealed thread's html either.
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
        let rows = stmt.query_map(params![account_id, thread_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (id, from_addr, from_name, received_at, body, body_html) = row?;
            messages.push(ClientMessage {
                id,
                from_addr,
                from_name,
                received_at: parse_dt(&received_at)?,
                content: body,
                html: body_html,
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
        // En-route by default (status != 'delivered'); delivered included only on
        // request. Ordered most-recently-updated first. No sealed filter needed:
        // the table holds no sealed rows by construction (detection never runs on
        // sealed mail).
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
        // Newest-first, within the last `days`. No sealed filter needed: the table
        // holds no sealed rows by construction (detection never runs on sealed
        // mail).
        let since = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, message_id, from_addr, from_name, amount, currency, received_at
             FROM receipts
             WHERE account_id=?1 AND received_at >= ?2
             ORDER BY received_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id, since], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<f64>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, acct, message_id, from_addr, from_name, amount, currency, received_at) = row?;
            out.push(Receipt {
                id,
                account_id: acct,
                message_id,
                from_addr,
                from_name,
                amount,
                currency,
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
        // Newest-RECEIVED first, within the last `hours` of mail arrival (the
        // window is on received_at, NOT the event's starts_at). No sealed filter
        // needed: the table holds no sealed rows by construction (detection
        // never runs on sealed mail).
        let since = (Utc::now() - chrono::Duration::hours(hours as i64)).to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT id, message_id, kind, event_title, starts_at, organizer, received_at
             FROM calendar_updates
             WHERE account_id=?1 AND received_at >= ?2
             ORDER BY received_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id, since], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, message_id, kind, event_title, starts_at, organizer, received_at) = row?;
            out.push(CalendarUpdate {
                id,
                message_id,
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
        // FAIL-CLOSED: the rule write and its audit row share ONE transaction. If
        // the audit INSERT errors, `?` bails before commit and the tx is rolled
        // back on drop — so the agent-door rule write never lands untraced.
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

        // 1b. Seed contacts from Sent-mail recipients (To/Cc), in the SAME
        //     transaction. `recipients` is empty for received mail and already
        //     excludes the account's own address.
        seed_contacts_conn(
            &tx,
            triaged.message.account_id,
            &triaged.recipients,
            &triaged.message.received_at.to_rfc3339(),
        )?;

        // 1c. UNSUBSCRIBE VIOLATION LEDGER: a NON-SENT inbound message from a
        //     sender the user asked to unsubscribe from — arriving past the 72h
        //     grace, with the request still unresolved — bumps that sender's
        //     violation_count. In the SAME transaction as the message insert so
        //     the ledger can never drift from the mail that drives it. No-op when
        //     there is no outstanding unsubscribe for this sender.
        if !triaged.message.is_sent {
            bump_unsub_violation_conn(
                &tx,
                triaged.message.account_id,
                &triaged.message.from_addr,
                triaged.message.received_at,
            )?;
        }

        // 2. Write the triage row IN THE SAME TRANSACTION. For sealed mail this
        //    is the whole point: sensitivity='sealed' is committed atomically
        //    with the message so there is no window where it is queryable as
        //    normal mail. `model_used` stays NULL; combined with
        //    sensitivity='normal' that is the Stage-2 queue predicate for
        //    non-confident rows.
        let deadline_dt = triaged.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
        // AUTO-RESOLVE receipts and calendar updates: both are RECORDS, not
        // something the user must act on (money already moved; the real calendar
        // is the source of truth). We set the attention-lifecycle status to the
        // terminal 'done' at ingest (stamping resolved_at) so the row is excluded
        // from the New/Attention/Aging bands and never becomes inbox clutter — it
        // lives ONLY in its category (Receipts / Calendar). Other rows start
        // 'new' as usual. Sealed mail never carries a receipt or calendar update,
        // so this only ever fires on non-sealed rows.
        let now_s = Utc::now().to_rfc3339();
        let auto_resolved = triaged.sensitivity != Sensitivity::Sealed
            && (triaged.receipt.is_some() || triaged.calendar.is_some());
        let (status, resolved_at) = if auto_resolved {
            ("done", Some(now_s.clone()))
        } else {
            ("new", None)
        };
        // On re-ingest of a non-auto-resolved row we must PRESERVE the existing
        // attention lifecycle (status/resolved_at) — a re-sync must not reopen an
        // item the user dismissed. A receipt/calendar row, however, is
        // force-resolved to 'done' on every ingest (a record is always a record).
        // The CASE keys off `excluded.status`: 'done' (only auto-resolved rows
        // pass 'done' in) overwrites; otherwise the existing lifecycle is kept.
        // Per-property Stage-1 reasons as a JSON blob (NULL when empty — sealed /
        // sent mail carry none). HUMAN-DOOR ONLY on read.
        let field_reasons_json = if triaged.field_reasons.is_empty() {
            None
        } else {
            serde_json::to_string(&triaged.field_reasons).ok()
        };
        // STAGE-1/STAGE-2 QUEUE MARKERS. `stage1_model_used` decides whether the
        // Stage-1 LLM refine pass will look at this row; `needs_stage2` is the
        // escalation seed. See the module docs in `triage/stage1_llm.rs`.
        //   * Sealed / Sent mail: never queued for any LLM (marked 'n/a').
        //   * Explicit Squelch/Surface rule (matched_rule set, confident): the
        //     user already ruled on this sender — NO Stage-1 model spend and no
        //     Stage-2 ('rule', needs_stage2=0).
        //   * Filtered rule (matched_rule set, NOT confident): skip Stage-1, go
        //     straight to Stage-2 for want_text ('rule', needs_stage2=1).
        //   * Everything else (matched_rule NONE): enter the Stage-1 LLM queue
        //     (stage1_model_used NULL); the `needs_stage2` seed = !confident is
        //     preserved on a heuristic-only fallback and overwritten by the LLM
        //     apply otherwise.
        let (stage1_model_used, needs_stage2): (Option<&str>, i64) =
            if triaged.sensitivity != Sensitivity::Normal || triaged.message.is_sent {
                (Some("n/a"), 0)
            } else if triaged.matched_rule.is_some() {
                (Some("rule"), if triaged.confident { 0 } else { 1 })
            } else {
                (None, if triaged.confident { 0 } else { 1 })
            };
        // RE-INGEST CLASSIFICATION GUARD. A re-delivery / backfill re-ingest carries
        // only the HEURISTIC SEED values (the LLM stages do not re-run at ingest).
        // If the existing row was already LLM-classified — Stage-2 stamped
        // `model_used`, or Stage-1 stamped a real model id (anything other than the
        // 'rule'/'n/a' sentinels) — clobbering importance/tier/one_line/reason/
        // field_reasons/deadline back to the seed would silently discard paid
        // classification while the model markers (untouched by this SET) stay put,
        // so the row would never re-queue to recover it. This predicate keeps the
        // paid columns on conflict for such rows; a genuinely-new or
        // still-heuristic-seed row (markers NULL) refreshes exactly as before.
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

        // 3. Deadlines: only ever present for non-sealed mail (Stage-1 does not
        //    run on sealed content). Replace any prior deadline for this message
        //    so re-ingest is idempotent.
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

        // 4. Shipment: only ever present for NON-SEALED mail (detection is not run
        //    on sealed content). Upsert into the tracker in the SAME transaction
        //    so a package's state and its source message land atomically. The
        //    upsert applies the no-regress status state machine. Sealed mail
        //    carries `shipment == None`, so this branch never runs for it — the
        //    `shipments` table is sealed-free by construction.
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

        // 5. Receipt: only ever present for NON-SEALED mail (detection is not run
        //    on sealed content). Insert into the receipts category in the SAME
        //    transaction. Independent of shipment detection — an order
        //    confirmation with a total AND tracking lands in BOTH tables. The
        //    triage row was already force-resolved to status='done' above so this
        //    message never surfaces as inbox clutter. Sealed mail carries
        //    `receipt == None`, so this branch never runs for it — the `receipts`
        //    table is sealed-free by construction.
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

            // 5b. RECEIPT -> OPEN-BILL AUTO-CLOSE, in the SAME transaction: if
            //     this payment plausibly settles an open bill (conservative
            //     merchant + amount + recency rules, see
            //     `auto_close_bill_for_receipt_conn`), resolve that bill's
            //     triage row to 'done' and audit why. A missed match is fine;
            //     a false close would hide an unpaid bill, so the rules prefer
            //     precision.
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

        // 6. Calendar update: only ever present for NON-SEALED mail (detection
        //    is not run on sealed content). Insert into the calendar category in
        //    the SAME transaction. Independent of the other detectors, exactly
        //    like receipts. The triage row was already force-resolved to
        //    status='done' above so a calendar notification never surfaces as
        //    inbox clutter — it lives only in the Calendar zone. Sealed mail
        //    carries `calendar == None`, so this branch never runs for it — the
        //    `calendar_updates` table is sealed-free by construction. NOTE:
        //    nothing is written back to Gmail; "resolved" is squelch-internal.
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
        // SECURITY: join triage and exclude sealed rows in SQL, exactly like
        // ranked_updates. A message with no triage row is treated as non-sealed
        // (LEFT JOIN) so freshly-ingested-but-untriaged mail is still findable,
        // but a sealed classification always hides the row.
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

        // Base predicate mirrors ranked_updates (sealed excluded, sent excluded,
        // since/importance window). Band/status add clauses; the ORDER BY differs
        // for the `open` band (age*importance) — documented below.
        //
        // Band semantics:
        //   standing = tier IN ('past_due','deadline') AND status != 'done'
        //   new      = surfaced_at IS NULL AND status != 'done'
        //   open     = status = 'open'
        // The `status != 'done'` on `new` keeps AUTO-RESOLVED receipts (done at
        // ingest, never surfaced) out of the Attention/New band — a receipt is a
        // record, not new inbox clutter. Only receipt-classified rows are auto-
        // done at ingest; every other row starts 'new', so genuine mail is
        // unaffected.
        let mut sql = String::from(
            "SELECT m.id, m.thread_id, t.tier, t.importance, m.from_addr, t.one_line,
                    t.reason, t.deadline, t.matched_rule_id,
                    t.status, t.surfaced_at, t.resolved_at, t.field_reasons
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
        // The `open` band is the aging/escalating band: sort by age*importance so
        // long-unresolved-and-important items float. `age` is (now - received_at)
        // in seconds; we compute it in SQL via julianday so the ordering lives
        // server-side. Other bands keep the ranked_updates ordering.
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
            ) = row?;
            let deadline = match deadline_s {
                Some(s) => Some(parse_dt(&s)?),
                None => None,
            };
            // Parse the per-property reasons JSON. A NULL column (row predates the
            // feature) or a malformed value yields None — defensive: a bad blob
            // must never fail the whole updates read.
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
            // Stamp surfaced_at only if NULL, and promote new->open. The
            // sensitivity guard means a sealed row is NEVER stamped, so it can
            // never leak into a "new since last check" delta. Idempotent: a
            // second call finds surfaced_at already set and changes nothing.
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
                        m.subject, m.received_at, t.sealed_kind, m.body
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
                    ))
                },
            )
            .optional()?;
        let (id, acct, thread_id, from_addr, from_name, subject, received_at, sealed_kind, body) =
            row.ok_or(CoreError::NotFound)?;
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

    fn message_unsub_fields(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<MessageUnsub>> {
        let conn = self.lock()?;
        // SECURITY: exclude sealed rows in SQL so an unsubscribe against a sealed
        // message resolves to `None` (=> 404) exactly like an unknown id. A
        // message with no triage row is treated as non-sealed (COALESCE).
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
        // A fresh request RESETS the ledger: the user re-asked, so violation_count
        // -> 0, last_violation_at -> NULL, resolution -> NULL (the 72h grace clock
        // restarts from this requested_at).
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

    fn list_audit(&self, account_id: AccountId, limit: u32) -> Result<Vec<AuditEntry>> {
        let conn = self.lock()?;
        // Enrich each row with the targeted message's sender/subject when `target`
        // parses as a message id that exists for this account. `target` is TEXT and
        // often holds non-numeric values (rule patterns, senders); CAST of such a
        // value yields 0 in SQLite (never errors), which cannot match a real id
        // (message ids are positive), so the LEFT JOIN just yields NULLs. Sealed
        // messages ARE included (human door): their sender/subject already show on
        // the Auth tab; sealed CONTENT is never selected here.
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
            // Sender = from_name if present, else from_addr. Both are None when the
            // join found no message (non-numeric target or unknown id).
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

        // Sitrep band counts over non-sealed rows. Definitions match the `band`
        // query on attention_updates so the header and the list agree.
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
        // Rows still needing the Stage-1 LLM refine pass: heuristic seed values
        // in place (stage1_model_used IS NULL), non-sealed, non-sent. Rows a
        // sender rule decided carry stage1_model_used='rule' and are excluded.
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

    fn stage1_apply(&self, applied: &Stage1Applied) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let deadline_dt = applied.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
        let field_reasons_json = if applied.field_reasons.is_empty() {
            None
        } else {
            serde_json::to_string(&applied.field_reasons).ok()
        };
        // Overwrite the heuristic seed values, stamp stage1_model_used (leaving
        // the Stage-1 queue), and set the escalation flag. `model_used` (the
        // Stage-2 marker) is left untouched. Guarded by sensitivity='normal'.
        tx.execute(
            "UPDATE triage SET
                 importance = ?3,
                 tier = ?4,
                 one_line = ?5,
                 reason = ?6,
                 deadline = ?7,
                 stage1_model_used = ?8,
                 needs_stage2 = ?9,
                 field_reasons = ?10
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
            ],
        )?;
        // (Re)write the deadlines row idempotently.
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
        tx.commit()?;
        Ok(())
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

    fn list_usage_stage1(&self, account_id: AccountId, days: u32) -> Result<Vec<Stage2UsageDay>> {
        let conn = self.lock()?;
        list_usage_category(&conn, account_id, days, "stage1")
    }

    fn stage2_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage2Queued>> {
        let conn = self.lock()?;
        // The Stage-2 queue predicate: Stage-1 finished with the row
        // (stage1_model_used IS NOT NULL) AND flagged it for escalation
        // (needs_stage2=1) AND Stage-2 hasn't processed it yet (model_used IS
        // NULL). Sealed rows carry sensitivity='sealed' and are structurally
        // excluded. Join the message for context and LEFT JOIN the matched sender
        // rule for its want_text (Filtered rules escalate here). is_known_contact
        // is derived from a correlated EXISTS against contacts.
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

    fn stage2_apply(&self, applied: &Stage2Applied) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        // Overwrite triage fields and stamp model_used. Guarded by
        // sensitivity='normal' so a sealed row can never be mutated here even if
        // a caller mis-targets one (defense in depth; the queue already excludes
        // sealed rows).
        let deadline_dt = applied.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
        // Stage-2 owns all three properties on apply, so its reasons fully replace
        // any Stage-1 blob. NULL when empty (defensive; apply always sets some).
        let field_reasons_json = if applied.field_reasons.is_empty() {
            None
        } else {
            serde_json::to_string(&applied.field_reasons).ok()
        };
        tx.execute(
            "UPDATE triage SET
                 importance = ?3,
                 tier = ?4,
                 one_line = ?5,
                 reason = ?6,
                 deadline = ?7,
                 model_used = ?8,
                 field_reasons = ?9
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
            ],
        )?;
        // (Re)write the deadlines row idempotently.
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
        tx.commit()?;
        Ok(())
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

    /// Trait override: expose the swappable, possibly-late-attached embedder so a
    /// generic `S: Store` caller (the sync engine) resolves the CURRENT embedder,
    /// including one attached in the background after `serve` bound its port.
    fn embedder(&self) -> Option<std::sync::Arc<dyn crate::embed::Embedder>> {
        SqliteStore::embedder(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SealedKind, Sensitivity, Tier};
    use chrono::TimeZone;

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
        // Simulate an existing install whose `triage` predates field_reasons.
        // `model_used`/`status` are original triage columns (they predate this
        // feature and the two-stage split), so a realistic pre-existing table
        // carries them — the two-stage backfill in migrate() reads them.
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
        // Simulate a PRE-TWO-STAGE install: a `triage` table that predates
        // stage1_model_used / needs_stage2 (and field_reasons). The additive
        // migration must add them so the first ingest + both queue SELECTs (which
        // NAME those columns) don't fail with "no such column".
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
        // init applies SCHEMA (IF NOT EXISTS keeps the old triage shape) then
        // migrate() (adds the two columns + backfill).
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

    /// Local mirror of `triage::stage1_llm::HEURISTIC_ONLY` for the tests above.
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
        // DETERMINISM: with N sender rules in the db, a Stage-2 prompt must carry
        // AT MOST the ONE rule's want_text whose id equals the row's
        // matched_rule_id (chosen by Stage-1's pure `match_sender_rule`), and NONE
        // of the others'. Rule selection is pure code: the queue LEFT JOINs
        // exactly `sr.id = t.matched_rule_id`, so the full rule list is NEVER fed
        // to the prompt.
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
        // Audit (c): a mailing-list storm — 30 messages, all in ONE thread —
        // must result in AT MOST `thread_daily_cap` API calls. This models the
        // exact check-BEFORE-increment discipline stage2_pass runs per row:
        // read the per-thread counter, skip if it's already at the cap, else
        // increment (which is what "make a call" costs). Any global cap is set
        // high so the per-thread cap is the binding constraint.
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
        // TASK 3: a chatty sender fanning 10 messages across 10 DIFFERENT threads
        // must cost AT MOST `sender_daily_cap` calls. Models the per-sender
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
        // TASK 5: bumping the usage ledger accumulates calls + tokens per day, and
        // reading returns the running totals (zeroed for an untouched day).
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
        // The Stage-2 pass reads stage2_cap_overrides at the START of each cycle
        // and uses override > config/env > default. Here a runtime override of 1
        // caps a thread that the config default (3) would have allowed 3 calls on.
        // Models the exact check-BEFORE-increment discipline stage2_pass runs,
        // driving the effective cap the same way the pass computes it.
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
        // TASK 6 (store layer): update_sender_rule overwrites pattern/want/disp by
        // id, returns false for an unknown id.
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
        // TASK 4: a row older than the cutoff is stale-skipped: marked processed
        // with model_used='stale-skip' (keeping Stage-1 values), leaving the
        // queue, and NOT touching any budget row. Models the pass-loop decision.
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
        // TASK 4 support: the queue surfaces received_at so the pass can skip
        // stale rows.
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
        };
        store.stage2_apply(&applied).unwrap();

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
            deadline: None,
        };
        store.stage2_apply(&applied).unwrap();
        // The sealed row's triage must be unchanged (guarded by sensitivity).
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
        // The structural gate lives at the CALLER (ingest/backfill only embed
        // non-sealed rows). `messages_missing_vectors` — the backfill's source —
        // must NEVER return a sealed row, so a sealed message can never acquire a
        // vector through the supported path. We assert both: the sealed row is
        // absent from the missing-vector list, and its vec slot stays empty.
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
        // TASK 3/7: a SENT message stores its full body (recall covers what the
        // USER wrote), and that body flows through the missing-vector backfill so
        // it becomes embeddable — even though sent mail is excluded from triage.
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
        // BUG 3 (issue #16) serve-bind model. This mirrors `squelchd serve`: the
        // store is already SHARED (behind Arc) and serving, with NO embedder yet.
        // 1) hybrid_search must work KEYWORD-ONLY (no embedder) — proving both
        //    doors stay useful while the model downloads in the background.
        // 2) semantic_search must fail gracefully (no embedder attached).
        // 3) attach_embedder on &self (post-Arc) must swap the embedder in live.
        // 4) semantic_search must then work — no restart, no rebind.
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
