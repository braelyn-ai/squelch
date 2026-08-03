//! Message ingest, thread views, attachments, deadlines, sync cursors
//! and the sealed-mail reads.

use super::*;
use super::specialists::{
    auto_close_bill_for_receipt_conn, upsert_calendar_conn, upsert_receipt_conn,
    upsert_shipment_conn,
};

/// Apply the unsubscribe VIOLATION bump for a just-stored inbound message, in
/// the caller's transaction: an unresolved `unsubscribes` row for
/// `(account_id, lower(from_addr))` whose request is more than 72h older than
/// this `received_at` gets `violation_count + 1` and `last_violation_at`. No
/// row, already resolved, or still within grace is a silent no-op.
fn bump_unsub_violation_conn(
    conn: &Connection,
    account_id: AccountId,
    from_addr: &str,
    received_at: DateTime<Utc>,
) -> Result<()> {
    let sender = from_addr.trim().to_ascii_lowercase();
    if sender.is_empty() {
        return Ok(());
    }
    // Read the outstanding request so the grace comparison runs on real
    // timestamps in Rust rather than as lexical string math in SQL.
    let row: Option<String> = conn
        .query_row(
            "SELECT requested_at FROM unsubscribes
             WHERE account_id = ?1 AND sender_addr = ?2 AND resolution IS NULL",
            params![account_id, sender],
            |r| r.get(0),
        )
        .optional()?;
    let Some(requested_s) = row else {
        return Ok(());
    };
    let requested_at = parse_dt(&requested_s)?;
    if received_at > requested_at + chrono::Duration::hours(72) {
        conn.execute(
            "UPDATE unsubscribes
             SET violation_count = violation_count + 1, last_violation_at = ?3
             WHERE account_id = ?1 AND sender_addr = ?2 AND resolution IS NULL",
            params![account_id, sender, received_at.to_rfc3339()],
        )?;
    }
    Ok(())
}

/// Upsert a message + FTS + Sent-derived contacts against an explicit
/// connection/transaction handle. Shared by [`SqliteStore::upsert_message`] and
/// the transactional [`Store::ingest_message`] path so both stay in sync.
fn upsert_message_conn(conn: &Connection, msg: &NewMessage) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages(account_id, gmail_msg_id, thread_id, from_addr, from_name,
             subject, received_at, snippet, body, body_html, is_sent,
             list_unsubscribe, list_unsub_one_click)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(account_id, gmail_msg_id) DO UPDATE SET
             thread_id=excluded.thread_id, from_addr=excluded.from_addr,
             from_name=excluded.from_name, subject=excluded.subject,
             received_at=excluded.received_at, snippet=excluded.snippet,
             body=excluded.body, body_html=excluded.body_html, is_sent=excluded.is_sent,
             list_unsubscribe=excluded.list_unsubscribe,
             list_unsub_one_click=excluded.list_unsub_one_click",
        params![
            msg.account_id,
            msg.gmail_msg_id,
            msg.thread_id,
            msg.from_addr,
            msg.from_name,
            msg.subject,
            msg.received_at.to_rfc3339(),
            msg.snippet,
            msg.body,
            msg.body_html,
            msg.is_sent as i64,
            msg.list_unsubscribe,
            msg.list_unsub_one_click as i64,
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM messages WHERE account_id=?1 AND gmail_msg_id=?2",
        params![msg.account_id, msg.gmail_msg_id],
        |r| r.get(0),
    )?;

    // Keep the FTS index in sync.
    conn.execute("DELETE FROM messages_fts WHERE rowid=?1", params![id])?;
    conn.execute(
        "INSERT INTO messages_fts(rowid, subject, body) VALUES(?1,?2,?3)",
        params![id, msg.subject, msg.body],
    )?;

    // Contacts are NOT seeded here: Sent mail's From header is the user's own
    // address. They come from the To/Cc recipients in `ingest_message`.
    Ok(id)
}

