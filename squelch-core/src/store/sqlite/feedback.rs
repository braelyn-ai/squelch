//! Human triage corrections, the feedback log and the triage debug read.

use super::*;

impl SqliteStore {
    pub(super) fn triage_debug(
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

    // ---- triage feedback (human corrections) -----------------------------

    pub(super) fn correct_triage(
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

    pub(super) fn list_triage_feedback(
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
        let out = stmt
            .query_map(params![account_id, limit], |r| {
                Ok(TriageFeedback {
                    id: r.get(0)?,
                    message_id: r.get(1)?,
                    corrected_at: dt(r, 2)?,
                    dimension: r.get(3)?,
                    from_value: r.get(4)?,
                    to_value: r.get(5)?,
                    // One unparseable snapshot must not take the whole list down.
                    original: serde_json::from_str(&r.get::<_, String>(6)?)
                        .unwrap_or(serde_json::Value::Null),
                    sender: r.get(7)?,
                    subject: r.get(8)?,
                    note: r.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }
}
