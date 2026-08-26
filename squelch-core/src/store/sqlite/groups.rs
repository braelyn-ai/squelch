//! Send groups: named audiences the user addresses as one, their membership,
//! and the two-source history behind "what have I sent these people".
//!
//! HUMAN-DOOR ONLY (`/client/groups`). The agent door never learns this table
//! exists — who the user talks to as a bloc is not something /mcp was handed.
//!
//! Every method here is an inherent `pub fn` rather than a [`crate::store::Store`]
//! trait method, exactly like `contacts::search_contacts`: squelch-api reaches
//! the concrete `SqliteStore` through `store_call`, and putting human-door-only
//! reads on the shared trait would put them one careless call away from the
//! agent door.

use super::*;

/// The most mailboxes one group may hold. Gmail's per-message recipient ceiling
/// is 100, so a `to`/`bcc` group larger than this could not be sent in one
/// message at all — and an `individual` group that big is a mailing list the
/// user should be running somewhere else.
pub const MAX_GROUP_MEMBERS: usize = 100;

/// The lookup key derived from a display name: lowercased, with every run of
/// whitespace collapsed to one space, trimmed. `"  Preseed   Investors "` and
/// `"preseed investors"` are the same group, which is what stops a second
/// "Preseed Investors" being created next to the first one.
pub fn group_slug(name: &str) -> String {
    name.split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize one member address for storage: trimmed and lowercased, with any
/// `Display Name <addr>` wrapper reduced to the address inside it.
///
/// The composer sends what its pills hold, and a pill minted from a contact
/// suggestion can carry the display form. Storing that verbatim would break the
/// membership key (`bob@x` and `Bob <bob@x>` as two members) AND the history
/// join, which matches against `message_recipients`' bare lowercased addresses.
fn normalize_addr(raw: &str) -> String {
    let trimmed = raw.trim();
    let inner = match (trimmed.rfind('<'), trimmed.rfind('>')) {
        (Some(open), Some(close)) if close > open + 1 => &trimmed[open + 1..close],
        _ => trimmed,
    };
    inner.trim().to_ascii_lowercase()
}

/// A member write as it arrives from the door, before normalization.
#[derive(Debug, Clone)]
pub struct NewGroupMember {
    pub addr: String,
    pub display_name: Option<String>,
}

/// Where one recipient of a group send has got to.
///
/// `Pending` is what makes the history row double as the progress indicator: a
/// fan-out writes every recipient pending up front, then settles them one at a
/// time, so "3 of 12" climbs on a plain re-read and no separate progress channel
/// has to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSendStatus {
    Pending,
    Sent,
    Failed,
}

impl GroupSendStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }
}

/// One recipient of a recorded group send, as the send path reports it.
#[derive(Debug, Clone)]
pub struct GroupSendRecipient {
    pub addr: String,
    /// The local id of this recipient's echoed copy, when one landed.
    pub message_id: Option<i64>,
    pub status: GroupSendStatus,
    /// Redacted reason, for a failed recipient only.
    pub error: Option<String>,
}

/// Validate and normalize a membership write. Returns the addresses in input
/// order, deduplicated, with empties dropped.
fn prepare_members(members: &[NewGroupMember]) -> Result<Vec<(String, Option<String>)>> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for m in members {
        let addr = normalize_addr(&m.addr);
        if addr.is_empty() || seen.contains(&addr) {
            continue;
        }
        // A member that cannot be a recipient is a typo the user needs told
        // about NOW, not at send time when the mail is already half away. The
        // shape check is deliberately the same one the send path applies to a
        // header address: exactly one `@` with something either side of it.
        if !addr.contains('@') || addr.starts_with('@') || addr.ends_with('@') {
            return Err(CoreError::InvalidInput(format!(
                "\"{}\" is not an email address",
                m.addr.trim()
            )));
        }
        seen.push(addr.clone());
        let name = m
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        out.push((addr, name));
    }
    if out.len() > MAX_GROUP_MEMBERS {
        return Err(CoreError::InvalidInput(format!(
            "a group holds at most {MAX_GROUP_MEMBERS} addresses (this one has {})",
            out.len()
        )));
    }
    Ok(out)
}

