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
    MissingVector, NewAuditEntry, SealedBody, SealedMessage, SitrepBand, Stage2Applied,
    Stage2Queued, Stage2Usage, Store, SyncState, TriagedMessage,
};
use crate::types::{
    AccountId, AttentionStatus, AttentionUpdate, AuditEntry, BandCounts, ClientMessage,
    ClientThreadView, Deadline, Disposition, NewMessage, SanitizedMessage, SearchHit, SenderRule,
    Sensitivity, StoreStats, ThreadView, Tier, Update,
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
        conn.execute(
            "INSERT INTO triage(message_id, account_id, importance, tier, sensitivity,
                 sealed_kind, one_line, reason, deadline, created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
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


/// Upsert a message + FTS + Sent-derived contacts against an explicit
/// connection/transaction handle. Shared by [`SqliteStore::upsert_message`] and
/// the transactional [`Store::ingest_message`] path so both stay in sync.
fn upsert_message_conn(conn: &Connection, msg: &NewMessage) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages(account_id, gmail_msg_id, thread_id, from_addr, from_name,
             subject, received_at, snippet, body, body_html, is_sent)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(account_id, gmail_msg_id) DO UPDATE SET
             thread_id=excluded.thread_id, from_addr=excluded.from_addr,
             from_name=excluded.from_name, subject=excluded.subject,
             received_at=excluded.received_at, snippet=excluded.snippet,
             body=excluded.body, body_html=excluded.body_html, is_sent=excluded.is_sent",
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

fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| CoreError::InvalidInput(format!("bad datetime {s:?}: {e}")))
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

        // 2. Write the triage row IN THE SAME TRANSACTION. For sealed mail this
        //    is the whole point: sensitivity='sealed' is committed atomically
        //    with the message so there is no window where it is queryable as
        //    normal mail. `model_used` stays NULL; combined with
        //    sensitivity='normal' that is the Stage-2 queue predicate for
        //    non-confident rows.
        let deadline_dt = triaged.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
        tx.execute(
            "INSERT INTO triage(message_id, account_id, importance, tier, sensitivity,
                 sealed_kind, one_line, reason, deadline, matched_rule_id, model_used, created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,NULL,?11)
             ON CONFLICT(message_id) DO UPDATE SET
                 importance=excluded.importance, tier=excluded.tier,
                 sensitivity=excluded.sensitivity, sealed_kind=excluded.sealed_kind,
                 one_line=excluded.one_line, reason=excluded.reason,
                 deadline=excluded.deadline, matched_rule_id=excluded.matched_rule_id",
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
                Utc::now().to_rfc3339(),
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
        //   new      = surfaced_at IS NULL
        //   open     = status = 'open'
        let mut sql = String::from(
            "SELECT m.id, m.thread_id, t.tier, t.importance, m.from_addr, t.one_line,
                    t.reason, t.deadline, t.matched_rule_id,
                    t.status, t.surfaced_at, t.resolved_at
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
            Some(SitrepBand::New) => sql.push_str(" AND t.surfaced_at IS NULL"),
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
            ) = row?;
            let deadline = match deadline_s {
                Some(s) => Some(parse_dt(&s)?),
                None => None,
            };
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

    fn list_audit(&self, account_id: AccountId, limit: u32) -> Result<Vec<AuditEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, ts, actor, action, target, detail
             FROM audit_log WHERE account_id=?1 ORDER BY id DESC LIMIT ?2",
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
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, acct, ts, actor, action, target, detail) = row?;
            out.push(AuditEntry {
                id,
                account_id: acct,
                ts: parse_dt(&ts)?,
                actor,
                action,
                target,
                detail,
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
                 COUNT(*) FILTER (WHERE surfaced_at IS NULL),
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

    fn stage2_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage2Queued>> {
        let conn = self.lock()?;
        // The Stage-2 queue predicate, verbatim: non-confident Stage-1 rows are
        // left with model_used IS NULL; sealed rows carry sensitivity='sealed'
        // and are structurally excluded. Join the message for context and
        // LEFT JOIN the matched sender rule for its want_text. is_known_contact
        // is derived from a correlated EXISTS against contacts (mirrors
        // `is_known_contact`).
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
        tx.execute(
            "UPDATE triage SET
                 importance = ?3,
                 tier = ?4,
                 one_line = ?5,
                 reason = ?6,
                 deadline = ?7,
                 model_used = ?8
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
        conn.execute(
            "INSERT INTO stage2_usage(account_id, day, calls, input_tokens, output_tokens)
             VALUES(?1, ?2, 1, ?3, ?4)
             ON CONFLICT(account_id, day) DO UPDATE SET
                 calls = calls + 1,
                 input_tokens = input_tokens + excluded.input_tokens,
                 output_tokens = output_tokens + excluded.output_tokens",
            params![account_id, day, input_tokens as i64, output_tokens as i64],
        )?;
        Ok(())
    }

    fn stage2_usage_today(&self, account_id: AccountId, day: &str) -> Result<Stage2Usage> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT calls, input_tokens, output_tokens FROM stage2_usage
                 WHERE account_id = ?1 AND day = ?2",
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
        }
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
