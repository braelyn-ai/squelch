//! Sender rules and the unsubscribe ledger.

use super::*;

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

impl SqliteStore {
    pub(super) fn set_sender_rule(
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

    pub(super) fn set_sender_rule_audited(
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

    pub(super) fn update_sender_rule(
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

    pub(super) fn list_sender_rules(&self, account_id: AccountId) -> Result<Vec<SenderRule>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, match_pattern, want_text, disposition, updated_at
             FROM sender_rules WHERE account_id=?1 ORDER BY updated_at DESC",
        )?;
        let out = stmt
            .query_map(params![account_id], |r| {
                Ok(SenderRule {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    match_pattern: r.get(2)?,
                    want_text: r.get(3)?,
                    disposition: Disposition::parse(&r.get::<_, String>(4)?)
                        .unwrap_or(Disposition::Surface),
                    updated_at: dt(r, 5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn delete_sender_rule(&self, account_id: AccountId, id: i64) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM sender_rules WHERE account_id=?1 AND id=?2",
            params![account_id, id],
        )?;
        Ok(n > 0)
    }

    pub(super) fn message_unsub_fields(
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

    pub(super) fn upsert_unsubscribe(
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

    pub(super) fn list_unsubscribes(&self, account_id: AccountId) -> Result<Vec<UnsubscribeRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT sender_addr, requested_at, method, violation_count,
                    last_violation_at, resolution
             FROM unsubscribes
             WHERE account_id = ?1
             ORDER BY requested_at DESC",
        )?;
        let out = stmt
            .query_map(params![account_id], |r| {
                Ok(UnsubscribeRecord {
                    sender: r.get(0)?,
                    requested_at: dt(r, 1)?,
                    method: r.get(2)?,
                    violation_count: r.get(3)?,
                    last_violation_at: dt_opt(r, 4)?,
                    resolution: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn set_unsubscribe_resolution(
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
}
