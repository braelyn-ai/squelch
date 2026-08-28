//! Keyword, semantic (vec0 KNN) and hybrid recall, plus the message-vector
//! writes that feed them.

use super::*;
use crate::store::recency;
use rusqlite::params_from_iter;
use rusqlite::types::Value;
use std::collections::{HashMap, HashSet};
use zerocopy::AsBytes;

/// The FTS5 MATCH WINDOW over the body column (column 1): up to 24 tokens
/// around the matched terms, `…` where the window jumps a gap.
///
/// The open marker is `char(1)` (U+0001) and exists ONLY as a did-the-body-
/// match probe: `snippet()` on a column the terms did not hit returns the
/// column's head — indistinguishable from a real window by content alone — so
/// a subject-only hit would silently swap the curated stored snippet for raw
/// body head. The marker is stripped before anything leaves the store; the
/// client paints highlights itself rather than decoding markup we invented.
const BODY_SNIPPET: &str = "snippet(messages_fts, 1, char(1), '', '…', 24)";

/// The marker `BODY_SNIPPET` plants on each matched term.
const SNIPPET_MARK: char = '\u{1}';

/// A `BODY_SNIPPET` value is a real match window only if the marker is present
/// — otherwise the terms hit the subject (or nothing) and the caller should
/// keep the stored snippet.
fn body_window(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    if raw.contains(SNIPPET_MARK) {
        Some(raw.replace(SNIPPET_MARK, ""))
    } else {
        None
    }
}

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

/// [`map_search_hit`] plus a trailing `BODY_SNIPPET` column (7): the window
/// replaces the stored snippet only when the body really matched.
fn map_search_hit_with_window(r: &rusqlite::Row<'_>) -> rusqlite::Result<SearchHit> {
    let mut hit = map_search_hit(r)?;
    if let Some(window) = body_window(r.get::<_, Option<String>>(7)?) {
        hit.snippet = window;
    }
    Ok(hit)
}

/// Escape the LIKE metacharacters (`%`, `_`) and the escape character itself so
/// a user value is matched LITERALLY. Pairs with `ESCAPE '\'` in the SQL; every
/// `from:` predicate must carry both halves or a `%` in the reader's text turns
/// into a wildcard.
fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Append the operator predicates ([`SearchFilter`]) to a WHERE clause under
/// construction, pushing each value onto `args` as a BOUND parameter — nothing
/// from the reader is ever interpolated into the SQL text.
///
/// Shared by the keyword path and the filter-only listing so the two cannot
/// drift apart in what `from:2026-01-01` means.
fn push_filter_clauses(sql: &mut String, args: &mut Vec<Value>, filter: &SearchFilter) {
    if let Some(from) = &filter.from {
        // Substring on EITHER sender field: `from:jane` should find the address
        // and the display name. SQLite's LIKE is already ASCII-case-insensitive;
        // COLLATE NOCASE on the column operand says so out loud (and sits on the
        // column, not on the ESCAPE literal, where it would bind wrong).
        sql.push_str(
            " AND (m.from_addr COLLATE NOCASE LIKE ? ESCAPE '\\'
                   OR COALESCE(m.from_name, '') COLLATE NOCASE LIKE ? ESCAPE '\\')",
        );
        let pattern = format!("%{}%", escape_like(from));
        args.push(Value::Text(pattern.clone()));
        args.push(Value::Text(pattern));
    }
    if let Some(after) = filter.after {
        // INCLUSIVE: 00:00:00 UTC of the named day is in range. Timestamps are
        // stored as RFC3339 UTC text, which sorts lexicographically.
        sql.push_str(" AND m.received_at >= ?");
        args.push(Value::Text(after.to_rfc3339()));
    }
    if let Some(before) = filter.before {
        // EXCLUSIVE: 00:00:00 UTC of the named day is out of range.
        sql.push_str(" AND m.received_at < ?");
        args.push(Value::Text(before.to_rfc3339()));
    }
}

/// One recall candidate as the leg that produced it saw it: the message id,
/// plus the `received_at` the recency blend needs.
///
/// The timestamp is carried OUT of the recall SQL rather than looked up
/// afterwards. Both legs already join `messages`, so it is free here — and the
/// blend has to happen BEFORE the top-`k` truncation, which is exactly when a
/// hydrated `SearchHit` does not exist yet.
#[derive(Clone, Copy)]
struct Candidate {
    id: i64,
    received_at: DateTime<Utc>,
}

/// The RRF smoothing constant (the standard 60): how far down a list a hit can
/// sit before its vote stops mattering much.
const RRF_K: f32 = 60.0;

