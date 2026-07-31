//! Local drafts: the human door's unsent compositions.

use super::*;

/// One `drafts` row, columns in SELECT order, into a [`Draft`].
fn map_draft(r: &rusqlite::Row<'_>) -> rusqlite::Result<Draft> {
    Ok(Draft {
        id: r.get(0)?,
        account_id: r.get(1)?,
        reply_to_message_id: r.get(2)?,
        to_addr: r.get(3)?,
        subject: r.get(4)?,
        body: r.get(5)?,
        created_at: dt(r, 6)?,
        updated_at: dt(r, 7)?,
    })
}

impl SqliteStore {
    /// HUMAN-DOOR ONLY (`/client/drafts`): save the draft for one reply target,
    /// where `reply_to_message_id = None` addresses the account's single
    /// new-message draft. Returns the stored row.
    pub fn upsert_draft(
        &self,
        account_id: AccountId,
        reply_to_message_id: Option<i64>,
        to_addr: &str,
        subject: &str,
        body: &str,
        now: DateTime<Utc>,
    ) -> Result<Draft> {
        let conn = self.lock()?;
        // SELECT-then-write under ONE held lock instead of an upsert: the
        // uniqueness lives in two PARTIAL indexes, which `ON CONFLICT(...)` cannot
        // name as a conflict target. `IS`, not `=`, so the NULL key (the
        // new-message draft) matches itself.
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM drafts WHERE account_id = ?1 AND reply_to_message_id IS ?2",
                params![account_id, reply_to_message_id],
                |r| r.get(0),
            )
            .optional()?;
        let id = match existing {
            // An edit is the SAME composition: `id` and `created_at` are left
            // alone, only `updated_at` moves.
            Some(id) => {
                conn.execute(
                    "UPDATE drafts SET to_addr = ?2, subject = ?3, body = ?4, updated_at = ?5
                     WHERE id = ?1",
                    params![id, to_addr, subject, body, now.to_rfc3339()],
                )?;
                id
            }
            None => {
                conn.execute(
                    "INSERT INTO drafts(account_id, reply_to_message_id, to_addr, subject, body,
                         created_at, updated_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?6)",
                    params![
                        account_id,
                        reply_to_message_id,
                        to_addr,
                        subject,
                        body,
                        now.to_rfc3339(),
                    ],
                )?;
                conn.last_insert_rowid()
            }
        };
        let draft = conn.query_row(
            "SELECT id, account_id, reply_to_message_id, to_addr, subject, body,
                    created_at, updated_at
             FROM drafts WHERE id = ?1",
            params![id],
            map_draft,
        )?;
        Ok(draft)
    }

    /// Every draft for an account, most recently touched first.
    pub fn list_drafts(&self, account_id: AccountId) -> Result<Vec<Draft>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, reply_to_message_id, to_addr, subject, body,
                    created_at, updated_at
             FROM drafts WHERE account_id = ?1 ORDER BY updated_at DESC",
        )?;
        let out = stmt
            .query_map(params![account_id], map_draft)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// Discard one draft by id. `false` when nothing matched, so another
    /// account's id is indistinguishable from an unknown one (the handler turns
    /// both into 404).
    pub fn delete_draft(&self, account_id: AccountId, id: i64) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM drafts WHERE account_id = ?1 AND id = ?2",
            params![account_id, id],
        )?;
        Ok(n > 0)
    }
}
