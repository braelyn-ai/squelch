//! The Stage-1 / Stage-2 / extract queues, their applies, the LLM usage
//! ledger and the per-account cap settings.

use super::messages::rewrite_deadline_conn;
use super::*;

// ---- LLM usage ledger helpers (shared by the stage-1 and stage-2 categories) --

/// Bump the `stage2_usage` ledger for `(account, day, category)`: +1 call and add
/// the token counts. Both triage stages share this table keyed by `category`.
#[allow(clippy::too_many_arguments)] // the parts of one usage ledger line
fn bump_usage_category(
    conn: &Connection,
    account_id: AccountId,
    day: &str,
    category: &str,
    tokens: UsageTokens,
) -> Result<()> {
    conn.execute(
        "INSERT INTO stage2_usage(account_id, day, category, calls, input_tokens, output_tokens,
                                  cache_creation_tokens, cache_read_tokens)
         VALUES(?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)
         ON CONFLICT(account_id, day, category) DO UPDATE SET
             calls = calls + 1,
             input_tokens = input_tokens + excluded.input_tokens,
             output_tokens = output_tokens + excluded.output_tokens,
             cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
             cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens",
        params![
            account_id,
            day,
            category,
            tokens.input as i64,
            tokens.output as i64,
            tokens.cache_creation as i64,
            tokens.cache_read as i64
        ],
    )?;
    Ok(())
}

/// How many thread siblings an escalated row carries into the prompt. Bounded
/// because a 200-message mailing-list thread is not context, it is a bill.
const STAGE2_THREAD_CONTEXT_LIMIT: usize = 8;

/// Aggregate this sender's track record: how much of their mail has surfaced,
/// and how often the account owner overruled the verdict. Counts only — no
/// subject, no body, nothing that could carry an instruction.
fn sender_history_conn(
    conn: &Connection,
    account_id: AccountId,
    from_addr: &str,
    exclude_message_id: i64,
) -> Result<SenderHistory> {
    // The row under judgement is excluded. Counting it would report "1 previous
    // message" for a sender who has never written before, which is the opposite
    // of the fact this field exists to convey.
    let (total, surfaced): (i64, i64) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN t.tier IN ('signal','deadline','past_due')
                                  THEN 1 ELSE 0 END), 0)
         FROM messages m
         JOIN triage t ON t.message_id = m.id
         WHERE m.account_id = ?1
           AND m.from_addr = ?2 COLLATE NOCASE
           AND m.id != ?3
           AND m.is_sent = 0 AND m.is_spam = 0
           AND t.sensitivity = 'normal'",
        params![account_id, from_addr, exclude_message_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let corrected: i64 = conn.query_row(
        "SELECT COUNT(*) FROM triage_feedback
         WHERE account_id = ?1 AND sender = ?2 COLLATE NOCASE",
        params![account_id, from_addr],
        |r| r.get(0),
    )?;
    Ok(SenderHistory {
        total,
        surfaced,
        corrected,
    })
}

