//! The `notify_decisions` ledger: what each lane decided about notifying.
//!
//! APPEND-ONLY by construction. There is deliberately no update and no delete in
//! this file, and adding one would not be a new query — it would be the end of
//! the property the table exists for (docs/NOTIFY.md §11.4).

use super::*;

/// One `notify_decisions` row, columns in SELECT order, into a
/// [`NotifyDecisionRow`].
///
/// UNRECOGNIZED VOCABULARY IS A MISS, not a fallback. `map_event` clamps an
/// unparseable kind to the least-alarming value because refusing to serve a
/// stored row would stall a client's cursor at it forever; nothing pages through
/// this table, and a row written by a newer daemon coerced into a neighbouring
/// variant would silently make the eval query wrong about the one thing it
/// measures. So the mapper yields `None` and the caller drops the row.
fn map_decision(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<NotifyDecisionRow>> {
    let (Some(lane), Some(decision)) = (
        NotifyLane::parse(&r.get::<_, String>(2)?),
        NotifyDecision::parse(&r.get::<_, String>(3)?),
    ) else {
        return Ok(None);
    };
    Ok(Some(NotifyDecisionRow {
        id: r.get(0)?,
        message_id: r.get(1)?,
        lane,
        decision,
        // Stored 0-100. Clamped rather than truncated so a value outside the
        // byte range can never wrap into a plausible-looking score.
        notify_importance: r.get::<_, Option<i64>>(4)?.map(|i| i.clamp(0, 255) as u8),
        model_used: r.get(5)?,
        latency_ms: r
            .get::<_, Option<i64>>(6)?
            .map(|ms| ms.clamp(0, u32::MAX as i64) as u32),
        created_at: dt(r, 7)?,
    }))
}

impl SqliteStore {
    pub(super) fn record_notify_decision(&self, decision: &NewNotifyDecision) -> Result<bool> {
        let conn = self.lock()?;
        // INSERT OR IGNORE on UNIQUE(message_id, lane): the FIRST answer a lane
        // gave about a message is the one kept, forever. A Stage-2 pass behind
        // Stage-1, or a re-triage a day later, changes 0 rows and says so.
        let n = conn.execute(
            "INSERT OR IGNORE INTO notify_decisions(account_id, message_id, lane, decision,
                 notify_importance, model_used, latency_ms, created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                decision.account_id,
                decision.message_id,
                decision.lane.as_str(),
                decision.decision.as_str(),
                decision.notify_importance.map(i64::from),
                decision.model_used,
                decision.latency_ms.map(i64::from),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(n > 0)
    }

    pub(super) fn notify_decision_exists(
        &self,
        account_id: AccountId,
        message_id: i64,
        lane: NotifyLane,
    ) -> Result<bool> {
        let conn = self.lock()?;
        // Rides `UNIQUE(message_id, lane)` directly, so this is an index probe
        // rather than a scan: it runs once per fast-lane candidate, on the
        // ingest path, in front of a user.
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM notify_decisions
             WHERE account_id = ?1 AND message_id = ?2 AND lane = ?3",
            params![account_id, message_id, lane.as_str()],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub(super) fn notify_decisions_since(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<NotifyDecisionRow>> {
        let conn = self.lock()?;
        // The WHERE rides `idx_notify_decisions_account_created`; the ORDER BY
        // is on `id` because insert order is the only total one (see the trait
        // doc). Both are cheap: `created_at` narrows and `id` is the rowid.
        let mut stmt = conn.prepare(
            "SELECT id, message_id, lane, decision, notify_importance, model_used,
                    latency_ms, created_at
             FROM notify_decisions
             WHERE account_id = ?1 AND created_at >= ?2
             ORDER BY id ASC
             LIMIT ?3",
        )?;
        let out = stmt
            .query_map(
                params![account_id, since.to_rfc3339(), limit as i64],
                map_decision,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(out)
    }
}
