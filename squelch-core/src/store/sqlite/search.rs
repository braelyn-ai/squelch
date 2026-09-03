//! Keyword, semantic (vec0 KNN) and hybrid recall, plus the message-vector
//! writes that feed them.

use super::*;
use crate::store::recency;
use rusqlite::params_from_iter;
use rusqlite::types::Value;
use std::collections::{HashMap, HashSet};
use zerocopy::AsBytes;

/// THE FTS5 MATCH WINDOW over the body column (column 1): up to 24 tokens
/// around the matched terms, `…` where the window jumps a gap. No markup: the
/// client paints highlights itself rather than decoding markup we invented.
///
/// Used where the query has ALREADY established that the body matched, which
/// on this leg means a body-scoped MATCH (`body : (...)`). Then a returned row
/// is the proof and the window needs no probe.
const BODY_WINDOW: &str = "snippet(messages_fts, 1, '', '', '…', 24)";

/// [`BODY_WINDOW`] with a did-the-body-match PROBE in the open marker slot.
///
/// `snippet()` on a column the terms did not hit returns that column's head,
/// which is indistinguishable from a real window by content alone, so a
/// subject-only hit would silently swap the curated stored snippet for the raw
/// head of the body. The marker says which happened, and is stripped before
/// anything leaves the store.
///
/// IT IS A HEURISTIC, NOT A GUARANTEE. `messages.body` is flattened
/// sender-controlled text and nothing strips C0 controls at ingest, so a sender
/// who plants U+0001 in their own body makes a subject-only hit on their own
/// mail report a window. The consequence is that the reader sees the head of
/// that sender's body instead of the head of that sender's stored snippet, on
/// their own authed door: cosmetic, and self-inflicted by the one party it
/// affects. The path that could not tolerate even that ([`SqliteStore::fts_snippet`],
/// which runs for hits the keyword leg never produced) asks the question in SQL
/// instead; this one cannot, because its MATCH has to span subject OR body.
const BODY_WINDOW_PROBED: &str = "snippet(messages_fts, 1, char(1), '', '…', 24)";

/// THE RELEVANCE SCORE for every keyword-leg query: bm25 with the SUBJECT
/// weighted four times the body.
///
/// A word in the subject is the sender saying what the mail is about; the same
/// word two hundred words into a newsletter is a coincidence. Unweighted bm25
/// cannot tell those apart — it only counts, and a long body can out-count a
/// three-word subject on term frequency alone. Four is a judgement, not a
/// measurement: enough that a subject match wins a near tie, small enough that
/// a body that is genuinely about the term still beats a subject that mentions
/// it once in passing.
///
/// Like `rank`, this is NEGATIVE and more negative is better, so `-bm25(...)`
/// is the relevance and every ORDER BY here reads biggest-first.
const BM25: &str = "bm25(messages_fts, 4.0, 1.0)";

/// The marker `BODY_WINDOW_PROBED` plants on each matched term.
const SNIPPET_MARK: char = '\u{1}';

/// HOW FAR A DIAGNOSTIC COUNT COUNTS before it answers "that many, at least".
///
/// A thousand is far past every threshold anything reads (see
/// [`SqliteStore::fts_count`] for who reads them) and far short of a full scan
/// of a real mailbox. A capped count is reported as the cap, so a client can
/// tell "a thousand" from "more than we bothered to count" only by knowing this
/// number, which is the trade: the alternative is walking every doclist for a
/// term on every keystroke of an as-you-type search.
pub const DIAGNOSTIC_COUNT_CAP: u32 = 1_000;

/// A `BODY_WINDOW_PROBED` value is a real match window only if the marker is
/// present — otherwise the terms hit the subject (or nothing) and the caller
/// should keep the stored snippet.
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

