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
//! authorization codes, no invite plaintext, and no session ids (the invite
//! reservation names its holder by fingerprint, which is all equality needs).
//! The refresh token this service
//! handles lives in memory for the length of one request and leaves as age
//! armor addressed to one tenant. There is nothing at rest on Railway that opens a
//! mailbox.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::invites::DEFAULT_TTL_DAYS;

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
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    code_hash      TEXT NOT NULL UNIQUE,
    created_at     TEXT NOT NULL,
    -- When the code stops being usable, whether or not anyone spent it.
    expires_at     TEXT,
    used_at        TEXT,
    used_by_label  TEXT,
    -- The signup session currently holding this code, by fingerprint, and until
    -- when. A live reservation makes the code unavailable to every other
    -- session; it self-releases when `reserved_until` passes.
    reserved_by    TEXT,
    reserved_until TEXT
);
";

/// Columns added to `invite_codes` after the first deployment, with the type
/// each is declared with. A store opened from an older file gets them here
/// rather than needing a migration tool: `CREATE TABLE IF NOT EXISTS` above
/// leaves an existing table exactly as it found it.
const ADDED_COLUMNS: [(&str, &str); 3] = [
    ("expires_at", "TEXT"),
    ("reserved_by", "TEXT"),
    ("reserved_until", "TEXT"),
];

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
    pub expires_at: Option<DateTime<Utc>>,
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
        migrate(&conn)?;
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

    /// Record a freshly minted code by hash, with the moment it stops working.
    /// The plaintext never comes here.
    pub fn insert_invite(&self, code_hash: &str, expires_at: DateTime<Utc>) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO invite_codes(code_hash, created_at, expires_at) VALUES(?1, ?2, ?3)",
            params![code_hash, stamp(Utc::now()), stamp(expires_at)],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// The id of an AVAILABLE code with this hash, or `None`.
    ///
    /// Available means: not spent, not expired, and not held by another
    /// session's live reservation. One answer for every failure, so the route
    /// above cannot become an oracle that tells a guesser which of those they
    /// hit.
    ///
    /// This is a GATE, not a claim: it spends nothing, and it exists so a
    /// nonsense code costs a point lookup instead of a round trip to the
    /// warden. [`Self::reserve_invite`] re-checks all three conditions in the
    /// statement that actually takes the code.
    pub fn find_available_invite(
        &self,
        code_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT id FROM invite_codes
                  WHERE code_hash = ?1
                    AND used_at IS NULL
                    AND (expires_at IS NULL OR expires_at > ?2)
                    AND (reserved_until IS NULL OR reserved_until <= ?2)",
                params![code_hash, stamp(now)],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// Hold an available code for one signup session until `until`, atomically.
    ///
    /// THE CHECK AND THE HOLD ARE ONE STATEMENT, and that is the whole point: a
    /// code is checked when the form is posted and spent only when provisioning
    /// has succeeded, minutes later at Google's callback. Without a hold taken
    /// in the same breath as the check, one code posted from N tabs passes N
    /// checks and provisions N tenants, and only the last consume loses.
    ///
    /// `holder` identifies the session, so nobody else can release or spend what
    /// this session is holding. The hold self-releases once `until` passes,
    /// which is what stops an abandoned signup from burning a code.
    pub fn reserve_invite(
        &self,
        code_hash: &str,
        holder: &str,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        Ok(self
            .lock()
            .query_row(
                "UPDATE invite_codes SET reserved_by = ?2, reserved_until = ?3
                  WHERE code_hash = ?1
                    AND used_at IS NULL
                    AND (expires_at IS NULL OR expires_at > ?4)
                    AND (reserved_until IS NULL OR reserved_until <= ?4)
                RETURNING id",
                params![code_hash, holder, stamp(until), stamp(now)],
                |r| r.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// Hand back a held code without spending it, so the person holding it can
    /// start again immediately rather than waiting out the reservation.
    ///
    /// Only the holder can release: a reservation that has already expired and
    /// been taken by somebody else must not be torn out from under them by a
    /// late failure path. Returns whether this holder was still the one holding
    /// it.
    pub fn release_invite(&self, id: i64, holder: &str) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE invite_codes SET reserved_by = NULL, reserved_until = NULL
             WHERE id = ?1 AND reserved_by = ?2 AND used_at IS NULL",
            params![id, holder],
        )?;
        Ok(changed == 1)
    }

    /// Spend a code the caller is holding, atomically.
    ///
    /// `used_at IS NULL` is the single-use guarantee and `reserved_by = holder`
    /// is the reservation one: a session may only spend the code it took, so a
    /// consume cannot succeed for a signup that never held it.
    ///
    /// Deliberately NOT checking `reserved_until`: the hold is taken for the
    /// session's lifetime and provisioning finishes at the far end of it, so a
    /// consume that arrives a moment late still spends the code it holds. The
    /// only thing that can take it away is another session reserving it after
    /// the hold lapsed.
    pub fn consume_invite(&self, id: i64, label: &str, holder: &str) -> Result<()> {
        let changed = self.lock().execute(
            "UPDATE invite_codes
                SET used_at = ?1, used_by_label = ?2, reserved_by = NULL, reserved_until = NULL
              WHERE id = ?3 AND used_at IS NULL AND reserved_by = ?4",
            params![stamp(Utc::now()), label, id, holder],
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
            "SELECT id, created_at, expires_at, used_at, used_by_label
               FROM invite_codes ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(InviteRow {
                id: r.get(0)?,
                created_at: parse_ts(r.get::<_, String>(1)?),
                expires_at: r.get::<_, Option<String>>(2)?.map(parse_ts),
                used_at: r.get::<_, Option<String>>(3)?.map(parse_ts),
                used_by_label: r.get(4)?,
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

    /// The mailbox that owns the ACTIVE tenant with this label, if there is one.
    ///
    /// The console login's whole question, asked twice: once before the user is
    /// sent to Google (does this tenant exist?) and once on the way back (is the
    /// Google account that came back the one that owns it?). Asked of the STORE
    /// both times rather than carried through the session, because a tenant that
    /// was torn down or changed hands while somebody was at Google must not be
    /// signed in to on the strength of a ten-minute-old lookup.
    ///
    /// `status = 'active'` is part of the question, not a filter on the answer: a
    /// row that is not active is not a mailbox anybody may sign in to.
    pub fn active_tenant_email(&self, label: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT account_email FROM tenants WHERE label = ?1 AND status = ?2",
                params![label, STATUS_ACTIVE],
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
            params![label, email, STATUS_ACTIVE, stamp(Utc::now())],
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

/// Bring an older store's `invite_codes` up to the schema above.
///
/// `CREATE TABLE IF NOT EXISTS` leaves an existing table exactly as it found it,
/// so a file written before expiry and reservations existed needs its new
/// columns added by hand. Adding a column is the only migration shape this
/// schema has ever needed; anything bigger would want a real tool.
fn migrate(conn: &Connection) -> Result<()> {
    let existing: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(invite_codes)")?;
        let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
        names.collect::<rusqlite::Result<_>>()?
    };
    for (name, ty) in ADDED_COLUMNS {
        if !existing.contains(name) {
            // The name and type are this file's own constants, never input.
            conn.execute(
                &format!("ALTER TABLE invite_codes ADD COLUMN {name} {ty}"),
                [],
            )?;
        }
    }
    backfill_expiry(conn)
}

/// Give every outstanding code minted before expiry existed one, counted from
/// when it was issued.
///
/// Backfilled rather than left NULL because a code with no expiry is exactly the
/// thing expiry was added to stop: one that sits in a stolen table of hashes for
/// as long as the attacker needs. Done in Rust rather than SQL so the stamp
/// written here has the same shape as every other stamp in the column, which the
/// availability checks compare as strings.
///
/// A row whose `created_at` will not parse is corrupt, and [`parse_ts`] dates it
/// to the epoch, so it expires immediately. That is the conservative direction:
/// a code nobody can account for stops working.
fn backfill_expiry(conn: &Connection) -> Result<()> {
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, created_at FROM invite_codes
              WHERE expires_at IS NULL AND used_at IS NULL",
        )?;
        let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        it.collect::<rusqlite::Result<_>>()?
    };
    for (id, created_at) in rows {
        let expires = parse_ts(created_at) + chrono::Duration::days(DEFAULT_TTL_DAYS);
        conn.execute(
            "UPDATE invite_codes SET expires_at = ?1 WHERE id = ?2",
            params![stamp(expires), id],
        )?;
    }
    Ok(())
}

/// Gmail addresses are compared case-insensitively, so the column stores one
/// spelling. Without this the "one mailbox, one daemon" index is defeated by
/// capitalizing a letter.
fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// The one shape a timestamp is written in: RFC3339, UTC, milliseconds, `Z`.
/// FIXED WIDTH ON PURPOSE. `expires_at` and `reserved_until` are compared
/// against a bound `now` by SQLite, which compares TEXT byte by byte, so two
/// spellings of the same instant would order wrongly and a live code would read
/// as expired.
fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
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

    /// `now`, truncated to the precision the column stores, so a test can
    /// compare a stamp it wrote against one it read back.
    fn now() -> DateTime<Utc> {
        parse_ts(stamp(Utc::now()))
    }

    fn days(n: i64) -> chrono::Duration {
        chrono::Duration::days(n)
    }

    /// Mint, store, and hold a code the way one signup does.
    fn held(s: &ControlStore, holder: &str) -> (String, i64) {
        let m = invites::mint().unwrap();
        let now = now();
        s.insert_invite(&m.code_hash, now + days(DEFAULT_TTL_DAYS))
            .unwrap();
        let id = s
            .reserve_invite(&m.code_hash, holder, now, now + days(1))
            .unwrap()
            .expect("a fresh code is available");
        (m.code_hash, id)
    }

    /// What the reservation columns say about a row, for the assertions that
    /// care that a hold was actually cleared rather than merely ignored.
    fn reservation(s: &ControlStore, id: i64) -> (Option<String>, Option<String>) {
        s.lock()
            .query_row(
                "SELECT reserved_by, reserved_until FROM invite_codes WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn an_invite_is_spent_exactly_once() {
        let s = store();
        let (code_hash, id) = held(&s, "session-a");

        s.consume_invite(id, "ada", "session-a").unwrap();
        // Both the lookup and a second consume refuse.
        assert_eq!(s.find_available_invite(&code_hash, now()).unwrap(), None);
        assert!(matches!(
            s.consume_invite(id, "grace", "session-a"),
            Err(StoreError::InviteUnavailable)
        ));
    }

    /// THE RACE F1 IS ABOUT: one code, two tabs. The second session is refused
    /// at the door instead of walking on to provision a second tenant with a
    /// code that only the first will manage to spend.
    #[test]
    fn a_held_code_is_unavailable_to_every_other_session() {
        let s = store();
        let (code_hash, id) = held(&s, "session-a");
        let now = now();

        assert_eq!(
            s.reserve_invite(&code_hash, "session-b", now, now + days(1))
                .unwrap(),
            None,
            "a second session cannot take a held code"
        );
        assert_eq!(
            s.find_available_invite(&code_hash, now).unwrap(),
            None,
            "and the gate says so too"
        );
        // The holder still has it: nothing about the refusal moved the hold.
        let (by, _) = reservation(&s, id);
        assert_eq!(by.as_deref(), Some("session-a"));
    }

    /// A signup abandoned at Google costs the code nothing beyond the session's
    /// own lifetime: the hold lapses and the next attempt takes it.
    #[test]
    fn a_lapsed_hold_frees_the_code() {
        let s = store();
        let (code_hash, id) = held(&s, "session-a");
        let later = now() + days(2);

        assert_eq!(
            s.find_available_invite(&code_hash, later).unwrap(),
            Some(id)
        );
        assert_eq!(
            s.reserve_invite(&code_hash, "session-b", later, later + days(1))
                .unwrap(),
            Some(id)
        );
        // ...and the session that lost it can no longer spend or release it.
        assert!(matches!(
            s.consume_invite(id, "ada", "session-a"),
            Err(StoreError::InviteUnavailable)
        ));
        assert!(!s.release_invite(id, "session-a").unwrap());
    }

    /// A signup that fails hands the code back, so the same person can start
    /// again immediately rather than waiting out their own reservation.
    #[test]
    fn releasing_frees_the_code_immediately() {
        let s = store();
        let (code_hash, id) = held(&s, "session-a");
        let now = now();

        assert!(s.release_invite(id, "session-a").unwrap());
        assert_eq!((None, None), reservation(&s, id));
        assert_eq!(s.find_available_invite(&code_hash, now).unwrap(), Some(id));
        assert_eq!(
            s.reserve_invite(&code_hash, "session-b", now, now + days(1))
                .unwrap(),
            Some(id)
        );
        // Releasing twice is not an error, it is simply not this session's to
        // release any more.
        assert!(!s.release_invite(id, "session-a").unwrap());
    }

    /// THE INVARIANT F1 BUYS: whoever holds the code can always spend it, so a
    /// consume after a successful provision cannot fail. Only the holder can.
    #[test]
    fn the_holder_and_only_the_holder_can_spend() {
        let s = store();
        let (_, id) = held(&s, "session-a");

        assert!(matches!(
            s.consume_invite(id, "grace", "session-b"),
            Err(StoreError::InviteUnavailable)
        ));
        s.consume_invite(id, "ada", "session-a")
            .expect("the holder always wins");
        // Spending clears the hold: nothing is left pointing at a finished
        // session.
        assert_eq!((None, None), reservation(&s, id));
        let rows = s.list_invites().unwrap();
        assert_eq!(rows[0].used_by_label.as_deref(), Some("ada"));
    }

    /// An expired code is refused exactly the way an unknown one is, and it can
    /// no longer be held.
    #[test]
    fn an_expired_code_is_simply_unavailable() {
        let s = store();
        let m = invites::mint().unwrap();
        let now = now();
        let id = s.insert_invite(&m.code_hash, now + days(30)).unwrap();

        assert_eq!(s.find_available_invite(&m.code_hash, now).unwrap(), Some(id));
        let after = now + days(31);
        assert_eq!(s.find_available_invite(&m.code_hash, after).unwrap(), None);
        assert_eq!(
            s.reserve_invite(&m.code_hash, "session-a", after, after + days(1))
                .unwrap(),
            None
        );
    }

    /// The plaintext must never reach the file. Read the whole table back as
    /// text and look for it.
    #[test]
    fn only_the_hash_is_at_rest() {
        let s = store();
        let m = invites::mint().unwrap();
        s.insert_invite(&m.code_hash, now() + days(30)).unwrap();

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
        s.insert_invite(&m.code_hash, now() + days(30)).unwrap();
        assert_eq!(
            s.find_available_invite(&invites::hash("ZZZZ-ZZZZ-ZZZZ-ZZZZ"), now())
                .unwrap(),
            None
        );
    }

    #[test]
    fn revoking_removes_only_unspent_codes() {
        let s = store();
        let (_, id_a) = held(&s, "session-a");
        let (_, id_b) = held(&s, "session-b");
        s.consume_invite(id_b, "ada", "session-b").unwrap();

        assert!(s.revoke_invite(id_a).unwrap());
        assert!(!s.revoke_invite(id_a).unwrap(), "already gone");
        assert!(!s.revoke_invite(id_b).unwrap(), "spent codes are kept");
        let rows = s.list_invites().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id_b);
        assert_eq!(rows[0].used_by_label.as_deref(), Some("ada"));
    }

    /// A file written before expiry and reservations existed opens, gains the
    /// columns, and keeps the codes already in people's inboxes working until
    /// thirty days after they were ISSUED.
    #[test]
    fn an_older_store_is_migrated_and_its_codes_dated_from_issue() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE invite_codes (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 code_hash     TEXT NOT NULL UNIQUE,
                 created_at    TEXT NOT NULL,
                 used_at       TEXT,
                 used_by_label TEXT
             );",
        )
        .unwrap();
        // An 8-character code, minted 29 days ago, exactly as the old CLI
        // stored it.
        let old = invites::hash("ABCD-EFGH");
        let issued = parse_ts(stamp(Utc::now() - days(29)));
        conn.execute(
            "INSERT INTO invite_codes(code_hash, created_at) VALUES(?1, ?2)",
            params![old, stamp(issued)],
        )
        .unwrap();

        let s = ControlStore::init(conn).unwrap();
        let row = s.list_invites().unwrap().remove(0);
        assert_eq!(
            row.expires_at,
            Some(issued + days(DEFAULT_TTL_DAYS)),
            "dated from issue, not from the migration"
        );
        assert!(
            s.find_available_invite(&old, now()).unwrap().is_some(),
            "an already-issued code keeps working"
        );
        assert!(
            s.find_available_invite(&old, now() + days(2))
                .unwrap()
                .is_none(),
            "...until the backfilled expiry passes"
        );
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

    /// What a console login asks the store, both times it asks: the label's
    /// mailbox, in the one spelling the column holds, and nothing at all for a
    /// label that was never provisioned.
    #[test]
    fn a_label_names_the_mailbox_that_owns_it() {
        let s = store();
        s.insert_tenant("ada", "Ada@Example.com").unwrap();
        assert_eq!(
            s.active_tenant_email("ada").unwrap().as_deref(),
            Some("ada@example.com")
        );
        assert_eq!(s.active_tenant_email("grace").unwrap(), None);
        // A row that is not active is not a mailbox anybody signs in to.
        s.lock()
            .execute(
                "UPDATE tenants SET status = 'stopped' WHERE label = 'ada'",
                [],
            )
            .unwrap();
        assert_eq!(s.active_tenant_email("ada").unwrap(), None);
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