/// Map the `UNIQUE(account_id, slug)` failure into something the user can act
/// on. Without this it reaches squelch-api as a raw sqlite error and collapses
/// to a 500, when what happened is that they already have a group by that name.
fn map_slug_conflict(e: rusqlite::Error) -> CoreError {
    match &e {
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            CoreError::InvalidInput("a group with that name already exists".into())
        }
        _ => CoreError::from(e),
    }
}

/// The SEALED-MAIL guard every sent-mail read carries, as a SQL fragment.
///
/// Lifted verbatim in shape from `messages::sent_listing`, and fail-closed for
/// the same two reasons: the INNER JOIN on `triage` excludes a sent row whose
/// triage row is missing (a broken row, not an untriaged one — sent mail always
/// gets its triage row in the same transaction), and the `NOT EXISTS` excludes
/// any message in a thread with ANY sealed sighting, because seal detection is
/// per-message and the user's own reply in a sealed thread commits as 'normal'.
///
/// A group history is a sent-mail listing like any other, so it inherits both.
const SEALED_GUARD: &str = "AND t.sensitivity != 'sealed'
     AND NOT EXISTS (
         SELECT 1 FROM messages m2
         JOIN triage t2 ON t2.message_id = m2.id
         WHERE m2.account_id = m.account_id
           AND m2.thread_id = m.thread_id
           AND t2.sensitivity = 'sealed')";

/// Read receipts recorded against one message, as a SQL scalar subquery. The
/// join through `send_trackers` is what scopes the count to this account —
/// `message_opens` carries no account of its own.
const OPENS_SUBQUERY: &str = "(SELECT COUNT(*) FROM message_opens o
      JOIN send_trackers st ON st.token = o.token
      WHERE st.account_id = m.account_id AND st.message_id = m.id)";

impl SqliteStore {
    /// Replace the normalized recipient index for one message.
    ///
    /// DELETE-then-INSERT rather than an upsert: a re-fetch can legitimately
    /// change the recipient set (a corrected parse, a backfill arriving after a
    /// partial one), and leaving a stale address behind would attribute a
    /// message to a group it never went to.
    ///
    /// Takes an explicit connection handle so the ingest path can call it inside
    /// the transaction that writes the message itself.
    pub(super) fn sync_message_recipients_conn(
        conn: &Connection,
        account_id: AccountId,
        message_id: i64,
        addrs: &[String],
    ) -> Result<()> {
        conn.execute(
            "DELETE FROM message_recipients WHERE account_id = ?1 AND message_id = ?2",
            params![account_id, message_id],
        )?;
        let mut seen: Vec<String> = Vec::new();
        for addr in addrs {
            let lc = addr.trim().to_ascii_lowercase();
            if lc.is_empty() || seen.contains(&lc) {
                continue;
            }
            seen.push(lc.clone());
            conn.execute(
                "INSERT OR IGNORE INTO message_recipients(account_id, message_id, addr)
                 VALUES(?1,?2,?3)",
                params![account_id, message_id, lc],
            )?;
        }
        Ok(())
    }

    // --- groups CRUD ---------------------------------------------------------

