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
//!
//! THE ONE EXCEPTION IS `waitlist`, and it is a deliberate one: it holds the
//! addresses of people who have asked for the hosted tier and are not tenants
//! yet. They are stored through [`normalize_email`] like every other address
//! here, they are shown only on the operator's admin page, and they never reach
//! a log line: a waitlist log carries the ROW ID and a count, never the address
//! and never the code that was mailed to it.

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

/// A waitlist row nobody has acted on yet.
pub const WAITLIST_PENDING: &str = "pending";

/// A waitlist row an operator has approved. The invite may or may not have
/// reached them; `notified_at` is what says which.
pub const WAITLIST_APPROVED: &str = "approved";

/// How many APPROVED rows one listing carries as history. The pending half is
/// NOT capped by this: a row that falls off the page is a person the operator
/// never approves, and a repairable failed send is an approved row, so the
/// history half is the only one a cap may touch.
pub const WAITLIST_APPROVED_LIMIT: i64 = 50;

/// The ceiling on the pending half. Not a page size: a hundred-user beta cannot
/// produce a pathological count, so this is the bound that stops one listing
/// from being unbounded memory, set where reaching it means something other
/// than signups happened.
pub const WAITLIST_PENDING_LIMIT: i64 = 500;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tenants (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    label         TEXT NOT NULL UNIQUE,
    account_email TEXT NOT NULL,
    status        TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    -- The Bifrost virtual-key ID installed for this tenant, and when it was
    -- minted. THE ID ONLY: the key's value is the tenant's LLM bearer, and it
    -- passes through this process without ever reaching this file.
    bifrost_vk_id TEXT,
    vk_minted_at  TEXT
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

-- People who asked for the hosted tier before there was a code to give them.
-- One row per address, UNIQUE so a second submission of the same address is a
-- no-op rather than a second entry for the operator to work through.
CREATE TABLE IF NOT EXISTS waitlist (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    email       TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL,
    -- 'pending' | 'approved'. The approval transition is the guard that makes
    -- one click mint one invite; see `approve_waitlist`.
    status      TEXT NOT NULL,
    approved_at TEXT,
    -- The invite row minted for this person at approval. THE ID ONLY: the code
    -- and its hash live where every other invite's do.
    invite_id   INTEGER,
    -- When Resend accepted the send. NULL on an approved row means the email
    -- did not go out and the operator has a button to try again.
    notified_at TEXT
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

/// The same, for `tenants`: the virtual-key columns arrived after the first
/// hosted deployment.
const TENANT_ADDED_COLUMNS: [(&str, &str); 2] =
    [("bifrost_vk_id", "TEXT"), ("vk_minted_at", "TEXT")];

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

/// One waitlist row, as the admin page renders it.
///
/// NO `Debug`: the whole row is somebody's address, and a derived `Debug` is
/// how it would reach a log line the day a handler formats an error with the
/// row in scope.
#[derive(Clone)]
pub struct WaitlistRow {
    pub id: i64,
    /// Normalized (lowercased, trimmed), the way it was stored.
    pub email: String,
    pub created_at: DateTime<Utc>,
    /// [`WAITLIST_PENDING`] or [`WAITLIST_APPROVED`].
    pub status: String,
    pub approved_at: Option<DateTime<Utc>>,
    /// The invite minted at approval, by id.
    pub invite_id: Option<i64>,
    /// When the invite email was accepted for delivery. `None` on an approved
    /// row is the "email not sent" case.
    pub notified_at: Option<DateTime<Utc>>,
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

    /// Revoke an unspent code UNLESS a signup is holding it right now.
    ///
    /// [`Self::revoke_invite`] deliberately ignores reservations, because an
    /// operator running the CLI is overriding on purpose. The admin page is not:
    /// a re-send that revokes a held code destroys a signup that has already
    /// reached Google consent, and the person loses a grant they cannot give
    /// twice without walking the whole flow again.
    ///
    /// ONE STATEMENT, and that is the whole point of it existing. Asking
    /// [`Self::invite_is_held`] first and deleting second is two statements with
    /// the lock released in between, which is exactly long enough for a signup
    /// to take the hold the check just said was absent. The condition has to
    /// travel WITH the delete.
    ///
    /// `false` covers spent, held, and never-there alike. The caller may then
    /// ask which, because by then nothing destructive is left to do and a race
    /// only changes the sentence the operator reads.
    pub fn revoke_unheld_invite(&self, id: i64, now: DateTime<Utc>) -> Result<bool> {
        let changed = self.lock().execute(
            "DELETE FROM invite_codes
              WHERE id = ?1 AND used_at IS NULL
                AND (reserved_until IS NULL OR reserved_until <= ?2)",
            params![id, stamp(now)],
        )?;
        Ok(changed == 1)
    }

    /// Whether a signup session is holding this code RIGHT NOW.
    ///
    /// Diagnosis only, for the sentence the dashboard shows after
    /// [`Self::revoke_unheld_invite`] declined. NEVER a guard in front of a
    /// delete: see that method for why the condition has to travel with it.
    pub fn invite_is_held(&self, id: i64, now: DateTime<Utc>) -> Result<bool> {
        Ok(self
            .lock()
            .query_row(
                "SELECT 1 FROM invite_codes
                  WHERE id = ?1 AND used_at IS NULL AND reserved_until > ?2",
                params![id, stamp(now)],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
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

    /// Record the Bifrost virtual-key id installed for this tenant, stamping
    /// when it was minted in the same RFC3339 shape as every other timestamp
    /// in this file. THE ID ONLY: the key's value never comes here.
    /// Returns whether a tenant row with `label` existed to take it.
    pub fn set_tenant_vk(&self, label: &str, vk_id: &str) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE tenants SET bifrost_vk_id = ?2, vk_minted_at = ?3 WHERE label = ?1",
            params![label, vk_id, stamp(Utc::now())],
        )?;
        Ok(changed == 1)
    }

    /// The virtual-key id recorded for `label`. `None` covers both "no such
    /// tenant" and "tenant with no key"; the callers that care ask
    /// [`Self::label_exists`] first.
    pub fn tenant_vk(&self, label: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT bifrost_vk_id FROM tenants WHERE label = ?1",
                params![label],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Forget the recorded virtual key, after `llm revoke` has revoked it in
    /// Bifrost. Returns whether there was a recorded key to forget.
    pub fn clear_tenant_vk(&self, label: &str) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE tenants SET bifrost_vk_id = NULL, vk_minted_at = NULL
              WHERE label = ?1 AND bifrost_vk_id IS NOT NULL",
            params![label],
        )?;
        Ok(changed == 1)
    }

    // ---- waitlist --------------------------------------------------------

    /// Record an address that asked for the hosted tier. `true` means this
    /// submission created the row.
    ///
    /// `INSERT OR IGNORE` rather than a SELECT then an INSERT: the form is
    /// public, so two submissions can race, and a UNIQUE column plus a
    /// tolerated conflict is the only shape where the loser is a no-op instead
    /// of an error. The caller answers the SAME thing either way, so the
    /// boolean is for counting, not for the page: a route that said "already on
    /// the list" would tell a stranger who else is.
    ///
    /// THE TIMING SIDE CHANNEL IS ACCEPTED, and named here so the acceptance is
    /// visible: an ignored conflict skips the WAL write, so a duplicate answers
    /// measurably faster than a new address and a determined prober can ask
    /// whether one address is on the list. Closing it would mean writing on
    /// every submission (a row per guess) or padding the response, and neither
    /// is worth it for a list whose members are a marketing signup; the route's
    /// own rate bucket is what bounds the probing.
    pub fn add_to_waitlist(&self, email: &str) -> Result<bool> {
        let changed = self.lock().execute(
            "INSERT OR IGNORE INTO waitlist(email, created_at, status) VALUES(?1, ?2, ?3)",
            params![normalize_email(email), stamp(Utc::now()), WAITLIST_PENDING],
        )?;
        Ok(changed == 1)
    }

    /// The admin page's listing: EVERY row still waiting, oldest first, then
    /// the most recently approved as history.
    ///
    /// TWO STATEMENTS BECAUSE THE TWO HALVES ARE CAPPED DIFFERENTLY. One
    /// listing capped as a whole loses pending rows once the history fills it,
    /// and a pending row that is not on the page is a person nobody approves.
    /// So the waiting half is bounded only by [`WAITLIST_PENDING_LIMIT`], which
    /// a beta cannot reach, and the cap that bites is on history.
    ///
    /// Ordered by id rather than by `created_at` because the id IS the arrival
    /// order (AUTOINCREMENT, one insert per submission) and two rows written in
    /// the same millisecond would otherwise tie.
    pub fn list_waitlist(&self) -> Result<Vec<WaitlistRow>> {
        let conn = self.lock();
        let mut rows = select_waitlist(
            &conn,
            "SELECT id, email, created_at, status, approved_at, invite_id, notified_at
               FROM waitlist WHERE status = ?1 ORDER BY id ASC LIMIT ?2",
            params![WAITLIST_PENDING, WAITLIST_PENDING_LIMIT],
        )?;
        rows.extend(select_waitlist(
            &conn,
            "SELECT id, email, created_at, status, approved_at, invite_id, notified_at
               FROM waitlist WHERE status <> ?1 ORDER BY id DESC LIMIT ?2",
            params![WAITLIST_PENDING, WAITLIST_APPROVED_LIMIT],
        )?);
        Ok(rows)
    }

    /// One waitlist row by id, or `None` when there is no such row.
    pub fn waitlist_entry(&self, id: i64) -> Result<Option<WaitlistRow>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT id, email, created_at, status, approved_at, invite_id, notified_at
                   FROM waitlist WHERE id = ?1",
                params![id],
                waitlist_row,
            )
            .optional()?)
    }

    /// Move a row from pending to approved, atomically. `true` means THIS call
    /// made the transition.
    ///
    /// `status = 'pending'` in the WHERE clause is the whole guard: approving
    /// mints an invite and sends an email, and an operator double-clicking the
    /// button (or a replayed POST) must mint exactly one. The loser gets
    /// `Ok(false)` and says "already approved" rather than minting a second
    /// code nobody asked for.
    pub fn approve_waitlist(&self, id: i64, now: DateTime<Utc>) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE waitlist SET status = ?1, approved_at = ?2
              WHERE id = ?3 AND status = ?4",
            params![WAITLIST_APPROVED, stamp(now), id, WAITLIST_PENDING],
        )?;
        Ok(changed == 1)
    }

    /// Point a waitlist row at the invite minted for it, CLEARING the notified
    /// stamp in the same statement: a fresh code has not been delivered yet,
    /// and a stamp left over from the previous one would show "invited" for an
    /// email that has not gone out.
    ///
    /// A COMPARE-AND-SWAP, and that is what makes it the gate on the email.
    /// `expected_prior` is the pointer the caller read off the row, so the
    /// write lands only if nothing moved it in between: two sends racing (one
    /// button, two clicks, no JavaScript to stop the second) both mint, and
    /// exactly one wins the pointer. `Ok(true)` means THIS caller won and its
    /// code is the one the row names; the loser revokes what it minted and
    /// mails nothing, because a live code no row points at is a code the
    /// dashboard cannot name and no button can revoke.
    ///
    /// `IS` rather than `=`, so a `None` expectation matches the NULL a row
    /// carries before its first invite; rusqlite binds `None` as NULL.
    pub fn set_waitlist_invite(
        &self,
        id: i64,
        invite_id: i64,
        expected_prior: Option<i64>,
    ) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE waitlist SET invite_id = ?2, notified_at = NULL
              WHERE id = ?1 AND invite_id IS ?3",
            params![id, invite_id, expected_prior],
        )?;
        Ok(changed == 1)
    }

    /// Stamp the moment the provider accepted the invite email. Returns whether
    /// this stamp was the one the row wanted.
    ///
    /// NAMES THE INVITE IT IS STAMPING FOR. The send is awaited, so the row can
    /// move while a request is in flight: a second re-send can replace the
    /// pointer and start its own send, and a bare stamp by row id would let the
    /// FIRST request's success mark the SECOND request's code as delivered.
    /// The row would then read "invited" for an email that may still fail, and
    /// the badge that would have told the operator to press the button again is
    /// the one thing this page cannot afford to get wrong.
    pub fn mark_waitlist_notified(
        &self,
        id: i64,
        invite_id: i64,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE waitlist SET notified_at = ?3 WHERE id = ?1 AND invite_id = ?2",
            params![id, invite_id, stamp(now)],
        )?;
        Ok(changed == 1)
    }
}