/// Seed the contacts table from a Sent message's recipients, each bumping its
/// `sent_count`. Addresses arrive de-duplicated and stripped of the account's own
/// address, so only empties are skipped. Received mail passes an empty list.
fn seed_contacts_conn(
    conn: &Connection,
    account_id: AccountId,
    recipients: &[String],
    first_seen: &str,
) -> Result<()> {
    for addr in recipients {
        if addr.trim().is_empty() {
            continue;
        }
        conn.execute(
            "INSERT INTO contacts(account_id, addr, sent_count, first_seen)
             VALUES(?1,?2,1,?3)
             ON CONFLICT(account_id, addr) DO UPDATE SET sent_count = sent_count + 1",
            params![account_id, addr, first_seen],
        )?;
    }
    Ok(())
}

/// Replace this message's attachment rows, in the caller's transaction.
/// DELETE-then-INSERT keeps re-ingest idempotent; `data == None` writes a NULL
/// blob (over-cap, metadata only) while `size_bytes` stays the real decoded size.
/// Written for sealed mail too — the byte-serving path guards sealed parents.
///
/// INSERT OR IGNORE, not plain INSERT: a sender can attach the same file twice,
/// and a UNIQUE(account,message,filename,size) violation would roll back the
/// ENTIRE message ingest — a remote ingest DoS. Collapsing identical duplicates
/// to one row is the wanted outcome.
fn insert_attachments_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
    attachments: &[AttachmentInfo],
) -> Result<()> {
    conn.execute(
        "DELETE FROM attachments WHERE account_id=?1 AND message_id=?2",
        params![account_id, message_id],
    )?;
    for a in attachments {
        conn.execute(
            "INSERT OR IGNORE INTO attachments(account_id, message_id, filename, mime, size_bytes, data)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                account_id,
                message_id,
                a.filename,
                a.mime,
                a.size_bytes,
                a.data.as_deref(),
            ],
        )?;
    }
    Ok(())
}

/// The shared entry guard for both thread views: SECURITY — if ANY message in
/// this thread is sealed, the whole thread is NotFound, as is a thread with no
/// messages at all. Returns the thread's subject (its earliest message's) so the
/// caller never re-runs the same lookup.
fn thread_guard_and_subject(
    conn: &Connection,
    account_id: AccountId,
    thread_id: &str,
) -> Result<String> {
    let sealed_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM triage
         WHERE account_id=?1 AND sensitivity='sealed'
           AND message_id IN (SELECT id FROM messages WHERE account_id=?1 AND thread_id=?2)",
        params![account_id, thread_id],
        |r| r.get(0),
    )?;
    if sealed_count > 0 {
        return Err(CoreError::NotFound);
    }
    let subject: Option<String> = conn
        .query_row(
            "SELECT subject FROM messages
             WHERE account_id=?1 AND thread_id=?2
             ORDER BY received_at ASC LIMIT 1",
            params![account_id, thread_id],
            |r| r.get(0),
        )
        .optional()?;
    subject.ok_or(CoreError::NotFound)
}

/// Replace this message's `deadlines` row, in the caller's transaction:
/// DELETE-then-INSERT so a re-apply/re-ingest is idempotent, and `None` simply
/// leaves the message dateless. Shared by ingest and both stage applies, whose
/// callers own the sealed guard (a sealed message must never grow a deadline).
pub(super) fn rewrite_deadline_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
    deadline: Option<&crate::triage::DeadlineHit>,
) -> Result<()> {
    conn.execute("DELETE FROM deadlines WHERE message_id=?1", params![message_id])?;
    if let Some(d) = deadline {
        conn.execute(
            "INSERT INTO deadlines(account_id, message_id, kind, amount, currency,
                 due_at, past_due, source)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                account_id,
                message_id,
                d.kind,
                d.amount,
                d.currency,
                d.due_at.to_rfc3339(),
                d.past_due as i64,
                d.source,
            ],
        )?;
    }
    Ok(())
}

