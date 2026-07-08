//! SQLite-backed [`Store`] implementation.
//!
//! rusqlite is synchronous, so the `Connection` is wrapped in a `Mutex` and the
//! trait is implemented synchronously. See `store/mod.rs` for rationale.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{CoreError, Result};
use crate::store::{NewAuditEntry, SealedBody, SealedMessage, Store, SyncState, TriagedMessage};
use crate::types::{
    AccountId, AuditEntry, Deadline, Disposition, NewMessage, SanitizedMessage, SearchHit,
    SenderRule, Sensitivity, StoreStats, ThreadView, Tier, Update,
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

    // Derived contacts: Sent mail = people I know.
    if msg.is_sent {
        conn.execute(
            "INSERT INTO contacts(account_id, addr, sent_count, first_seen)
             VALUES(?1,?2,1,?3)
             ON CONFLICT(account_id, addr) DO UPDATE SET sent_count = sent_count + 1",
            params![msg.account_id, msg.from_addr, msg.received_at.to_rfc3339()],
        )?;
    }

    Ok(id)
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

        // 1. Upsert the message row (+ FTS + Sent-derived contacts).
        let id = upsert_message_conn(&tx, &triaged.message)?;

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

        Ok(StoreStats {
            tier_counts,
            total,
            sealed,
            last_history_id: last_history_id.map(|v| v as u64),
        })
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
}
