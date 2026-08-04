//! Outbound read tracking: minted `send_trackers` and the `message_opens` they
//! collect.
//!
//! Every write here is account-scoped through the tracker row, so a token this
//! account did not mint can neither record an open nor read one back.

use super::*;

/// Opens kept per token. The relay caps its own buffer the same way, but that
/// bound resets at every drain, so it does not bound what accumulates here.
pub const MAX_OPENS_PER_TOKEN: usize = 50;

/// One `message_opens` row, columns in SELECT order.
fn map_open(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageOpen> {
    Ok(MessageOpen {
        opened_at: r.get(0)?,
        user_agent: r.get(1)?,
        classification: r.get(2)?,
    })
}

impl SqliteStore {
    pub(super) fn insert_send_tracker(
        &self,
        account_id: AccountId,
        token: &str,
        message_id: Option<i64>,
        created_at: i64,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO send_trackers(token, account_id, message_id, created_at)
             VALUES(?1,?2,?3,?4)",
            params![token, account_id, message_id, created_at],
        )?;
        Ok(())
    }

    pub(super) fn set_send_tracker_message(
        &self,
        account_id: AccountId,
        token: &str,
        message_id: i64,
    ) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE send_trackers SET message_id = ?3
             WHERE token = ?1 AND account_id = ?2",
            params![token, account_id, message_id],
        )?;
        Ok(n > 0)
    }

    pub(super) fn record_open(
        &self,
        account_id: AccountId,
        token: &str,
        opened_at: i64,
        user_agent: Option<&str>,
        classification: &str,
    ) -> Result<bool> {
        let conn = self.lock()?;
        // Two guards in one statement, so neither can interleave with a
        // concurrent open of the same token. The tracker SELECT means an unknown
        // token inserts zero rows — the pixel route's answer is identical either
        // way and unsolicited traffic never grows the table. The count means a
        // token that IS ours is still not an unbounded write channel: anyone
        // holding a live pixel URL (the recipient, a forward, a scanner) can
        // refetch it forever, and past MAX_OPENS_PER_TOKEN there is no signal
        // left to record.
        let n = conn.execute(
            "INSERT INTO message_opens(token, opened_at, user_agent, classification)
             SELECT ?1, ?3, ?4, ?5 FROM send_trackers
             WHERE token = ?1 AND account_id = ?2
               AND (SELECT count(*) FROM message_opens WHERE token = ?1) < ?6",
            params![
                token,
                account_id,
                opened_at,
                user_agent,
                classification,
                MAX_OPENS_PER_TOKEN as i64
            ],
        )?;
        Ok(n > 0)
    }

    pub(super) fn message_opens(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Vec<MessageOpen>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT o.opened_at, o.user_agent, o.classification
             FROM message_opens o
             JOIN send_trackers t ON t.token = o.token
             WHERE t.account_id = ?1 AND t.message_id = ?2
             ORDER BY o.opened_at ASC, o.id ASC",
        )?;
        let out = stmt
            .query_map(params![account_id, message_id], map_open)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn tracked_message(
        &self,
        account_id: AccountId,
        token: &str,
    ) -> Result<Option<TrackedMessage>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT m.id, m.thread_id, m.subject, m.from_addr
                 FROM send_trackers t
                 JOIN messages m ON m.id = t.message_id AND m.account_id = t.account_id
                 WHERE t.token = ?1 AND t.account_id = ?2",
                params![token, account_id],
                |r| {
                    Ok(TrackedMessage {
                        message_id: r.get(0)?,
                        thread_id: r.get(1)?,
                        subject: r.get(2)?,
                        from_addr: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }
}