/// [`map_search_hit`] plus a trailing `BODY_WINDOW_PROBED` column (7): the window
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
        let qvec = embedder.embed(&crate::embed::query_embed_text(query_text))?;
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
               AND m.is_spam = 0
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
        partial: bool,
        k: usize,
    ) -> Result<(Vec<SearchHit>, bool)> {
        // Windows ON: this is the shape a reader's surface uses. A caller that
        // throws the snippet away should say so and ask
        // [`hybrid_search_legs`](Self::hybrid_search_legs) directly.
        let (hits, window_full) =
            self.hybrid_search_legs(account_id, query_text, filter, sort, partial, true, k)?;
        Ok((hits.into_iter().map(|h| h.hit).collect(), window_full))
    }

    /// [`hybrid_search`](Self::hybrid_search), plus WHICH LEG produced each hit.
    ///
    /// The human door reports this per item, because "the keyword leg and the
    /// vector leg both found this" and "only the vectors did" are different
    /// answers to how much to trust a result, and a client that cannot tell
    /// them apart cannot say so to the reader. The plain method above is the
    /// same search with the provenance dropped.
    ///
    /// `want_windows` BUYS ONE QUERY PER HIT, so a caller that drops the
    /// snippet says no. The agent door does: `search_mail` builds its result
    /// from the subject alone, and `k` there is `limit + offset` capped at 600,
    /// so windowing a deep page would compile and run hundreds of statements
    /// for a field nothing reads.
    #[allow(clippy::too_many_arguments)] // the query, the operators, the order, the window
    pub fn hybrid_search_legs(
        &self,
        account_id: AccountId,
        query_text: &str,
        filter: &SearchFilter,
        sort: SearchSort,
        partial: bool,
        want_windows: bool,
        k: usize,
    ) -> Result<(Vec<LeggedHit>, bool)> {
        // ONE clock for both legs of one search.
        let now = Utc::now();

        // No embedder (e.g. before the background attach) => keyword-only.
        let vec_hits: Vec<Candidate> = match self.embedder() {
            Some(embedder) => {
                // The instruction is the QUERY side of BGE's asymmetry; the
                // corpus vectors were embedded without it, on purpose.
                let qvec = embedder.embed(&crate::embed::query_embed_text(query_text))?;
                self.knn_by_vector(account_id, &qvec, k)?
                    .into_iter()
                    .map(|(c, _dist)| c)
                    .collect()
            }
            None => Vec::new(),
        };

        // FTS ranks over the SAME query text, sent mail included.
        let fts = FtsQuery::build(query_text, partial);
        let fts_hits = self.fts_recall(account_id, &fts, k)?;

        let mut ranked = fuse_ranked(&[&vec_hits, &fts_hits], sort, now);
        ranked.truncate(k);
        // Judged BEFORE the filter drops anything: fullness is a property of
        // the recall window, not of what survived the operators.
        let window_full = ranked.len() == k;

        let from_fts: HashSet<i64> = fts_hits.iter().map(|c| c.id).collect();
        let from_vec: HashSet<i64> = vec_hits.iter().map(|c| c.id).collect();

        // ONE LOCK FOR THE WHOLE HYDRATION. Both recall legs are done, so
        // nothing below needs the connection back; taking and releasing the
        // store mutex per hit (twice per hit, with the window) would hand the
        // daemon's sync, triage and notify lanes hundreds of acquisitions to
        // interleave with, to serve one page.
        let conn = self.lock()?;
        let mut out = Vec::with_capacity(ranked.len());
        for c in ranked {
            if let Some(mut hit) = self.search_hit_by_id(&conn, account_id, c.id)? {
                if !filter.matches(&hit) {
                    continue;
                }
                // EVERY hit is asked for a window, not just the ones the
                // keyword leg produced. A vector hit surfaced by meaning may
                // still carry one of the reader's words somewhere deep in its
                // body, and the sentence around that word is the reason to
                // believe the result; the stored head is what a hit gets when
                // the body holds no term at all. The ANY expression is what is
                // asked, because a strict window would find nothing in exactly
                // the mail this whole wave exists for.
                if want_windows
                    && let Some(window) = self.fts_snippet(&conn, account_id, c.id, &fts.any)?
                {
                    hit.snippet = window;
                }
                out.push(LeggedHit {
                    keyword: from_fts.contains(&c.id),
                    vector: from_vec.contains(&c.id),
                    hit,
                });
            }
        }
        Ok((out, window_full))
    }

    /// The FTS match window for ONE message under `match_expr` (an expression
    /// [`FtsQuery`] built, never raw reader text), or `None` when there is
    /// nothing better than the stored snippet to show: the row is not in the
    /// index, the terms hit the subject rather than the body, or the MATCH
    /// expression is malformed. Every one of those keeps the caller's existing
    /// snippet, so a bad query degrades the preview instead of failing the
    /// search.
    ///
    /// THE MATCH IS SCOPED TO THE BODY COLUMN (`body : (...)`), which is what
    /// makes "the terms hit the subject rather than the body" an answer SQLite
    /// gives rather than one we infer. Its sibling on the keyword page has to
    /// infer it from a marker planted in the snippet, because that query's
    /// MATCH must span subject OR body; here there is one message and one
    /// question, so the column filter can ask it outright and a sender cannot
    /// forge the answer by planting the marker in their own body.
    ///
    /// `prepare_cached`, and the connection comes from the caller: this runs
    /// once per hydrated hit, up to `recall_k` (600) of them for a deep page.
    ///
    /// SECURITY: `messages_fts` indexes bodies at INGEST, before triage seals
    /// anything, so the index does contain sealed text. The account and sealed
    /// guards are IN THIS QUERY, not delegated to the caller's hydration order
    /// — a future second caller must not be one refactor away from windowing a
    /// sealed body.
    fn fts_snippet(
        &self,
        conn: &Connection,
        account_id: AccountId,
        message_id: i64,
        match_expr: &str,
    ) -> Result<Option<String>> {
        if match_expr.is_empty() {
            return Ok(None);
        }
        let sql = format!(
            "SELECT {BODY_WINDOW}
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE f.rowid = ?1
               AND m.account_id = ?2
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
               AND m.is_spam = 0
               AND messages_fts MATCH ?3"
        );
        let mut stmt = match conn.prepare_cached(&sql) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        // A syntactically-invalid MATCH errors at step time, not prepare time;
        // both collapse to "no window".
        let scoped = format!("body : ({match_expr})");
        stmt.query_row(params![message_id, account_id, scoped], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()
        .map(|raw| raw.flatten().filter(|w| !w.is_empty()))
        .or(Ok(None))
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
        partial: bool,
        k: usize,
    ) -> Result<(Vec<SearchHit>, bool)> {
        let knn = self.semantic_knn(account_id, query_text, k)?;
        let window_full = knn.len() == k;
        let candidates: Vec<Candidate> = knn.into_iter().map(|(c, _dist)| c).collect();
        let ranked = fuse_ranked(&[&candidates], sort, Utc::now());
        // The vector leg matched by meaning, but the mail it found may still
        // SAY one of the reader's words, and if it does that sentence is worth
        // more than the head of the message. Asked with the ANY expression, so
        // a single term landing anywhere in the body wins a window.
        let fts = FtsQuery::build(query_text, partial);
        // ONE LOCK for the whole hydration; see the twin loop in
        // [`hybrid_search_legs`](Self::hybrid_search_legs).
        let conn = self.lock()?;
        let mut out = Vec::with_capacity(ranked.len());
        for c in ranked {
            if let Some(mut hit) = self.search_hit_by_id(&conn, account_id, c.id)?
                && filter.matches(&hit)
            {
                if let Some(window) = self.fts_snippet(&conn, account_id, c.id, &fts.any)? {
                    hit.snippet = window;
                }
                out.push(hit);
            }
        }
        Ok((out, window_full))
    }

    /// FTS5 recall helper for [`hybrid_search`]: candidates in bm25 rank order,
    /// STRICT MATCHES FIRST and then any-only ones, exactly the order
    /// [`search_filtered`](Self::search_filtered) serves. The two legs share a
    /// builder AND a running order, so keyword mode and hybrid mode cannot
    /// disagree about what a query means.
    ///
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
        fts: &FtsQuery,
        limit: usize,
    ) -> Result<Vec<Candidate>> {
        if fts.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock()?;
        let mut out = self.fts_recall_pass(&conn, account_id, &fts.strict, None, limit)?;
        // One term means the two expressions are identical; anything else and
        // the any-only pass fills the rest of the window BELOW the strict hits.
        if fts.terms.len() > 1 && out.len() < limit {
            let rest = self.fts_recall_pass(
                &conn,
                account_id,
                &fts.any,
                Some(&fts.strict),
                limit - out.len(),
            )?;
            out.extend(rest);
        }
        Ok(out)
    }

    /// One pass of [`fts_recall`](Self::fts_recall): the rows matching `expr`
    /// minus those matching `exclude`, in bm25 order.
    fn fts_recall_pass(
        &self,
        conn: &Connection,
        account_id: AccountId,
        expr: &str,
        exclude: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Candidate>> {
        let mut sql = String::from(
            "SELECT m.id, m.received_at
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
               AND m.is_spam = 0
               AND messages_fts MATCH ?",
        );
        let mut args = vec![Value::Integer(account_id), Value::Text(expr.to_string())];
        if let Some(exclude) = exclude {
            // The subquery carries no sealed/spam guard and needs none: it only
            // SUBTRACTS from a set the outer WHERE has already narrowed, so the
            // worst it could do is hide a row, never reveal one.
            sql.push_str(
                " AND f.rowid NOT IN (
                     SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)",
            );
            args.push(Value::Text(exclude.to_string()));
        }
        // bm25 ascending is best-first (the score is negative), which is what
        // `ORDER BY rank` meant before the subject weight moved the ranking off
        // the table's default.
        sql.push_str(&format!(" ORDER BY {BM25} LIMIT ?"));
        args.push(Value::Integer(limit as i64));
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Ok(Vec::new()),
        };
        let rows = stmt.query_map(params_from_iter(args), |r| {
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
    ///
    /// The connection comes from the caller and the statement is cached: this
    /// runs once per candidate in a hydration loop, so a lock per call and a
    /// fresh compile per call are both paid `recall_k` times over.
    fn search_hit_by_id(
        &self,
        conn: &Connection,
        account_id: AccountId,
        id: i64,
    ) -> Result<Option<SearchHit>> {
        let row = conn
            .prepare_cached(
                "SELECT m.id, m.thread_id, m.from_addr, m.from_name, m.subject,
                        m.received_at, m.snippet
                 FROM messages m
                 LEFT JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2
                   AND COALESCE(t.sensitivity, 'normal') != 'sealed'
                   AND m.is_spam = 0",
            )?
            .query_row(params![account_id, id], map_search_hit)
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
            false,
            limit,
            offset,
        )
    }

    /// ONE PAGE OF THE KEYWORD LEG: the messages matching `expr`, MINUS those
    /// matching `exclude` when it is given, ranked by bm25 blended with recency
    /// (see the sort-key comment in [`search_filtered`](Self::search_filtered)).
    ///
    /// `exclude` is what makes the any-term pass an ANY-ONLY pass: the rows the
    /// strict pass already served are subtracted here rather than deduplicated
    /// in Rust, so LIMIT/OFFSET keep cutting an exact page out of a real
    /// ordering instead of out of a fetched window.
    ///
    /// SECURITY: the sealed, spam and sent predicates live in this one place, so
    /// both passes carry them by construction.
    #[allow(clippy::too_many_arguments)] // one keyword query, one argument per part
    fn keyword_page(
        &self,
        conn: &Connection,
        account_id: AccountId,
        expr: &str,
        exclude: Option<&str>,
        filter: &SearchFilter,
        sort: SearchSort,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>> {
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
                    m.received_at, m.snippet, {BODY_WINDOW_PROBED}
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
               AND m.is_sent = 0
               AND m.is_spam = 0
               AND messages_fts MATCH ?"
        );
        let mut args = vec![Value::Integer(account_id), Value::Text(expr.to_string())];
        if let Some(exclude) = exclude {
            // The subquery carries no sealed/spam guard and needs none: it only
            // SUBTRACTS from a set the outer WHERE has already narrowed, so the
            // worst it could do is hide a row, never reveal one.
            sql.push_str(
                " AND f.rowid NOT IN (
                     SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)",
            );
            args.push(Value::Text(exclude.to_string()));
        }
        push_filter_clauses(&mut sql, &mut args, filter);
        // THE SORT KEY. Both branches are BIGGEST FIRST — bm25 is NEGATIVE
        // (more negative = better), so `-bm25(...)` is the relevance, which is
        // what lets the tiebreakers below read the same way under either one.
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
            format!("(-{BM25}) * {}", recency::boost_sql("m.received_at", "?"))
        } else {
            format!("(-{BM25})")
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
        // clean prepare. `FtsQuery` makes that unreachable from a search box,
        // but the sibling legs read a bad MATCH as "no keyword hits" and this
        // one must agree, or the same bad query 200s in hybrid mode and 500s in
        // keyword mode.
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

    /// How many messages the STRICT expression matches under exactly the
    /// predicates [`keyword_page`](Self::keyword_page) applies. The offset the
    /// any-only pass starts from is `offset - this`, so the two passes join
    /// without a gap and without a repeat.
    fn keyword_total(
        &self,
        conn: &Connection,
        account_id: AccountId,
        expr: &str,
        filter: &SearchFilter,
    ) -> Result<u32> {
        let mut sql = String::from(
            "SELECT COUNT(*)
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
               AND m.is_sent = 0
               AND m.is_spam = 0
               AND messages_fts MATCH ?",
        );
        let mut args = vec![Value::Integer(account_id), Value::Text(expr.to_string())];
        push_filter_clauses(&mut sql, &mut args, filter);
        let mut stmt = conn.prepare(&sql)?;
        // Same reading as every other MATCH here: unparseable means nothing
        // matched, not "the search failed".
        let n: i64 = stmt
            .query_row(params_from_iter(args), |r| r.get(0))
            .unwrap_or(0);
        Ok(n.max(0) as u32)
    }

    /// KEYWORD PATH with the operator half applied in SQL. `text` is already
    /// parsed (see [`crate::store::parse_search_query`]); empty text plus a
    /// filter routes to [`filter_only_listing`](Self::filter_only_listing).
    ///
    /// AND, THEN OR. The page is the STRICT matches (every term) in rank order,
    /// followed by the mail carrying only SOME of the terms, in rank order,
    /// with the strict rows subtracted so nothing appears twice. A message that
    /// matches everything therefore can never sink below one that matches less,
    /// and a query with one word the right mail happens to lack stops returning
    /// nothing at all. See [`FtsQuery::build`] for the mailbox that taught us.
    ///
    /// Ranked by bm25, SCALED BY RECENCY under [`SearchSort::Recent`] (see
    /// [`crate::store::recency`]) and left alone under
    /// [`SearchSort::BestMatch`]. PAGINATION STAYS EXACT across the boundary
    /// between the two passes: the strict pass is counted, the page is cut out
    /// of it by LIMIT/OFFSET, and whatever the page still owes is taken from
    /// the any-only pass at `offset - strict_total`.
    ///
    /// `partial` matches the LAST term as a prefix, for the panel's
    /// as-you-type fetch.
    #[allow(clippy::too_many_arguments)] // the query, the operators, the order, the page
    pub(super) fn search_filtered(
        &self,
        account_id: AccountId,
        text: &str,
        filter: &SearchFilter,
        sort: SearchSort,
        partial: bool,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>> {
        let fts = FtsQuery::build(text, partial);
        if fts.is_empty() {
            // No text AND no filter is not a search — it is "page me the whole
            // mailbox", which no caller means. Refusing here keeps the old
            // `search("")`-errors contract instead of silently listing mail
            // for the next caller who forgets to validate.
            if filter.is_empty() {
                // The reader typed SOMETHING the index cannot hold ("???"),
                // which is an honest empty result rather than an error and
                // definitely not an invitation to list the mailbox.
                if text.trim().is_empty() {
                    return Err(CoreError::InvalidInput("empty search query".into()));
                }
                return Ok(Vec::new());
            }
            return self.filter_only_listing(account_id, filter, limit, offset);
        }
        let conn = self.lock()?;
        // ONE TERM means strict and any are the same expression, so there is no
        // any-only pass to run and no count to take: the strict page IS the
        // page. Skipping both is not just an optimisation — a `NOT IN` against
        // an identical MATCH would correctly return nothing, and paying for two
        // more FTS scans to learn that is silly.
        if fts.terms.len() == 1 {
            return self.keyword_page(
                &conn,
                account_id,
                &fts.strict,
                None,
                filter,
                sort,
                limit,
                offset,
            );
        }
        let strict_total = self.keyword_total(&conn, account_id, &fts.strict, filter)?;
        let mut out = Vec::new();
        if offset < strict_total {
            out = self.keyword_page(
                &conn,
                account_id,
                &fts.strict,
                None,
                filter,
                sort,
                limit,
                offset,
            )?;
        }
        // What the page still owes comes off the any-only ranking. Its offset
        // is how far past the strict block this page starts (zero when the page
        // straddles the boundary, because the strict rows above have already
        // been served).
        let owed = limit as usize - out.len().min(limit as usize);
        if owed > 0 {
            let any_offset = offset.saturating_sub(strict_total);
            let rest = self.keyword_page(
                &conn,
                account_id,
                &fts.any,
                Some(&fts.strict),
                filter,
                sort,
                owed as u32,
                any_offset,
            )?;
            out.extend(rest);
        }
        Ok(out)
    }

    /// WHAT RETRIEVAL MADE OF THE READER'S WORDS: how many messages match every
    /// term, how many match any term, and the document frequency of each term
    /// on its own. The door reports this beside the hits; §5 of docs/SEARCH.md
    /// is what reads it.
    ///
    /// `include_sent` FOLLOWS THE LEG THAT RAN, and the caller passes what its
    /// own mode did: keyword mode excludes the user's sent mail from its hits,
    /// the recall legs include it. Counts that disagreed with the list beside
    /// them would be worse than no counts, because they would look like a
    /// missing page rather than a different question.
    ///
    /// EVERY COUNT STOPS AT [`DIAGNOSTIC_COUNT_CAP`]. This runs on every
    /// request, the panel's as-you-type ones included, so an exact frequency is
    /// not worth a full doclist walk under the store mutex; nothing that reads
    /// these needs one.
    ///
    /// SECURITY: account-scoped, sealed rows excluded, spam rows excluded —
    /// exactly the predicates the hit queries carry, and for a sharper reason
    /// here. A document frequency is a yes/no oracle over message text, so a
    /// count that could see sealed mail would answer questions about sealed
    /// mail one word at a time, without ever returning a row.
    pub fn search_diagnostics(
        &self,
        account_id: AccountId,
        text: &str,
        partial: bool,
        include_sent: bool,
    ) -> Result<SearchDiagnostics> {
        let fts = FtsQuery::build(text, partial);
        if fts.is_empty() {
            // Operators only, or punctuation only: nothing was looked up, and
            // zero is the honest report rather than a count of the mailbox.
            return Ok(SearchDiagnostics::default());
        }
        let conn = self.lock()?;
        let mut terms = Vec::with_capacity(fts.terms.len());
        for (term, expr) in fts.terms.iter().zip(fts.term_exprs.iter()) {
            // Each term is counted BY THE EXPRESSION THAT RANKED IT, straight
            // off the builder — with `partial` on, the tail matched a whole OR
            // group, and a df for the bare word would describe a search nobody
            // ran. Rebuilding the string here is what let the two drift.
            terms.push(TermDf {
                text: term.clone(),
                df: self.fts_count(&conn, account_id, expr, include_sent)?,
            });
        }
        Ok(SearchDiagnostics {
            strict_hits: self.fts_count(&conn, account_id, &fts.strict, include_sent)?,
            any_hits: self.fts_count(&conn, account_id, &fts.any, include_sent)?,
            terms,
        })
    }

    /// How many non-sealed, non-spam messages of this account match `expr`,
    /// COUNTED NO FURTHER THAN [`DIAGNOSTIC_COUNT_CAP`]. The counting half of
    /// [`search_diagnostics`](Self::search_diagnostics); no operators, because
    /// a count of "what the index holds" is not a count of one filtered page.
    ///
    /// The cap is what makes this affordable on every keystroke. An uncapped
    /// `COUNT(*)` over a MATCH walks the whole doclist, and this runs
    /// `terms + 2` times per request while holding the store mutex that sync,
    /// triage and notify also queue on — the shape of the p95 floor the
    /// kill-p95 branch had to remove. Nothing reads an exact frequency: §5 of
    /// docs/SEARCH.md asks "did anything match all of it", and the panel asks
    /// "is this word rare". Zero, some and many is the whole vocabulary.
    fn fts_count(
        &self,
        conn: &Connection,
        account_id: AccountId,
        expr: &str,
        include_sent: bool,
    ) -> Result<u32> {
        let sent = if include_sent {
            ""
        } else {
            " AND m.is_sent = 0"
        };
        // The LIMIT sits INSIDE the counted subquery, which is what stops the
        // scan; a LIMIT on the COUNT itself would only limit the one row it
        // returns, after the whole doclist had been walked.
        let sql = format!(
            "SELECT COUNT(*) FROM (
                 SELECT 1
                 FROM messages_fts f
                 JOIN messages m ON m.id = f.rowid
                 LEFT JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1
                   AND COALESCE(t.sensitivity, 'normal') != 'sealed'
                   AND m.is_spam = 0{sent}
                   AND messages_fts MATCH ?2
                 LIMIT ?3
             )"
        );
        // Same reading as every other MATCH on this leg.
        let n: i64 = conn
            .query_row(
                &sql,
                params![account_id, expr, DIAGNOSTIC_COUNT_CAP as i64],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(n.max(0) as u32)
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
               AND m.is_sent = 0
               AND m.is_spam = 0",
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
               AND m.is_spam = 0
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