    /// Every group, most recently addressed first, then alphabetically. Carries
    /// member COUNTS but not membership: see [`SendGroup::members`].
    pub fn list_send_groups(&self, account_id: AccountId) -> Result<Vec<SendGroup>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT g.id, g.name, g.slug, g.mode, g.note, g.created_at, g.updated_at,
                    (SELECT COUNT(*) FROM group_members gm WHERE gm.group_id = g.id),
                    (SELECT MAX(gs.sent_at) FROM group_sends gs
                     WHERE gs.account_id = g.account_id AND gs.group_id = g.id)
             FROM send_groups g
             WHERE g.account_id = ?1
             ORDER BY g.name COLLATE NOCASE ASC",
        )?;
        let out = stmt
            .query_map(params![account_id], |r| {
                Ok(SendGroup {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    slug: r.get(2)?,
                    mode: GroupMode::parse(&r.get::<_, String>(3)?),
                    note: r.get(4)?,
                    created_at: dt(r, 5)?,
                    updated_at: dt(r, 6)?,
                    member_count: r.get(7)?,
                    members: Vec::new(),
                    last_sent_at: dt_opt(r, 8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// One group WITH its membership. `None` for an id this account does not
    /// own, which is what makes the door's 404 cover both "gone" and "not
    /// yours".
    pub fn get_send_group(&self, account_id: AccountId, id: i64) -> Result<Option<SendGroup>> {
        let conn = self.lock()?;
        let group = conn
            .query_row(
                "SELECT g.id, g.name, g.slug, g.mode, g.note, g.created_at, g.updated_at,
                        (SELECT COUNT(*) FROM group_members gm WHERE gm.group_id = g.id),
                        (SELECT MAX(gs.sent_at) FROM group_sends gs
                         WHERE gs.account_id = g.account_id AND gs.group_id = g.id)
                 FROM send_groups g
                 WHERE g.account_id = ?1 AND g.id = ?2",
                params![account_id, id],
                |r| {
                    Ok(SendGroup {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        slug: r.get(2)?,
                        mode: GroupMode::parse(&r.get::<_, String>(3)?),
                        note: r.get(4)?,
                        created_at: dt(r, 5)?,
                        updated_at: dt(r, 6)?,
                        member_count: r.get(7)?,
                        members: Vec::new(),
                        last_sent_at: dt_opt(r, 8)?,
                    })
                },
            )
            .optional()?;
        let Some(mut group) = group else {
            return Ok(None);
        };
        group.members = Self::members_of(&conn, id)?;
        Ok(Some(group))
    }

    fn members_of(conn: &Connection, group_id: i64) -> Result<Vec<GroupMember>> {
        let mut stmt = conn.prepare(
            "SELECT addr, display_name FROM group_members
             WHERE group_id = ?1
             ORDER BY COALESCE(display_name, addr) COLLATE NOCASE ASC",
        )?;
        let out = stmt
            .query_map(params![group_id], |r| {
                Ok(GroupMember {
                    addr: r.get(0)?,
                    display_name: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// Composer autocomplete: groups whose name matches a typed fragment.
    /// Prefix matches sort above substring matches, then alphabetically —
    /// mirroring `search_contacts`, because the two lists render as one menu.
    pub fn search_send_groups(
        &self,
        account_id: AccountId,
        q: &str,
        limit: u32,
    ) -> Result<Vec<SendGroup>> {
        let q = q.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        // LIKE metacharacters in the fragment are literal text to the user.
        let escaped = q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
            .to_ascii_lowercase();
        let contains = format!("%{escaped}%");
        let prefix = format!("{escaped}%");

        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT g.id, g.name, g.slug, g.mode, g.note, g.created_at, g.updated_at,
                    (SELECT COUNT(*) FROM group_members gm WHERE gm.group_id = g.id),
                    (SELECT MAX(gs.sent_at) FROM group_sends gs
                     WHERE gs.account_id = g.account_id AND gs.group_id = g.id)
             FROM send_groups g
             WHERE g.account_id = ?1 AND g.slug LIKE ?2 ESCAPE '\\'
             ORDER BY (g.slug LIKE ?3 ESCAPE '\\') DESC, g.name COLLATE NOCASE ASC
             LIMIT ?4",
        )?;
        let out = stmt
            .query_map(params![account_id, contains, prefix, limit], |r| {
                Ok(SendGroup {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    slug: r.get(2)?,
                    mode: GroupMode::parse(&r.get::<_, String>(3)?),
                    note: r.get(4)?,
                    created_at: dt(r, 5)?,
                    updated_at: dt(r, 6)?,
                    member_count: r.get(7)?,
                    members: Vec::new(),
                    last_sent_at: dt_opt(r, 8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// Resolve a group by its slug — how the send path turns the composer's
    /// group token back into an audience.
    pub fn send_group_by_slug(&self, account_id: AccountId, slug: &str) -> Result<Option<i64>> {
        let conn = self.lock()?;
        let id = conn
            .query_row(
                "SELECT id FROM send_groups WHERE account_id = ?1 AND slug = ?2",
                params![account_id, group_slug(slug)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// The addresses a group expands to, in the order the member list renders.
    pub fn send_group_addrs(&self, account_id: AccountId, group_id: i64) -> Result<Vec<String>> {
        let conn = self.lock()?;
        // account_id is in the predicate, not just the caller: an id from a
        // request body must never reach another account's membership.
        let mut stmt = conn.prepare(
            "SELECT addr FROM group_members
             WHERE account_id = ?1 AND group_id = ?2
             ORDER BY COALESCE(display_name, addr) COLLATE NOCASE ASC",
        )?;
        let out = stmt
            .query_map(params![account_id, group_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }

    /// Create a group and its membership in ONE transaction — a group that
    /// committed without its members would read as an empty audience the user
    /// then has to notice and repair.
    pub fn create_send_group(
        &self,
        account_id: AccountId,
        name: &str,
        mode: GroupMode,
        note: &str,
        members: &[NewGroupMember],
    ) -> Result<i64> {
        let name = name.trim();
        let slug = group_slug(name);
        if slug.is_empty() {
            return Err(CoreError::InvalidInput("a group needs a name".into()));
        }
        let members = prepare_members(members)?;
        let now = Utc::now().to_rfc3339();

        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO send_groups(account_id, name, slug, mode, note, created_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?6)",
            params![account_id, name, slug, mode.as_str(), note.trim(), now],
        )
        .map_err(map_slug_conflict)?;
        let id = tx.last_insert_rowid();
        Self::write_members(&tx, account_id, id, &members, &now)?;
        tx.commit()?;
        Ok(id)
    }

    /// Rename / re-mode / re-populate a group. `members` REPLACES the membership
    /// wholesale: the editor sends the list it is showing, and a diff computed
    /// client-side would be one dropped request away from silently keeping
    /// someone the user removed.
    ///
    /// `false` means no such group for this account.
    pub fn update_send_group(
        &self,
        account_id: AccountId,
        id: i64,
        name: &str,
        mode: GroupMode,
        note: &str,
        members: &[NewGroupMember],
    ) -> Result<bool> {
        let name = name.trim();
        let slug = group_slug(name);
        if slug.is_empty() {
            return Err(CoreError::InvalidInput("a group needs a name".into()));
        }
        let members = prepare_members(members)?;
        let now = Utc::now().to_rfc3339();

        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let n = tx
            .execute(
                "UPDATE send_groups SET name = ?3, slug = ?4, mode = ?5, note = ?6, updated_at = ?7
                 WHERE account_id = ?1 AND id = ?2",
                params![account_id, id, name, slug, mode.as_str(), note.trim(), now],
            )
            .map_err(map_slug_conflict)?;
        if n == 0 {
            return Ok(false);
        }
        tx.execute("DELETE FROM group_members WHERE group_id = ?1", params![id])?;
        Self::write_members(&tx, account_id, id, &members, &now)?;
        tx.commit()?;
        Ok(true)
    }

    fn write_members(
        tx: &Connection,
        account_id: AccountId,
        group_id: i64,
        members: &[(String, Option<String>)],
        now: &str,
    ) -> Result<()> {
        for (addr, display_name) in members {
            tx.execute(
                "INSERT INTO group_members(group_id, account_id, addr, display_name, added_at)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(group_id, addr) DO UPDATE SET
                     display_name = excluded.display_name",
                params![group_id, account_id, addr, display_name, now],
            )?;
        }
        Ok(())
    }

    /// Delete a group. Its membership goes with it (ON DELETE CASCADE); its
    /// `group_sends` rows DO NOT — see the schema comment. Deleting a group says
    /// something about who you will address next, not about what you already
    /// sent.
    pub fn delete_send_group(&self, account_id: AccountId, id: i64) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM send_groups WHERE account_id = ?1 AND id = ?2",
            params![account_id, id],
        )?;
        Ok(n > 0)
    }

    // --- recorded sends ------------------------------------------------------

    /// Record one group send and its per-recipient outcome, in one transaction.
    ///
    /// `recipients` is the SNAPSHOT: its length is what the history's
    /// denominator will say forever, so a later change to membership cannot
    /// rewrite what "9 of 12" meant.
    pub fn record_group_send(
        &self,
        account_id: AccountId,
        group_id: i64,
        subject: &str,
        mode: GroupMode,
        sent_at: DateTime<Utc>,
        recipients: &[GroupSendRecipient],
    ) -> Result<i64> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO group_sends(account_id, group_id, subject, mode, sent_at, recipients)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                account_id,
                group_id,
                subject,
                mode.as_str(),
                sent_at.to_rfc3339(),
                recipients.len() as i64,
            ],
        )?;
        let id = tx.last_insert_rowid();
        for r in recipients {
            tx.execute(
                "INSERT INTO group_send_recipients(
                     group_send_id, account_id, addr, message_id, status, error)
                 VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(group_send_id, addr) DO UPDATE SET
                     message_id = excluded.message_id,
                     status = excluded.status,
                     error = excluded.error",
                params![
                    id,
                    account_id,
                    normalize_addr(&r.addr),
                    r.message_id,
                    r.status.as_str(),
                    r.error,
                ],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Settle one recipient of a recorded send: its outcome, and the local id of
    /// its echoed copy when one landed.
    ///
    /// A second write rather than part of [`Self::record_group_send`] because a
    /// fan-out records the whole audience as pending BEFORE it sends anything —
    /// a batch that crashed mid-flight must leave a record of who it had not
    /// reached yet, not a record of nobody.
    ///
    /// `message_id` is only ever written FORWARD: `COALESCE(?5, message_id)`, so
    /// the later echo-linking call does not blank the id an earlier one set.
    pub fn set_group_send_result(
        &self,
        account_id: AccountId,
        group_send_id: i64,
        addr: &str,
        status: GroupSendStatus,
        message_id: Option<i64>,
        error: Option<&str>,
    ) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE group_send_recipients
             SET status = ?4, message_id = COALESCE(?5, message_id), error = ?6
             WHERE account_id = ?1 AND group_send_id = ?2 AND addr = ?3",
            params![
                account_id,
                group_send_id,
                normalize_addr(addr),
                status.as_str(),
                message_id,
                error,
            ],
        )?;
        Ok(n > 0)
    }

    /// Mark every still-pending recipient of a send failed.
    ///
    /// The crash guard: a fan-out that dies mid-flight (the daemon restarts, the
    /// task is dropped) would otherwise leave its remainder pending forever, and
    /// the history would read as a send still in progress months later. Called on
    /// the way out of the job, and on open for anything a previous run stranded.
    pub fn fail_pending_group_sends(&self, account_id: AccountId, reason: &str) -> Result<usize> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE group_send_recipients SET status = 'failed', error = ?2
             WHERE account_id = ?1 AND status = 'pending'",
            params![account_id, reason],
        )?;
        Ok(n)
    }

    // --- history -------------------------------------------------------------

    /// A group's send history, newest first: the RECORDED sends unioned with
    /// mail DERIVED from matching stored recipients against current membership.
    ///
    /// Both halves are needed and neither is redundant. Recorded rows are exact
    /// and know about fan-outs (N messages, one entry) and failures. Derived
    /// rows are the only thing that can speak about the past — a group is
    /// created for people the user has been emailing for a year, and a history
    /// that only knew its own sends would open empty on the day it shipped.
    ///
    /// A recorded send ALSO matches the derived query, so the derived half
    /// excludes any message a `group_send_recipients` row already claims.
    /// Merging happens in Rust rather than as a SQL UNION: the two halves carry
    /// genuinely different columns (a snapshot denominator and a failure count
    /// on one side, a live member match on the other), and padding each to the
    /// other's shape would make both unreadable.
    pub fn group_history(
        &self,
        account_id: AccountId,
        group_id: i64,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<GroupHistoryEntry>> {
        // Each half is read to the depth the merged page could need, then the
        // merge is sliced: an entry at merged position `offset + limit` can come
        // entirely from either side.
        let depth = (limit as i64).saturating_add(offset as i64);
        let conn = self.lock()?;

        let mut entries = Self::recorded_history(&conn, account_id, group_id, depth)?;
        entries.extend(Self::derived_history(&conn, account_id, group_id, depth)?);
        // Newest first, with the id as the tiebreak so a page boundary is stable
        // across calls when two sends share a timestamp.
        entries.sort_by(|a, b| {
            b.sent_at
                .cmp(&a.sent_at)
                .then_with(|| b.message_id.cmp(&a.message_id))
        });
        Ok(entries
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect())
    }

    fn recorded_history(
        conn: &Connection,
        account_id: AccountId,
        group_id: i64,
        depth: i64,
    ) -> Result<Vec<GroupHistoryEntry>> {
        // The representative message is the LOWEST recipient message_id: for a
        // to/bcc send every row names the same one, and for a fan-out the batch
        // has no single message, so the first recipient's copy is what the
        // history row opens. Its thread and snippet come from that message,
        // which may be absent when no echo ever landed.
        let mut stmt = conn.prepare(
            "SELECT gs.id, gs.subject, gs.mode, gs.sent_at, gs.recipients,
                    (SELECT COUNT(*) FROM group_send_recipients r
                     WHERE r.group_send_id = gs.id AND r.status = 'sent'),
                    (SELECT COUNT(*) FROM group_send_recipients r
                     WHERE r.group_send_id = gs.id AND r.status = 'failed'),
                    (SELECT COUNT(*) FROM group_send_recipients r
                     WHERE r.group_send_id = gs.id AND r.status = 'pending'),
                    (SELECT MIN(r.message_id) FROM group_send_recipients r
                     WHERE r.group_send_id = gs.id AND r.message_id IS NOT NULL),
                    (SELECT COUNT(*) FROM group_send_recipients r
                     JOIN send_trackers st
                       ON st.account_id = gs.account_id AND st.message_id = r.message_id
                     JOIN message_opens o ON o.token = st.token
                     WHERE r.group_send_id = gs.id)
             FROM group_sends gs
             WHERE gs.account_id = ?1 AND gs.group_id = ?2
             ORDER BY gs.sent_at DESC, gs.id DESC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![account_id, group_id, depth], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    dt(r, 3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, i64>(9)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut out = Vec::with_capacity(rows.len());
        for (id, subject, mode, sent_at, snapshot, reached, failed, pending, message_id, opens) in
            rows
        {
            // The snippet is the echoed message's, when there is one to read.
            let (thread_id, snippet) = match message_id {
                Some(mid) => conn
                    .query_row(
                        "SELECT thread_id, snippet FROM messages
                         WHERE account_id = ?1 AND id = ?2",
                        params![account_id, mid],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()?
                    .map(|(t, s)| (Some(t), s))
                    .unwrap_or((None, String::new())),
                None => (None, String::new()),
            };
            out.push(GroupHistoryEntry {
                group_send_id: Some(id),
                message_id,
                thread_id,
                subject,
                snippet,
                sent_at,
                mode: GroupMode::parse(&mode),
                reached,
                group_size: snapshot,
                failed,
                pending,
                opens,
            });
        }
        Ok(out)
    }

    fn derived_history(
        conn: &Connection,
        account_id: AccountId,
        group_id: i64,
        depth: i64,
    ) -> Result<Vec<GroupHistoryEntry>> {
        // The denominator for a derived row is the group AS IT IS NOW: nothing
        // recorded what it was when this mail went out, and inventing a
        // historical size would be a claim the data cannot support.
        let group_size: i64 = conn.query_row(
            "SELECT COUNT(*) FROM group_members WHERE account_id = ?1 AND group_id = ?2",
            params![account_id, group_id],
            |r| r.get(0),
        )?;
        if group_size == 0 {
            return Ok(Vec::new());
        }

        let sql = format!(
            "SELECT m.id, m.thread_id, m.subject, m.snippet, m.received_at,
                    COUNT(DISTINCT mr.addr) AS reached,
                    {OPENS_SUBQUERY} AS opens
             FROM messages m
             JOIN triage t ON t.message_id = m.id
             JOIN message_recipients mr
               ON mr.account_id = m.account_id AND mr.message_id = m.id
             JOIN group_members gm
               ON gm.account_id = m.account_id AND gm.group_id = ?2 AND gm.addr = mr.addr
             WHERE m.account_id = ?1
               AND m.is_sent = 1
               {SEALED_GUARD}
               AND NOT EXISTS (
                   SELECT 1 FROM group_send_recipients gsr
                   JOIN group_sends gs ON gs.id = gsr.group_send_id
                   WHERE gs.account_id = m.account_id
                     AND gs.group_id = ?2
                     AND gsr.message_id = m.id)
             GROUP BY m.id
             ORDER BY m.received_at DESC, m.id DESC
             LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let out = stmt
            .query_map(params![account_id, group_id, depth], |r| {
                Ok(GroupHistoryEntry {
                    group_send_id: None,
                    message_id: Some(r.get(0)?),
                    thread_id: Some(r.get(1)?),
                    subject: r.get(2)?,
                    snippet: r.get(3)?,
                    sent_at: dt(r, 4)?,
                    // A derived row cannot know how the mail was addressed: the
                    // To/Cc split is not stored, and Bcc is in no header at all.
                    // It reports the GROUP'S mode, which is what the reader is
                    // looking at the history of.
                    mode: GroupMode::To,
                    reached: r.get(5)?,
                    group_size,
                    failed: 0,
                    // Derived rows describe mail that already went; there is
                    // nothing in flight to be pending about.
                    pending: 0,
                    opens: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_folds_case_and_collapses_whitespace() {
        assert_eq!(group_slug("  Preseed   Investors "), "preseed investors");
        assert_eq!(group_slug("preseed investors"), "preseed investors");
        assert_eq!(group_slug("\tSeed\nRound  "), "seed round");
        assert_eq!(group_slug("   "), "");
    }

    #[test]
    fn normalize_addr_unwraps_display_form() {
        assert_eq!(normalize_addr("Bob <BOB@x.com>"), "bob@x.com");
        assert_eq!(normalize_addr("  bob@x.com "), "bob@x.com");
        assert_eq!(normalize_addr("\"Doe, Jane\" <j@x.com>"), "j@x.com");
        // No closing bracket: nothing to unwrap, so the text stands as written
        // and the shape check downstream is what rejects it.
        assert_eq!(normalize_addr("<bob@x.com"), "<bob@x.com");
    }

    #[test]
    fn members_dedupe_across_display_forms() {
        let members = prepare_members(&[
            NewGroupMember {
                addr: "Bob <bob@x.com>".into(),
                display_name: Some("Bob".into()),
            },
            NewGroupMember {
                addr: "BOB@x.com".into(),
                display_name: None,
            },
        ])
        .unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].0, "bob@x.com");
    }

    #[test]
    fn members_reject_non_addresses() {
        let err = prepare_members(&[NewGroupMember {
            addr: "not an address".into(),
            display_name: None,
        }])
        .unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)));
    }

    #[test]
    fn members_are_capped() {
        let many: Vec<NewGroupMember> = (0..=MAX_GROUP_MEMBERS)
            .map(|i| NewGroupMember {
                addr: format!("p{i}@x.com"),
                display_name: None,
            })
            .collect();
        assert!(matches!(
            prepare_members(&many).unwrap_err(),
            CoreError::InvalidInput(_)
        ));
    }
}
