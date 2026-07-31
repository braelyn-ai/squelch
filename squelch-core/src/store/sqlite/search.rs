//! Keyword, semantic (vec0 KNN) and hybrid recall, plus the message-vector
//! writes that feed them.

use super::*;
use zerocopy::AsBytes;

/// The envelope columns both hit-producing SELECTs share, in SELECT order.
fn map_search_hit(r: &rusqlite::Row<'_>) -> rusqlite::Result<SearchHit> {
    Ok(SearchHit {
        id: r.get(0)?,
        thread_id: r.get(1)?,
        from_addr: r.get(2)?,
        from_name: r.get(3)?,
        subject: r.get(4)?,
        received_at: dt(r, 5)?,
        snippet: r.get(6)?,
    })
}

impl SqliteStore {
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
                map_search_hit,
            )
            .optional()?;
        Ok(row)
    }

    pub(super) fn search(
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
        let out = stmt
            .query_map(
                params![account_id, query, limit as i64, offset as i64],
                map_search_hit,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn upsert_message_vector(
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

    pub(super) fn messages_missing_vectors(
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
}
