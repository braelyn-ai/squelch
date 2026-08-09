//! The Stage-1 / Stage-2 / extract queues, their applies, the LLM usage
//! ledger and the per-account cap settings.

use super::messages::rewrite_deadline_conn;
use super::*;

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
        params![
            account_id,
            day,
            category,
            input_tokens as i64,
            output_tokens as i64
        ],
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
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
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
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn retriage_reset(
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
                    is_known_contact: r.get::<_, i64>(7)? != 0,
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
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        let conn = self.lock()?;
        bump_usage_category(
            &conn,
            account_id,
            day,
            "stage1",
            input_tokens,
            output_tokens,
        )
    }

    pub(super) fn stage1_usage_since(
        &self,
        account_id: AccountId,
        since_day: &str,
    ) -> Result<Stage2Usage> {
        let conn = self.lock()?;
        usage_since_category(&conn, account_id, since_day, "stage1")
    }

    pub(super) fn extract_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        category: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        let conn = self.lock()?;
        bump_usage_category(
            &conn,
            account_id,
            day,
            category,
            input_tokens,
            output_tokens,
        )
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
        let out = stmt
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
                    is_known_contact: r.get::<_, i64>(8)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
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
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        let conn = self.lock()?;
        bump_usage_category(
            &conn,
            account_id,
            day,
            "stage2",
            input_tokens,
            output_tokens,
        )
    }

    pub(super) fn stage2_usage_today(
        &self,
        account_id: AccountId,
        day: &str,
    ) -> Result<Stage2Usage> {
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
             WHERE account_id = ?1 AND is_sent = 0 AND received_at >= ?2",
            params![account_id, since.to_rfc3339()],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }
}
