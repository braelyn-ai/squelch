//! The audit log and the auth-mail shredder ledger.

use super::*;

impl SqliteStore {
    pub(super) fn append_audit(&self, account_id: AccountId, entry: &NewAuditEntry) -> Result<i64> {
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

    // ---- auth-mail shredder (retention) ----------------------------------

    pub(super) fn shred_candidates(
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
        let out = stmt
            .query_map(params![account_id, cutoff.to_rfc3339(), limit], |r| {
                Ok(ShredCandidate {
                    message_id: r.get(0)?,
                    gmail_msg_id: r.get(1)?,
                    sender: r.get(2)?,
                    kind: r.get(3)?,
                    received_at: dt(r, 4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn shred_pending_count(&self, account_id: AccountId, cutoff: DateTime<Utc>) -> Result<i64> {
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

    pub(super) fn record_shred(
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

    pub(super) fn shred_counts(
        &self,
        account_id: AccountId,
        recent_since: DateTime<Utc>,
    ) -> Result<(i64, i64, Option<DateTime<Utc>>)> {
        let conn = self.lock()?;
        let counts = conn.query_row(
            "SELECT
               COALESCE(SUM(CASE WHEN shredded_at >= ?2 THEN 1 ELSE 0 END), 0),
               COUNT(*),
               MAX(shredded_at)
             FROM shred_log WHERE account_id = ?1",
            params![account_id, recent_since.to_rfc3339()],
            |r| Ok((r.get(0)?, r.get(1)?, dt_opt(r, 2)?)),
        )?;
        Ok(counts)
    }

    pub(super) fn list_audit(&self, account_id: AccountId, limit: u32) -> Result<Vec<AuditEntry>> {
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
        let out = stmt
            .query_map(params![account_id, limit as i64], |r| {
                let from_addr: Option<String> = r.get(7)?;
                let from_name: Option<String> = r.get(8)?;
                Ok(AuditEntry {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    ts: dt(r, 2)?,
                    actor: r.get(3)?,
                    action: r.get(4)?,
                    target: r.get(5)?,
                    detail: r.get(6)?,
                    // from_name if present, else from_addr; both None when the
                    // join found no message.
                    target_sender: from_name.filter(|s| !s.is_empty()).or(from_addr),
                    target_subject: r.get(9)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }
}