/// Load one message's attachment metadata (NO bytes) as the human-door wire
/// shape, ordered by row id; `downloadable` is `data IS NOT NULL`. The CALLER
/// owns the sealed guard.
fn load_client_attachments_conn(
    conn: &Connection,
    account_id: AccountId,
    message_id: i64,
) -> Result<Vec<ClientAttachment>> {
    let mut stmt = conn.prepare(
        "SELECT id, filename, mime, size_bytes, data IS NOT NULL
         FROM attachments
         WHERE account_id=?1 AND message_id=?2
         ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map(params![account_id, message_id], |r| {
            Ok(ClientAttachment {
                id: r.get(0)?,
                filename: r.get(1)?,
                mime: r.get(2)?,
                size: r.get(3)?,
                downloadable: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

impl SqliteStore {
    pub(super) fn upsert_message(&self, msg: &NewMessage) -> Result<i64> {
        let conn = self.lock()?;
        upsert_message_conn(&conn, msg)
    }

    pub(super) fn thread_view(&self, account_id: AccountId, thread_id: &str) -> Result<ThreadView> {
        let conn = self.lock()?;
        let subject = thread_guard_and_subject(&conn, account_id, thread_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, from_addr, from_name, received_at, body
             FROM messages
             WHERE account_id=?1 AND thread_id=?2
             ORDER BY received_at ASC",
        )?;
        let messages = stmt
            .query_map(params![account_id, thread_id], |r| {
                Ok(SanitizedMessage {
                    id: r.get(0)?,
                    from_addr: r.get(1)?,
                    from_name: r.get(2)?,
                    received_at: dt(r, 3)?,
                    content: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if messages.is_empty() {
            return Err(CoreError::NotFound);
        }

        Ok(ThreadView {
            thread_id: thread_id.to_string(),
            subject,
            messages,
        })
    }

    pub(super) fn thread_id_for_message(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<Option<String>> {
        let conn = self.lock()?;
        // SECURITY: a sealed message id resolves to `None` exactly like a
        // nonexistent one, so the `get_thread` fallback cannot confirm that a
        // sealed message exists. A message with no triage row COALESCEs to
        // non-sealed so plain mail still resolves.
        let thread_id: Option<String> = conn
            .query_row(
                "SELECT m.thread_id
                 FROM messages m
                 LEFT JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2
                   AND COALESCE(t.sensitivity, 'normal') != 'sealed'",
                params![account_id, message_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(thread_id)
    }

    pub(super) fn thread_view_with_html(
        &self,
        account_id: AccountId,
        thread_id: &str,
    ) -> Result<ClientThreadView> {
        let conn = self.lock()?;
        // SECURITY: same sealed/nonexistent -> NotFound guard as `thread_view`,
        // so this human-door variant never reveals a sealed thread's html.
        let subject = thread_guard_and_subject(&conn, account_id, thread_id)?;

        // Per-message triage rides along for in-thread attention highlighting.
        // LEFT JOIN: a message somehow missing its triage row still renders,
        // just unhighlighted.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.from_addr, m.from_name, m.received_at, m.body, m.body_html,
                    t.tier, t.deadline, t.status, t.one_line
             FROM messages m
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE m.account_id=?1 AND m.thread_id=?2
             ORDER BY m.received_at ASC",
        )?;
        // Collect first, releasing `stmt`'s borrow of `conn`, so the per-message
        // attachment query below can run on the same connection.
        let mut messages = stmt
            .query_map(params![account_id, thread_id], |r| {
                Ok(ClientMessage {
                    id: r.get(0)?,
                    from_addr: r.get(1)?,
                    from_name: r.get(2)?,
                    received_at: dt(r, 3)?,
                    content: r.get(4)?,
                    html: r.get(5)?,
                    attachments: Vec::new(), // filled below, once `stmt` is gone
                    tier: r.get::<_, Option<String>>(6)?.as_deref().and_then(Tier::parse),
                    deadline: dt_opt(r, 7)?,
                    attention_open: r
                        .get::<_, Option<String>>(8)?
                        .map(|s| s != "done"),
                    one_line: r
                        .get::<_, Option<String>>(9)?
                        .filter(|s| !s.is_empty()),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        for m in &mut messages {
            // ALWAYS present on the wire ([] when none). No sealed guard needed:
            // the whole view 404s any thread containing a sealed message.
            m.attachments = load_client_attachments_conn(&conn, account_id, m.id)?;
        }
        if messages.is_empty() {
            return Err(CoreError::NotFound);
        }

        Ok(ClientThreadView {
            thread_id: thread_id.to_string(),
            subject,
            messages,
        })
    }

    pub(super) fn attachment_bytes(
        &self,
        account_id: AccountId,
        attachment_id: i64,
    ) -> Result<Option<AttachmentBytes>> {
        let conn = self.lock()?;
        // SECURITY: the parent message must be non-sealed (a missing triage row
        // COALESCEs to 'normal'), so a sealed parent yields no row and the caller
        // 404s, indistinguishable from an unknown id. An existing row with NULL
        // `data` (over the ingest cap) flows out as `Some((.., None))` => 410.
        let row = conn
            .query_row(
                "SELECT a.filename, a.mime, a.data
                 FROM attachments a
                 JOIN messages m ON m.id = a.message_id AND m.account_id = a.account_id
                 LEFT JOIN triage t ON t.message_id = a.message_id
                 WHERE a.account_id = ?1 AND a.id = ?2
                   AND COALESCE(t.sensitivity, 'normal') = 'normal'",
                params![account_id, attachment_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    pub(super) fn deadlines(
        &self,
        account_id: AccountId,
        within_days: Option<u32>,
    ) -> Result<Vec<Deadline>> {
        let conn = self.lock()?;
        // SECURITY: exclude deadlines whose source message is sealed.
        // within_days = None means "all".
        let cutoff = within_days
            .map(|d| (Utc::now() + chrono::Duration::days(d as i64)).to_rfc3339());
        let cutoff_ref: &dyn rusqlite::ToSql = match &cutoff {
            Some(s) => s,
            None => &"9999-12-31T23:59:59+00:00",
        };

        let mut stmt = conn.prepare(
            "SELECT d.id, d.account_id, d.message_id, d.kind, d.amount, d.currency,
                    d.due_at, d.past_due, d.source
             FROM deadlines d
             WHERE d.account_id = ?1
               AND d.due_at <= ?2
               AND NOT EXISTS (
                   SELECT 1 FROM triage t
                   WHERE t.message_id = d.message_id AND t.sensitivity = 'sealed'
               )
             ORDER BY d.due_at ASC",
        )?;
        let out = stmt
            .query_map(params![account_id, cutoff_ref], |r| {
                Ok(Deadline {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    message_id: r.get(2)?,
                    kind: r.get(3)?,
                    amount: r.get(4)?,
                    currency: r.get(5)?,
                    due_at: dt(r, 6)?,
                    past_due: r.get::<_, i64>(7)? != 0,
                    source: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn ingest_message(&self, triaged: &TriagedMessage) -> Result<i64> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;

        // 1. Upsert the message row (+ FTS).
        let id = upsert_message_conn(&tx, &triaged.message)?;

        // 1b. Contacts from Sent-mail To/Cc, in the SAME transaction.
        seed_contacts_conn(
            &tx,
            triaged.message.account_id,
            &triaged.recipients,
            &triaged.message.received_at.to_rfc3339(),
        )?;

        // 1c. UNSUBSCRIBE VIOLATION LEDGER: inbound mail from a sender the user
        //     unsubscribed from, past the 72h grace, bumps that sender's
        //     violation_count — in the SAME transaction as the message insert, so
        //     the ledger cannot drift from the mail that drives it.
        if !triaged.message.is_sent {
            bump_unsub_violation_conn(
                &tx,
                triaged.message.account_id,
                &triaged.message.from_addr,
                triaged.message.received_at,
            )?;
        }

        // 2. Write the triage row IN THE SAME TRANSACTION. That is the whole
        //    point for sealed mail: sensitivity='sealed' commits atomically with
        //    the message, so there is no window in which it is queryable as
        //    normal mail. `model_used` stays NULL, which with
        //    sensitivity='normal' is the Stage-2 queue predicate.
        let deadline_dt = triaged.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
        // AUTO-RESOLVE receipts and calendar updates: both are RECORDS, not
        // things to act on, so they start terminal ('done' + resolved_at), stay
        // out of the New/Attention/Aging bands, and live only in their category.
        // Other rows start 'new'.
        let now_s = Utc::now().to_rfc3339();
        let auto_resolved = triaged.sensitivity != Sensitivity::Sealed
            && (triaged.receipt.is_some() || triaged.calendar.is_some());
        let (status, resolved_at) = if auto_resolved {
            ("done", Some(now_s.clone()))
        } else {
            ("new", None)
        };
        // Re-ingest PRESERVES the existing attention lifecycle: a re-sync must not
        // reopen an item the user dismissed. Receipt/calendar rows are the
        // exception, force-resolved on every ingest — the CASE keys off
        // `excluded.status`, and only auto-resolved rows pass 'done' in.
        //
        // Per-property Stage-1 reasons as JSON (NULL when empty, as for sealed /
        // sent mail). HUMAN-DOOR ONLY on read.
        let field_reasons_json = if triaged.field_reasons.is_empty() {
            None
        } else {
            serde_json::to_string(&triaged.field_reasons).ok()
        };
        // STAGE-1/STAGE-2 QUEUE MARKERS: `stage1_model_used` decides whether the
        // Stage-1 pass looks at this row, `needs_stage2` is the escalation seed.
        //   * Sealed / Sent: never queued for any LLM ('n/a').
        //   * Explicit Squelch/Surface rule: the user already ruled on this
        //     sender, so no model spend at all ('rule', needs_stage2=0).
        //   * Filtered rule: skip Stage-1, straight to Stage-2 for the want_text
        //     ('rule', needs_stage2=1).
        //   * Everything else: enter the Stage-1 queue (NULL), seeding
        //     `needs_stage2` from heuristic confidence.
        let (stage1_model_used, needs_stage2): (Option<&str>, i64) =
            if triaged.sensitivity != Sensitivity::Normal || triaged.message.is_sent {
                (Some("n/a"), 0)
            } else if triaged.matched_rule.is_some() {
                (Some("rule"), if triaged.confident { 0 } else { 1 })
            } else {
                (None, if triaged.confident { 0 } else { 1 })
            };
        // RE-INGEST CLASSIFICATION GUARD. A re-ingest carries only HEURISTIC SEED
        // values, so for a row an LLM already classified (`model_used` set, or a
        // `stage1_model_used` other than the 'rule'/'n/a' sentinels) writing the
        // seed back would discard paid classification while the model markers
        // stay put — the row would never re-queue to recover it. This predicate
        // keeps those columns on conflict; still-seed rows refresh normally.
        const PROCESSED: &str = "(triage.model_used IS NOT NULL \
             OR (triage.stage1_model_used IS NOT NULL \
                 AND triage.stage1_model_used NOT IN ('rule', 'n/a')))";
        let triage_upsert = format!(
            "INSERT INTO triage(message_id, account_id, importance, tier, sensitivity,
                 sealed_kind, one_line, reason, deadline, matched_rule_id,
                 stage1_model_used, needs_stage2, model_used,
                 status, resolved_at, created_at, field_reasons)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,NULL,?13,?14,?15,?16)
             ON CONFLICT(message_id) DO UPDATE SET
                 importance=CASE WHEN {PROCESSED} THEN triage.importance ELSE excluded.importance END,
                 tier=CASE WHEN {PROCESSED} THEN triage.tier ELSE excluded.tier END,
                 sensitivity=excluded.sensitivity, sealed_kind=excluded.sealed_kind,
                 one_line=CASE WHEN {PROCESSED} THEN triage.one_line ELSE excluded.one_line END,
                 reason=CASE WHEN {PROCESSED} THEN triage.reason ELSE excluded.reason END,
                 field_reasons=CASE WHEN {PROCESSED} THEN triage.field_reasons ELSE excluded.field_reasons END,
                 deadline=CASE WHEN {PROCESSED} THEN triage.deadline ELSE excluded.deadline END,
                 matched_rule_id=excluded.matched_rule_id,
                 status=CASE WHEN excluded.status='done' THEN 'done' ELSE triage.status END,
                 resolved_at=CASE WHEN excluded.status='done'
                     THEN excluded.resolved_at ELSE triage.resolved_at END"
        );
        tx.execute(
            &triage_upsert,
            params![
                id,
                triaged.message.account_id,
                triaged.importance as i64,
                triaged.tier.as_str(),
                triaged.sensitivity.as_str(),
                triaged.sealed_kind.map(|k| k.as_str()),
                triaged.one_line,
                triaged.reason,
                deadline_dt,
                triaged.matched_rule,
                stage1_model_used,
                needs_stage2,
                status,
                resolved_at,
                now_s,
                field_reasons_json,
            ],
        )?;

        // 2b. LOCAL DRAFT scrub, in the SAME transaction: a re-ingest can turn a
        //     row that was normal when the draft was saved into a sealed one, and
        //     `put_draft` would never accept a sealed parent. The reply
        //     composition goes with the seal.
        if triaged.sensitivity == Sensitivity::Sealed {
            tx.execute(
                "DELETE FROM drafts WHERE account_id = ?1 AND reply_to_message_id = ?2",
                params![triaged.message.account_id, id],
            )?;
        }

        // 3. Deadlines: non-sealed mail only (Stage-1 never runs on sealed
        //    content), so a sealed re-ingest passes None and only clears.
        let ingest_deadline = if triaged.sensitivity == Sensitivity::Sealed {
            None
        } else {
            triaged.deadline.as_ref()
        };
        rewrite_deadline_conn(&tx, triaged.message.account_id, id, ingest_deadline)?;

        // 4. Shipment: NON-SEALED mail only, so `shipments` is sealed-free by
        //    construction. Upserted in the SAME transaction so a package's state
        //    and its source message land atomically.
        if triaged.sensitivity != Sensitivity::Sealed
            && let Some(s) = &triaged.shipment
        {
            upsert_shipment_conn(
                &tx,
                triaged.message.account_id,
                id,
                s,
                triaged.message.received_at,
            )?;
        }

        // 5. Receipt: NON-SEALED mail only, so `receipts` is sealed-free by
        //    construction. Independent of shipment detection — an order
        //    confirmation with a total AND tracking lands in BOTH tables.
        if triaged.sensitivity != Sensitivity::Sealed
            && let Some(r) = &triaged.receipt
        {
            upsert_receipt_conn(
                &tx,
                triaged.message.account_id,
                id,
                &triaged.message.from_addr,
                triaged.message.from_name.as_deref(),
                r,
                triaged.message.received_at,
            )?;

            // 5b. RECEIPT -> OPEN-BILL AUTO-CLOSE, in the SAME transaction:
            //     resolve the bill this payment settles and audit why. A missed
            //     match is fine; a false close would hide an unpaid bill.
            auto_close_bill_for_receipt_conn(
                &tx,
                triaged.message.account_id,
                id,
                &triaged.message.from_addr,
                triaged.message.from_name.as_deref(),
                r,
                triaged.message.received_at,
            )?;
        }

        // 6. Calendar update: NON-SEALED mail only, so `calendar_updates` is
        //    sealed-free by construction. Independent of the other detectors,
        //    exactly like receipts. Nothing is written back to Gmail — "resolved"
        //    is squelch-internal.
        if triaged.sensitivity != Sensitivity::Sealed
            && let Some(c) = &triaged.calendar
        {
            upsert_calendar_conn(
                &tx,
                triaged.message.account_id,
                id,
                c,
                triaged.message.received_at,
            )?;
        }

        // 7. Attachments: written for sealed mail too — the byte-serving endpoint
        //    guards sealed parents. Replaces prior rows so re-ingest is
        //    idempotent; over-cap parts store a NULL blob.
        insert_attachments_conn(&tx, triaged.message.account_id, id, &triaged.attachments)?;

        tx.commit()?;
        Ok(id)
    }

    pub(super) fn is_known_contact(&self, account_id: AccountId, addr: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM contacts
             WHERE account_id=?1 AND addr=?2 COLLATE NOCASE AND sent_count > 0",
            params![account_id, addr],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub(super) fn sync_state(&self, account_id: AccountId, mailbox: &str) -> Result<Option<SyncState>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT uidvalidity, last_uid FROM sync_state
                 WHERE account_id=?1 AND mailbox=?2",
                params![account_id, mailbox],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(row.map(|(uv, lu)| SyncState {
            uidvalidity: uv as u32,
            last_uid: lu as u64,
        }))
    }

    pub(super) fn set_sync_state(
        &self,
        account_id: AccountId,
        mailbox: &str,
        state: &SyncState,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sync_state(account_id, mailbox, uidvalidity, last_uid)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(account_id, mailbox) DO UPDATE SET
                 uidvalidity=excluded.uidvalidity, last_uid=excluded.last_uid",
            params![
                account_id,
                mailbox,
                state.uidvalidity as i64,
                state.last_uid as i64,
            ],
        )?;
        Ok(())
    }

    pub(super) fn sealed_messages(&self, account_id: AccountId) -> Result<Vec<SealedMessage>> {
        // LOCAL-ONLY: the only method that returns sealed rows. TUI use only.
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT m.id, m.account_id, m.thread_id, m.from_addr, m.subject,
                    m.received_at, t.sealed_kind
             FROM messages m
             JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?1 AND t.sensitivity = 'sealed'
             ORDER BY m.received_at DESC",
        )?;
        let out = stmt
            .query_map(params![account_id], |r| {
                Ok(SealedMessage {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    thread_id: r.get(2)?,
                    from_addr: r.get(3)?,
                    subject: r.get(4)?,
                    received_at: dt(r, 5)?,
                    sealed_kind: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn sealed_body(&self, account_id: AccountId, message_id: i64) -> Result<SealedBody> {
        // HUMAN-DOOR-ONLY. Returns NotFound for a missing OR non-sealed message.
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT m.id, m.account_id, m.thread_id, m.from_addr, m.from_name,
                        m.subject, m.received_at, t.sealed_kind, m.body, m.body_html
                 FROM messages m
                 JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2 AND t.sensitivity = 'sealed'",
                params![account_id, message_id],
                |r| {
                    Ok(SealedBody {
                        id: r.get(0)?,
                        account_id: r.get(1)?,
                        thread_id: r.get(2)?,
                        from_addr: r.get(3)?,
                        from_name: r.get(4)?,
                        subject: r.get(5)?,
                        received_at: dt(r, 6)?,
                        sealed_kind: r.get(7)?,
                        body: r.get(8)?,
                        body_html: r.get(9)?,
                    })
                },
            )
            .optional()?;
        row.ok_or(CoreError::NotFound)
    }
}
