//! Files staged for a send: the human door's half-attached mail. See the
//! `outbound_attachments` table comment in schema.sql for the lifetime rules
//! every function here enforces one piece of.

use super::*;
use crate::store::{OutboundAttachment, OutboundAttachmentMeta};

/// How long an UNCLAIMED upload lives. A file is unclaimed between its upload
/// and the composer's next autosave — one debounce tick in the ordinary case —
/// so a day is not a budget, it is the line past which the row is certainly a
/// leftover: a composer closed before it ever saved, a send that failed and
/// was abandoned, a crash.
pub const OUTBOUND_UNCLAIMED_TTL: chrono::Duration = chrono::Duration::hours(24);

/// One `outbound_attachments` row's metadata, columns in SELECT order.
fn map_meta(r: &rusqlite::Row<'_>) -> rusqlite::Result<OutboundAttachmentMeta> {
    Ok(OutboundAttachmentMeta {
        id: r.get(0)?,
        filename: r.get(1)?,
        mime: r.get(2)?,
        size_bytes: r.get(3)?,
        content_id: r.get(4)?,
    })
}

const META_COLS: &str = "id, filename, mime, size_bytes, content_id";

/// The files a draft has claimed, upload order. Called with the connection
/// already held, from the draft reads that embed it.
pub(super) fn draft_attachments(
    conn: &Connection,
    account_id: AccountId,
    draft_id: i64,
) -> rusqlite::Result<Vec<OutboundAttachmentMeta>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {META_COLS} FROM outbound_attachments
         WHERE account_id = ?1 AND draft_id = ?2
         ORDER BY id"
    ))?;
    stmt.query_map(params![account_id, draft_id], map_meta)?
        .collect()
}