/// Run one of the listing statements above under a lock the caller already
/// holds, so both halves of a listing read the same snapshot.
fn select_waitlist(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<WaitlistRow>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, waitlist_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One `waitlist` row in the column order every statement above selects.
fn waitlist_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WaitlistRow> {
    Ok(WaitlistRow {
        id: r.get(0)?,
        email: r.get(1)?,
        created_at: parse_ts(r.get::<_, String>(2)?),
        status: r.get(3)?,
        approved_at: r.get::<_, Option<String>>(4)?.map(parse_ts),
        invite_id: r.get(5)?,
        notified_at: r.get::<_, Option<String>>(6)?.map(parse_ts),
    })
}

/// Bring an older store's `invite_codes` up to the schema above.
///
/// `CREATE TABLE IF NOT EXISTS` leaves an existing table exactly as it found it,
/// so a file written before expiry and reservations existed needs its new
/// columns added by hand. Adding a column is the only migration shape this
/// schema has ever needed; anything bigger would want a real tool.
fn migrate(conn: &Connection) -> Result<()> {
    add_missing_columns(conn, "invite_codes", &ADDED_COLUMNS)?;
    add_missing_columns(conn, "tenants", &TENANT_ADDED_COLUMNS)?;
    backfill_expiry(conn)
}

fn add_missing_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> Result<()> {
    let existing: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
        names.collect::<rusqlite::Result<_>>()?
    };
    for (name, ty) in columns {
        if !existing.contains(*name) {
            // The table, name, and type are this file's own constants, never
            // input.
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {ty}"), [])?;
        }
    }
    Ok(())
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

    /// The raw `vk_minted_at` cell for `label`, for asserting the shape it is
    /// written in.
    fn vk_minted_at(s: &ControlStore, label: &str) -> Option<String> {
        s.lock()
            .query_row(
                "SELECT vk_minted_at FROM tenants WHERE label = ?1",
                params![label],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The vk id rides on the tenant row: set, rotate, clear, and the two
    /// nothing-there cases. Only ever the ID; the value has no path here.
    #[test]
    fn the_vk_id_rides_on_the_tenant_row() {
        let s = store();
        s.insert_tenant("ada", "ada@example.com").unwrap();

        assert_eq!(s.tenant_vk("ada").unwrap(), None);
        assert_eq!(vk_minted_at(&s, "ada"), None);
        assert!(s.set_tenant_vk("ada", "vk-1").unwrap());
        assert_eq!(s.tenant_vk("ada").unwrap(), Some("vk-1".to_string()));
        // The mint stamp is TEXT RFC3339 in the ONE shape every timestamp
        // column in this file uses, not unix seconds: it round-trips through
        // `parse_ts`/`stamp` unchanged.
        let minted = vk_minted_at(&s, "ada").expect("stamped alongside the id");
        assert_eq!(stamp(parse_ts(minted.clone())), minted, "{minted}");
        // A rotation overwrites; the store tracks only the installed key.
        assert!(s.set_tenant_vk("ada", "vk-2").unwrap());
        assert_eq!(s.tenant_vk("ada").unwrap(), Some("vk-2".to_string()));

        assert!(s.clear_tenant_vk("ada").unwrap());
        assert_eq!(s.tenant_vk("ada").unwrap(), None);
        assert_eq!(vk_minted_at(&s, "ada"), None, "cleared with the id");
        assert!(!s.clear_tenant_vk("ada").unwrap(), "nothing left to forget");

        // A label with no row takes nothing and answers nothing.
        assert!(!s.set_tenant_vk("ghost", "vk-9").unwrap());
        assert_eq!(s.tenant_vk("ghost").unwrap(), None);
    }

    /// A tenants table written before the vk columns existed opens, gains
    /// them, and takes a key id like any other row.
    #[test]
    fn an_older_tenants_table_gains_the_vk_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tenants (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 label         TEXT NOT NULL UNIQUE,
                 account_email TEXT NOT NULL,
                 status        TEXT NOT NULL,
                 created_at    TEXT NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tenants(label, account_email, status, created_at)
             VALUES('ada', 'ada@example.com', 'active', ?1)",
            params![stamp(Utc::now())],
        )
        .unwrap();

        let s = ControlStore::init(conn).unwrap();
        assert_eq!(s.tenant_vk("ada").unwrap(), None, "migrated, keyless");
        assert!(s.set_tenant_vk("ada", "vk-1").unwrap());
        assert_eq!(s.tenant_vk("ada").unwrap(), Some("vk-1".to_string()));
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

    /// The public form can be submitted twice, from two tabs, with two
    /// capitalizations. The operator sees ONE person to approve.
    #[test]
    fn an_address_joins_the_waitlist_once() {
        let s = store();
        assert!(s.add_to_waitlist("Ada@Example.com").unwrap());
        assert!(!s.add_to_waitlist("ada@example.com").unwrap());
        assert!(!s.add_to_waitlist("  ADA@EXAMPLE.COM  ").unwrap());

        let rows = s.list_waitlist().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].email, "ada@example.com");
        assert_eq!(rows[0].status, WAITLIST_PENDING);
        assert_eq!(rows[0].approved_at, None);
        assert_eq!(rows[0].invite_id, None);
        assert_eq!(rows[0].notified_at, None);
    }

    /// THE RACE THE ADMIN PAGE IS ABOUT: one row, two clicks. Only the first
    /// transition wins, so only one invite is ever minted for one person.
    #[test]
    fn a_waitlist_row_is_approved_exactly_once() {
        let s = store();
        s.add_to_waitlist("ada@example.com").unwrap();
        let id = s.list_waitlist().unwrap()[0].id;
        let now = now();

        assert!(s.approve_waitlist(id, now).unwrap());
        assert!(
            !s.approve_waitlist(id, now + days(1)).unwrap(),
            "the second click mints nothing"
        );
        let row = s.waitlist_entry(id).unwrap().expect("still there");
        assert_eq!(row.status, WAITLIST_APPROVED);
        assert_eq!(row.approved_at, Some(now), "and keeps the first stamp");

        assert!(!s.approve_waitlist(id + 1, now).unwrap(), "no such row");
        assert!(s.waitlist_entry(id + 1).unwrap().is_none());
    }

    /// The invite pointer and the delivery stamp round-trip in the same
    /// RFC3339 shape every other timestamp column here uses, and a re-send
    /// clears the old stamp so a row cannot show "invited" for a code that has
    /// not gone out.
    #[test]
    fn the_invite_and_its_delivery_stamp_ride_on_the_row() {
        let s = store();
        s.add_to_waitlist("ada@example.com").unwrap();
        let id = s.list_waitlist().unwrap()[0].id;
        let now = now();
        s.approve_waitlist(id, now).unwrap();

        assert!(s.set_waitlist_invite(id, 7, None).unwrap());
        let row = s.waitlist_entry(id).unwrap().unwrap();
        assert_eq!(row.invite_id, Some(7));
        assert_eq!(row.notified_at, None, "nothing sent yet");

        assert!(s.mark_waitlist_notified(id, 7, now).unwrap());
        assert_eq!(
            s.waitlist_entry(id).unwrap().unwrap().notified_at,
            Some(now)
        );

        // A fresh invite for the same person: new pointer, no stamp.
        assert!(s.set_waitlist_invite(id, 9, Some(7)).unwrap());
        let row = s.waitlist_entry(id).unwrap().unwrap();
        assert_eq!(row.invite_id, Some(9));
        assert_eq!(row.notified_at, None, "the old delivery is not this one");

        // A send for the code the row has MOVED OFF stamps nothing: its
        // delivery is not the one this row is waiting on.
        assert!(!s.mark_waitlist_notified(id, 7, now).unwrap());
        assert_eq!(
            s.waitlist_entry(id).unwrap().unwrap().notified_at,
            None,
            "the row still wants to hear about the code it names"
        );
        assert!(s.mark_waitlist_notified(id, 9, now).unwrap());

        assert!(
            !s.set_waitlist_invite(id + 1, 7, None).unwrap(),
            "no such row"
        );
        assert!(!s.mark_waitlist_notified(id + 1, 7, now).unwrap());
    }

    /// The reservation check has to travel WITH the delete, because two
    /// statements leave a gap a signup can take the hold in.
    #[test]
    fn a_held_invite_survives_the_admin_revoke() {
        let s = store();
        let now = now();
        let id = s
            .insert_invite("a".repeat(64).as_str(), now + days(30))
            .unwrap();

        // Held by a signup that is off at Google right now.
        assert!(
            s.reserve_invite(&"a".repeat(64), "holder", now, now + days(1))
                .is_ok()
        );
        assert!(
            !s.revoke_unheld_invite(id, now).unwrap(),
            "the hold refuses the delete"
        );
        assert!(s.invite_is_held(id, now).unwrap(), "and it is still there");

        // Once the hold lapses, the same call takes it.
        assert!(s.revoke_unheld_invite(id, now + days(2)).unwrap());
        assert!(s.list_invites().unwrap().is_empty());
    }

    /// The compare-and-swap that decides which of two racing sends gets to mail
    /// its code: the one whose expectation still matches the row.
    #[test]
    fn a_stale_expectation_loses_the_pointer() {
        let s = store();
        s.add_to_waitlist("ada@example.com").unwrap();
        let id = s.list_waitlist().unwrap()[0].id;
        s.approve_waitlist(id, now()).unwrap();

        // Both callers read the same empty pointer. The first wins it.
        assert!(s.set_waitlist_invite(id, 11, None).unwrap());
        assert!(
            !s.set_waitlist_invite(id, 12, None).unwrap(),
            "the second read a pointer that has since moved"
        );
        assert_eq!(
            s.waitlist_entry(id).unwrap().unwrap().invite_id,
            Some(11),
            "and the loser left the row alone"
        );

        // A caller that read the CURRENT pointer replaces it, which is the
        // re-send path.
        assert!(s.set_waitlist_invite(id, 13, Some(11)).unwrap());
        assert_eq!(s.waitlist_entry(id).unwrap().unwrap().invite_id, Some(13));
    }

    /// What the operator reads top to bottom: everyone still waiting, oldest
    /// first (the longest wait is the next thing to do), then the most recent
    /// approvals as history.
    #[test]
    fn the_listing_puts_the_longest_wait_first() {
        let s = store();
        for who in ["a@example.com", "b@example.com", "c@example.com"] {
            s.add_to_waitlist(who).unwrap();
        }
        let ids: Vec<i64> = s.list_waitlist().unwrap().iter().map(|r| r.id).collect();
        s.approve_waitlist(ids[0], now()).unwrap();
        s.approve_waitlist(ids[1], now()).unwrap();

        let rows = s.list_waitlist().unwrap();
        let emails: Vec<&str> = rows.iter().map(|r| r.email.as_str()).collect();
        assert_eq!(
            emails,
            vec!["c@example.com", "b@example.com", "a@example.com"],
            "pending oldest-first, then approved newest-first"
        );
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
