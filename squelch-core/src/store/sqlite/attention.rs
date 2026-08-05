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
    })
}

/// Membership test for the `standing` band, written against the `triage t` /
/// `messages m` join that both band sites share. It lives in ONE place because
/// the list query and the header's count must agree row for row, or the sitrep
/// header contradicts the list it heads.
///
/// STANDING IS A PROPERTY, NOT A TIMESTAMP: the band is mail owed the user's
/// attention — a dated obligation (`past_due`/`deadline`), or live
/// correspondence: a thread the user has written in, or a sender the user has
/// written to. A dateless "can you send me the form?" from a real correspondent
/// is exactly as owed as a bill, and the surfacing clock must never rotate it
/// out. Because this is a definition over stored rows, widening it is
/// retroactive: mail already triaged joins the band on the next read.
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
/// be assumed normalized.
const STANDING_BAND: &str = "(t.tier IN ('past_due','deadline')
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
        //   standing = dated obligation OR live correspondence, not yet done
        //              (see STANDING_BAND for the definition and its limits)
        //   new      = surfaced_at IS NULL AND status != 'done'
        //   open     = status = 'open'
        // The `status != 'done'` on `new` keeps AUTO-RESOLVED receipts out of the
        // band — a receipt is a record, not new inbox clutter.
        let mut where_sql = String::from(
            "WHERE t.account_id = ?1
               AND t.sensitivity != 'sealed'
               AND m.is_sent = 0
               AND m.received_at >= ?2
               AND t.importance >= ?3",
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
        // The `open` band is the aging one: age*importance floats
        // long-unresolved-and-important items, computed in SQL via julianday so
        // the ordering stays server-side. Other bands sort by importance. The
        // same expression orders WITHIN a thread (below) and BETWEEN the
        // representatives, so the row shown is the one the sort would have put
        // first anyway.
        let (inner_order, outer_order) = if band == Some(SitrepBand::Open) {
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
                      ROW_NUMBER() OVER (
                          PARTITION BY COALESCE(NULLIF(m.thread_id, ''), 'msg-' || m.id)
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
            // HUMAN DOOR: the two extra columns the agent door never sees. A NULL
            // or malformed reasons blob yields None — one bad row must never fail
            // the whole updates read.
            update.field_reasons = r
                .get::<_, Option<String>>(12)?
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            update.has_attachments = Some(r.get::<_, i64>(13)? != 0);
            // Blank is the same as absent: an empty display name must not
            // render as "" <addr>, which parses to a nameless sender anyway.
            update.from_name = r
                .get::<_, Option<String>>(14)?
                .filter(|n| !n.trim().is_empty());
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
        let n = match status {
            AttentionStatus::Done => conn.execute(
                "UPDATE triage
                 SET status = ?1, resolved_at = ?2
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
        // the `band` query on attention_updates, or header and list disagree —
        // including its one-row-per-thread collapse, hence DISTINCT thread keys
        // (blank thread_id falls back to the message id, same as the list),
        // and its received_at window, hence `bands_since` (the list's default
        // min importance is 0, a no-op, so it is not mirrored here).
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
                         AND m.received_at >= ?2) t"
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