impl SqliteStore {
    /// HUMAN-DOOR ONLY (`POST /client/compose/attachments`): store one file's
    /// bytes for a later send. Unclaimed until a draft save names it. Returns
    /// the stored metadata, id included — the id is how every later request
    /// refers to the file.
    ///
    /// SWEEPS ON THE WAY IN: every upload first deletes this account's
    /// unclaimed rows older than [`OUTBOUND_UNCLAIMED_TTL`]. There is no
    /// background task for this table, and the moment somebody is attaching a
    /// file is the moment the table is being used at all.
    pub fn stage_outbound_attachment(
        &self,
        account_id: AccountId,
        filename: &str,
        mime: &str,
        content_id: &str,
        data: &[u8],
        now: DateTime<Utc>,
    ) -> Result<OutboundAttachmentMeta> {
        let conn = self.lock()?;
        let cutoff = (now - OUTBOUND_UNCLAIMED_TTL).to_rfc3339();
        conn.execute(
            "DELETE FROM outbound_attachments
             WHERE account_id = ?1 AND draft_id IS NULL AND created_at < ?2",
            params![account_id, cutoff],
        )?;
        conn.execute(
            "INSERT INTO outbound_attachments(account_id, draft_id, filename, mime,
                 size_bytes, content_id, data, created_at)
             VALUES(?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                account_id,
                filename,
                mime,
                data.len() as i64,
                content_id,
                data,
                now.to_rfc3339(),
            ],
        )?;
        let id = conn.last_insert_rowid();
        let meta = conn.query_row(
            &format!("SELECT {META_COLS} FROM outbound_attachments WHERE id = ?1"),
            params![id],
            map_meta,
        )?;
        Ok(meta)
    }

    /// One staged file with its bytes. `None` for an unknown id AND for another
    /// account's id — the two are one 404 at the handler.
    pub fn outbound_attachment(
        &self,
        account_id: AccountId,
        id: i64,
    ) -> Result<Option<OutboundAttachment>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                &format!(
                    "SELECT {META_COLS}, data FROM outbound_attachments
                     WHERE account_id = ?1 AND id = ?2"
                ),
                params![account_id, id],
                |r| {
                    Ok(OutboundAttachment {
                        meta: map_meta(r)?,
                        data: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// The files a send names, bytes included, IN THE ORDER THE IDS WERE GIVEN
    /// — that order is the order the user attached them and the order the
    /// parts go out in. An id that names nothing (unknown, swept, another
    /// account's) is simply absent from the result: the caller compares
    /// lengths and refuses the send, because a mail going out with fewer files
    /// than were reviewed is the one failure this table must never allow.
    pub fn outbound_attachments(
        &self,
        account_id: AccountId,
        ids: &[i64],
    ) -> Result<Vec<OutboundAttachment>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {META_COLS}, data FROM outbound_attachments
             WHERE account_id = ?1 AND id = ?2"
        ))?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let row = stmt
                .query_row(params![account_id, id], |r| {
                    Ok(OutboundAttachment {
                        meta: map_meta(r)?,
                        data: r.get(5)?,
                    })
                })
                .optional()?;
            if let Some(a) = row {
                out.push(a);
            }
        }
        Ok(out)
    }

    /// Drop one staged file. `false` when nothing matched, so another account's
    /// id is indistinguishable from an unknown one.
    pub fn delete_outbound_attachment(&self, account_id: AccountId, id: i64) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM outbound_attachments WHERE account_id = ?1 AND id = ?2",
            params![account_id, id],
        )?;
        Ok(n > 0)
    }

    /// Drop the files a send has CONSUMED. Called once the mail is away — the
    /// bytes are in the sent copy now, and a row that outlived its send would
    /// come back on the next draft restore as a file attached to nothing.
    pub fn delete_outbound_attachments(&self, account_id: AccountId, ids: &[i64]) -> Result<()> {
        let conn = self.lock()?;
        let mut stmt =
            conn.prepare("DELETE FROM outbound_attachments WHERE account_id = ?1 AND id = ?2")?;
        for id in ids {
            stmt.execute(params![account_id, id])?;
        }
        Ok(())
    }

    /// Make `ids` EXACTLY the set of files draft `draft_id` holds: each named
    /// row is claimed for it, and any row the draft held that is no longer
    /// named is released back to unclaimed (a file removed from the tray), from
    /// where the sweep takes it. Ids that name nothing are ignored — the draft
    /// is a best-effort mirror of the composer, and the send is where a missing
    /// file is refused.
    ///
    /// A row already claimed by ANOTHER draft is not stolen: the composer that
    /// uploaded it is the composer whose draft it belongs to, and two drafts
    /// naming one file would let a send from one delete a file the other still
    /// shows.
    pub fn claim_outbound_attachments(
        &self,
        account_id: AccountId,
        draft_id: i64,
        ids: &[i64],
    ) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE outbound_attachments SET draft_id = NULL
             WHERE account_id = ?1 AND draft_id = ?2",
            params![account_id, draft_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "UPDATE outbound_attachments SET draft_id = ?2
                 WHERE account_id = ?1 AND id = ?3 AND draft_id IS NULL",
            )?;
            for id in ids {
                stmt.execute(params![account_id, draft_id, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (SqliteStore, AccountId) {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        (store, acct)
    }

    #[test]
    fn staged_bytes_round_trip_by_id_in_the_order_asked() {
        let (store, acct) = store();
        let now = Utc::now();
        let a = store
            .stage_outbound_attachment(acct, "a.png", "image/png", "cid-a", b"AAA", now)
            .unwrap();
        let b = store
            .stage_outbound_attachment(acct, "b.pdf", "application/pdf", "cid-b", b"BB", now)
            .unwrap();
        assert_eq!(a.size_bytes, 3);
        assert_eq!(b.size_bytes, 2);

        let got = store.outbound_attachments(acct, &[b.id, a.id]).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].meta.id, b.id, "the caller's order, not the table's");
        assert_eq!(got[0].data, b"BB");
        assert_eq!(got[1].meta.filename, "a.png");
        assert_eq!(got[1].data, b"AAA");

        // An id that names nothing is ABSENT, not an error: the length check
        // at the send is what refuses it.
        let got = store.outbound_attachments(acct, &[a.id, 9999]).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn another_account_cannot_read_or_delete_a_staged_file() {
        let (store, acct) = store();
        let other = store.ensure_account("other@example.com").unwrap();
        let a = store
            .stage_outbound_attachment(acct, "a.png", "image/png", "cid-a", b"AAA", Utc::now())
            .unwrap();
        assert!(store.outbound_attachment(other, a.id).unwrap().is_none());
        assert!(!store.delete_outbound_attachment(other, a.id).unwrap());
        assert!(store.outbound_attachment(acct, a.id).unwrap().is_some());
        assert!(store.delete_outbound_attachment(acct, a.id).unwrap());
        assert!(store.outbound_attachment(acct, a.id).unwrap().is_none());
    }

    #[test]
    fn the_upload_sweep_takes_only_stale_unclaimed_rows() {
        let (store, acct) = store();
        let t0 = Utc::now();
        let stale = store
            .stage_outbound_attachment(acct, "old.txt", "text/plain", "cid-old", b"x", t0)
            .unwrap();
        let claimed = store
            .stage_outbound_attachment(acct, "kept.txt", "text/plain", "cid-kept", b"y", t0)
            .unwrap();
        let draft = store
            .upsert_draft(acct, None, DraftFields::default(), t0)
            .unwrap();
        store
            .claim_outbound_attachments(acct, draft.id, &[claimed.id])
            .unwrap();

        // Two days on, a fresh upload runs the sweep.
        let later = t0 + chrono::Duration::days(2);
        store
            .stage_outbound_attachment(acct, "new.txt", "text/plain", "cid-new", b"z", later)
            .unwrap();
        assert!(
            store.outbound_attachment(acct, stale.id).unwrap().is_none(),
            "unclaimed and past the TTL: swept"
        );
        assert!(
            store
                .outbound_attachment(acct, claimed.id)
                .unwrap()
                .is_some(),
            "a draft's file is the draft's to keep"
        );
    }

    #[test]
    fn claiming_is_exact_and_never_steals_from_another_draft() {
        let (store, acct) = store();
        let now = Utc::now();
        let a = store
            .stage_outbound_attachment(acct, "a", "text/plain", "cid-a", b"a", now)
            .unwrap();
        let b = store
            .stage_outbound_attachment(acct, "b", "text/plain", "cid-b", b"b", now)
            .unwrap();
        let d1 = store
            .upsert_draft(acct, None, DraftFields::default(), now)
            .unwrap();
        store
            .claim_outbound_attachments(acct, d1.id, &[a.id, b.id])
            .unwrap();
        let d1 = store.list_drafts(acct).unwrap().remove(0);
        assert_eq!(
            d1.attachments.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![a.id, b.id]
        );

        // Removing `a` from the tray: the next save names only `b`.
        store
            .claim_outbound_attachments(acct, d1.id, &[b.id])
            .unwrap();
        let d1 = store.list_drafts(acct).unwrap().remove(0);
        assert_eq!(d1.attachments.len(), 1);
        assert_eq!(d1.attachments[0].id, b.id);

        // A reply draft cannot claim what the new-message draft holds.
        let m = store
            .upsert_message(&crate::types::NewMessage {
                account_id: acct,
                gmail_msg_id: "g1".into(),
                thread_id: "t1".into(),
                from_addr: "alice@example.com".into(),
                from_name: None,
                subject: "hi".into(),
                received_at: now,
                snippet: String::new(),
                body: "hello".into(),
                body_html: None,
                is_sent: false,
                is_spam: false,
                to_addrs: None,
                list_unsubscribe: None,
                list_unsub_one_click: false,
                auth_pass: None,
            })
            .unwrap();
        let d2 = store
            .upsert_draft(acct, Some(m), DraftFields::default(), now)
            .unwrap();
        store
            .claim_outbound_attachments(acct, d2.id, &[b.id])
            .unwrap();
        let drafts = store.list_drafts(acct).unwrap();
        let d2 = drafts.iter().find(|d| d.id == d2.id).unwrap();
        let d1 = drafts.iter().find(|d| d.id == d1.id).unwrap();
        assert!(d2.attachments.is_empty(), "not stolen");
        assert_eq!(d1.attachments.len(), 1, "still the first draft's");
    }

    #[test]
    fn deleting_a_draft_takes_its_files_with_it() {
        let (store, acct) = store();
        let now = Utc::now();
        let a = store
            .stage_outbound_attachment(acct, "a", "text/plain", "cid-a", b"a", now)
            .unwrap();
        let d = store
            .upsert_draft(acct, None, DraftFields::default(), now)
            .unwrap();
        store
            .claim_outbound_attachments(acct, d.id, &[a.id])
            .unwrap();
        assert!(store.delete_draft(acct, d.id).unwrap());
        assert!(store.outbound_attachment(acct, a.id).unwrap().is_none());
    }
}
