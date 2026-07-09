//! SQLite-backed [`Store`] implementation.
//!
//! rusqlite is synchronous, so the `Connection` is wrapped in a `Mutex` and the
//! trait is implemented synchronously. See `store/mod.rs` for rationale.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{CoreError, Result};
use crate::store::{
    NewAuditEntry, SealedBody, SealedMessage, SitrepBand, Stage2Applied, Stage2Queued, Stage2Usage,
    Store, SyncState, TriagedMessage,
};
use crate::types::{
    AccountId, AttentionStatus, AttentionUpdate, AuditEntry, BandCounts, Deadline, Disposition,
    NewMessage, SanitizedMessage, SearchHit, SenderRule, Sensitivity, StoreStats, ThreadView, Tier,
    Update,
};

const SCHEMA: &str = include_str!("schema.sql");

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (or create) a store at `path`, applying the schema.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an in-memory store (tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| CoreError::Other(anyhow::anyhow!("store mutex poisoned")))
    }

    /// Convenience for tests/other crates: create an account, return its id.
    pub fn ensure_account(&self, email: &str) -> Result<AccountId> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO accounts(email, created_at) VALUES(?1, ?2)
             ON CONFLICT(email) DO NOTHING",
            params![email, Utc::now().to_rfc3339()],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM accounts WHERE email = ?1",
            params![email],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// HUMAN-DOOR ACTION SUPPORT (squelch-api only): resolve a local message id
    /// to the Gmail ids + headers an action needs (archive/label/send).
    ///
    /// SECURITY: this INTENTIONALLY excludes `sensitivity = 'sealed'` rows in
    /// SQL, so an action can never target a sealed message (NotFound is returned
    /// for a missing OR sealed message, keeping the two indistinguishable). It is
    /// read-only and is never called by sync/triage/MCP. It does not touch bodies.
    pub fn action_message_ref(
        &self,
        account_id: AccountId,
        message_id: i64,
    ) -> Result<crate::store::ActionMessageRef> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT m.id, m.gmail_msg_id, m.thread_id, m.from_addr, m.from_name, m.subject
                 FROM messages m
                 JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2 AND t.sensitivity != 'sealed'",
                params![account_id, message_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let (id, gmail_msg_id, thread_id, from_addr, from_name, subject) =
            row.ok_or(CoreError::NotFound)?;
        Ok(crate::store::ActionMessageRef {
            id,
            account_id,
            gmail_msg_id,
            thread_id,
            from_addr,
            from_name,
            subject,
        })
    }

    /// Test/local helper: write a triage row for a message. Real triage is
    /// written by the triage pipeline; this keeps the store self-contained.
    #[allow(clippy::too_many_arguments)]
    pub fn set_triage(
        &self,
        message_id: i64,
        account_id: AccountId,
        importance: u8,
        tier: Tier,
        sensitivity: crate::types::Sensitivity,
        sealed_kind: Option<crate::types::SealedKind>,
        one_line: &str,
        reason: &str,
        deadline: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO triage(message_id, account_id, importance, tier, sensitivity,
                 sealed_kind, one_line, reason, deadline, created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(message_id) DO UPDATE SET
                 importance=excluded.importance, tier=excluded.tier,
                 sensitivity=excluded.sensitivity, sealed_kind=excluded.sealed_kind,
                 one_line=excluded.one_line, reason=excluded.reason,
                 deadline=excluded.deadline",
            params![
                message_id,
                account_id,
                importance as i64,
                tier.as_str(),
                sensitivity.as_str(),
                sealed_kind.map(|k| k.as_str()),
                one_line,
                reason,
                deadline.map(|d| d.to_rfc3339()),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }
}

/// Upsert a message + FTS + Sent-derived contacts against an explicit
/// connection/transaction handle. Shared by [`SqliteStore::upsert_message`] and
/// the transactional [`Store::ingest_message`] path so both stay in sync.
fn upsert_message_conn(conn: &Connection, msg: &NewMessage) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages(account_id, gmail_msg_id, thread_id, from_addr, from_name,
             subject, received_at, snippet, body, is_sent)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(account_id, gmail_msg_id) DO UPDATE SET
             thread_id=excluded.thread_id, from_addr=excluded.from_addr,
             from_name=excluded.from_name, subject=excluded.subject,
             received_at=excluded.received_at, snippet=excluded.snippet,
             body=excluded.body, is_sent=excluded.is_sent",
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
            msg.is_sent as i64,
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

    // NOTE: contacts are NOT seeded here. Sent mail's From header is the user's
    // OWN address, so seeding from it produced exactly one bogus self-contact.
    // Contacts are instead seeded from the To/Cc recipients of Sent mail in
    // `ingest_message` (which carries the pre-filtered recipient list).
    Ok(id)
}

/// Seed the contacts table from the recipients of a Sent message. Each recipient
/// increments its `sent_count`. Addresses are already de-duplicated and stripped
/// of the account's own address at ingest, so no self-guard is needed here — but
/// we defensively skip empties. Received mail passes an empty list (no-op).
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

fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| CoreError::InvalidInput(format!("bad datetime {s:?}: {e}")))
}

impl Store for SqliteStore {
    fn upsert_message(&self, msg: &NewMessage) -> Result<i64> {
        let conn = self.lock()?;
        upsert_message_conn(&conn, msg)
    }

