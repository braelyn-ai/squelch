//! The sender directory behind the search field's `from:` autocomplete
//! (`/client/senders`): who has written to this account, ranked for a typed
//! fragment. HUMAN-DOOR ONLY, like `contacts`; the agent door has `search_mail`
//! and no business holding a list of everyone who mails the user.
//!
//! The directory is MAINTAINED AT INGEST (`bump_sender_conn`, called from the
//! message upsert and from the not-spam action) rather than computed per
//! keystroke. The obvious query, a GROUP BY over `messages` with a substring
//! LIKE on address and name, measured 110-160 ms at 100k rows, taken while
//! holding the store's single connection mutex, which is exactly the shape of
//! p95 floor kill-p95 had to remove. A few thousand directory rows scan in
//! well under a millisecond.
//!
//! `contacts` cannot serve this. It is the people the user writes TO, seeded
//! from Sent mail's recipients; the people who write to you are a much larger
//! set, and `from:dan` has to find the Dan who has only ever replied.

use super::*;

/// Record one INBOUND, NON-SPAM sighting of `addr` in the directory.
///
/// `msg_count` is RECOMPUTED from `messages` rather than incremented: the
/// upsert that calls this runs again for every re-sighting of the same
/// `gmail_msg_id` (a re-fetch, a second history walk), and an increment would
/// count the same mail twice. The count is an indexed probe
/// (`idx_messages_from`) and follows the `is_sent = 0 AND is_spam = 0` rule
/// every listing does.
///
/// The newest sighting's display name wins when it has one, and a sighting
/// without a name never blanks a stored one: "Dan" arriving after "Dan Smith"
/// is the sender's own most recent choice of name; an empty header is not.
///
/// Sealed is NOT known here. Sensitivity is triage's verdict and arrives after
/// ingest, so a sender whose only mail is sealed still gets a row; the reader
/// side (`search_senders`) refuses to offer one until at least one non-sealed
/// message exists behind it.
pub(super) fn bump_sender_conn(
    conn: &Connection,
    account_id: AccountId,
    addr: &str,
    display_name: Option<&str>,
    received_at: &str,
) -> Result<()> {
    if addr.trim().is_empty() {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO senders(account_id, addr, display_name, msg_count, first_seen,
                             last_received_at)
         VALUES(?1, ?2, NULLIF(?3, ''),
                (SELECT COUNT(*) FROM messages
                  WHERE account_id = ?1 AND from_addr = ?2
                    AND is_sent = 0 AND is_spam = 0),
                ?4, ?4)
         ON CONFLICT(account_id, addr) DO UPDATE SET
             display_name = COALESCE(NULLIF(excluded.display_name, ''), senders.display_name),
             msg_count = excluded.msg_count,
             last_received_at = MAX(senders.last_received_at, excluded.last_received_at)",
        params![account_id, addr, display_name, received_at],
    )?;
    Ok(())
}

impl SqliteStore {
    /// HUMAN-DOOR ONLY: rank senders for a typed fragment. The same tiers as
    /// `search_contacts`: a prefix match on the address or the display name
    /// sorts above a substring match, then by how much mail the sender has
    /// sent and how recently, then by address so the order is stable.
    ///
    /// AN EMPTY FRAGMENT IS NOT AN EMPTY ANSWER, unlike contacts: it lists the
    /// senders with the most mail. The reader has just typed `from:` and has
    /// not said who yet, and the people who write to you most are the likeliest
    /// answer; a menu that stays blank until the second keystroke reads as a
    /// menu that did not open. The cap on `limit` at the door keeps this from
    /// being a directory dump.
    ///
    /// A sender is offered only while at least one of their messages is
    /// something search could return: inbound, not spam, not sealed. The
    /// directory cannot know sealed at write time (see [`bump_sender_conn`]), so
    /// the EXISTS probe here is what keeps `from:` from suggesting an address
    /// whose search would come back empty, and keeps a sealed-only sender out
    /// of a surface sealed mail is structurally absent from.
    ///
    /// THE PROBE RUNS LAZILY, AFTER THE RANKING. Ranking is a LIKE and a sort
    /// over the whole directory and costs about a millisecond at 4k senders;
    /// the probe is an indexed lookup (`idx_messages_from`) per row, and a
    /// one-letter fragment matches nearly every row. Probing every candidate
    /// before the sort measured 6 ms at 4k senders against 2 ms for this shape,
    /// and the gap grows with the mailbox. So the inner query ranks and the
    /// OUTER query probes, stopping as soon as `limit` rows survive: SQLite runs
    /// the inner SELECT as a co-routine (the inner LIMIT is what stops it being
    /// flattened into one query, which would put the probe back before the
    /// sort) and streams ranked rows out in order. The inner LIMIT is also the
    /// probe budget, 25 candidates per row asked for: a mailbox where more than
    /// 24 of every 25 senders have nothing but sealed mail is not a mailbox.
    pub fn search_senders(
        &self,
        account_id: AccountId,
        q: &str,
        limit: u32,
    ) -> Result<Vec<SenderEntry>> {
        let q = q.trim();
        // LIKE metacharacters in the fragment are literal text to the user. An
        // empty fragment escapes to an empty string, so both patterns below
        // become `%`, which every row matches: the prefix tier is then a tie
        // for all and volume decides, which is exactly the empty-fragment
        // listing described above.
        let escaped = q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let contains = format!("%{escaped}%");
        let prefix = format!("{escaped}%");
        let budget = limit.saturating_mul(25);

        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT s.addr, s.display_name, s.msg_count, s.last_received_at
             FROM (SELECT s.account_id, s.addr, s.display_name, s.msg_count,
                          s.last_received_at
                   FROM senders s
                   WHERE s.account_id = ?1
                     AND (s.addr LIKE ?2 ESCAPE '\\'
                          OR COALESCE(s.display_name, '') LIKE ?2 ESCAPE '\\')
                   ORDER BY (s.addr LIKE ?3 ESCAPE '\\'
                             OR COALESCE(s.display_name, '') LIKE ?3 ESCAPE '\\') DESC,
                            s.msg_count DESC,
                            s.last_received_at DESC,
                            s.addr ASC
                   LIMIT ?5) s
             WHERE EXISTS (SELECT 1 FROM messages m
                           LEFT JOIN triage t ON t.message_id = m.id
                           WHERE m.account_id = s.account_id AND m.from_addr = s.addr
                             AND m.is_sent = 0 AND m.is_spam = 0
                             AND COALESCE(t.sensitivity, 'normal') != 'sealed')
             LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(params![account_id, contains, prefix, limit, budget], |r| {
                Ok(SenderEntry {
                    addr: r.get(0)?,
                    display_name: r.get(1)?,
                    msg_count: r.get(2)?,
                    last_received_at: dt(r, 3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
