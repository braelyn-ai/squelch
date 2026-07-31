//! The attention ledger: ranked/attention updates, sitrep bands, surfacing
//! stamps and store stats.

use super::*;

/// Columns 0..=8 of the updates SELECT — the prefix `ranked_updates` and
/// `attention_updates` share verbatim — into an [`Update`]. An unparseable tier
/// falls back to the least-alarming value rather than failing the whole read.
///
/// The two HUMAN-DOOR-ONLY fields are left `None`: that is exactly right for the
/// agent door, and `attention_updates` fills them from its extra columns.
fn update_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Update> {
    Ok(Update {
        id: r.get(0)?,
        thread_id: r.get(1)?,
        tier: Tier::parse(&r.get::<_, String>(2)?).unwrap_or(Tier::Noise),
        importance: r.get::<_, i64>(3)?.clamp(0, 255) as u8,
        sender: r.get(4)?,
        one_line: r.get(5)?,
        reason: r.get(6)?,
        deadline: dt_opt(r, 7)?,
        matched_rule: r.get(8)?,
        field_reasons: None,
        has_attachments: None,
    })
}

impl SqliteStore {
    pub(super) fn ranked_updates(
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
        // AGENT DOOR: `update_from_row` leaves field_reasons/has_attachments None.
        let out = stmt
            .query_map(params![account_id, since.to_rfc3339(), min], update_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn attention_updates(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        min_importance: Option<u8>,
        status: Option<AttentionStatus>,
        band: Option<SitrepBand>,
    ) -> Result<Vec<AttentionUpdate>> {
        let conn = self.lock()?;
        let min = min_importance.unwrap_or(0) as i64;

        // Base predicate: sealed excluded, sent excluded, since/importance window.
        // Bands:
        //   standing = tier IN ('past_due','deadline') AND status != 'done'
        //   new      = surfaced_at IS NULL AND status != 'done'
        //   open     = status = 'open'
        // The `status != 'done'` on `new` keeps AUTO-RESOLVED receipts out of the
        // band — a receipt is a record, not new inbox clutter.
        let mut sql = String::from(
            "SELECT m.id, m.thread_id, t.tier, t.importance, m.from_addr, t.one_line,
                    t.reason, t.deadline, t.matched_rule_id,
                    t.status, t.surfaced_at, t.resolved_at, t.field_reasons,
                    EXISTS(SELECT 1 FROM attachments a
                           WHERE a.account_id = m.account_id
                             AND a.message_id = m.id) AS has_atts
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
        // The `open` band is the aging one: age*importance floats
        // long-unresolved-and-important items, computed in SQL via julianday so
        // the ordering stays server-side. Other bands sort by importance.
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
            let mut update = update_from_row(r)?;
            // HUMAN DOOR: the two extra columns the agent door never sees. A NULL
            // or malformed reasons blob yields None — one bad row must never fail
            // the whole updates read.
            update.field_reasons = r
                .get::<_, Option<String>>(12)?
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            update.has_attachments = Some(r.get::<_, i64>(13)? != 0);
            Ok(AttentionUpdate {
                update,
                status: AttentionStatus::parse(&r.get::<_, String>(9)?)
                    .unwrap_or(AttentionStatus::New),
                surfaced_at: dt_opt(r, 10)?,
                resolved_at: dt_opt(r, 11)?,
            })
        };
        let out = if band == Some(SitrepBand::Open) {
            stmt.query_map(params![account_id, since.to_rfc3339(), min, now], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![account_id, since.to_rfc3339(), min], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(out)
    }

    pub(super) fn mark_surfaced(&self, account_id: AccountId, message_ids: &[i64]) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        let tx = conn.transaction()?;
        let mut first_surfaced = 0usize;
        {
            // Stamp surfaced_at only if NULL and promote new->open. The
            // sensitivity guard means a sealed row is NEVER stamped, so it cannot
            // leak into a "new since last check" delta. Idempotent.
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

    pub(super) fn set_attention_status(
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

    pub(super) fn stats(&self, account_id: AccountId) -> Result<StoreStats> {
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

        // Sitrep band counts over non-sealed rows. These definitions MUST match
        // the `band` query on attention_updates, or header and list disagree.
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

        let last_surfaced_at = conn.query_row(
            "SELECT MAX(surfaced_at) FROM triage
             WHERE account_id = ?1 AND sensitivity != 'sealed'",
            params![account_id],
            |r| dt_opt(r, 0),
        )?;

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
}