    fn ranked_updates(
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
        let rows = stmt.query_map(
            params![account_id, since.to_rfc3339(), min],
            |r| {
                let tier_s: String = r.get(2)?;
                let deadline_s: Option<String> = r.get(7)?;
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    tier_s,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    deadline_s,
                    r.get::<_, Option<i64>>(8)?,
                ))
            },
        )?;

        let mut out = Vec::new();
        for row in rows {
            let (id, thread_id, tier_s, importance, sender, one_line, reason, deadline_s, rule) =
                row?;
            let deadline = match deadline_s {
                Some(s) => Some(parse_dt(&s)?),
                None => None,
            };
            out.push(Update {
                id,
                thread_id,
                tier: Tier::parse(&tier_s).unwrap_or(Tier::Noise),
                importance: importance.clamp(0, 255) as u8,
                sender,
                one_line,
                reason,
                deadline,
                matched_rule: rule,
            });
        }
        Ok(out)
    }

    fn thread_view(&self, account_id: AccountId, thread_id: &str) -> Result<ThreadView> {
        let conn = self.lock()?;

        // SECURITY: if ANY message in this thread is sealed, treat the whole
        // thread as NotFound (indistinguishable from nonexistent). Also, a
        // thread with no visible messages is NotFound.
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
        let subject = subject.ok_or(CoreError::NotFound)?;

        let mut stmt = conn.prepare(
            "SELECT id, from_addr, from_name, received_at, body
             FROM messages
             WHERE account_id=?1 AND thread_id=?2
             ORDER BY received_at ASC",
        )?;
        let rows = stmt.query_map(params![account_id, thread_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (id, from_addr, from_name, received_at, body) = row?;
            messages.push(SanitizedMessage {
                id,
                from_addr,
                from_name,
                received_at: parse_dt(&received_at)?,
                content: body,
            });
        }
        if messages.is_empty() {
            return Err(CoreError::NotFound);
        }

        Ok(ThreadView {
            thread_id: thread_id.to_string(),
            subject,
            messages,
        })
    }

    fn deadlines(
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
        let rows = stmt.query_map(params![account_id, cutoff_ref], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<f64>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, String>(8)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, acct, message_id, kind, amount, currency, due_at, past_due, source) = row?;
            out.push(Deadline {
                id,
                account_id: acct,
                message_id,
                kind,
                amount,
                currency,
                due_at: parse_dt(&due_at)?,
                past_due: past_due != 0,
                source,
            });
        }
        Ok(out)
    }

    fn set_sender_rule(
        &self,
        account_id: AccountId,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<i64> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sender_rules(account_id, match_pattern, want_text, disposition, updated_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(account_id, match_pattern) DO UPDATE SET
                 want_text=excluded.want_text, disposition=excluded.disposition,
                 updated_at=excluded.updated_at",
            params![
                account_id,
                match_pattern,
                want_text,
                disposition.as_str(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM sender_rules WHERE account_id=?1 AND match_pattern=?2",
            params![account_id, match_pattern],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    fn update_sender_rule(
        &self,
        account_id: AccountId,
        id: i64,
        match_pattern: &str,
        want_text: &str,
        disposition: Disposition,
    ) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE sender_rules SET
                 match_pattern = ?3, want_text = ?4, disposition = ?5, updated_at = ?6
             WHERE account_id = ?1 AND id = ?2",
            params![
                account_id,
                id,
                match_pattern,
                want_text,
                disposition.as_str(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(n > 0)
    }

    fn list_sender_rules(&self, account_id: AccountId) -> Result<Vec<SenderRule>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, match_pattern, want_text, disposition, updated_at
             FROM sender_rules WHERE account_id=?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, acct, match_pattern, want_text, disposition, updated_at) = row?;
            out.push(SenderRule {
                id,
                account_id: acct,
                match_pattern,
                want_text,
                disposition: Disposition::parse(&disposition).unwrap_or(Disposition::Surface),
                updated_at: parse_dt(&updated_at)?,
            });
        }
        Ok(out)
    }

    fn ingest_message(&self, triaged: &TriagedMessage) -> Result<i64> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;

        // 1. Upsert the message row (+ FTS).
        let id = upsert_message_conn(&tx, &triaged.message)?;

        // 1b. Seed contacts from Sent-mail recipients (To/Cc), in the SAME
        //     transaction. `recipients` is empty for received mail and already
        //     excludes the account's own address.
        seed_contacts_conn(
            &tx,
            triaged.message.account_id,
            &triaged.recipients,
            &triaged.message.received_at.to_rfc3339(),
        )?;

        // 2. Write the triage row IN THE SAME TRANSACTION. For sealed mail this
        //    is the whole point: sensitivity='sealed' is committed atomically
        //    with the message so there is no window where it is queryable as
        //    normal mail. `model_used` stays NULL; combined with
        //    sensitivity='normal' that is the Stage-2 queue predicate for
        //    non-confident rows.
        let deadline_dt = triaged.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
        tx.execute(
            "INSERT INTO triage(message_id, account_id, importance, tier, sensitivity,
                 sealed_kind, one_line, reason, deadline, matched_rule_id, model_used, created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,NULL,?11)
             ON CONFLICT(message_id) DO UPDATE SET
                 importance=excluded.importance, tier=excluded.tier,
                 sensitivity=excluded.sensitivity, sealed_kind=excluded.sealed_kind,
                 one_line=excluded.one_line, reason=excluded.reason,
                 deadline=excluded.deadline, matched_rule_id=excluded.matched_rule_id",
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
                Utc::now().to_rfc3339(),
            ],
        )?;

        // 3. Deadlines: only ever present for non-sealed mail (Stage-1 does not
        //    run on sealed content). Replace any prior deadline for this message
        //    so re-ingest is idempotent.
        tx.execute(
            "DELETE FROM deadlines WHERE message_id=?1",
            params![id],
        )?;
        if triaged.sensitivity != Sensitivity::Sealed
            && let Some(d) = &triaged.deadline
        {
                tx.execute(
                    "INSERT INTO deadlines(account_id, message_id, kind, amount, currency,
                         due_at, past_due, source)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        triaged.message.account_id,
                        id,
                        d.kind,
                        d.amount,
                        d.currency,
                        d.due_at.to_rfc3339(),
                        d.past_due as i64,
                        d.source,
                    ],
                )?;
        }

        tx.commit()?;
        Ok(id)
    }

    fn is_known_contact(&self, account_id: AccountId, addr: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM contacts
             WHERE account_id=?1 AND addr=?2 COLLATE NOCASE AND sent_count > 0",
            params![account_id, addr],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    fn sync_state(&self, account_id: AccountId, mailbox: &str) -> Result<Option<SyncState>> {
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

    fn set_sync_state(
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

    fn sealed_messages(&self, account_id: AccountId) -> Result<Vec<SealedMessage>> {
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
        let rows = stmt.query_map(params![account_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (id, acct, thread_id, from_addr, subject, received_at, sealed_kind) = row?;
            out.push(SealedMessage {
                id,
                account_id: acct,
                thread_id,
                from_addr,
                subject,
                received_at: parse_dt(&received_at)?,
                sealed_kind,
            });
        }
        Ok(out)
    }

    fn search(
        &self,
        account_id: AccountId,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchHit>> {
        let conn = self.lock()?;
        // SECURITY: join triage and exclude sealed rows in SQL, exactly like
        // ranked_updates. A message with no triage row is treated as non-sealed
        // (LEFT JOIN) so freshly-ingested-but-untriaged mail is still findable,
        // but a sealed classification always hides the row.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.thread_id, m.from_addr, m.from_name, m.subject,
                    m.received_at, m.snippet
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             LEFT JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = ?1
               AND COALESCE(t.sensitivity, 'normal') != 'sealed'
               AND m.is_sent = 0
               AND messages_fts MATCH ?2
             ORDER BY rank
             LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(
            params![account_id, query, limit as i64, offset as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )?;

        let mut out = Vec::new();
        for row in rows {
            let (id, thread_id, from_addr, from_name, subject, received_at, snippet) = row?;
            out.push(SearchHit {
                id,
                thread_id,
                from_addr,
                from_name,
                subject,
                received_at: parse_dt(&received_at)?,
                snippet,
            });
        }
        Ok(out)
    }

    fn attention_updates(
        &self,
        account_id: AccountId,
        since: DateTime<Utc>,
        min_importance: Option<u8>,
        status: Option<AttentionStatus>,
        band: Option<SitrepBand>,
    ) -> Result<Vec<AttentionUpdate>> {
        let conn = self.lock()?;
        let min = min_importance.unwrap_or(0) as i64;

        // Base predicate mirrors ranked_updates (sealed excluded, sent excluded,
        // since/importance window). Band/status add clauses; the ORDER BY differs
        // for the `open` band (age*importance) — documented below.
        //
        // Band semantics:
        //   standing = tier IN ('past_due','deadline') AND status != 'done'
        //   new      = surfaced_at IS NULL
        //   open     = status = 'open'
        let mut sql = String::from(
            "SELECT m.id, m.thread_id, t.tier, t.importance, m.from_addr, t.one_line,
                    t.reason, t.deadline, t.matched_rule_id,
                    t.status, t.surfaced_at, t.resolved_at
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.account_id = ?1
               AND t.sensitivity != 'sealed'
               AND m.is_sent = 0
               AND m.received_at >= ?2
               AND t.importance >= ?3",
        );
        if let Some(s) = status {
            sql.push_str(match s {
                AttentionStatus::New => " AND t.status = 'new'",
                AttentionStatus::Open => " AND t.status = 'open'",
                AttentionStatus::Done => " AND t.status = 'done'",
            });
        }
        match band {
            Some(SitrepBand::Standing) => {
                sql.push_str(" AND t.tier IN ('past_due','deadline') AND t.status != 'done'");
            }
            Some(SitrepBand::New) => sql.push_str(" AND t.surfaced_at IS NULL"),
            Some(SitrepBand::Open) => sql.push_str(" AND t.status = 'open'"),
            None => {}
        }
        // The `open` band is the aging/escalating band: sort by age*importance so
        // long-unresolved-and-important items float. `age` is (now - received_at)
        // in seconds; we compute it in SQL via julianday so the ordering lives
        // server-side. Other bands keep the ranked_updates ordering.
        if band == Some(SitrepBand::Open) {
            sql.push_str(
                " ORDER BY (julianday(?4) - julianday(m.received_at)) * t.importance DESC,
                          m.received_at DESC",
            );
        } else {
            sql.push_str(" ORDER BY t.importance DESC, m.received_at DESC");
        }

        let now = Utc::now().to_rfc3339();
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &rusqlite::Row| {
            let tier_s: String = r.get(2)?;
            let deadline_s: Option<String> = r.get(7)?;
            let status_s: String = r.get(9)?;
            let surfaced_s: Option<String> = r.get(10)?;
            let resolved_s: Option<String> = r.get(11)?;
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                tier_s,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                deadline_s,
                r.get::<_, Option<i64>>(8)?,
                status_s,
                surfaced_s,
                resolved_s,
            ))
        };
        let rows = if band == Some(SitrepBand::Open) {
            stmt.query_map(params![account_id, since.to_rfc3339(), min, now], map_row)?
        } else {
            stmt.query_map(params![account_id, since.to_rfc3339(), min], map_row)?
        };

        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                thread_id,
                tier_s,
                importance,
                sender,
                one_line,
                reason,
                deadline_s,
                rule,
                status_s,
                surfaced_s,
                resolved_s,
            ) = row?;
            let deadline = match deadline_s {
                Some(s) => Some(parse_dt(&s)?),
                None => None,
            };
            let surfaced_at = match surfaced_s {
                Some(s) => Some(parse_dt(&s)?),
                None => None,
            };
            let resolved_at = match resolved_s {
                Some(s) => Some(parse_dt(&s)?),
                None => None,
            };
            out.push(AttentionUpdate {
                update: Update {
                    id,
                    thread_id,
                    tier: Tier::parse(&tier_s).unwrap_or(Tier::Noise),
                    importance: importance.clamp(0, 255) as u8,
                    sender,
                    one_line,
                    reason,
                    deadline,
                    matched_rule: rule,
                },
                status: AttentionStatus::parse(&status_s).unwrap_or(AttentionStatus::New),
                surfaced_at,
                resolved_at,
            });
        }
        Ok(out)
    }

    fn mark_surfaced(&self, account_id: AccountId, message_ids: &[i64]) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        let tx = conn.transaction()?;
        let mut first_surfaced = 0usize;
        {
            // Stamp surfaced_at only if NULL, and promote new->open. The
            // sensitivity guard means a sealed row is NEVER stamped, so it can
            // never leak into a "new since last check" delta. Idempotent: a
            // second call finds surfaced_at already set and changes nothing.
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

    fn set_attention_status(
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
        let n = conn.execute(
            "UPDATE triage
             SET status = ?1, resolved_at = ?2
             WHERE account_id = ?3 AND message_id = ?4 AND sensitivity != 'sealed'",
            params![status.as_str(), resolved_at, account_id, message_id],
        )?;
        Ok(n > 0)
    }

    fn delete_sender_rule(&self, account_id: AccountId, id: i64) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM sender_rules WHERE account_id=?1 AND id=?2",
            params![account_id, id],
        )?;
        Ok(n > 0)
    }

    fn sealed_body(&self, account_id: AccountId, message_id: i64) -> Result<SealedBody> {
        // HUMAN-DOOR-ONLY. Returns NotFound for a missing OR non-sealed message.
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT m.id, m.account_id, m.thread_id, m.from_addr, m.from_name,
                        m.subject, m.received_at, t.sealed_kind, m.body
                 FROM messages m
                 JOIN triage t ON t.message_id = m.id
                 WHERE m.account_id = ?1 AND m.id = ?2 AND t.sensitivity = 'sealed'",
                params![account_id, message_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, Option<String>>(7)?,
                        r.get::<_, String>(8)?,
                    ))
                },
            )
            .optional()?;
        let (id, acct, thread_id, from_addr, from_name, subject, received_at, sealed_kind, body) =
            row.ok_or(CoreError::NotFound)?;
        Ok(SealedBody {
            id,
            account_id: acct,
            thread_id,
            from_addr,
            from_name,
            subject,
            received_at: parse_dt(&received_at)?,
            sealed_kind,
            body,
        })
    }

    fn append_audit(&self, account_id: AccountId, entry: &NewAuditEntry) -> Result<i64> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO audit_log(account_id, ts, actor, action, target, detail)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                account_id,
                Utc::now().to_rfc3339(),
                entry.actor,
                entry.action,
                entry.target,
                entry.detail,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn list_audit(&self, account_id: AccountId, limit: u32) -> Result<Vec<AuditEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, ts, actor, action, target, detail
             FROM audit_log WHERE account_id=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account_id, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, acct, ts, actor, action, target, detail) = row?;
            out.push(AuditEntry {
                id,
                account_id: acct,
                ts: parse_dt(&ts)?,
                actor,
                action,
                target,
                detail,
            });
        }
        Ok(out)
    }

    fn stats(&self, account_id: AccountId) -> Result<StoreStats> {
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

        // Sitrep band counts over non-sealed rows. Definitions match the `band`
        // query on attention_updates so the header and the list agree.
        let (standing, new_count, open_count): (i64, i64, i64) = conn.query_row(
            "SELECT
                 COUNT(*) FILTER (
                     WHERE tier IN ('past_due','deadline') AND status != 'done'),
                 COUNT(*) FILTER (WHERE surfaced_at IS NULL),
                 COUNT(*) FILTER (WHERE status = 'open')
             FROM triage
             WHERE account_id = ?1 AND sensitivity != 'sealed'",
            params![account_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        let last_surfaced_s: Option<String> = conn.query_row(
            "SELECT MAX(surfaced_at) FROM triage
             WHERE account_id = ?1 AND sensitivity != 'sealed'",
            params![account_id],
            |r| r.get(0),
        )?;
        let last_surfaced_at = match last_surfaced_s {
            Some(s) => Some(parse_dt(&s)?),
            None => None,
        };

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

    // ---- STAGE-2 ----------------------------------------------------------

    fn stage2_queue(&self, account_id: AccountId, limit: usize) -> Result<Vec<Stage2Queued>> {
        let conn = self.lock()?;
        // The Stage-2 queue predicate, verbatim: non-confident Stage-1 rows are
        // left with model_used IS NULL; sealed rows carry sensitivity='sealed'
        // and are structurally excluded. Join the message for context and
        // LEFT JOIN the matched sender rule for its want_text. is_known_contact
        // is derived from a correlated EXISTS against contacts (mirrors
        // `is_known_contact`).
        let mut stmt = conn.prepare(
            "SELECT m.id, m.thread_id, m.from_addr, m.subject, m.body, t.sensitivity,
                    sr.want_text, m.received_at,
                    EXISTS(
                        SELECT 1 FROM contacts c
                        WHERE c.account_id = m.account_id
                          AND c.addr = m.from_addr COLLATE NOCASE
                          AND c.sent_count > 0
                    ) AS is_known
             FROM triage t
             JOIN messages m ON m.id = t.message_id
             LEFT JOIN sender_rules sr ON sr.id = t.matched_rule_id
             WHERE t.account_id = ?1
               AND t.model_used IS NULL
               AND t.sensitivity = 'normal'
               AND m.is_sent = 0
             ORDER BY m.received_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account_id, limit as i64], |r| {
            let sensitivity: String = r.get(5)?;
            let want_text: Option<String> = r.get(6)?;
            let received_at: String = r.get(7)?;
            let is_known: i64 = r.get(8)?;
            Ok((
                Stage2Queued {
                    message_id: r.get(0)?,
                    account_id,
                    thread_id: r.get(1)?,
                    from_addr: r.get(2)?,
                    subject: r.get(3)?,
                    body: r.get(4)?,
                    received_at: Utc::now(), // replaced below after parse
                    is_known_contact: is_known != 0,
                    rule_want_text: want_text.filter(|s| !s.is_empty()),
                    sensitivity: Sensitivity::parse(&sensitivity),
                },
                received_at,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (mut q, received_at) = row?;
            q.received_at = parse_dt(&received_at)?;
            out.push(q);
        }
        Ok(out)
    }

    fn stage2_budget_used(
        &self,
        account_id: AccountId,
        thread_id: &str,
        day: &str,
    ) -> Result<u32> {
        let conn = self.lock()?;
        let n: i64 = conn
            .query_row(
                "SELECT model_calls FROM wake_budget
                 WHERE account_id=?1 AND thread_id=?2 AND day=?3",
                params![account_id, thread_id, day],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(n.max(0) as u32)
    }

    fn stage2_increment_budget(
        &self,
        account_id: AccountId,
        thread_id: &str,
        day: &str,
    ) -> Result<u32> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO wake_budget(account_id, thread_id, day, model_calls)
             VALUES(?1, ?2, ?3, 1)
             ON CONFLICT(account_id, thread_id, day)
             DO UPDATE SET model_calls = model_calls + 1",
            params![account_id, thread_id, day],
        )?;
        let n: i64 = conn.query_row(
            "SELECT model_calls FROM wake_budget
             WHERE account_id=?1 AND thread_id=?2 AND day=?3",
            params![account_id, thread_id, day],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u32)
    }

    fn stage2_apply(&self, applied: &Stage2Applied) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        // Overwrite triage fields and stamp model_used. Guarded by
        // sensitivity='normal' so a sealed row can never be mutated here even if
        // a caller mis-targets one (defense in depth; the queue already excludes
        // sealed rows).
        let deadline_dt = applied.deadline.as_ref().map(|d| d.due_at.to_rfc3339());
        tx.execute(
            "UPDATE triage SET
                 importance = ?3,
                 tier = ?4,
                 one_line = ?5,
                 reason = ?6,
                 deadline = ?7,
                 model_used = ?8
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![
                applied.message_id,
                applied.account_id,
                applied.importance as i64,
                applied.tier.as_str(),
                applied.one_line,
                applied.reason,
                deadline_dt,
                applied.model_used,
            ],
        )?;
        // (Re)write the deadlines row idempotently.
        tx.execute(
            "DELETE FROM deadlines WHERE message_id=?1",
            params![applied.message_id],
        )?;
        if let Some(d) = &applied.deadline {
            tx.execute(
                "INSERT INTO deadlines(account_id, message_id, kind, amount, currency,
                     due_at, past_due, source)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    applied.account_id,
                    applied.message_id,
                    d.kind,
                    d.amount,
                    d.currency,
                    d.due_at.to_rfc3339(),
                    d.past_due as i64,
                    d.source,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn stage2_mark_processed(
        &self,
        account_id: AccountId,
        message_id: i64,
        model_used: &str,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE triage SET model_used = ?3
             WHERE message_id = ?1 AND account_id = ?2 AND sensitivity = 'normal'",
            params![message_id, account_id, model_used],
        )?;
        Ok(())
    }

    fn stage2_bump_usage(
        &self,
        account_id: AccountId,
        day: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO stage2_usage(account_id, day, calls, input_tokens, output_tokens)
             VALUES(?1, ?2, 1, ?3, ?4)
             ON CONFLICT(account_id, day) DO UPDATE SET
                 calls = calls + 1,
                 input_tokens = input_tokens + excluded.input_tokens,
                 output_tokens = output_tokens + excluded.output_tokens",
            params![account_id, day, input_tokens as i64, output_tokens as i64],
        )?;
        Ok(())
    }

    fn stage2_usage_today(&self, account_id: AccountId, day: &str) -> Result<Stage2Usage> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT calls, input_tokens, output_tokens FROM stage2_usage
                 WHERE account_id = ?1 AND day = ?2",
                params![account_id, day],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row
            .map(|(calls, in_tok, out_tok)| Stage2Usage {
                calls: calls.max(0) as u64,
                input_tokens: in_tok.max(0) as u64,
                output_tokens: out_tok.max(0) as u64,
            })
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SealedKind, Sensitivity, Tier};

    fn sample_msg(account_id: AccountId, gmail_id: &str, thread: &str) -> NewMessage {
        NewMessage {
            account_id,
            gmail_msg_id: gmail_id.to_string(),
            thread_id: thread.to_string(),
            from_addr: "alice@example.com".to_string(),
            from_name: Some("Alice".to_string()),
            subject: "Lunch?".to_string(),
            received_at: Utc::now(),
            snippet: "want to grab lunch".to_string(),
            body: "Hey, want to grab lunch tomorrow?".to_string(),
            is_sent: false,
        }
    }

    #[test]
    fn round_trips_a_message() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(
                id, acct, 80, Tier::Signal, Sensitivity::Normal, None, "Lunch invite",
                "known contact", None,
            )
            .unwrap();

        let updates = store
            .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), Some(1))
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].sender, "alice@example.com");
        assert_eq!(updates[0].tier, Tier::Signal);

        let tv = store.thread_view(acct, "t1").unwrap();
        assert_eq!(tv.messages.len(), 1);
        assert_eq!(tv.subject, "Lunch?");
    }

    #[test]
    fn sealed_rows_absent_from_updates_but_present_in_sealed_messages() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // A normal message.
        let normal = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(
                normal, acct, 80, Tier::Signal, Sensitivity::Normal, None, "Lunch", "", None,
            )
            .unwrap();

        // A sealed OTP message in a different thread.
        let mut otp = sample_msg(acct, "g2", "t2");
        otp.subject = "Your verification code".to_string();
        otp.from_addr = "noreply@bank.com".to_string();
        let sealed_id = store.upsert_message(&otp).unwrap();
        store
            .set_triage(
                sealed_id,
                acct,
                90,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "code",
                "otp",
                None,
            )
            .unwrap();

        // ranked_updates must NOT include the sealed row.
        let updates = store
            .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), None)
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert!(updates.iter().all(|u| u.thread_id != "t2"));

        // thread_view on the sealed thread => NotFound.
        let err = store.thread_view(acct, "t2").unwrap_err();
        assert!(matches!(err, CoreError::NotFound));

        // Nonexistent thread also => NotFound (indistinguishable).
        let err2 = store.thread_view(acct, "does-not-exist").unwrap_err();
        assert!(matches!(err2, CoreError::NotFound));

        // sealed_messages (local-only) DOES surface it.
        let sealed = store.sealed_messages(acct).unwrap();
        assert_eq!(sealed.len(), 1);
        assert_eq!(sealed[0].thread_id, "t2");
        assert_eq!(sealed[0].sealed_kind.as_deref(), Some("otp"));
    }

    #[test]
    fn deadlines_exclude_sealed_source() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let mid = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(
                mid, acct, 50, Tier::Deadline, Sensitivity::Sealed, None, "", "", None,
            )
            .unwrap();

        {
            let conn = store.lock().unwrap();
            conn.execute(
                "INSERT INTO deadlines(account_id, message_id, kind, due_at, past_due, source)
                 VALUES(?1,?2,'bill',?3,0,'regex')",
                params![acct, mid, (Utc::now() + chrono::Duration::days(2)).to_rfc3339()],
            )
            .unwrap();
        }

        let ds = store.deadlines(acct, Some(30)).unwrap();
        assert!(ds.is_empty(), "sealed-source deadline must be hidden");
    }

    #[test]
    fn search_excludes_sealed_and_delete_rule_works() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let mut normal = sample_msg(acct, "g1", "t1");
        normal.subject = "verification steps".to_string();
        normal.body = "how to verify your account".to_string();
        let n = store.upsert_message(&normal).unwrap();
        store
            .set_triage(n, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        let mut sealed = sample_msg(acct, "g2", "t2");
        sealed.subject = "verification code".to_string();
        sealed.body = "code 999".to_string();
        let s = store.upsert_message(&sealed).unwrap();
        store
            .set_triage(
                s, acct, 90, Tier::Noise, Sensitivity::Sealed, Some(SealedKind::Otp), "", "", None,
            )
            .unwrap();

        let hits = store.search(acct, "verification", 10, 0).unwrap();
        assert_eq!(hits.len(), 1, "sealed row must be excluded from search");
        assert_eq!(hits[0].thread_id, "t1");

        // delete_sender_rule
        let rid = store
            .set_sender_rule(acct, "*@x.com", "no", Disposition::Squelch)
            .unwrap();
        assert!(store.delete_sender_rule(acct, rid).unwrap());
        assert!(!store.delete_sender_rule(acct, rid).unwrap());
        assert!(store.list_sender_rules(acct).unwrap().is_empty());
    }

    #[test]
    fn sealed_body_reveal_audit_and_stats() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let mut sealed = sample_msg(acct, "g1", "t1");
        sealed.body = "secret 123456".to_string();
        let s = store.upsert_message(&sealed).unwrap();
        store
            .set_triage(
                s, acct, 90, Tier::Noise, Sensitivity::Sealed, Some(SealedKind::Otp), "", "", None,
            )
            .unwrap();

        let mut normal = sample_msg(acct, "g2", "t2");
        normal.thread_id = "t2".to_string();
        let nid = store.upsert_message(&normal).unwrap();
        store
            .set_triage(nid, acct, 80, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        // sealed_body returns only for the sealed message.
        let body = store.sealed_body(acct, s).unwrap();
        assert_eq!(body.body, "secret 123456");
        assert!(matches!(
            store.sealed_body(acct, nid).unwrap_err(),
            CoreError::NotFound
        ));

        // audit append + list
        let aid = store
            .append_audit(
                acct,
                &crate::store::NewAuditEntry {
                    actor: "human".into(),
                    action: "reveal_sealed".into(),
                    target: Some(s.to_string()),
                    detail: None,
                },
            )
            .unwrap();
        assert!(aid > 0);
        let audit = store.list_audit(acct, 10).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].action, "reveal_sealed");

        // stats: 1 signal (t2), 1 sealed.
        let stats = store.stats(acct).unwrap();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.tier_counts.get("signal").copied(), Some(1));
        assert_eq!(stats.sealed, 1);
    }

    // --- sitrep seen-ledger --------------------------------------------------

    /// Helper: a non-sealed triaged message with a chosen tier/importance.
    fn ingest_normal(
        store: &SqliteStore,
        acct: AccountId,
        gmail: &str,
        thread: &str,
        tier: Tier,
        importance: u8,
        received: DateTime<Utc>,
    ) -> i64 {
        let mut m = sample_msg(acct, gmail, thread);
        m.received_at = received;
        let id = store.upsert_message(&m).unwrap();
        store
            .set_triage(id, acct, importance, tier, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        id
    }

    #[test]
    fn mark_surfaced_is_stamp_once_and_promotes_new_to_open() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(1);
        let id = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, Utc::now());

        // Pre-stamp: status new, surfaced_at NULL.
        let before = store
            .attention_updates(acct, since, None, None, None)
            .unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].status, AttentionStatus::New);
        assert!(before[0].surfaced_at.is_none());

        // First surface: stamps + promotes.
        let n = store.mark_surfaced(acct, &[id]).unwrap();
        assert_eq!(n, 1, "first surface counts as a transition");
        let after = store
            .attention_updates(acct, since, None, None, None)
            .unwrap();
        assert_eq!(after[0].status, AttentionStatus::Open);
        let stamp = after[0].surfaced_at.expect("surfaced_at set");

        // Second surface: idempotent, surfaced_at unchanged, no transition.
        let n2 = store.mark_surfaced(acct, &[id]).unwrap();
        assert_eq!(n2, 0, "second surface transitions nothing");
        let after2 = store
            .attention_updates(acct, since, None, None, None)
            .unwrap();
        assert_eq!(after2[0].surfaced_at, Some(stamp));
        assert_eq!(after2[0].status, AttentionStatus::Open);
    }

    #[test]
    fn band_queries_bucket_correctly() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(30);

        // A past_due bill (standing), a fresh signal (new), an aged signal.
        let bill = ingest_normal(&store, acct, "g1", "t1", Tier::PastDue, 90, Utc::now());
        let fresh = ingest_normal(&store, acct, "g2", "t2", Tier::Signal, 70, Utc::now());
        let aged = ingest_normal(
            &store,
            acct,
            "g3",
            "t3",
            Tier::Signal,
            60,
            Utc::now() - chrono::Duration::days(14),
        );

        // STANDING: only the bill (tier past_due/deadline, not done).
        let standing = store
            .attention_updates(acct, since, None, None, Some(SitrepBand::Standing))
            .unwrap();
        assert_eq!(standing.len(), 1);
        assert_eq!(standing[0].update.id, bill);

        // NEW: everything (nothing surfaced yet).
        let new = store
            .attention_updates(acct, since, None, None, Some(SitrepBand::New))
            .unwrap();
        assert_eq!(new.len(), 3);

        // Surface fresh + aged -> they become 'open'; bill stays new.
        store.mark_surfaced(acct, &[fresh, aged]).unwrap();

        // NEW now only the bill.
        let new2 = store
            .attention_updates(acct, since, None, None, Some(SitrepBand::New))
            .unwrap();
        assert_eq!(new2.len(), 1);
        assert_eq!(new2[0].update.id, bill);

        // OPEN band sorted by age*importance: aged (14d*60) before fresh (0d*70).
        let open = store
            .attention_updates(acct, since, None, None, Some(SitrepBand::Open))
            .unwrap();
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].update.id, aged, "older*importance floats to top");
        assert_eq!(open[1].update.id, fresh);
    }

    #[test]
    fn set_attention_status_resolves_and_reopens() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(1);
        let id = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, Utc::now());

        assert!(store
            .set_attention_status(acct, id, AttentionStatus::Done)
            .unwrap());
        let done = store
            .attention_updates(acct, since, None, Some(AttentionStatus::Done), None)
            .unwrap();
        assert_eq!(done.len(), 1);
        assert!(done[0].resolved_at.is_some(), "done stamps resolved_at");

        // Reopen clears resolved_at.
        assert!(store
            .set_attention_status(acct, id, AttentionStatus::Open)
            .unwrap());
        let open = store
            .attention_updates(acct, since, None, Some(AttentionStatus::Open), None)
            .unwrap();
        assert_eq!(open.len(), 1);
        assert!(open[0].resolved_at.is_none(), "reopen clears resolved_at");

        // Unknown id => false.
        assert!(!store
            .set_attention_status(acct, 999, AttentionStatus::Done)
            .unwrap());
    }

    #[test]
    fn sealed_rows_never_surface_through_the_ledger() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let since = Utc::now() - chrono::Duration::days(1);

        let mut otp = sample_msg(acct, "g1", "t1");
        otp.subject = "Your verification code".to_string();
        let sealed = store.upsert_message(&otp).unwrap();
        store
            .set_triage(
                sealed,
                acct,
                90,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "",
                "",
                None,
            )
            .unwrap();

        // Never appears in attention_updates (any band).
        assert!(store
            .attention_updates(acct, since, None, None, None)
            .unwrap()
            .is_empty());
        assert!(store
            .attention_updates(acct, since, None, None, Some(SitrepBand::New))
            .unwrap()
            .is_empty());

        // mark_surfaced refuses to stamp a sealed row.
        let n = store.mark_surfaced(acct, &[sealed]).unwrap();
        assert_eq!(n, 0);
        // set_attention_status refuses a sealed row.
        assert!(!store
            .set_attention_status(acct, sealed, AttentionStatus::Done)
            .unwrap());

        // Stats: sealed row contributes to `sealed`, never to any band, and
        // never advances last_surfaced_at.
        let stats = store.stats(acct).unwrap();
        assert_eq!(stats.sealed, 1);
        assert_eq!(stats.bands.new, 0);
        assert_eq!(stats.bands.standing, 0);
        assert_eq!(stats.bands.open, 0);
        assert!(stats.last_surfaced_at.is_none());
    }

    #[test]
    fn stats_bands_and_last_surfaced_at() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        let bill = ingest_normal(&store, acct, "g1", "t1", Tier::Deadline, 90, Utc::now());
        let sig = ingest_normal(&store, acct, "g2", "t2", Tier::Signal, 70, Utc::now());

        let s0 = store.stats(acct).unwrap();
        assert_eq!(s0.bands.standing, 1, "deadline tier counts as standing");
        assert_eq!(s0.bands.new, 2);
        assert_eq!(s0.bands.open, 0);
        assert!(s0.last_surfaced_at.is_none());

        store.mark_surfaced(acct, &[bill, sig]).unwrap();
        let s1 = store.stats(acct).unwrap();
        assert_eq!(s1.bands.new, 0, "both surfaced");
        assert_eq!(s1.bands.open, 2);
        assert_eq!(s1.bands.standing, 1, "surfacing doesn't change standing");
        assert!(s1.last_surfaced_at.is_some());
    }

    #[test]
    fn sender_rules_round_trip() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = store
            .set_sender_rule(acct, "*@newsletter.com", "no marketing", Disposition::Squelch)
            .unwrap();
        assert!(id > 0);
        let rules = store.list_sender_rules(acct).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].disposition, Disposition::Squelch);
    }

    // ---- Stage-2 store methods -------------------------------------------

    /// Insert a message + a triage row with model_used NULL (queued) or set
    /// (processed), controlling sensitivity so the sealed-exclusion is testable.
    fn seed_triage_row(
        store: &SqliteStore,
        acct: AccountId,
        gmail_id: &str,
        thread: &str,
        sensitivity: Sensitivity,
    ) -> i64 {
        let id = store
            .upsert_message(&sample_msg(acct, gmail_id, thread))
            .unwrap();
        store
            .set_triage(
                id, acct, 40, Tier::Noise, sensitivity, None, "ambiguous",
                "no rule matched", None,
            )
            .unwrap();
        id
    }

    #[test]
    fn stage2_queue_selects_only_normal_unprocessed_rows() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // A queued (normal, model_used NULL) row.
        let q1 = seed_triage_row(&store, acct, "g-normal", "t-1", Sensitivity::Normal);
        // A sealed row must be excluded.
        seed_triage_row(&store, acct, "g-sealed", "t-2", Sensitivity::Sealed);
        // A processed row (model_used set) must be excluded.
        let done = seed_triage_row(&store, acct, "g-done", "t-3", Sensitivity::Normal);
        store
            .stage2_mark_processed(acct, done, "claude-haiku-4-5")
            .unwrap();

        let rows = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(rows.len(), 1, "only the normal, unprocessed row is queued");
        assert_eq!(rows[0].message_id, q1);
        assert_eq!(rows[0].sensitivity, Sensitivity::Normal);
        assert!(rows[0].rule_want_text.is_none());
    }

    #[test]
    fn stage2_queue_surfaces_matched_rule_want_text() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let rule_id = store
            .set_sender_rule(
                acct,
                "*@shop.com",
                "only discounts, clearance, new collections",
                Disposition::Filtered,
            )
            .unwrap();
        let id = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(
                id, acct, 30, Tier::Noise, Sensitivity::Normal, None, "filtered",
                "matched filtered rule", None,
            )
            .unwrap();
        // Attach the matched rule id (set_triage leaves matched_rule_id NULL).
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "UPDATE triage SET matched_rule_id=?2 WHERE message_id=?1",
                params![id, rule_id],
            )
            .unwrap();
        }

        let rows = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].rule_want_text.as_deref(),
            Some("only discounts, clearance, new collections")
        );
    }

    #[test]
    fn stage2_prompt_carries_only_the_matched_rules_want_text() {
        // DETERMINISM: with N sender rules in the db, a Stage-2 prompt must carry
        // AT MOST the ONE rule's want_text whose id equals the row's
        // matched_rule_id (chosen by Stage-1's pure `match_sender_rule`), and NONE
        // of the others'. Rule selection is pure code: the queue LEFT JOINs
        // exactly `sr.id = t.matched_rule_id`, so the full rule list is NEVER fed
        // to the prompt.
        use crate::triage::stage2::{RowContext, build_user_message};

        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();

        // Three distinct Filtered rules, each with a unique, greppable want_text.
        let wants = [
            "WANT_ALPHA only closures",
            "WANT_BRAVO only invoices",
            "WANT_CHARLIE only shipments",
        ];
        let patterns = ["*@alpha.com", "*@bravo.com", "*@charlie.com"];
        let mut rule_ids = Vec::new();
        for (pat, want) in patterns.iter().zip(wants.iter()) {
            rule_ids.push(
                store
                    .set_sender_rule(acct, pat, want, Disposition::Filtered)
                    .unwrap(),
            );
        }

        // A queued row whose Stage-1 match landed on rule #2 (bravo). We stamp
        // matched_rule_id exactly as Stage-1 would (it selects a single rule id).
        let matched_id = rule_ids[1];
        let id = store.upsert_message(&sample_msg(acct, "g1", "t1")).unwrap();
        store
            .set_triage(
                id, acct, 30, Tier::Noise, Sensitivity::Normal, None, "filtered",
                "matched filtered rule", None,
            )
            .unwrap();
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "UPDATE triage SET matched_rule_id=?2 WHERE message_id=?1",
                params![id, matched_id],
            )
            .unwrap();
        }

        let rows = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(rows.len(), 1);
        // Only the matched rule's want_text surfaces from the store.
        assert_eq!(rows[0].rule_want_text.as_deref(), Some("WANT_BRAVO only invoices"));

        // And the BUILT prompt contains exactly that one rule's text — none of
        // the other two rules leak in.
        let ctx = RowContext::from_queued(&rows[0], 4000);
        let prompt = build_user_message(&ctx);
        assert!(prompt.contains("WANT_BRAVO only invoices"), "matched want must appear");
        assert!(!prompt.contains("WANT_ALPHA"), "non-matched rule must not leak");
        assert!(!prompt.contains("WANT_CHARLIE"), "non-matched rule must not leak");
        assert_eq!(
            prompt.matches("WANT_").count(),
            1,
            "exactly one rule's want_text in the prompt"
        );

        // NO-MATCH case: a row with matched_rule_id NULL carries zero rule text.
        let id2 = store.upsert_message(&sample_msg(acct, "g2", "t2")).unwrap();
        store
            .set_triage(
                id2, acct, 40, Tier::Noise, Sensitivity::Normal, None, "ambiguous",
                "no rule matched", None,
            )
            .unwrap();
        let rows2 = store.stage2_queue(acct, 10).unwrap();
        let unmatched = rows2.iter().find(|r| r.message_id == id2).unwrap();
        assert!(unmatched.rule_want_text.is_none(), "no rule => no want_text");
        let prompt2 = build_user_message(&RowContext::from_queued(unmatched, 4000));
        assert!(!prompt2.contains("WANT_"), "unmatched row prompt has zero rule text");
        assert!(prompt2.contains("standing_instruction_for_this_sender: none"));
    }

    #[test]
    fn stage2_budget_increment_and_exhaustion() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let day = "2026-07-09";

        assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 0);
        assert_eq!(store.stage2_increment_budget(acct, "t-abc", day).unwrap(), 1);
        assert_eq!(store.stage2_increment_budget(acct, "t-abc", day).unwrap(), 2);
        assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 2);

        // A different thread and a different day are independent counters.
        assert_eq!(store.stage2_budget_used(acct, "t-other", day).unwrap(), 0);
        assert_eq!(store.stage2_budget_used(acct, "t-abc", "2026-07-10").unwrap(), 0);

        // The global sentinel is a separate scope in the same table.
        assert_eq!(store.stage2_increment_budget(acct, "__global__", day).unwrap(), 1);
        assert_eq!(store.stage2_budget_used(acct, "__global__", day).unwrap(), 1);
        // The per-thread counter is unaffected by the global increment.
        assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 2);
    }

    #[test]
    fn mailing_list_storm_capped_at_thread_daily_cap() {
        // Audit (c): a mailing-list storm — 30 messages, all in ONE thread —
        // must result in AT MOST `thread_daily_cap` API calls. This models the
        // exact check-BEFORE-increment discipline stage2_pass runs per row:
        // read the per-thread counter, skip if it's already at the cap, else
        // increment (which is what "make a call" costs). Any global cap is set
        // high so the per-thread cap is the binding constraint.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let day = "2026-07-09";
        let thread = "t-listserv";
        let thread_daily_cap: u32 = 3; // matches Stage2Config default

        let mut calls = 0u32;
        for _ in 0..30 {
            let used = store.stage2_budget_used(acct, thread, day).unwrap();
            if used >= thread_daily_cap {
                continue; // capped: row stays queued, no call
            }
            // "Make the call": increment BEFORE the attempt.
            store.stage2_increment_budget(acct, thread, day).unwrap();
            calls += 1;
        }

        assert_eq!(
            calls, thread_daily_cap,
            "30-message storm on one thread must cost at most thread_daily_cap calls"
        );
        assert_eq!(
            store.stage2_budget_used(acct, thread, day).unwrap(),
            thread_daily_cap,
            "counter must not exceed the cap"
        );
    }

    #[test]
    fn one_sender_across_many_threads_capped_at_sender_daily_cap() {
        // TASK 3: a chatty sender fanning 10 messages across 10 DIFFERENT threads
        // must cost AT MOST `sender_daily_cap` calls. Models the per-sender
        // check-BEFORE-increment the pass runs (keyed by sender:<addr>), with the
        // per-thread and global caps set high so the per-sender cap binds.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let day = "2026-07-09";
        let sender_key = "sender:chatty@example.com";
        let sender_daily_cap: u32 = 5; // matches Stage2Config default

        let mut calls = 0u32;
        for i in 0..10 {
            // Each message is in its OWN thread — the per-thread cap never binds.
            let _thread = format!("t-{i}");
            let used = store.stage2_budget_used(acct, sender_key, day).unwrap();
            if used >= sender_daily_cap {
                continue; // sender capped: row stays queued, no call
            }
            store.stage2_increment_budget(acct, sender_key, day).unwrap();
            calls += 1;
        }

        assert_eq!(
            calls, sender_daily_cap,
            "10 messages from one sender across 10 threads cost at most sender_daily_cap"
        );
        assert_eq!(
            store.stage2_budget_used(acct, sender_key, day).unwrap(),
            sender_daily_cap
        );
    }

    #[test]
    fn stage2_usage_ledger_bumps_and_reads() {
        // TASK 5: bumping the usage ledger accumulates calls + tokens per day, and
        // reading returns the running totals (zeroed for an untouched day).
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let day = "2026-07-09";

        // Untouched day reads as zeros.
        let z = store.stage2_usage_today(acct, day).unwrap();
        assert_eq!(z, Stage2Usage::default());

        store.stage2_bump_usage(acct, day, 1200, 60).unwrap();
        store.stage2_bump_usage(acct, day, 800, 40).unwrap();
        let u = store.stage2_usage_today(acct, day).unwrap();
        assert_eq!(u.calls, 2);
        assert_eq!(u.input_tokens, 2000);
        assert_eq!(u.output_tokens, 100);

        // A different day is an independent row.
        assert_eq!(
            store.stage2_usage_today(acct, "2026-07-10").unwrap(),
            Stage2Usage::default()
        );
    }

    #[test]
    fn update_sender_rule_edits_by_id_and_404s_unknown() {
        // TASK 6 (store layer): update_sender_rule overwrites pattern/want/disp by
        // id, returns false for an unknown id.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = store
            .set_sender_rule(acct, "*@old.com", "old want", Disposition::Squelch)
            .unwrap();

        let updated = store
            .update_sender_rule(acct, id, "*@new.com", "new want", Disposition::Surface)
            .unwrap();
        assert!(updated);
        let rules = store.list_sender_rules(acct).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_pattern, "*@new.com");
        assert_eq!(rules[0].want_text, "new want");
        assert_eq!(rules[0].disposition, Disposition::Surface);

        // Unknown id => false (handler turns this into 404).
        assert!(!store
            .update_sender_rule(acct, 9999, "*@x.com", "", Disposition::Squelch)
            .unwrap());
    }

    #[test]
    fn stale_skip_marks_processed_without_budget() {
        // TASK 4: a row older than the cutoff is stale-skipped: marked processed
        // with model_used='stale-skip' (keeping Stage-1 values), leaving the
        // queue, and NOT touching any budget row. Models the pass-loop decision.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let max_age_days: i64 = 7;
        let now = Utc::now();
        let cutoff = now - chrono::Duration::days(max_age_days);

        // A stale row (received 30d ago) and a fresh row (now).
        let mut stale = sample_msg(acct, "g-stale", "t-stale");
        stale.received_at = now - chrono::Duration::days(30);
        let stale_id = store.upsert_message(&stale).unwrap();
        store
            .set_triage(stale_id, acct, 40, Tier::Noise, Sensitivity::Normal, None, "amb", "", None)
            .unwrap();
        let mut fresh = sample_msg(acct, "g-fresh", "t-fresh");
        fresh.received_at = now;
        let fresh_id = store.upsert_message(&fresh).unwrap();
        store
            .set_triage(fresh_id, acct, 40, Tier::Noise, Sensitivity::Normal, None, "amb", "", None)
            .unwrap();

        // Apply the pass-loop decision: stale-skip old rows, keep fresh queued.
        let day = "2026-07-09";
        for row in store.stage2_queue(acct, 10).unwrap() {
            if row.received_at < cutoff {
                store
                    .stage2_mark_processed(acct, row.message_id, "stale-skip")
                    .unwrap();
            }
        }

        // Only the fresh row remains queued.
        let remaining = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].message_id, fresh_id);

        // No budget was spent on the stale skip.
        assert_eq!(store.stage2_budget_used(acct, "t-stale", day).unwrap(), 0);
        assert_eq!(
            store.stage2_budget_used(acct, "__global__", day).unwrap(),
            0
        );

        // The stale row's triage is stamped 'stale-skip' with Stage-1 values kept.
        let conn = store.lock().unwrap();
        let (imp, model): (i64, Option<String>) = conn
            .query_row(
                "SELECT importance, model_used FROM triage WHERE message_id=?1",
                params![stale_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(imp, 40, "stale-skip keeps Stage-1 importance");
        assert_eq!(model.as_deref(), Some("stale-skip"));
    }

    #[test]
    fn stage2_queue_carries_received_at() {
        // TASK 4 support: the queue surfaces received_at so the pass can skip
        // stale rows.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let mut m = sample_msg(acct, "g1", "t1");
        let when = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        m.received_at = when;
        let id = store.upsert_message(&m).unwrap();
        store
            .set_triage(
                id, acct, 40, Tier::Noise, Sensitivity::Normal, None, "amb", "", None,
            )
            .unwrap();
        let rows = store.stage2_queue(acct, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].received_at, when);
    }

    #[test]
    fn stage2_apply_updates_row_stamps_model_and_writes_deadline() {
        use crate::triage::DeadlineHit;
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = seed_triage_row(&store, acct, "g1", "t1", Sensitivity::Normal);

        let due = DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let applied = Stage2Applied {
            message_id: id,
            account_id: acct,
            importance: 88,
            tier: Tier::Deadline,
            one_line: "invoice due sep 1".into(),
            reason: "stage-2 (m): real bill".into(),
            model_used: "claude-haiku-4-5".into(),
            deadline: Some(DeadlineHit {
                kind: "invoice".into(),
                amount: None,
                currency: None,
                due_at: due,
                past_due: false,
                source: "stage2".into(),
            }),
        };
        store.stage2_apply(&applied).unwrap();

        // Row left the queue (model_used stamped).
        assert!(store.stage2_queue(acct, 10).unwrap().is_empty());
        // A deadlines row was written.
        let ds = store.deadlines(acct, Some(365)).unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].kind, "invoice");
        // The ranked update reflects the new tier/importance.
        let ups = store
            .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), None)
            .unwrap();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].tier, Tier::Deadline);
        assert_eq!(ups[0].importance, 88);
    }

    #[test]
    fn stage2_apply_never_touches_sealed_row() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let id = seed_triage_row(&store, acct, "g-sealed", "t1", Sensitivity::Sealed);
        let applied = Stage2Applied {
            message_id: id,
            account_id: acct,
            importance: 99,
            tier: Tier::Signal,
            one_line: "leak".into(),
            reason: "should not apply".into(),
            model_used: "m".into(),
            deadline: None,
        };
        store.stage2_apply(&applied).unwrap();
        // The sealed row's triage must be unchanged (guarded by sensitivity).
        let conn = store.lock().unwrap();
        let (imp, model): (i64, Option<String>) = conn
            .query_row(
                "SELECT importance, model_used FROM triage WHERE message_id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(imp, 40, "sealed row importance unchanged");
        assert!(model.is_none(), "sealed row model_used untouched");
    }
}
