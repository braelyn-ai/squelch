//! The control store: tenants and invite codes, in this crate's own small
//! SQLite file.
//!
//! Deliberately NOT the daemon's store. The two have nothing in common but the
//! engine: this one holds a handful of rows describing who has been provisioned,
//! and it must never be able to open a tenant's mail database. rusqlite is
//! synchronous, so the connection sits behind a `Mutex` and the handlers call it
//! from async code the way the relay's open buffer does; every method here is
//! one short statement.
//!
//! WHAT IS NOT IN THIS SCHEMA IS THE POINT: no tokens, no ciphertext, no
//! authorization codes, no invite plaintext. The refresh token this service
//! handles lives in memory for the length of one request and leaves as age
//! armor addressed to the VPS. There is nothing at rest on Railway that opens a
//! mailbox.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

/// How long a connection waits out another writer before giving up. There is
/// exactly one process on this file, so this only ever covers the store's own
/// brief overlap between a handler and the CLI running on the same box.
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// `status` values a tenant row can carry. Strings rather than an enum column
/// because the warden is the authority on liveness and this column is a record
/// of what the control plane last did, not a lock.
pub const STATUS_ACTIVE: &str = "active";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tenants (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    label         TEXT NOT NULL UNIQUE,
    account_email TEXT NOT NULL,
    status        TEXT NOT NULL,
    created_at    TEXT NOT NULL
);
-- One mailbox, one daemon. A PARTIAL unique index rather than a plain one, so a
-- tenant that has been torn down frees its address for a later signup while an
-- active one cannot be duplicated by two requests racing past the SELECT.
CREATE UNIQUE INDEX IF NOT EXISTS idx_tenants_active_email
    ON tenants(account_email) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS invite_codes (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    code_hash     TEXT NOT NULL UNIQUE,
    created_at    TEXT NOT NULL,
    used_at       TEXT,
    used_by_label TEXT
);
";

/// Store errors. `Sqlite` carries rusqlite's message, which never contains a
/// code or a token: the only values bound into these statements are hashes,
/// labels, addresses, and timestamps.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("control store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The single-use guarantee lost a race, or the row was already spent.
    #[error("that invite code is no longer available")]
    InviteUnavailable,
    /// Another signup took the label between the availability check and the
    /// insert.
    #[error("that address was just taken")]
    LabelTaken,
    /// This mailbox already has a daemon.
    #[error("that Google account already has a Passband mailbox")]
    AccountTaken,
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// One invite row as the CLI lists it. NO HASH: `invite list` prints what an
/// operator needs (which ones are spent, and by whom) and nothing that could be
/// used to check a guess offline.
#[derive(Debug, Clone)]
pub struct InviteRow {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
    pub used_by_label: Option<String>,
}

/// One tenant row.
#[derive(Debug, Clone)]
pub struct TenantRow {
    pub label: String,
    pub account_email: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

pub struct ControlStore {
    conn: Mutex<Connection>,
}

impl ControlStore {
    /// Open (creating if needed) the control store at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// An in-memory store, for tests. A separate constructor rather than a
    /// magic path string so nothing in production can reach it by typo.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL so a `squelch-control invite issue` at the shell does not block
        // the serving process, and a busy timeout so it waits instead of
        // failing when they do overlap. `foreign_keys` is on for the same
        // reason it is everywhere else: a schema that grows a reference later
        // should not silently stop enforcing it.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Poisoning is RECOVERED rather than propagated: the guarded value is a
    /// connection with no invariant a panic could corrupt, and an `.expect()`
    /// here would brick every later request while `/healthz` kept answering.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- invite codes ----------------------------------------------------

