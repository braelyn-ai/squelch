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
        from_name: None,
        subject: None,
        preview: None,
    })
}

/// BLANK IS THE SAME AS ABSENT for every optional string this listing serves.
/// A display name of `""` renders as `"" <addr>`, which parses back to a
/// nameless sender anyway; a subject of one non-breaking space (what a mailer
/// that "cleared" the field sends) draws a row with nothing on it, which reads
/// as a bug rather than as blank mail. One spelling of the rule, so the three
/// fields cannot drift into three answers.
fn non_blank(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

/// The recency window every human-door listing runs under (`?2` is the caller's
/// `since`), plus the ONE exemption from it: a row carrying a reminder, pending
/// or fired.
///
/// THE WINDOW EXISTS SO OLD MAIL CANNOT RE-LITIGATE ITSELF — a sitrep that
/// reaches back over the whole corpus is not a sitrep. A reminder is the exact
/// opposite case: the user pointed at that mail themselves and said "show me
/// this again", so it has to outlive the window or the feature's core promise
/// dies on day 30. Without the exemption the pending lens stops listing parked
/// mail a month after it was RECEIVED (not a month after it was parked), and a
/// reminder set on older mail — the "next month" pick on a two-month-old
/// thread — fires into a row that no band, no count and no page will ever show.
///
/// Like [`STANDING_BAND`] it lives in ONE place because the list query and the
/// header's count must agree row for row.
const WITHIN_WINDOW: &str = "(m.received_at >= ?2
            OR t.remind_at IS NOT NULL
            OR t.reminded_at IS NOT NULL)";