/// What one full recency vote is worth, as a fraction of being ranked FIRST on
/// one relevance list.
///
/// ON THESE LEGS RECENCY IS A TERM, NOT A FACTOR — the opposite of the keyword
/// leg's multiplication, and for the opposite reason. RRF scores are
/// deliberately FLAT at the head of a list (ranks 1 through 10 span 15% of one
/// list's vote), so a multiplicative boost with any useful range would stop
/// being a tilt and simply re-sort the top of the results by date. An additive
/// term denominated in the same `1 / (RRF_K + rank)` units stays comparable to
/// the thing it votes against: at 0.5, a fresh hit has to be within roughly a
/// dozen ranks on BOTH legs to overtake an ancient top hit.
const RRF_RECENCY_WEIGHT: f32 = 0.5;

/// The recency term added to one candidate's fused score.
fn recency_vote(received_at: DateTime<Utc>, now: DateTime<Utc>) -> f32 {
    RRF_RECENCY_WEIGHT * recency::boost(received_at, now) as f32 / (RRF_K + 1.0)
}

/// Fuse ranked candidate lists into one order, best first: Reciprocal Rank
/// Fusion across the lists a candidate appears in, plus its recency vote when
/// `sort` asks for one.
///
/// One list in is legal and useful — that is the semantic leg, where RRF is a
/// monotone restatement of the KNN order and the vote is the only thing that
/// can move a row. Under [`SearchSort::BestMatch`] that leg therefore returns
/// the KNN order untouched, which is the honest answer to "no time decay".
///
/// TIES BREAK by `received_at DESC, id DESC`, and that is not decoration. The
/// fused order is the sequence the door's cursor indexes into, while the score
/// map is a `HashMap` whose iteration order is not stable between calls — with
/// no explicit tiebreaker, equal scores could reshuffle between one page and
/// the next and drop or repeat rows across the boundary.
fn fuse_ranked(lists: &[&[Candidate]], sort: SearchSort, now: DateTime<Utc>) -> Vec<Candidate> {
    let mut score: HashMap<i64, f32> = HashMap::new();
    let mut seen: HashMap<i64, Candidate> = HashMap::new();
    for list in lists {
        for (rank, c) in list.iter().enumerate() {
            *score.entry(c.id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
            seen.entry(c.id).or_insert(*c);
        }
    }
    let mut ranked: Vec<(Candidate, f32)> = score
        .into_iter()
        .map(|(id, s)| {
            let c = seen[&id];
            let vote = if sort.considers_recency() {
                recency_vote(c.received_at, now)
            } else {
                0.0
            };
            (c, s + vote)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then(b.0.received_at.cmp(&a.0.received_at))
            .then(b.0.id.cmp(&a.0.id))
    });
    ranked.into_iter().map(|(c, _)| c).collect()
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
    ///
    /// RAW KNN: this is nearest-by-meaning and nothing else. Recency belongs to
    /// the SEARCH surfaces built on top of it — see
    /// [`semantic_search_hits`](Self::semantic_search_hits) — not to the
    /// primitive they share.
    pub fn semantic_search(
        &self,
        account_id: AccountId,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(i64, f32)>> {
        Ok(self
            .semantic_knn(account_id, query_text, k)?
            .into_iter()
            .map(|(c, dist)| (c.id, dist))
            .collect())
    }

    /// Embed `query_text` and KNN it: the shared body of [`semantic_search`] and
    /// [`semantic_search_hits`](Self::semantic_search_hits). Errors when no
    /// embedder is attached, which is what makes `mode=semantic` a hard failure
    /// before the background attach rather than a silently empty result.
    fn semantic_knn(
        &self,
        account_id: AccountId,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(Candidate, f32)>> {
        let embedder = self
            .embedder()
            .ok_or_else(|| CoreError::InvalidInput("no embedder attached".into()))?;
        let qvec = embedder.embed(query_text)?;
        self.knn_by_vector(account_id, &qvec, k)
    }

    /// Lower-level KNN used by [`semantic_knn`](Self::semantic_knn) (and reused
    /// by [`hybrid_search`]): given an already-computed query vector, return the
    /// `k` nearest non-sealed messages for the account, each with its distance.
    fn knn_by_vector(
        &self,
        account_id: AccountId,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(Candidate, f32)>> {
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
            "SELECT v.message_id, v.distance, m.received_at
             FROM message_vecs v
             JOIN messages m ON m.id = v.message_id
             LEFT JOIN triage t ON t.message_id = v.message_id
             WHERE v.embedding MATCH ?1
               AND v.account_id = ?2
               AND v.k = ?3
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
             ORDER BY v.distance",
        )?;
        let rows = stmt.query_map(params![query.as_bytes(), account_id, k as i64], |r| {
            Ok((
                Candidate {
                    id: r.get::<_, i64>(0)?,
                    received_at: dt(r, 2)?,
                },
                r.get::<_, f64>(1)? as f32,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// HYBRID RECALL: merge FTS5 keyword rank and vector distance with Reciprocal
    /// Rank Fusion — each candidate scores `sum(1 / (rrf_k + rank))` across the
    /// lists it appears in, `rrf_k` being the standard smoothing constant (60),
    /// plus a RECENCY vote (see [`fuse_ranked`]). Keyword catches exact tokens,
    /// vectors catch paraphrase, recency breaks the near-ties those two leave
    /// behind, and any one of them alone still produces results. Both recall
    /// legs exclude sealed rows and include sent mail (recall).
    ///
    /// The recency vote is applied BEFORE the top-`k` truncation, so it decides
    /// what makes the window rather than just how the window is displayed.
    ///
    /// `filter` is applied POST-HOC to the hydrated top-`k` window, because
    /// neither recall leg can express `from:`/date bounds in its ranking. See
    /// [`semantic_search_hits`](Self::semantic_search_hits) for what that costs.
    ///
    /// The second return value is WINDOW FULL: the fused candidate pool reached
    /// `k`, so mail below the window may exist. The door's pagination needs it —
    /// a filter can shrink a full window to a short page, and "short page"
    /// alone would read as "no more results" when there may be plenty.
    pub fn hybrid_search(
        &self,
        account_id: AccountId,
        query_text: &str,
        filter: &SearchFilter,
        sort: SearchSort,
        k: usize,
    ) -> Result<(Vec<SearchHit>, bool)> {
        // ONE clock for both legs of one search.
        let now = Utc::now();

        // No embedder (e.g. before the background attach) => keyword-only.
        let vec_hits: Vec<Candidate> = match self.embedder() {
            Some(embedder) => {
                let qvec = embedder.embed(query_text)?;
                self.knn_by_vector(account_id, &qvec, k)?
                    .into_iter()
                    .map(|(c, _dist)| c)
                    .collect()
            }
            None => Vec::new(),
        };

        // FTS ranks over the SAME query text, sent mail included.
        let fts_hits = self.fts_recall(account_id, query_text, k)?;

        let mut ranked = fuse_ranked(&[&vec_hits, &fts_hits], sort, now);
        ranked.truncate(k);
        // Judged BEFORE the filter drops anything: fullness is a property of
        // the recall window, not of what survived the operators.
        let window_full = ranked.len() == k;

        // Only the ids the KEYWORD leg produced have a match window to show; a
        // vector-only hit matched by meaning, not by any term in the body.
        let from_fts: HashSet<i64> = fts_hits.iter().map(|c| c.id).collect();

        let mut out = Vec::with_capacity(ranked.len());
        for c in ranked {
            if let Some(mut hit) = self.search_hit_by_id(account_id, c.id)? {
                if !filter.matches(&hit) {
                    continue;
                }
                if from_fts.contains(&c.id)
                    && let Some(window) = self.fts_snippet(account_id, c.id, query_text)?
                {
                    hit.snippet = window;
                }
                out.push(hit);
            }
        }
        Ok((out, window_full))
    }

    /// The FTS match window for ONE message, or `None` when there is nothing
    /// better than the stored snippet to show: the row is not in the index, the
    /// terms hit the subject rather than the body, or the MATCH expression is
    /// malformed. Every one of those keeps the caller's existing snippet, so a
    /// bad query degrades the preview instead of failing the search.
    ///
    /// SECURITY: `messages_fts` indexes bodies at INGEST, before triage seals
    /// anything, so the index does contain sealed text. The account and sealed
    /// guards are IN THIS QUERY, not delegated to the caller's hydration order
    /// — a future second caller must not be one refactor away from windowing a
    /// sealed body.
    fn fts_snippet(
        &self,
        account_id: AccountId,
        message_id: i64,
        query: &str,
    ) -> Result<Option<String>> {
        let conn = self.lock()?;
        let sql = format!(
            "SELECT {BODY_SNIPPET}
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE f.rowid = ?1
               AND m.account_id = ?2
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
               AND messages_fts MATCH ?3"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        // A syntactically-invalid MATCH errors at step time, not prepare time;
        // both collapse to "no window".
        let raw = stmt
            .query_row(params![message_id, account_id, query], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()
            .unwrap_or(None)
            .flatten();
        Ok(body_window(raw))
    }

    /// SEMANTIC-ONLY recall as hydrated [`SearchHit`]s for the human door's
    /// `mode=semantic` search: the KNN window, reordered by distance rank AND
    /// recency (see [`fuse_ranked`], which a one-list call reduces to exactly
    /// that). Errors without an attached embedder. Sealed rows are excluded in
    /// SQL; sent mail is included (recall). Snippets stay the stored
    /// head-of-message text: a vector hit matched by meaning, so there is no
    /// term window to cut around.
    ///
    /// Recency reorders WITHIN the KNN window and cannot reach outside it —
    /// nothing the vector index did not return can be lifted in by being fresh.
    ///
    /// APPROXIMATION: `filter` narrows the top-`k` window AFTER ranking, since
    /// KNN cannot carry a `from:`/date predicate. A heavily-filtered query can
    /// therefore under-fill a page even when more matching mail exists deeper in
    /// the index; callers over-fetch `k` to soften it (see the door handler).
    ///
    /// The second return value is WINDOW FULL — same contract as
    /// [`hybrid_search`](Self::hybrid_search).
    pub fn semantic_search_hits(
        &self,
        account_id: AccountId,
        query_text: &str,
        filter: &SearchFilter,
        sort: SearchSort,
        k: usize,
    ) -> Result<(Vec<SearchHit>, bool)> {
        let knn = self.semantic_knn(account_id, query_text, k)?;
        let window_full = knn.len() == k;
        let candidates: Vec<Candidate> = knn.into_iter().map(|(c, _dist)| c).collect();
        let ranked = fuse_ranked(&[&candidates], sort, Utc::now());
        let mut out = Vec::with_capacity(ranked.len());
        for c in ranked {
            if let Some(hit) = self.search_hit_by_id(account_id, c.id)?
                && filter.matches(&hit)
            {
                out.push(hit);
            }
        }
        Ok((out, window_full))
    }

    /// FTS5 recall helper for [`hybrid_search`]: candidates in bm25 rank order.
    /// Unlike [`Store::search`] it INCLUDES sent mail, because recall wants the
    /// user's own outbound mail. Sealed rows are excluded in SQL, and a malformed
    /// FTS query yields an empty list rather than an error.
    ///
    /// PURE RELEVANCE ORDER, unlike the keyword leg's own `ORDER BY`: this list
    /// is an INPUT to the fusion, which applies the recency vote once, across
    /// every leg. Blending it in here too would count it twice.
    fn fts_recall(
        &self,
        account_id: AccountId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Candidate>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT m.id, m.received_at
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
            Ok(Candidate {
                id: r.get(0)?,
                received_at: dt(r, 1)?,
            })
        });
        let rows = match rows {
            Ok(r) => r,
            // A syntactically-invalid MATCH expression => no keyword hits.
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(c) => out.push(c),
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

    /// Unfiltered keyword search: [`search_filtered`](Self::search_filtered)
    /// with no operators. The plain-query entry point every non-door caller
    /// uses.
    pub(super) fn search(
        &self,
        account_id: AccountId,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>> {
        self.search_filtered(
            account_id,
            query,
            &SearchFilter::default(),
            SearchSort::default(),
            limit,
            offset,
        )
    }

    /// KEYWORD PATH with the operator half applied in SQL. `text` is already
    /// parsed (see [`crate::store::parse_search_query`]); empty text plus a
    /// filter routes to [`filter_only_listing`](Self::filter_only_listing).
    ///
    /// Ranked by bm25, SCALED BY RECENCY under [`SearchSort::Recent`] (see
    /// [`crate::store::recency`]) and left alone under
    /// [`SearchSort::BestMatch`]. Pagination stays exact either way: the blend
    /// happens in SQL, so LIMIT/OFFSET still cut a page out of the true
    /// ordering instead of out of a fetched window.
    pub(super) fn search_filtered(
        &self,
        account_id: AccountId,
        text: &str,
        filter: &SearchFilter,
        sort: SearchSort,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>> {
        if text.trim().is_empty() {
            // No text AND no filter is not a search — it is "page me the whole
            // mailbox", which no caller means. Refusing here keeps the old
            // `search("")`-errors contract instead of silently listing mail
            // for the next caller who forgets to validate.
            if filter.is_empty() {
                return Err(CoreError::InvalidInput("empty search query".into()));
            }
            return self.filter_only_listing(account_id, filter, limit, offset);
        }
        let conn = self.lock()?;
        // SECURITY: sealed rows excluded in SQL. An untriaged message COALESCEs
        // to non-sealed so freshly-ingested mail is still findable, but a sealed
        // classification always hides the row.
        //
        // The trailing column is the FTS match window; the mapper swaps it in
        // over the stored head-of-message snippet only when the body really
        // matched (see `body_window`) — a subject-only hit keeps the curated
        // snippet.
        let mut sql = format!(
            "SELECT m.id, m.thread_id, m.from_addr, m.from_name, m.subject,
                    m.received_at, m.snippet, {BODY_SNIPPET}
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
               AND m.is_sent = 0
               AND messages_fts MATCH ?"
        );
        let mut args = vec![Value::Integer(account_id), Value::Text(text.to_string())];
        push_filter_clauses(&mut sql, &mut args, filter);
        // THE SORT KEY. Both branches are BIGGEST FIRST — `rank` is bm25 and
        // NEGATIVE (more negative = better), so `-f.rank` is the relevance —
        // which is what lets the tiebreakers below read the same way under
        // either one.
        //
        // RECENCY IS BLENDED IN SQL, not in Rust. This leg paginates with
        // LIMIT/OFFSET and must keep doing that exactly; re-ranking a fetched
        // window would turn exact pagination into the recall legs' over-fetch
        // approximation for no reason.
        //
        // MULTIPLICATIVE, not additive. bm25's magnitude swings by orders of
        // magnitude with how many terms the reader typed and how rare they are
        // — the scores in one result set are comparable only to each other. An
        // additive recency bonus would therefore drown one query and vanish
        // under the next; a factor means the same thing at every scale.
        let relevance = if sort.considers_recency() {
            format!("(-f.rank) * {}", recency::boost_sql("m.received_at", "?"))
        } else {
            "(-f.rank)".to_string()
        };
        sql.push_str(&format!(
            " ORDER BY {relevance} DESC, m.received_at DESC, m.id DESC LIMIT ? OFFSET ?"
        ));
        // The clock is pushed HERE, after the filter's parameters and before
        // LIMIT/OFFSET, because anonymous `?` are numbered in the order they
        // appear in the SQL TEXT and ORDER BY is parsed after WHERE. Under
        // BestMatch the expression has no placeholder, so neither may the args.
        if sort.considers_recency() {
            args.push(Value::Text(Utc::now().to_rfc3339()));
        }
        args.push(Value::Integer(limit as i64));
        args.push(Value::Integer(offset as i64));

        let mut stmt = conn.prepare(&sql)?;
        // A syntactically-invalid MATCH expression errors at STEP time, after a
        // clean prepare. Its siblings (`fts_recall_ids`, `fts_snippet`) already
        // read that as "no keyword hits"; this leg must agree, or the same bad
        // query 200s in hybrid mode and 500s in keyword mode.
        let rows = match stmt.query_map(params_from_iter(args), map_search_hit_with_window) {
            Ok(rows) => rows,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(hit) => out.push(hit),
                Err(_) => return Ok(out),
            }
        }
        Ok(out)
    }

    /// FILTER-ONLY LISTING: the reader typed operators and nothing else
    /// (`from:jane after:2026-01-01`), so there is no text to rank on and no
    /// MATCH to run — this is a plain newest-first page over `messages`.
    ///
    /// SECURITY: identical guarantees to [`search`](Self::search) — sealed rows
    /// excluded via the triage LEFT JOIN, sent mail excluded (`is_sent = 0`), so
    /// dropping the FTS join cannot widen what the door can see. With no filter
    /// at all it is simply the newest non-sealed inbound mail.
    fn filter_only_listing(
        &self,
        account_id: AccountId,
        filter: &SearchFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>> {
        let conn = self.lock()?;
        let mut sql = String::from(
            "SELECT m.id, m.thread_id, m.from_addr, m.from_name, m.subject,
                    m.received_at, m.snippet
             FROM messages m
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
               AND m.is_sent = 0",
        );
        let mut args = vec![Value::Integer(account_id)];
        push_filter_clauses(&mut sql, &mut args, filter);
        // The id tiebreaker matters: Date headers are second-resolution, so a
        // list blast ties routinely, and OFFSET over an unstable sort drops and
        // repeats rows across page boundaries.
        sql.push_str(" ORDER BY m.received_at DESC, m.id DESC LIMIT ? OFFSET ?");
        args.push(Value::Integer(limit as i64));
        args.push(Value::Integer(offset as i64));

        let mut stmt = conn.prepare(&sql)?;
        let out = stmt
            .query_map(params_from_iter(args), map_search_hit)?
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