    /// Record a freshly minted code by hash. The plaintext never comes here.
    pub fn insert_invite(&self, code_hash: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO invite_codes(code_hash, created_at) VALUES(?1, ?2)",
            params![code_hash, Utc::now().to_rfc3339()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// The id of an UNSPENT code with this hash, or `None`.
    ///
    /// One answer for every failure: no such code, already used, and revoked
    /// are indistinguishable here by construction, so the route above cannot
    /// become an oracle that tells a guesser which of those they hit.
    pub fn find_unused_invite(&self, code_hash: &str) -> Result<Option<i64>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT id FROM invite_codes WHERE code_hash = ?1 AND used_at IS NULL",
                params![code_hash],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// Spend a code, atomically. The `used_at IS NULL` predicate is the
    /// single-use guarantee: two requests that both passed
    /// [`Self::find_unused_invite`] race here, and exactly one updates a row.
    pub fn consume_invite(&self, id: i64, label: &str) -> Result<()> {
        let changed = self.lock().execute(
            "UPDATE invite_codes SET used_at = ?1, used_by_label = ?2
             WHERE id = ?3 AND used_at IS NULL",
            params![Utc::now().to_rfc3339(), label, id],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::InviteUnavailable)
        }
    }

    /// Every invite, newest first. Hashes are not selected.
    pub fn list_invites(&self) -> Result<Vec<InviteRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, created_at, used_at, used_by_label FROM invite_codes ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(InviteRow {
                id: r.get(0)?,
                created_at: parse_ts(r.get::<_, String>(1)?),
                used_at: r.get::<_, Option<String>>(2)?.map(parse_ts),
                used_by_label: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Revoke an UNSPENT code by id. Returns false when there was no unspent
    /// code with that id, which covers both "already used" and "no such row":
    /// an operator command may distinguish them, so the caller reports what it
    /// sees rather than this returning a reason.
    ///
    /// Deleting rather than flagging: a revoked code has no history worth
    /// keeping, and a row that stays behind with `used_at` set would lie in
    /// `invite list` about a signup that never happened.
    pub fn revoke_invite(&self, id: i64) -> Result<bool> {
        let changed = self.lock().execute(
            "DELETE FROM invite_codes WHERE id = ?1 AND used_at IS NULL",
            params![id],
        )?;
        Ok(changed == 1)
    }

    // ---- tenants ---------------------------------------------------------

    /// Whether this control plane has already recorded a tenant with `label`.
    /// The warden is asked the same question separately; this catches the case
    /// where a provision succeeded and only the record is being repeated.
    pub fn label_exists(&self, label: &str) -> Result<bool> {
        Ok(self
            .lock()
            .query_row(
                "SELECT 1 FROM tenants WHERE label = ?1",
                params![label],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// The label of the ACTIVE tenant for this mailbox, if any. One mailbox,
    /// one daemon: a second signup from the same Google account is refused
    /// politely rather than provisioned into a second, competing sync loop.
    pub fn active_tenant_for_email(&self, account_email: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT label FROM tenants WHERE account_email = ?1 AND status = ?2",
                params![normalize_email(account_email), STATUS_ACTIVE],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Record a provisioned tenant. Both unique constraints are mapped to
    /// their own errors rather than surfacing a SQLite message: these two races
    /// are expected (two tabs, two signups) and the pages above say different
    /// things about them.
    pub fn insert_tenant(&self, label: &str, account_email: &str) -> Result<()> {
        let email = normalize_email(account_email);
        let conn = self.lock();
        let res = conn.execute(
            "INSERT INTO tenants(label, account_email, status, created_at)
             VALUES(?1, ?2, ?3, ?4)",
            params![label, email, STATUS_ACTIVE, Utc::now().to_rfc3339()],
        );
        match res {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(ref e, ref msg))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                // The message names the index; the label constraint and the
                // partial email index are told apart by it. A message we do
                // not recognize falls through as the account error, which is
                // the more conservative of the two (it does not invite a retry
                // that would provision a second daemon for one mailbox).
                let msg = msg.clone().unwrap_or_default();
                if msg.contains("tenants.label") {
                    Err(StoreError::LabelTaken)
                } else {
                    Err(StoreError::AccountTaken)
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Every tenant, newest first. For the operator CLI.
    pub fn list_tenants(&self) -> Result<Vec<TenantRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT label, account_email, status, created_at FROM tenants ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(TenantRow {
                label: r.get(0)?,
                account_email: r.get(1)?,
                status: r.get(2)?,
                created_at: parse_ts(r.get::<_, String>(3)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// Gmail addresses are compared case-insensitively, so the column stores one
/// spelling. Without this the "one mailbox, one daemon" index is defeated by
/// capitalizing a letter.
fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// A stored RFC3339 stamp. These are written by this crate a line above, so a
/// value that will not parse is a corrupt row, not user input; the epoch is a
/// visibly wrong timestamp in a listing rather than a panic in a CLI.
fn parse_ts(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invites;

    fn store() -> ControlStore {
        ControlStore::open_in_memory().unwrap()
    }

    #[test]
    fn an_invite_is_spent_exactly_once() {
        let s = store();
        let m = invites::mint().unwrap();
        let id = s.insert_invite(&m.code_hash).unwrap();

        assert_eq!(s.find_unused_invite(&m.code_hash).unwrap(), Some(id));
        s.consume_invite(id, "ada").unwrap();
        // Both the lookup and a second consume refuse.
        assert_eq!(s.find_unused_invite(&m.code_hash).unwrap(), None);
        assert!(matches!(
            s.consume_invite(id, "grace"),
            Err(StoreError::InviteUnavailable)
        ));
    }

    /// The plaintext must never reach the file. Read the whole table back as
    /// text and look for it.
    #[test]
    fn only_the_hash_is_at_rest() {
        let s = store();
        let m = invites::mint().unwrap();
        s.insert_invite(&m.code_hash).unwrap();

        let conn = s.lock();
        let mut stmt = conn.prepare("SELECT code_hash FROM invite_codes").unwrap();
        let stored: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(stored, vec![m.code_hash.clone()]);
        assert!(!stored[0].contains(&invites::normalize(&m.code)));
        assert_eq!(stored[0], invites::hash(&m.code));
    }

    #[test]
    fn a_wrong_or_unknown_code_is_simply_absent() {
        let s = store();
        let m = invites::mint().unwrap();
        s.insert_invite(&m.code_hash).unwrap();
        assert_eq!(
            s.find_unused_invite(&invites::hash("ZZZZ-ZZZZ")).unwrap(),
            None
        );
    }

    #[test]
    fn revoking_removes_only_unspent_codes() {
        let s = store();
        let a = invites::mint().unwrap();
        let b = invites::mint().unwrap();
        let id_a = s.insert_invite(&a.code_hash).unwrap();
        let id_b = s.insert_invite(&b.code_hash).unwrap();
        s.consume_invite(id_b, "ada").unwrap();

        assert!(s.revoke_invite(id_a).unwrap());
        assert!(!s.revoke_invite(id_a).unwrap(), "already gone");
        assert!(!s.revoke_invite(id_b).unwrap(), "spent codes are kept");
        let rows = s.list_invites().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id_b);
        assert_eq!(rows[0].used_by_label.as_deref(), Some("ada"));
    }

    #[test]
    fn a_label_is_claimed_once() {
        let s = store();
        s.insert_tenant("ada", "ada@example.com").unwrap();
        assert!(s.label_exists("ada").unwrap());
        assert!(!s.label_exists("grace").unwrap());
        assert!(matches!(
            s.insert_tenant("ada", "someone@example.com"),
            Err(StoreError::LabelTaken)
        ));
    }

    /// One mailbox, one daemon, however the address is capitalized.
    #[test]
    fn a_mailbox_gets_one_tenant() {
        let s = store();
        s.insert_tenant("ada", "Ada@Example.com").unwrap();
        assert_eq!(
            s.active_tenant_for_email("ada@example.com").unwrap(),
            Some("ada".to_string())
        );
        assert_eq!(
            s.active_tenant_for_email("ADA@EXAMPLE.COM").unwrap(),
            Some("ada".to_string())
        );
        assert!(matches!(
            s.insert_tenant("grace", "ADA@example.com"),
            Err(StoreError::AccountTaken)
        ));
        assert_eq!(s.active_tenant_for_email("other@example.com").unwrap(), None);
    }
}