/// The rest of a thread, oldest first, verdict and envelope only.
///
/// SEALED SIBLINGS ARE EXCLUDED IN SQL. A sealed message's subject is exactly as
/// forbidden to a model as its body, and "it was only context for another row"
/// is not an exception — see docs/SECURITY.md. Sent siblings ARE included, and
/// are the most valuable rows here: a thread the owner has replied in is one
/// they have already voted for.
///
/// THE MOST RECENT `limit` SIBLINGS, re-sorted ascending for display. Taking the
/// oldest instead would drop the owner's own recent reply off the end of any
/// thread longer than the cap, and the prompt states "the account owner HAS
/// WRITTEN / has never written in it" as a fact — a window that can silently
/// omit the reply turns the single most useful signal here into a false one.
fn thread_siblings_conn(
    conn: &Connection,
    account_id: AccountId,
    thread_id: &str,
    exclude_message_id: i64,
    limit: usize,
) -> Result<Vec<ThreadSibling>> {
    if thread_id.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT m.from_addr, m.subject, COALESCE(t.one_line, ''), COALESCE(t.tier, 'noise'),
                m.received_at, m.is_sent
         FROM messages m
         JOIN triage t ON t.message_id = m.id
         WHERE m.account_id = ?1
           AND m.thread_id = ?2
           AND m.id != ?3
           AND t.sensitivity = 'normal'
         ORDER BY m.received_at DESC
         LIMIT ?4",
    )?;
    let mut out = stmt
        .query_map(
            params![account_id, thread_id, exclude_message_id, limit as i64],
            |r| {
                Ok(ThreadSibling {
                    from_addr: r.get(0)?,
                    subject: r.get(1)?,
                    one_line: r.get(2)?,
                    tier: Tier::parse(&r.get::<_, String>(3)?).unwrap_or(Tier::Noise),
                    received_at: dt(r, 4)?,
                    is_sent: r.get::<_, i64>(5)? != 0,
                })
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    // Newest-first off the index, oldest-first for the reader: the prompt renders
    // these as a conversation, and a conversation runs forwards.
    out.reverse();
    Ok(out)
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
                COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0)
         FROM stage2_usage
         WHERE account_id = ?1 AND day >= ?2 AND category = ?3",
        params![account_id, since_day, category],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        },
    )?;
    Ok(Stage2Usage {
        calls: row.0.max(0) as u64,
        input_tokens: row.1.max(0) as u64,
        output_tokens: row.2.max(0) as u64,
        cache_creation_tokens: row.3.max(0) as u64,
        cache_read_tokens: row.4.max(0) as u64,
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
        "SELECT day, calls, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens
         FROM stage2_usage
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
                cache_creation_tokens: r.get::<_, i64>(4)?.max(0) as u64,
                cache_read_tokens: r.get::<_, i64>(5)?.max(0) as u64,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

impl SqliteStore {
    pub(super) fn extract_queue(
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
        // Hand-requested rows sort first; see `stage1_queue`.
        let sql = format!(
            "SELECT m.id, m.thread_id, m.from_addr, m.from_name, m.subject, m.body,
                    t.category, t.sensitivity, m.received_at, t.retriage_at
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1
               AND t.category IN ({placeholders})
               AND t.extractor_model_used IS NULL
               AND t.sensitivity = 'normal'
               AND m.is_sent = 0 AND m.is_spam = 0
               AND NOT EXISTS(
                   SELECT 1 FROM receipts r
                   WHERE r.account_id = t.account_id AND r.message_id = t.message_id
               )
             ORDER BY t.retriage_at IS NULL, t.retriage_at DESC, m.received_at DESC
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
        let out = stmt
            .query_map(bind_refs.as_slice(), |r| {
                Ok(ExtractQueued {
                    message_id: r.get(0)?,
                    account_id,
                    thread_id: r.get(1)?,
                    from_addr: r.get(2)?,
                    from_name: r.get(3)?,
                    subject: r.get(4)?,
                    body: r.get(5)?,
                    category: r.get(6)?,
                    sensitivity: Sensitivity::parse(&r.get::<_, String>(7)?),
                    received_at: dt(r, 8)?,
                    retriage_at: dt_opt(r, 9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn ship_extract_queue(
        &self,
        account_id: AccountId,
        limit: usize,
    ) -> Result<Vec<ExtractQueued>> {
        let conn = self.lock()?;
        // Same projection as `extract_queue`, DIFFERENT predicate — and
        // deliberately not that method with another category list. `extract_queue`
        // routes on `triage.category` (no shipping category exists) and excludes
        // receipt-bearing messages, which most order confirmations are; both would
        // silently empty this queue.
        //
        // COALESCE on the category because the trigger is stamped at INGEST: a row
        // can queue here before Stage-1 ever assigns a category, and
        // `ExtractQueued.category` is a `String`.
        // Hand-requested rows sort first; see `stage1_queue`.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.thread_id, m.from_addr, m.from_name, m.subject, m.body,
                    COALESCE(t.category, ''), t.sensitivity, m.received_at, t.retriage_at
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1
               AND t.ship_extract_model = 'pending'
               AND t.sensitivity = 'normal'
               AND m.is_sent = 0 AND m.is_spam = 0
             ORDER BY t.retriage_at IS NULL, t.retriage_at DESC, m.received_at DESC
             LIMIT ?2",
        )?;
        let out = stmt
            .query_map(params![account_id, limit as i64], |r| {
                Ok(ExtractQueued {
                    message_id: r.get(0)?,
                    account_id,
                    thread_id: r.get(1)?,
                    from_addr: r.get(2)?,
                    from_name: r.get(3)?,
                    subject: r.get(4)?,
                    body: r.get(5)?,
                    category: r.get(6)?,
                    sensitivity: Sensitivity::parse(&r.get::<_, String>(7)?),
                    received_at: dt(r, 8)?,
                    retriage_at: dt_opt(r, 9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn ship_extract_mark(
        &self,
        account_id: AccountId,
        message_id: i64,
        marker: &str,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE triage SET ship_extract_model = ?3
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![message_id, account_id, marker],
        )?;
        Ok(())
    }

    pub(super) fn retriage_reset(
        &self,
        account_id: AccountId,
        message_id: Option<i64>,
        days: u32,
    ) -> Result<u64> {
        let conn = self.lock()?;
        let now = Utc::now();
        let cutoff = (now - chrono::Duration::days(days as i64)).to_rfc3339();
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
        // The shipments trigger is NOT a model marker to clear: NULL means "no
        // shipping signal at ingest", and blanking it would be indistinguishable
        // from that. Only a row that EVER carried a signal is re-pended; a NULL
        // stays NULL and re-enters nothing.
        //
        // AND THE STAMP THAT SAYS A HUMAN ASKED. Every LLM pass skips mail older
        // than its age cutoff, so without this a re-triage of anything past the
        // window requeues the row only to have the next tick mark it processed
        // with no model call — a request answered by a no-op. The passes read it
        // through `triage::retriage_forced`, which expires it after a day so the
        // force covers this request and not the row's whole future.
        let update = format!(
            "UPDATE triage SET stage1_model_used = NULL, model_used = NULL,
                    needs_stage2 = 0, extractor_model_used = NULL,
                    retriage_at = ?3,
                    ship_extract_model = CASE
                        WHEN ship_extract_model IS NOT NULL THEN 'pending' ELSE NULL END
             WHERE account_id = ?1
               AND COALESCE(sensitivity, 'normal') = 'normal'
               AND COALESCE(stage1_model_used, '') NOT IN ('rule', 'n/a', 'human')
               AND message_id IN (
                   SELECT m.id FROM messages m
                   WHERE m.account_id = ?1 AND m.is_sent = 0 AND m.is_spam = 0 AND {scope_sql}
               )"
        );
        let n = conn.execute(
            &update,
            rusqlite::params![account_id, scope_param, now.to_rfc3339()],
        )?;
        // The rows the UPDATE just reset (their Stage-1 marker is now NULL),
        // reused by every specialist cleanup below.
        let reset_scope = format!(
            "SELECT t.message_id FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1 AND t.stage1_model_used IS NULL
               AND m.is_sent = 0 AND m.is_spam = 0 AND {scope_sql}"
        );
        // Drop stale specialist rows; re-extraction recreates them, possibly
        // under a different category verdict. MARKETING is here for the same
        // reason banking is — it was missing, so re-triage left its rows behind
        // pointing at a category the row no longer has.
        for table in ["banking", "marketing"] {
            let del = format!(
                "DELETE FROM {table}
                 WHERE account_id = ?1 AND message_id IN ({reset_scope})"
            );
            conn.execute(&del, rusqlite::params![account_id, scope_param])?;
        }
        // Staged orders too: they are keyed by the retailer's order reference,
        // not by a tracking number, so unlike `shipments` they carry no
        // carrier-poll state worth preserving and the re-run recreates them.
        // `shipments` rows are deliberately NOT deleted here — identity-keyed and
        // poll-bearing, they outlive any one email.
        let del_orders = format!(
            "DELETE FROM shipment_orders
             WHERE account_id = ?1 AND last_message_id IN ({reset_scope})"
        );
        conn.execute(&del_orders, rusqlite::params![account_id, scope_param])?;
        // AND THE NAMES THOSE MESSAGES MERELY DONATED, in both tables. The
        // `shipments` row itself survives (identity-keyed, poll-bearing), but its
        // `item_name` is mail-derived and three extractor paths write one onto a
        // row a DIFFERENT message feeds — which is why `item_name_msg` records
        // whose extraction supplied it. Without this, a re-extraction that finds
        // no item name, or decides the mail was never a shipment, leaves the OLD
        // name on the card forever: re-triage is supposed to redo the verdict,
        // not preserve half of it. Scrubbing by provenance is exact, and matches
        // what sealing already does in `feedback.rs` — only scoped to the reset
        // set rather than to one message.
        //
        // `shipments` also loses its `item_name_source` marker (back to
        // 'regex'), for the reason sealing does: a source that outlives its name
        // would lock the row out of taking a regex name on the next email.
        // `shipment_orders` has no such column — only the extractor writes it.
        for (table, source_reset) in [
            ("shipments", ", item_name_source = 'regex'"),
            ("shipment_orders", ""),
        ] {
            let scrub = format!(
                "UPDATE {table} SET item_name = '', item_name_msg = NULL{source_reset}
                 WHERE account_id = ?1 AND item_name_msg IN ({reset_scope})"
            );
            conn.execute(&scrub, rusqlite::params![account_id, scope_param])?;
        }
        Ok(n as u64)
    }

    /// How far the live re-triage has got — see [`RetriageProgress`].
    ///
    /// PENDING IS THE TWO QUEUE PREDICATES, NOT A MARKER OF ITS OWN. A row is
    /// still being worked exactly when `stage1_queue` or `stage2_queue` would
    /// still hand it out, so the two spellings are kept side by side here and
    /// any change to a queue's WHERE has to be answered in this one. Anything
    /// else drifts: a "done = stage1_model_used IS NOT NULL" counter would
    /// reach 100% while every escalated row was still waiting on Stage-2.
    ///
    /// The window matches [`crate::triage::retriage_forced`] rather than
    /// restating 24 hours, and it is a `>=` on the stamp, which keeps a
    /// FUTURE-dated stamp (clock skew) in the run exactly as that predicate does.
    pub(super) fn retriage_progress(&self, account_id: AccountId) -> Result<RetriageProgress> {
        let conn = self.lock()?;
        let since = (Utc::now() - crate::triage::RETRIAGE_FORCE_WINDOW).to_rfc3339();
        // Sealed and sent rows are excluded for the same reason the queues
        // exclude them: they re-enter nothing, so counting one would leave the
        // run permanently short of its own total. `retriage_reset` never stamps
        // them, but a row SEALED AFTER its stamp was written would otherwise
        // wedge the counter at 99%.
        let (total, done, started_at) = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(
                        CASE WHEN t.stage1_model_used IS NULL
                                  OR (t.needs_stage2 = 1 AND t.model_used IS NULL)
                             THEN 0 ELSE 1 END), 0),
                    MIN(t.retriage_at)
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1
               AND t.retriage_at >= ?2
               AND t.sensitivity = 'normal'
               AND m.is_sent = 0 AND m.is_spam = 0",
            params![account_id, since],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )?;
        Ok(RetriageProgress {
            total,
            done,
            started_at: started_at
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc)),
        })
    }

    pub(super) fn extract_mark_processed(
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

    // ---- STAGE-2 ----------------------------------------------------------

    pub(super) fn stage1_queue(
        &self,
        account_id: AccountId,
        limit: usize,
    ) -> Result<Vec<Stage1Queued>> {
        let conn = self.lock()?;
        // Rows still needing Stage-1: heuristic seed values in place
        // (stage1_model_used IS NULL), non-sealed, non-sent. Rule-decided rows
        // carry stage1_model_used='rule' and are excluded.
        //
        // ORDER: a row a human asked for by hand comes FIRST, most recent request
        // first, and only then the newest-first backlog. `LIMIT batch_per_cycle`
        // is a real ceiling — behind a backlog, a re-triaged row sorted purely by
        // age would wait ticks for its turn, which reads exactly like the skip it
        // is not. Every other queue orders the same way, for the same reason.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.thread_id, m.from_addr, m.subject, m.body, t.sensitivity,
                    m.received_at, t.retriage_at,
                    EXISTS(
                        SELECT 1 FROM contacts c
                        WHERE c.account_id = m.account_id
                          AND c.addr = m.from_addr COLLATE NOCASE
                          AND c.sent_count > 0
                    ) AS is_known,
                    EXISTS(
                        SELECT 1 FROM triage_feedback f
                        WHERE f.account_id = m.account_id
                          AND f.sender = m.from_addr COLLATE NOCASE
                    ) AS sender_corrected
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1
               AND t.stage1_model_used IS NULL
               AND t.sensitivity = 'normal'
               AND m.is_sent = 0 AND m.is_spam = 0
             ORDER BY t.retriage_at IS NULL, t.retriage_at DESC, m.received_at DESC
             LIMIT ?2",
        )?;
        let out = stmt
            .query_map(params![account_id, limit as i64], |r| {
                Ok(Stage1Queued {
                    message_id: r.get(0)?,
                    account_id,
                    thread_id: r.get(1)?,
                    from_addr: r.get(2)?,
                    subject: r.get(3)?,
                    body: r.get(4)?,
                    sensitivity: Sensitivity::parse(&r.get::<_, String>(5)?),
                    received_at: dt(r, 6)?,
                    retriage_at: dt_opt(r, 7)?,
                    is_known_contact: r.get::<_, i64>(8)? != 0,
                    sender_corrected: r.get::<_, i64>(9)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn stage1_apply(&self, applied: &Stage1Applied) -> Result<bool> {
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
                 category = COALESCE(?11, category),
                 escalation_reason = ?12
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
                applied.escalation_reason,
            ],
        )?;
        // TOCTOU: a row sealed by hand between the queue SELECT and this apply
        // makes the guard match nothing. The verdict did NOT land, so skip the
        // deadlines rewrite too — a sealed message must not grow a fresh deadline
        // row — and report `false` so the caller emits no event.
        if n > 0 {
            rewrite_deadline_conn(
                &tx,
                applied.account_id,
                applied.message_id,
                applied.deadline.as_ref(),
            )?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    // ---- REVISITS ---------------------------------------------------------

    /// Store a message's planned re-evaluations, replacing any still-PENDING
    /// ones. Replacing rather than appending is what keeps a re-triaged row from
    /// accumulating a backlog of stale schedules from every prior verdict; rows
    /// already FIRED are left alone, because those are history.
    pub(super) fn revisits_schedule(
        &self,
        account_id: AccountId,
        message_id: i64,
        requests: &[crate::triage::revisit::RevisitRequest],
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM triage_revisits
             WHERE account_id = ?1 AND message_id = ?2 AND fired_at IS NULL",
            params![account_id, message_id],
        )?;
        // A SEALED row must never carry a schedule: firing one would put the
        // message back in front of a model. The guard lives here rather than at
        // the call site so no future caller can route around it.
        let sealed: bool = tx
            .query_row(
                "SELECT sensitivity != 'normal' FROM triage
                 WHERE account_id = ?1 AND message_id = ?2",
                params![account_id, message_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|v| v != 0)
            .unwrap_or(true);
        if !sealed {
            for r in requests {
                tx.execute(
                    "INSERT INTO triage_revisits
                         (account_id, message_id, revisit_at, reason, source, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        account_id,
                        message_id,
                        r.at.to_rfc3339(),
                        r.why,
                        r.source.as_str(),
                        now.to_rfc3339(),
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Revisits that have come due: pending, past their date, on a NON-SEALED,
    /// NOT-DONE row that has not exhausted its lifetime budget and that the
    /// account owner has not corrected by hand.
    ///
    /// The human-correction exclusion is the important one. A revisit rewrites a
    /// verdict, and a verdict the owner personally fixed is the one thing in
    /// this system a model may never overwrite.
    ///
    /// `status = 'done'` is excluded for a weaker version of the same reason, and
    /// it is not a rare case: receipts and banking rows are INGESTED done so they
    /// live only in their rail, and a model-scheduled revisit on one of those
    /// would spend a call to re-open something nobody asked to see again. The
    /// staleness sweep has always excluded them; the due queue has to agree.
    pub(super) fn revisit_queue(
        &self,
        account_id: AccountId,
        now: DateTime<Utc>,
        max_lifetime: u32,
        limit: usize,
    ) -> Result<Vec<RevisitQueued>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT v.id, m.id, m.thread_id, m.from_addr, m.subject, m.body,
                    t.sensitivity, m.received_at, v.revisit_at, v.reason, v.source,
                    t.tier, t.importance, t.one_line,
                    EXISTS(
                        SELECT 1 FROM contacts c
                        WHERE c.account_id = m.account_id
                          AND c.addr = m.from_addr COLLATE NOCASE
                          AND c.sent_count > 0
                    ) AS is_known,
                    EXISTS(
                        SELECT 1 FROM triage_feedback f
                        WHERE f.account_id = m.account_id
                          AND f.sender = m.from_addr COLLATE NOCASE
                    ) AS sender_corrected
             FROM triage_revisits v
             JOIN triage t   ON t.message_id = v.message_id AND t.account_id = v.account_id
             JOIN messages m ON m.id = v.message_id
             WHERE v.account_id = ?1
               AND v.fired_at IS NULL
               AND v.revisit_at <= ?2
               AND t.sensitivity = 'normal'
               AND t.status != 'done'
               AND t.revisit_count < ?3
               AND m.is_sent = 0 AND m.is_spam = 0
               AND NOT EXISTS(
                   SELECT 1 FROM triage_feedback f2
                   WHERE f2.account_id = v.account_id AND f2.message_id = v.message_id
               )
             ORDER BY v.revisit_at ASC
             LIMIT ?4",
        )?;
        let out = stmt
            .query_map(
                params![
                    account_id,
                    now.to_rfc3339(),
                    max_lifetime as i64,
                    limit as i64
                ],
                |r| {
                    Ok(RevisitQueued {
                        revisit_id: r.get(0)?,
                        message_id: r.get(1)?,
                        account_id,
                        thread_id: r.get(2)?,
                        from_addr: r.get(3)?,
                        subject: r.get(4)?,
                        body: r.get(5)?,
                        sensitivity: Sensitivity::parse(&r.get::<_, String>(6)?),
                        received_at: dt(r, 7)?,
                        revisit_at: dt(r, 8)?,
                        reason: r.get(9)?,
                        source: r.get(10)?,
                        // An unparseable stored tier reads as Noise: the revisit
                        // is about to overwrite it anyway, and a nonsense value
                        // must not abort the whole queue read.
                        prior_tier: Tier::parse(&r.get::<_, String>(11)?).unwrap_or(Tier::Noise),
                        prior_importance: r.get::<_, i64>(12)?.clamp(0, 100) as u8,
                        prior_one_line: r.get(13)?,
                        is_known_contact: r.get::<_, i64>(14)? != 0,
                        sender_corrected: r.get::<_, i64>(15)? != 0,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// Stamp a revisit as fired and charge it against the message's lifetime
    /// budget. Called whether or not the re-classification produced a new
    /// verdict: a revisit that failed still happened, and leaving it pending
    /// would retry it every cycle forever.
    pub(super) fn revisit_mark_fired(
        &self,
        account_id: AccountId,
        revisit_id: i64,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        // Guarded on `fired_at IS NULL` so a double-fire cannot double-charge
        // the lifetime counter.
        let n = tx.execute(
            "UPDATE triage_revisits SET fired_at = ?3
             WHERE id = ?2 AND account_id = ?1 AND fired_at IS NULL",
            params![account_id, revisit_id, now.to_rfc3339()],
        )?;
        if n > 0 {
            tx.execute(
                "UPDATE triage SET revisit_count = revisit_count + 1
                 WHERE account_id = ?1
                   AND message_id = (SELECT message_id FROM triage_revisits WHERE id = ?2)",
                params![account_id, revisit_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Rows sitting in the standing band with nothing pending and no action
    /// taken: after long enough, a row is either misfiled or finished, and both
    /// deserve another look. Returns message ids to schedule an immediate
    /// [`RevisitSource::FyeStale`](crate::triage::revisit::RevisitSource::FyeStale)
    /// revisit for.
    ///
    /// `older_than` is BOTH bounds of one window. A row qualifies when its mail
    /// is older than that moment AND nothing has re-scored it since — the pending
    /// check alone is not a cooldown, because a swept row schedules at `now`,
    /// fires in the same pass, and is pending no longer. Without the fired-since
    /// clause the same row re-sweeps on the NEXT sync tick, forty-five seconds
    /// later, and every stale row in the standing band turns into a metronome
    /// billing a frontier-model call until the daily cap runs out.
    pub(super) fn revisit_stale_standing(
        &self,
        account_id: AccountId,
        older_than: DateTime<Utc>,
        max_lifetime: u32,
        limit: usize,
    ) -> Result<Vec<i64>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT t.message_id
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1
               AND t.sensitivity = 'normal'
               AND t.tier IN ('past_due', 'deadline')
               AND t.status != 'done'
               AND t.stage1_model_used IS NOT NULL
               AND t.revisit_count < ?3
               AND m.is_sent = 0 AND m.is_spam = 0
               AND m.received_at <= ?2
               AND NOT EXISTS(
                   SELECT 1 FROM triage_revisits v
                   WHERE v.account_id = t.account_id
                     AND v.message_id = t.message_id
                     AND (v.fired_at IS NULL OR v.fired_at > ?2)
               )
               AND NOT EXISTS(
                   SELECT 1 FROM triage_feedback f
                   WHERE f.account_id = t.account_id AND f.message_id = t.message_id
               )
             ORDER BY m.received_at ASC
             LIMIT ?4",
        )?;
        let out = stmt
            .query_map(
                params![
                    account_id,
                    older_than.to_rfc3339(),
                    max_lifetime as i64,
                    limit as i64
                ],
                |r| r.get::<_, i64>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// Apply a re-evaluated verdict. Same write as [`Self::stage1_apply`] plus
    /// two things only a revisit needs: `model_used` is CLEARED so a newly
    /// escalated row can re-enter the Stage-2 queue (its old Stage-2 marker
    /// describes a verdict that no longer exists), and the guard also refuses a
    /// row the owner has corrected by hand or already resolved. Both refusals
    /// re-check in the write what [`Self::revisit_queue`] filtered in the read:
    /// the two are separated by a model call, which is plenty of time for
    /// someone to mark a row done from a client.
    pub(super) fn revisit_apply(&self, applied: &Stage1Applied) -> Result<bool> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let deadline_dt = applied.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
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
                 stage1_model_used = ?8,
                 needs_stage2 = ?9,
                 field_reasons = ?10,
                 category = COALESCE(?11, category),
                 escalation_reason = ?12,
                 model_used = NULL
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'
               AND status != 'done'
               AND NOT EXISTS(
                   SELECT 1 FROM triage_feedback f
                   WHERE f.account_id = ?2 AND f.message_id = ?1
               )",
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
                applied.escalation_reason,
            ],
        )?;
        if n > 0 {
            rewrite_deadline_conn(
                &tx,
                applied.account_id,
                applied.message_id,
                applied.deadline.as_ref(),
            )?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// The heuristic seed verdict as it currently stands on a row, for the
    /// Stage-1 fallback's notification decision. `None` when the row is gone or
    /// sealed.
    pub(super) fn triage_seed_verdict(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<SeedVerdict>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT tier, importance, one_line, needs_stage2, deadline
                 FROM triage
                 WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
                params![message_id, account_id],
                |r| {
                    Ok(SeedVerdict {
                        tier: Tier::parse(&r.get::<_, String>(0)?).unwrap_or(Tier::Noise),
                        importance: r.get::<_, i64>(1)?.clamp(0, 100) as u8,
                        one_line: r.get(2)?,
                        needs_stage2: r.get::<_, i64>(3)? != 0,
                        deadline: r
                            .get::<_, Option<String>>(4)?
                            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                            .map(|d| d.with_timezone(&Utc)),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub(super) fn stage1_mark_processed(
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

    pub(super) fn stage1_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        tokens: UsageTokens,
    ) -> Result<()> {
        let conn = self.lock()?;
        bump_usage_category(&conn, account_id, day, "stage1", tokens)
    }

    pub(super) fn stage1_usage_since(
        &self,
        account_id: AccountId,
        since_day: &str,
    ) -> Result<Stage2Usage> {
        let conn = self.lock()?;
        usage_since_category(&conn, account_id, since_day, "stage1")
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn extract_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        category: &str,
        tokens: UsageTokens,
    ) -> Result<()> {
        let conn = self.lock()?;
        bump_usage_category(&conn, account_id, day, category, tokens)
    }

    pub(super) fn list_usage_stage1(
        &self,
        account_id: AccountId,
        days: u32,
    ) -> Result<Vec<Stage2UsageDay>> {
        let conn = self.lock()?;
        list_usage_category(&conn, account_id, days, "stage1")
    }

    /// Distinct categories first, then each one's history through the same
    /// per-category helper the named readers use — rather than one windowed
    /// query, because `days` is a per-category ROW limit and reproducing that
    /// across categories in SQL buys nothing at this cardinality (a handful of
    /// stages and extractors).
    pub(super) fn list_usage_by_category(
        &self,
        account_id: AccountId,
        days: u32,
    ) -> Result<Vec<(String, Vec<Stage2UsageDay>)>> {
        let conn = self.lock()?;
        let categories: Vec<String> = conn
            .prepare(
                "SELECT DISTINCT category FROM stage2_usage
                 WHERE account_id = ?1
                 ORDER BY category",
            )?
            .query_map(params![account_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        categories
            .into_iter()
            .map(|c| {
                let rows = list_usage_category(&conn, account_id, days, &c)?;
                Ok((c, rows))
            })
            .collect()
    }

    pub(super) fn stage2_queue(
        &self,
        account_id: AccountId,
        limit: usize,
    ) -> Result<Vec<Stage2Queued>> {
        let conn = self.lock()?;
        // Queue predicate: Stage-1 finished the row, flagged it for escalation,
        // and Stage-2 has not processed it. Sealed rows are structurally
        // excluded. The LEFT JOIN carries a matched Filtered rule's want_text.
        // Hand-requested rows sort first; see `stage1_queue`.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.thread_id, m.from_addr, m.subject, m.body, t.sensitivity,
                    sr.want_text, m.received_at, t.retriage_at,
                    EXISTS(
                        SELECT 1 FROM contacts c
                        WHERE c.account_id = m.account_id
                          AND c.addr = m.from_addr COLLATE NOCASE
                          AND c.sent_count > 0
                    ) AS is_known,
                    t.escalation_reason
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             LEFT JOIN sender_rules sr ON sr.id = t.matched_rule_id
             WHERE t.account_id = ?1
               AND t.stage1_model_used IS NOT NULL
               AND t.needs_stage2 = 1
               AND t.model_used IS NULL
               AND t.sensitivity = 'normal'
               AND m.is_sent = 0 AND m.is_spam = 0
             ORDER BY t.retriage_at IS NULL, t.retriage_at DESC, m.received_at DESC
             LIMIT ?2",
        )?;
        let mut out = stmt
            .query_map(params![account_id, limit as i64], |r| {
                Ok(Stage2Queued {
                    message_id: r.get(0)?,
                    account_id,
                    thread_id: r.get(1)?,
                    from_addr: r.get(2)?,
                    subject: r.get(3)?,
                    body: r.get(4)?,
                    sensitivity: Sensitivity::parse(&r.get::<_, String>(5)?),
                    rule_want_text: r.get::<_, Option<String>>(6)?.filter(|s| !s.is_empty()),
                    received_at: dt(r, 7)?,
                    retriage_at: dt_opt(r, 8)?,
                    is_known_contact: r.get::<_, i64>(9)? != 0,
                    escalation_reason: r.get::<_, Option<String>>(10)?,
                    // Filled per row below; both need the row's own identifiers,
                    // and the batch is `batch_per_cycle` rows, not a table scan.
                    sender_history: SenderHistory::default(),
                    thread: Vec::new(),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // ---- The context an escalation is actually buying --------------------
        for row in &mut out {
            row.sender_history =
                sender_history_conn(&conn, account_id, &row.from_addr, row.message_id)?;
            row.thread = thread_siblings_conn(
                &conn,
                account_id,
                &row.thread_id,
                row.message_id,
                STAGE2_THREAD_CONTEXT_LIMIT,
            )?;
        }
        Ok(out)
    }

    pub(super) fn stage2_budget_used(
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

    pub(super) fn stage2_increment_budget(
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

    /// Give back one charge taken by [`Self::stage2_increment_budget`].
    ///
    /// FLOORED AT ZERO, and the floor is not paranoia: the counter is keyed by
    /// UTC day, so a call charged at 23:59:59 and refunded at 00:00:01 lands on
    /// the NEXT day's row, which has nothing in it. `MAX(0, ...)` makes that
    /// refund a no-op instead of a negative balance that would silently hand
    /// the new day a free call.
    pub(super) fn stage2_refund_budget(
        &self,
        account_id: AccountId,
        thread_id: &str,
        day: &str,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE wake_budget SET model_calls = MAX(0, model_calls - 1)
             WHERE account_id=?1 AND thread_id=?2 AND day=?3",
            params![account_id, thread_id, day],
        )?;
        Ok(())
    }

    pub(super) fn stage2_apply(&self, applied: &Stage2Applied) -> Result<bool> {
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
            rewrite_deadline_conn(
                &tx,
                applied.account_id,
                applied.message_id,
                applied.deadline.as_ref(),
            )?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    pub(super) fn stage2_mark_processed(
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

    pub(super) fn stage2_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        tokens: UsageTokens,
    ) -> Result<()> {
        let conn = self.lock()?;
        bump_usage_category(&conn, account_id, day, "stage2", tokens)
    }

    pub(super) fn stage2_usage_today(
        &self,
        account_id: AccountId,
        day: &str,
    ) -> Result<Stage2Usage> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT calls, input_tokens, output_tokens, cache_creation_tokens,
                        cache_read_tokens
                 FROM stage2_usage
                 WHERE account_id = ?1 AND day = ?2 AND category = 'stage2'",
                params![account_id, day],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(row
            .map(|(calls, in_tok, out_tok, cache_w, cache_r)| Stage2Usage {
                calls: calls.max(0) as u64,
                input_tokens: in_tok.max(0) as u64,
                output_tokens: out_tok.max(0) as u64,
                cache_creation_tokens: cache_w.max(0) as u64,
                cache_read_tokens: cache_r.max(0) as u64,
            })
            .unwrap_or_default())
    }

    pub(super) fn list_usage(
        &self,
        account_id: AccountId,
        days: u32,
    ) -> Result<Vec<Stage2UsageDay>> {
        let conn = self.lock()?;
        list_usage_category(&conn, account_id, days, "stage2")
    }

    pub(super) fn stage2_usage_since(
        &self,
        account_id: AccountId,
        since_day: &str,
    ) -> Result<Stage2Usage> {
        let conn = self.lock()?;
        usage_since_category(&conn, account_id, since_day, "stage2")
    }

    pub(super) fn get_app_setting(
        &self,
        account_id: AccountId,
        key: &str,
    ) -> Result<Option<String>> {
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

    pub(super) fn set_app_setting(
        &self,
        account_id: AccountId,
        key: &str,
        value: &str,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO app_settings(account_id, key, value)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(account_id, key) DO UPDATE SET value = excluded.value",
            params![account_id, key, value],
        )?;
        Ok(())
    }

    pub(super) fn stage2_cap_overrides(&self, account_id: AccountId) -> Result<Stage2CapOverrides> {
        let conn = self.lock()?;
        // One SELECT pulls all four cap rows (only those that exist come back).
        let mut stmt = conn.prepare(
            "SELECT key, value FROM app_settings
             WHERE account_id = ?1 AND key IN (?2, ?3, ?4, ?5)",
        )?;
        // A stored value only counts if it parses as an integer in the valid
        // range; anything else is treated as absent (fall back to config/default).
        let valid = |s: String| -> Option<u32> {
            s.trim().parse::<u32>().ok().filter(|n| {
                (crate::config::STAGE2_CAP_MIN..=crate::config::STAGE2_CAP_MAX).contains(n)
            })
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

    pub(super) fn count_inbound_since(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
    ) -> Result<u64> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages
             WHERE account_id = ?1 AND is_sent = 0 AND is_spam = 0 AND received_at >= ?2",
            params![account_id, since.to_rfc3339()],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }
}
