//! Local drafts: the human door's unsent compositions.

use super::*;

/// One `drafts` row, columns in SELECT order, into a [`Draft`].
fn map_draft(r: &rusqlite::Row<'_>) -> rusqlite::Result<Draft> {
    Ok(Draft {
        id: r.get(0)?,
        account_id: r.get(1)?,
        reply_to_message_id: r.get(2)?,
        to_addr: r.get(3)?,
        cc_addr: r.get(4)?,
        bcc_addr: r.get(5)?,
        subject: r.get(6)?,
        body: r.get(7)?,
        // Filled by the caller, which still holds the connection: a row map
        // cannot run a second query.
        attachments: Vec::new(),
        created_at: dt(r, 8)?,
        updated_at: dt(r, 9)?,
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
        fields: DraftFields<'_>,
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
                    "UPDATE drafts SET to_addr = ?2, cc_addr = ?3, bcc_addr = ?4,
                            subject = ?5, body = ?6, updated_at = ?7
                     WHERE id = ?1",
                    params![
                        id,
                        fields.to_addr,
                        fields.cc_addr,
                        fields.bcc_addr,
                        fields.subject,
                        fields.body,
                        now.to_rfc3339()
                    ],
                )?;
                id
            }
            None => {
                conn.execute(
                    "INSERT INTO drafts(account_id, reply_to_message_id, to_addr, cc_addr,
                         bcc_addr, subject, body, created_at, updated_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8)",
                    params![
                        account_id,
                        reply_to_message_id,
                        fields.to_addr,
                        fields.cc_addr,
                        fields.bcc_addr,
                        fields.subject,
                        fields.body,
                        now.to_rfc3339(),
                    ],
                )?;
                conn.last_insert_rowid()
            }
        };
        let mut draft = conn.query_row(
            "SELECT id, account_id, reply_to_message_id, to_addr, cc_addr, bcc_addr,
                    subject, body, created_at, updated_at
             FROM drafts WHERE id = ?1",
            params![id],
            map_draft,
        )?;
        draft.attachments = super::outbound::draft_attachments(&conn, account_id, id)?;
        Ok(draft)
    }

    /// One draft by id, files included. `None` for an unknown id and for
    /// another account's.
    pub fn draft_by_id(&self, account_id: AccountId, id: i64) -> Result<Option<Draft>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT id, account_id, reply_to_message_id, to_addr, cc_addr, bcc_addr,
                        subject, body, created_at, updated_at
                 FROM drafts WHERE account_id = ?1 AND id = ?2",
                params![account_id, id],
                map_draft,
            )
            .optional()?;
        let Some(mut draft) = row else {
            return Ok(None);
        };
        draft.attachments = super::outbound::draft_attachments(&conn, account_id, id)?;
        Ok(Some(draft))
    }

    /// Every draft for an account, most recently touched first.
    pub fn list_drafts(&self, account_id: AccountId) -> Result<Vec<Draft>> {
        let conn = self.lock()?;
        // SECURITY BELT (same shape as `deadlines`): exclude a draft whose parent
        // is sealed. The seal paths already DELETE such drafts, so this only fires
        // if one is ever missed — a draft quoting mail the user has since decided
        // is auth must not come back out of the list.
        //
        // The NULL key (the new-message draft) compares NULL against
        // `triage.message_id`, so the subquery matches nothing and NOT EXISTS
        // holds: it is never filtered.
        let mut stmt = conn.prepare(
            "SELECT id, account_id, reply_to_message_id, to_addr, cc_addr, bcc_addr,
                    subject, body, created_at, updated_at
             FROM drafts
             WHERE account_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM triage t
                   WHERE t.message_id = drafts.reply_to_message_id
                     AND t.sensitivity = 'sealed'
               )
             ORDER BY updated_at DESC",
        )?;
        let mut out = stmt
            .query_map(params![account_id], map_draft)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for draft in &mut out {
            draft.attachments = super::outbound::draft_attachments(&conn, account_id, draft.id)?;
        }
        Ok(out)
    }

    /// Discard one draft by id. `false` when nothing matched, so another
    /// account's id is indistinguishable from an unknown one (the handler turns
    /// both into 404).
    ///
    /// THE DRAFT'S FILES GO WITH IT. A staged upload lives as long as the draft
    /// that claimed it (see `outbound_attachments` in schema.sql); a discarded
    /// draft's files would otherwise sit unreachable until the sweep, holding
    /// their bytes for nothing.
    pub fn delete_draft(&self, account_id: AccountId, id: i64) -> Result<bool> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM outbound_attachments WHERE account_id = ?1 AND draft_id = ?2",
            params![account_id, id],
        )?;
        let n = conn.execute(
            "DELETE FROM drafts WHERE account_id = ?1 AND id = ?2",
            params![account_id, id],
        )?;
        Ok(n > 0)
    }
}