/// Membership test for the `standing` band, written against the `triage t` /
/// `messages m` join that both band sites share. It lives in ONE place because
/// the list query and the header's count must agree row for row, or the sitrep
/// header contradicts the list it heads.
///
/// STANDING IS A PROPERTY, NOT A TIMESTAMP: the band is mail owed the user's
/// attention — a dated obligation (`past_due`/`deadline`), a fired reminder, or
/// live correspondence: a thread the user has written in, or a sender the user
/// has written to. A dateless "can you send me the form?" from a real
/// correspondent is exactly as owed as a bill, and the surfacing clock must
/// never rotate it out. Because this is a definition over stored rows, widening
/// it is retroactive: mail already triaged joins the band on the next read.
///
/// THE REMINDER ARM (`reminded_at IS NOT NULL`) carries no tier test on purpose.
/// Every other arm infers what the user cares about; this one is the user saying
/// it outright, so a noise-tier newsletter they asked to see again comes back
/// exactly as loudly as a past-due bill. It is the fired stamp and not the
/// pending one because a pending reminder means "not now" — `set_reminder`
/// marks the thread done precisely to get it out of the bands until it is due.
///
/// What participation deliberately does NOT do: it never unseals anything (the
/// base predicate's `sensitivity != 'sealed'` is the only gate that matters,
/// and it is outside this expression), and it never surfaces the user's own
/// sent mail — the sent sibling is evidence, and `m.is_sent = 0` keeps it out
/// of every listing.
///
/// Address matching folds case on BOTH sides: `messages.from_addr` is stored as
/// the header spelled it, and `contacts.addr` is lowercased by the Sent-history
/// harvest but kept verbatim by the per-message Sent ingest, so neither side can
/// be assumed normalized. That `COLLATE NOCASE` is served by
/// `idx_contacts_addr_nocase` and by nothing else: the primary key is BINARY,
/// and without the collated index this correlated probe walks every contact
/// once per message in the window. `pub(super)` so the plan test can pin that.
pub(super) const STANDING_BAND: &str = "(t.tier IN ('past_due','deadline')
        OR t.reminded_at IS NOT NULL
        OR (m.thread_id != '' AND EXISTS(
                SELECT 1 FROM messages s
                WHERE s.account_id = m.account_id
                  AND s.thread_id = m.thread_id
                  AND s.is_sent = 1))
        OR EXISTS(
                SELECT 1 FROM contacts c
                WHERE c.account_id = t.account_id
                  AND c.addr = lower(trim(m.from_addr)) COLLATE NOCASE
                  AND c.sent_count > 0))
       AND t.status != 'done'";

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
               AND m.is_spam = 0
               AND m.received_at >= ?2
               AND t.importance >= ?3
             ORDER BY t.importance DESC, m.received_at DESC",
        )?;
        // AGENT DOOR: `update_from_row` leaves field_reasons/has_attachments None.
        let out = stmt
            .query_map(
                params![account_id, since.to_rfc3339(), min],
                update_from_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)] // the filters of one listing, one per axis
    pub(super) fn attention_updates(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        min_importance: Option<u8>,
        status: Option<AttentionStatus>,
        band: Option<SitrepBand>,
        pending_reminders: bool,
        spam: SpamScope,
    ) -> Result<Vec<AttentionUpdate>> {
        let conn = self.lock()?;
        let min = min_importance.unwrap_or(0) as i64;

        // Base predicate: sealed excluded, sent excluded, the provider's spam
        // verdict on whichever side `spam` asked for, since/importance window
        // (see WITHIN_WINDOW for why a reminder row is exempt from the `since`).
        // Bands:
        //   standing = dated obligation OR live correspondence, not yet done
        //              (see STANDING_BAND for the definition and its limits)
        //   new      = surfaced_at IS NULL AND status != 'done'
        //   open     = status = 'open'
        // The `status != 'done'` on `new` keeps AUTO-RESOLVED receipts out of the
        // band — a receipt is a record, not new inbox clutter.
        //
        // THE BANDS NEVER PASS `SpamScope::Only`. Only the flat spam page does,
        // and it passes no band with it: spam rows are all status='new' with
        // importance 0 and no reminder, so every band but `new` would be empty
        // anyway, and `new` would be the same list under a name that promises
        // the user something needs doing about it.
        let spam_sql = spam.predicate();
        let mut where_sql = format!(
            "WHERE t.account_id = ?1
               AND t.sensitivity != 'sealed'
               AND m.is_sent = 0
               AND {spam_sql}
               AND {WITHIN_WINDOW}
               AND t.importance >= ?3"
        );
        if let Some(s) = status {
            where_sql.push_str(match s {
                AttentionStatus::New => " AND t.status = 'new'",
                AttentionStatus::Open => " AND t.status = 'open'",
                AttentionStatus::Done => " AND t.status = 'done'",
            });
        }
        match band {
            Some(SitrepBand::Standing) => {
                where_sql.push_str(&format!(" AND {STANDING_BAND}"));
            }
            Some(SitrepBand::New) => {
                where_sql.push_str(" AND t.surfaced_at IS NULL AND t.status != 'done'")
            }
            Some(SitrepBand::Open) => where_sql.push_str(" AND t.status = 'open'"),
            None => {}
        }
        // THE PENDING-REMINDER LISTING, deliberately orthogonal to `status`: every
        // row here is done (that is what `set_reminder` does to it), so this is
        // the one listing that must NOT inherit the bands' "done means gone"
        // reading. It is a schedule, not a band.
        if pending_reminders {
            where_sql.push_str(" AND t.remind_at IS NOT NULL");
        }
        // The `open` band is the aging one: age*importance floats
        // long-unresolved-and-important items, computed in SQL via julianday so
        // the ordering stays server-side. Other bands sort by importance. The
        // same expression orders WITHIN a thread (below) and BETWEEN the
        // representatives, so the row shown is the one the sort would have put
        // first anyway.
        //
        // A pending-reminder listing sorts by DUE DATE instead, and wins over the
        // aging sort when both are asked for: soonest-first is the only ordering
        // a schedule can have. It is also what decides whether `?4` is bound at
        // all, so the two must be read off the SAME flag or the bind count and
        // the SQL disagree.
        let age_order = band == Some(SitrepBand::Open) && !pending_reminders;
        let (inner_order, outer_order) = if pending_reminders {
            (
                "t.remind_at ASC, m.received_at DESC",
                "remind_at ASC, received_at DESC",
            )
        } else if age_order {
            (
                "(julianday(?4) - julianday(m.received_at)) * t.importance DESC, m.received_at DESC",
                "(julianday(?4) - julianday(received_at)) * importance DESC, received_at DESC",
            )
        } else {
            (
                "t.importance DESC, m.received_at DESC",
                "importance DESC, received_at DESC",
            )
        };
        // ONE ROW PER THREAD: a two-message thread is one conversation and must
        // not occupy two band rows. ROW_NUMBER picks the band-sort-first message
        // as the representative; resolving it resolves the whole thread (see
        // set_attention_status), so the hidden siblings can never pop back in.
        // The thread key falls back to the message id for a blank thread_id, so
        // an unthreaded row can only ever collapse with itself.
        //
        // EXCEPT ON THE SCHEDULE, which lists one row per REMINDER: two siblings
        // of one thread can each carry their own, and collapsing them hides one
        // behind the other — unseeable and uncancellable, yet still due to fire.
        // Only the partition key changes; every row keys on itself, so nothing
        // collapses.
        let thread_key = if pending_reminders {
            "'msg-' || m.id"
        } else {
            "COALESCE(NULLIF(m.thread_id, ''), 'msg-' || m.id)"
        };
        let sql = format!(
            "SELECT * FROM (
               SELECT m.id, m.thread_id, t.tier, t.importance, m.from_addr, t.one_line,
                      t.reason, t.deadline, t.matched_rule_id,
                      t.status, t.surfaced_at, t.resolved_at, t.field_reasons,
                      EXISTS(SELECT 1 FROM attachments a
                             WHERE a.account_id = m.account_id
                               AND a.message_id = m.id) AS has_atts,
                      m.from_name AS from_name,
                      m.received_at AS received_at,
                      t.remind_at AS remind_at,
                      t.reminded_at AS reminded_at,
                      m.subject AS subject,
                      m.snippet AS snippet,
                      ROW_NUMBER() OVER (
                          PARTITION BY {thread_key}
                          ORDER BY {inner_order}
                      ) AS rn
               FROM triage t
               JOIN messages m ON m.id = t.message_id
               {where_sql})
             WHERE rn = 1
             ORDER BY {outer_order}"
        );

        let now = Utc::now().to_rfc3339();
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &rusqlite::Row| {
            let mut update = update_from_row(r)?;
            // HUMAN DOOR: the extra columns the agent door never sees. A NULL
            // or malformed reasons blob yields None — one bad row must never fail
            // the whole updates read.
            update.field_reasons = r
                .get::<_, Option<String>>(12)?
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            update.has_attachments = Some(r.get::<_, i64>(13)? != 0);
            update.from_name = non_blank(r.get::<_, Option<String>>(14)?);
            // THE SPAM PAGE'S ROW TEXT, on the spam page and nowhere else.
            // Every row here was skipped by triage, so `one_line` is empty on
            // all of them and the middle of the row would be blank; the mail's
            // own subject and opening words are the only thing left to put
            // there. Reading these columns for the bands too would be 200
            // characters a row, on every poll, for a fill their summaries
            // already made unnecessary — see `Update::subject`.
            if spam == SpamScope::Only {
                update.subject = non_blank(r.get::<_, Option<String>>(18)?);
                update.preview = non_blank(r.get::<_, Option<String>>(19)?);
            }
            Ok(AttentionUpdate {
                update,
                status: AttentionStatus::parse(&r.get::<_, String>(9)?)
                    .unwrap_or(AttentionStatus::New),
                surfaced_at: dt_opt(r, 10)?,
                resolved_at: dt_opt(r, 11)?,
                // HUMAN DOOR again: columns 16/17, past the `received_at` (15)
                // the outer sort needs but no field reads.
                remind_at: dt_opt(r, 16)?,
                reminded_at: dt_opt(r, 17)?,
            })
        };
        let out = if age_order {
            stmt.query_map(params![account_id, since.to_rfc3339(), min, now], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![account_id, since.to_rfc3339(), min], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(out)
    }

    pub(super) fn mark_surfaced(
        &self,
        account_id: AccountId,
        message_ids: &[i64],
    ) -> Result<usize> {
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

    /// Stamp `opened_at` on rows the user has opened.
    ///
    /// Deliberately NOT folded into [`Self::mark_surfaced`], which every list
    /// door calls: the two answer different questions, and merging them would
    /// make "opened" mean "appeared in a list" (see the column comment in
    /// `schema.sql`).
    ///
    /// FIRST OPEN ONLY, like `surfaced_at`: re-reading a thread next week says
    /// nothing new about whether it needed opening, and a moving stamp would
    /// make the rate drift with re-reads. Sealed rows are excluded, which is
    /// belt and braces - the human door never serves one through this path -
    /// and it keeps the denominator and numerator counting the same universe.
    pub(super) fn mark_opened(&self, account_id: AccountId, message_ids: &[i64]) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        let tx = conn.transaction()?;
        let mut first_opened = 0usize;
        {
            let mut stmt = tx.prepare(
                "UPDATE triage
                 SET opened_at = ?1
                 WHERE account_id = ?2 AND message_id = ?3
                   AND sensitivity != 'sealed'
                   AND opened_at IS NULL",
            )?;
            for &id in message_ids {
                first_opened += stmt.execute(params![now, account_id, id])?;
            }
        }
        tx.commit()?;
        Ok(first_opened)
    }

    /// Stamp every message in one thread opened, by thread id.
    ///
    /// THE THREAD, NOT A LIST OF IDS, because the client should not have to
    /// tell the daemon which messages a thread holds - it would be sending back
    /// a list the daemon gave it, and the two could disagree after a sync. The
    /// reader shows the whole thread oldest-first anyway, so opening it is
    /// opening all of it.
    ///
    /// Returns the first-open count.
    pub(super) fn mark_thread_opened(
        &self,
        account_id: AccountId,
        thread_id: &str,
    ) -> Result<usize> {
        let conn = self.lock()?;
        // The sealed guard and the once-only guard are the same ones
        // `mark_opened` keeps, in one statement because the ids are a subquery
        // rather than a caller's list.
        let n = conn.execute(
            "UPDATE triage
                SET opened_at = ?1
              WHERE account_id = ?2
                AND sensitivity != 'sealed'
                AND opened_at IS NULL
                AND message_id IN (
                    SELECT id FROM messages WHERE account_id = ?2 AND thread_id = ?3
                )",
            params![Utc::now().to_rfc3339(), account_id, thread_id],
        )?;
        Ok(n)
    }

    /// How much of this mailbox's incoming mail the user has had to open, over
    /// the rows received since `since`.
    ///
    /// WHAT IT COUNTS, and every exclusion is a decision:
    ///
    /// - RECEIVED MAIL ONLY (`is_sent = 0`). Nobody opens their own outbox, and
    ///   counting it would dilute the rate with a number that has no opinion.
    /// - AND NOT PROVIDER SPAM (`is_spam = 0`), which would dilute it far
    ///   harder and in the flattering direction: spam is unopened by definition
    ///   and there is a lot of it, so counting it would drive "mail the user had
    ///   to open themselves" toward zero on volume alone.
    /// - SEALED MAIL IS IN THE DENOMINATOR. A login code is mail that arrived
    ///   and did not need opening, which is the most honest example there is of
    ///   the thing being measured. It can never be in the numerator, because
    ///   [`Self::mark_opened`] refuses to stamp it.
    /// - Rows with no triage row at all are absent from both, by the join: a
    ///   message the daemon never triaged is one it cannot speak for.
    ///
    /// AND IT NEVER LOOKS BACK FURTHER THAN THE LEDGER ITSELF. `opened_at` was
    /// added to an existing product, so on the day it ships every mailbox has
    /// years of mail and no opens at all; a window that reached past the
    /// column's own arrival would divide a full denominator by an empty
    /// numerator and report that this person opens almost nothing. The caller's
    /// sample floors cannot catch that - they measure how old the MAIL is,
    /// which is ample, not how old the LEDGER is, which is seconds. See
    /// `migrate::stamp_open_ledger_start`.
    ///
    /// WHAT IT CANNOT SEE is mail read somewhere else. `opened_at` is stamped
    /// by this user's own Passband client and nowhere else, so somebody who
    /// reads half their mail in Gmail on a phone will find this FLATTERINGLY
    /// LOW.
    /// Every consumer has to carry that caveat with it, and the one consumer
    /// (the invite mail) does.
    pub(super) fn share_open_rate(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
    ) -> Result<OpenRate> {
        let conn = self.lock()?;
        // The window, clamped to the ledger. Whichever floor is LATER wins: a
        // caller asking for ninety days of a thirty-day-old ledger gets thirty.
        let ledger_since = open_ledger_since(&conn, account_id)?;
        let floor = since.max(ledger_since);
        let floor_text = floor.to_rfc3339();
        let (received, opened): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COUNT(t.opened_at)
               FROM messages m JOIN triage t ON t.message_id = m.id
              WHERE m.account_id = ?1 AND m.is_sent = 0 AND m.is_spam = 0
                AND m.received_at >= ?2",
            params![account_id, floor_text],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // The oldest received row in the window, which is what says how much
        // history the answer actually rests on. A mailbox synced three days ago
        // has a rate; it does not have a rate worth mailing to a stranger.
        let oldest: Option<String> = conn.query_row(
            "SELECT MIN(received_at) FROM messages
              WHERE account_id = ?1 AND is_sent = 0 AND is_spam = 0
                AND received_at >= ?2",
            params![account_id, floor_text],
            |r| r.get(0),
        )?;
        Ok(OpenRate {
            received: received.max(0) as u64,
            opened: opened.max(0) as u64,
            oldest_received_at: oldest.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|d| d.with_timezone(&Utc))
            }),
        })
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
        // DONE RESOLVES THE WHOLE THREAD: the bands show one row per thread
        // (attention_updates), so resolving the representative must also resolve
        // its siblings — otherwise a hidden sibling pops straight back into the
        // band and done becomes whack-a-mole. Reopen (open/new) stays
        // message-scoped: the undo path restores exactly the one row it removed,
        // which is enough to re-surface the thread.
        //
        // DONE ALSO RETIRES THE FIRED REMINDER. `reminded_at` is what holds a
        // row in the standing band at any tier, so a stamp left behind means a
        // reminder the user answered comes back every time the row is ever
        // reopened — the reminder fired, it was dealt with, it is spent. Reopen
        // does not restore it for the same reason: undo restores the row, not
        // the schedule.
        let n = match status {
            AttentionStatus::Done => conn.execute(
                "UPDATE triage
                 SET status = ?1, resolved_at = ?2, reminded_at = NULL
                 WHERE account_id = ?3 AND sensitivity != 'sealed'
                   AND (message_id = ?4 OR message_id IN (
                       SELECT sib.id FROM messages me
                       JOIN messages sib ON sib.account_id = me.account_id
                                        AND sib.thread_id = me.thread_id
                       WHERE me.account_id = ?3 AND me.id = ?4
                         AND me.thread_id != ''))",
                params![status.as_str(), resolved_at, account_id, message_id],
            )?,
            _ => conn.execute(
                "UPDATE triage
                 SET status = ?1, resolved_at = ?2
                 WHERE account_id = ?3 AND message_id = ?4 AND sensitivity != 'sealed'",
                params![status.as_str(), resolved_at, account_id, message_id],
            )?,
        };
        Ok(n > 0)
    }

    /// "Remind me about this at T." Thread-wide done PLUS the stamp, in one
    /// transaction: a reminder that half-applied would either nag a mail the
    /// user just deferred or lose the deferral entirely, and both are worse than
    /// the write failing.
    ///
    /// THE STAMP GOES FIRST, and its row count — not the done sweep's — is the
    /// missing/sealed answer. The sweep is thread-wide, so a sealed target with
    /// an unsealed sibling would report `true` off a row the caller never named
    /// while the stamp itself landed nowhere.
    pub(super) fn set_reminder(
        &self,
        account_id: AccountId,
        message_id: i64,
        remind_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        let tx = conn.transaction()?;
        // SECURITY: sealed rows excluded in SQL, so missing and sealed are the
        // same `false`. `reminded_at` is cleared because a new reminder replaces
        // whatever the old one already said — the two stamps are the pending and
        // fired halves of ONE reminder, never a history.
        //
        // AND NOT THE USER'S OWN SENT MAIL, NOR PROVIDER SPAM, by the same
        // indistinguishability rule as sealed: both carry a triage row (neutral,
        // tier=noise) that no band lists, so a reminder stamped on one would be
        // unreachable forever — it could never be listed, seen or cancelled, and
        // firing it would surface nothing. Spam is the sharper case of the two,
        // because `reminded_at` is the one standing-band arm that carries no tier
        // test: the stamp is meant to outrank triage's opinion, and the only
        // thing still keeping the row out of the band would be the spam
        // predicate. No row, no `true`, and the handler 404s.
        let stamped = tx.execute(
            "UPDATE triage
             SET remind_at = ?1, reminded_at = NULL
             WHERE account_id = ?2 AND message_id = ?3 AND sensitivity != 'sealed'
               AND EXISTS(SELECT 1 FROM messages mm
                          WHERE mm.account_id = ?2 AND mm.id = ?3
                            AND mm.is_sent = 0 AND mm.is_spam = 0)",
            params![remind_at.to_rfc3339(), account_id, message_id],
        )?;
        if stamped == 0 {
            return Ok(false);
        }
        // DEFERRING IS RESOLVING, thread-wide, with the SAME SQL shape as
        // `set_attention_status`'s Done arm and for the same reason: the bands
        // show one row per thread, so leaving a sibling open puts the mail the
        // user just snoozed straight back in front of them. It clears
        // `reminded_at` for that same reason — a sibling still wearing an old
        // fired stamp sits in the standing band while the thread is supposedly
        // parked until the new date.
        tx.execute(
            "UPDATE triage
             SET status = 'done', resolved_at = ?1, reminded_at = NULL
             WHERE account_id = ?2 AND sensitivity != 'sealed'
               AND (message_id = ?3 OR message_id IN (
                   SELECT sib.id FROM messages me
                   JOIN messages sib ON sib.account_id = me.account_id
                                    AND sib.thread_id = me.thread_id
                   WHERE me.account_id = ?2 AND me.id = ?3
                     AND me.thread_id != ''))",
            params![now, account_id, message_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Drop a PENDING reminder, leaving everything else exactly where it is.
    ///
    /// Deliberately NOT the inverse of [`Self::set_reminder`]: it does not reopen
    /// the thread (the user may well have meant to resolve it) and it does not
    /// touch `reminded_at` (clearing a pending reminder says nothing about one
    /// that already came due). Idempotent — a row with no reminder is a
    /// successful no-op, so only missing/sealed returns `false`.
    pub(super) fn clear_reminder(&self, account_id: AccountId, message_id: i64) -> Result<bool> {
        let conn = self.lock()?;
        // SECURITY: sealed rows excluded in SQL.
        let n = conn.execute(
            "UPDATE triage
             SET remind_at = NULL
             WHERE account_id = ?1 AND message_id = ?2 AND sensitivity != 'sealed'",
            params![account_id, message_id],
        )?;
        Ok(n > 0)
    }

    /// THE SWEEP: every reminder now due, fired in one statement, returning the
    /// message ids it moved.
    ///
    /// The stamp MOVES rather than being copied — `remind_at` NULLs as
    /// `reminded_at` takes its value — which is what makes this idempotent
    /// without a cooldown: a fired row no longer matches the predicate, so a
    /// second tick (or a second daemon) cannot fire it twice. `resolved_at`
    /// clears alongside the status because a reopened row that still carries a
    /// resolution timestamp reads as done to everything downstream.
    ///
    /// `now` is a parameter, not `Utc::now()`, so a test can hold the clock.
    pub(super) fn fire_due_reminders(
        &self,
        account_id: AccountId,
        now: DateTime<Utc>,
    ) -> Result<Vec<i64>> {
        let conn = self.lock()?;
        // SECURITY: sealed rows excluded in SQL. A sealed row cannot carry a
        // reminder in the first place (`set_reminder` refuses it), and the guard
        // is repeated here anyway because "cannot happen" is not a gate.
        let mut stmt = conn.prepare(
            "UPDATE triage
             SET status = 'open',
                 resolved_at = NULL,
                 reminded_at = remind_at,
                 remind_at = NULL
             WHERE account_id = ?1
               AND remind_at IS NOT NULL
               AND remind_at <= ?2
               AND sensitivity != 'sealed'
             RETURNING message_id",
        )?;
        let out = stmt
            .query_map(params![account_id, now.to_rfc3339()], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    pub(super) fn resolve_sender(&self, account_id: AccountId, sender_addr: &str) -> Result<usize> {
        let sender = sender_addr.trim().to_lowercase();
        if sender.is_empty() {
            return Ok(0);
        }
        let conn = self.lock()?;
        // Already-done rows are left alone so their original resolved_at — and
        // whatever resolved them — survives. Sealed excluded, as everywhere.
        let n = conn.execute(
            "UPDATE triage
             SET status = 'done', resolved_at = ?1
             WHERE account_id = ?2
               AND sensitivity != 'sealed'
               AND status != 'done'
               AND message_id IN (
                   SELECT m.id FROM messages m
                   WHERE m.account_id = ?2
                     AND LOWER(TRIM(m.from_addr)) = ?3
               )",
            params![Utc::now().to_rfc3339(), account_id, sender],
        )?;
        Ok(n)
    }

    /// `bands_since` windows ONLY the band counts, mirroring the `since` the
    /// list queries run under: standing's correspondence arms admit mail from
    /// every contact the user has ever written to, so an unwindowed count
    /// grows monotonically with corpus age and outruns the list it heads. The
    /// inventory counts (tiers, total, sealed) stay all-time on purpose —
    /// they describe the store, not the sitrep.
    pub(super) fn stats(
        &self,
        account_id: AccountId,
        bands_since: DateTime<Utc>,
    ) -> Result<StoreStats> {
        let conn = self.lock()?;

        let mut tier_counts = std::collections::BTreeMap::new();
        {
            // JOINED TO `messages` for the two exclusions, which this count did
            // without for a long time and should not have. Sent mail and
            // provider spam are both written tier=noise by ingest without a
            // model ever looking at them, and neither is listed by the noise
            // page, so counting them made the header's noise number a promise
            // the page could not keep.
            let mut stmt = conn.prepare(
                "SELECT t.tier, COUNT(*) FROM triage t
                 JOIN messages m ON m.id = t.message_id
                 WHERE t.account_id=?1 AND t.sensitivity != 'sealed'
                   AND m.is_sent = 0 AND m.is_spam = 0
                 GROUP BY t.tier",
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

        // WHEN THE SPAM FOLDER WAS LAST FETCHED, which the page needs in order to
        // tell "we looked and it is empty" from "nobody has looked yet".
        let spam_synced_at: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE account_id = ?1 AND key = ?2",
                params![account_id, SPAM_SYNCED_AT_KEY],
                |r| r.get(0),
            )
            .optional()?;
        let spam_synced_at = spam_synced_at.as_deref().and_then(|s| parse_dt(s).ok());

        // THE SPAM PAGE'S DOOR NUMBER. Counted the same way the page lists —
        // non-sealed, and served by `idx_messages_spam` — so the chip and the
        // page it opens cannot disagree.
        let spam: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages m
             JOIN triage t ON t.message_id = m.id
             WHERE m.account_id=?1 AND m.is_spam = 1 AND t.sensitivity != 'sealed'",
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
        // the `band` query on attention_updates, or header and list disagree —
        // including its one-row-per-thread collapse, hence DISTINCT thread keys
        // (blank thread_id falls back to the message id, same as the list),
        // and its received_at window, hence `bands_since` AND the same
        // WITHIN_WINDOW reminder exemption the list runs under — a fired
        // reminder the header refused to count is a header contradicting its
        // list (the list's default min importance is 0, a no-op, so it is not
        // mirrored here).
        // Standing is decided in the subquery, where the `m` join alias the
        // shared expression is written against is still in scope.
        let (standing, new_count, open_count): (i64, i64, i64) = conn.query_row(
            &format!(
                "SELECT
                     COUNT(DISTINCT CASE WHEN t.standing = 1 THEN tkey END),
                     COUNT(DISTINCT CASE WHEN t.surfaced_at IS NULL
                         AND t.status != 'done' THEN tkey END),
                     COUNT(DISTINCT CASE WHEN t.status = 'open' THEN tkey END)
                 FROM (SELECT t.*, COALESCE(NULLIF(m.thread_id, ''), 'msg-' || m.id) AS tkey,
                              ({STANDING_BAND}) AS standing
                       FROM triage t
                       JOIN messages m ON m.id = t.message_id
                       WHERE t.account_id = ?1 AND t.sensitivity != 'sealed'
                         AND m.is_sent = 0
                         AND m.is_spam = 0
                         AND {WITHIN_WINDOW}) t"
            ),
            params![account_id, bands_since.to_rfc3339()],
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
            spam,
            spam_synced_at,
            last_history_id: last_history_id.map(|v| v as u64),
            bands: BandCounts {
                standing,
                new: new_count,
                open: open_count,
            },
            last_surfaced_at,
        })
    }

    /// See [`Store::mail_activity`]. One pass over the window's messages,
    /// bucketed in SQL: `received_at` is stored as RFC3339 UTC, so its first
    /// ten characters ARE the ledger's day key, and the bounds compare as text
    /// against the same `to_rfc3339` shape the writes use.
    ///
    /// LEFT JOIN, not JOIN: a message the triage pipeline has not reached yet
    /// is still mail that arrived, so it counts as received and in no tier.
    /// Sent mail carries a neutral triage row (see `ingest`: "the user's own
    /// outbox must never pollute the ranked inbox"), which is why every tier
    /// bucket re-checks `is_sent = 0` rather than trusting the row's tier.
    pub(super) fn mail_activity(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<MailActivityDay>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT substr(m.received_at, 1, 10) AS day,
                    COALESCE(SUM(m.is_sent = 0), 0),
                    COALESCE(SUM(m.is_sent = 1), 0),
                    COALESCE(SUM(m.is_sent = 0 AND t.sensitivity = 'sealed'), 0),
                    COALESCE(SUM(m.is_sent = 0 AND t.sensitivity != 'sealed'
                                 AND t.tier = 'past_due'), 0),
                    COALESCE(SUM(m.is_sent = 0 AND t.sensitivity != 'sealed'
                                 AND t.tier = 'deadline'), 0),
                    COALESCE(SUM(m.is_sent = 0 AND t.sensitivity != 'sealed'
                                 AND t.tier = 'signal'), 0),
                    COALESCE(SUM(m.is_sent = 0 AND t.sensitivity != 'sealed'
                                 AND t.tier = 'noise'), 0)
             FROM messages m
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?1 AND m.is_spam = 0
               AND m.received_at >= ?2 AND m.received_at < ?3
             GROUP BY day
             ORDER BY day",
        )?;
        let count = |r: &rusqlite::Row<'_>, i: usize| -> rusqlite::Result<u64> {
            Ok(r.get::<_, i64>(i)?.max(0) as u64)
        };
        let rows = stmt
            .query_map(
                params![account_id, since.to_rfc3339(), until.to_rfc3339()],
                |r| {
                    Ok(MailActivityDay {
                        day: r.get(0)?,
                        received: count(r, 1)?,
                        sent: count(r, 2)?,
                        sealed: count(r, 3)?,
                        past_due: count(r, 4)?,
                        deadline: count(r, 5)?,
                        signal: count(r, 6)?,
                        noise: count(r, 7)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// The moment an account's open ledger started running.
///
/// The migration's stamp when there is one (see
/// `migrate::stamp_open_ledger_start`), and the ACCOUNT'S OWN `created_at` when
/// there is not: an account made after the column existed has been recording
/// since it existed, which is exactly what that says.
///
/// EVERY FAILURE FALLS TOWARD "NO EVIDENCE YET". A stamp that will not parse,
/// or an account row that is not there, resolves to NOW rather than to the
/// epoch — because the epoch reads as "the ledger has always been running",
/// which is the one answer that produces the flattering number this whole
/// mechanism exists to prevent.
fn open_ledger_since(conn: &Connection, account_id: AccountId) -> Result<DateTime<Utc>> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE account_id = ?1 AND key = ?2",
            params![account_id, OPEN_LEDGER_SINCE_KEY],
            |r| r.get(0),
        )
        .optional()?;
    let stored = match stored {
        Some(s) => Some(s),
        None => conn
            .query_row(
                "SELECT created_at FROM accounts WHERE id = ?1",
                params![account_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?,
    };
    Ok(stored
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now))
}
