//! The control store: tenants and invite codes, in this crate's own small
//! Postgres database.
//!
//! Deliberately NOT the daemon's store, and since the Postgres port not even
//! the same engine: a tenant's mail lives in a SQLite file on that tenant's own
//! volume, one file per mailbox, and this holds a handful of rows describing
//! who has been provisioned. It must never be able to open a tenant's mail
//! database, and now it could not if it tried.
//!
//! THE POOL IS FOR RESILIENCE, NOT THROUGHPUT. This service's concurrency is a
//! handful of signups; what a single `Client` cannot survive is the connection
//! task dying, which a managed Postgres does on every maintenance restart and a
//! private network does on a blip. That client is then permanently broken and
//! every request after it fails until the process is restarted. The pool
//! notices and dials again. Every method here is still one short statement; the
//! two that are not ([`ControlStore::init`] and [`ControlStore::list_waitlist`])
//! say at their own definitions why they are a transaction.
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

use chrono::{DateTime, SecondsFormat, Utc};
use deadpool_postgres::{Client, Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;
use tokio_postgres::{IsolationLevel, NoTls, Row, Transaction};

use crate::invites::DEFAULT_TTL_DAYS;

/// Connections the pool will open. Four, because the reason it exists is
/// surviving a dropped connection rather than serving load: the hosted tier is
/// a handful of concurrent signups, and every statement here is a point lookup
/// or a single-row write.
const POOL_MAX_SIZE: usize = 4;

/// The advisory-lock key [`ControlStore::init`] holds while it creates and
/// migrates. Any fixed number would do; this one spells `SQCTLINI` in ASCII so
/// a `pg_locks` row is recognizable as ours rather than looking like a hash
/// collision with something else's lock.
///
/// A TRANSACTION-SCOPED lock (`pg_advisory_xact_lock`), so it is released by
/// the commit or the rollback and never by a process remembering to. Two
/// deploys overlapping, or an operator running the CLI while the service boots,
/// would otherwise run `CREATE TABLE IF NOT EXISTS` and `ADD COLUMN IF NOT
/// EXISTS` against each other, which Postgres answers with a duplicate-object
/// error rather than the no-op the `IF NOT EXISTS` promises.
const INIT_LOCK_KEY: i64 = 0x5351_4354_4c49_4e49;

/// The UNIQUE constraint on `tenants.label`, NAMED so [`ControlStore::insert_tenant`]
/// can tell it apart from the partial email index in a violation. Postgres puts
/// the constraint's name in the error and nothing else that identifies it, so
/// this string is load-bearing in two places at once: the DDL below and the
/// match in that method.
const TENANTS_LABEL_KEY: &str = "tenants_label_key";

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

/// TIMESTAMPS ARE RFC3339 TEXT, AND EVERY ONE OF THEM IS `COLLATE "C"`.
///
/// The shape is [`stamp`]'s and did not change with the engine, because the
/// availability checks compare `expires_at` and `reserved_until` against a
/// bound `now` AS STRINGS. SQLite compares TEXT byte by byte and has no other
/// option; Postgres compares it under the COLUMN'S COLLATION, which on a
/// database created in a `en_US.UTF-8` locale ignores punctuation at the first
/// level — exactly the `-`, `:`, `.`, `T` and `Z` these stamps are made of. Two
/// spellings of the same instant would then order wrongly and a live code would
/// read as expired. `COLLATE "C"` is memcmp, which is the promise the format
/// was designed against.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tenants (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label         TEXT NOT NULL,
    account_email TEXT NOT NULL,
    status        TEXT NOT NULL,
    created_at    TEXT COLLATE \"C\" NOT NULL,
    -- The Bifrost virtual-key IDs installed for this tenant — triage and
    -- assistant — and when each was minted. THE IDS ONLY: the keys' values
    -- are the tenant's LLM bearers, and they pass through this process
    -- without ever reaching this database.
    bifrost_vk_id           TEXT,
    vk_minted_at            TEXT COLLATE \"C\",
    bifrost_assistant_vk_id TEXT,
    assistant_vk_minted_at  TEXT COLLATE \"C\",
    -- NAMED, and the name is `TENANTS_LABEL_KEY`. An anonymous UNIQUE would
    -- still be told apart by whatever name Postgres invented for it, which is
    -- a thing to guess rather than a thing to read.
    CONSTRAINT tenants_label_key UNIQUE (label)
);
-- One mailbox, one daemon. A PARTIAL unique index rather than a plain one, so a
-- tenant that has been torn down frees its address for a later signup while an
-- active one cannot be duplicated by two requests racing past the SELECT.
CREATE UNIQUE INDEX IF NOT EXISTS idx_tenants_active_email
    ON tenants(account_email) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS invite_codes (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    code_hash      TEXT NOT NULL UNIQUE,
    created_at     TEXT COLLATE \"C\" NOT NULL,
    -- When the code stops being usable, whether or not anyone spent it.
    expires_at     TEXT COLLATE \"C\",
    used_at        TEXT COLLATE \"C\",
    used_by_label  TEXT,
    -- The signup session currently holding this code, by fingerprint, and until
    -- when. A live reservation makes the code unavailable to every other
    -- session; it self-releases when `reserved_until` passes.
    reserved_by    TEXT,
    reserved_until TEXT COLLATE \"C\"
);

-- People who asked for the hosted tier before there was a code to give them.
-- One row per address, UNIQUE so a second submission of the same address is a
-- no-op rather than a second entry for the operator to work through.
CREATE TABLE IF NOT EXISTS waitlist (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email       TEXT NOT NULL UNIQUE,
    created_at  TEXT COLLATE \"C\" NOT NULL,
    -- 'pending' | 'approved'. The approval transition is the guard that makes
    -- one click mint one invite; see `approve_waitlist`.
    status      TEXT NOT NULL,
    approved_at TEXT COLLATE \"C\",
    -- The invite row minted for this person at approval. THE ID ONLY: the code
    -- and its hash live where every other invite's do.
    invite_id   BIGINT,
    -- When Resend accepted the send. NULL on an approved row means the email
    -- did not go out and the operator has a button to try again.
    notified_at TEXT COLLATE \"C\"
);
";

/// Columns added to `invite_codes` after the first deployment, with the type
/// each is declared with. A database opened from an older shape gets them here
/// rather than needing a migration tool: `CREATE TABLE IF NOT EXISTS` above
/// leaves an existing table exactly as it found it.
///
/// The timestamp columns carry their `COLLATE "C"` here too, for the reason
/// [`SCHEMA`] gives: a column that gained its collation by being created rather
/// than by being added would compare differently from its neighbour, and the
/// difference would only show up as a live code reading as expired.
const ADDED_COLUMNS: [(&str, &str); 3] = [
    ("expires_at", "TEXT COLLATE \"C\""),
    ("reserved_by", "TEXT"),
    ("reserved_until", "TEXT COLLATE \"C\""),
];

/// The same, for `tenants`: the triage virtual-key columns arrived after the
/// first hosted deployment, and the assistant pair after them.
const TENANT_ADDED_COLUMNS: [(&str, &str); 4] = [
    ("bifrost_vk_id", "TEXT"),
    ("vk_minted_at", "TEXT COLLATE \"C\""),
    ("bifrost_assistant_vk_id", "TEXT"),
    ("assistant_vk_minted_at", "TEXT COLLATE \"C\""),
];

/// Store errors.
///
/// `Pg` CARRIES A MESSAGE THAT IS SAFE TO LOG, AND ONLY BECAUSE OF HOW IT IS
/// LOGGED. tokio-postgres's `Display` is the KIND of failure — "db error",
/// "connection closed" — and never the server's DETAIL line, which on a unique
/// violation spells the conflicting values: for `idx_tenants_active_email` that
/// is somebody's email address. The detail hangs off the error as a SOURCE, so
/// `{:?}` prints it and `%e` does not. Every line in this crate that carries one
/// of these writes `error = %e`, and that is a rule rather than a habit.
///
/// `Pool` is the pool refusing to hand out a connection: exhausted, closed, or
/// the backend failing to dial. Its `Display` names the same kinds and no
/// connection string, so the password in the URL cannot ride out on it.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("control store: {0}")]
    Pg(#[from] tokio_postgres::Error),
    #[error("control store: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),
    /// The pool would not be built. deadpool has exactly one such failure —
    /// timeouts configured with no runtime — and this pool configures no
    /// timeouts, so it exists to keep [`ControlStore::connect`] total rather
    /// than because anything is expected to produce it.
    #[error("control store: {0}")]
    Build(#[from] deadpool_postgres::BuildError),
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
    /// When this row's invite was SPENT, from the code it was minted: the
    /// moment somebody finished signup and a mailbox came into existence.
    ///
    /// Joined rather than stored, because `invite_codes` is already the one
    /// place redemption is written and a copy here would be a second truth to
    /// keep in step. `None` means the code is still outstanding (or the row was
    /// never approved, or its code was revoked from the CLI and the row now
    /// points at nothing).
    pub accepted_at: Option<DateTime<Utc>>,
    /// The mailbox that code became, by label. Present exactly when
    /// [`Self::accepted_at`] is.
    pub accepted_label: Option<String>,
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
    pool: Pool,
}

impl ControlStore {
    /// Open the control store at `database_url`, creating and migrating the
    /// schema.
    ///
    /// NOTHING IS DIALED HERE. A deadpool pool is lazy, so this returns before
    /// any TCP connection exists and the first real connection is made by
    /// [`Self::init`] on the line below — which is what turns "the database is
    /// unreachable" into a startup failure rather than a 500 on the first
    /// signup.
    ///
    /// `NoTls`, and that is a deployment fact rather than a shortcut: this
    /// travels Railway's private network (`postgres.railway.internal`) and
    /// never leaves the project. Pointing it at the PUBLIC proxy means adding
    /// `tokio-postgres-rustls` and handing a `MakeTlsConnector` to the manager
    /// here; nothing else in this file would change.
    pub async fn connect(database_url: &str) -> Result<Self> {
        // Parsed here rather than handed to deadpool's own config type, so a
        // malformed URL fails before a pool exists. The error is
        // `invalid connection string` and carries no part of the string, which
        // matters because the string carries the password.
        let pg_config: tokio_postgres::Config = database_url.parse()?;
        let manager = Manager::from_config(
            pg_config,
            NoTls,
            ManagerConfig {
                // `Fast` checks `is_closed()` on the way out of the pool and
                // runs no test query. The statements here are all one round
                // trip, so a connection that died between checkout and use
                // costs one retriable error rather than a corrupted write.
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let store = Self {
            pool: Pool::builder(manager).max_size(POOL_MAX_SIZE).build()?,
        };
        store.init().await?;
        Ok(store)
    }

    /// Create the schema and migrate it, ON EVERY CONNECT.
    ///
    /// Idempotent by construction (`IF NOT EXISTS` throughout), which is what
    /// lets the serving process and the operator CLI each run it without
    /// anybody deciding who owns the schema.
    ///
    /// ONE TRANSACTION HOLDING [`INIT_LOCK_KEY`], because `IF NOT EXISTS` is a
    /// promise about the state of the catalog and not a lock on it: two
    /// deployments overlapping (Railway starts the new pod before it stops the
    /// old one) run these statements against each other, and Postgres answers
    /// the loser with a duplicate-object error instead of the no-op the clause
    /// promises. The lock serializes them; the transaction is what releases it
    /// whichever way this ends.
    async fn init(&self) -> Result<()> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&INIT_LOCK_KEY])
            .await?;
        tx.batch_execute(SCHEMA).await?;
        migrate(&tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// One connection out of the pool.
    ///
    /// Replaces the `MutexGuard<Connection>` the SQLite store handed out, and
    /// weakens nothing: every guard in this file travels INSIDE the single
    /// statement it protects, so two handlers on two connections race exactly
    /// the way two handlers taking one mutex in turn did. The one place the
    /// lock WAS the guarantee is [`Self::list_waitlist`], which says so and
    /// takes a transaction instead.
    ///
    /// `pub(crate)` rather than private, for the two callers that are not
    /// handlers: the importer ([`crate::import`]), which writes rows this file
    /// has no method for and must not grow one, and this file's own tests,
    /// which assert on columns no method returns.
    pub(crate) async fn client(&self) -> Result<Client> {
        Ok(self.pool.get().await?)
    }

    // ---- invite codes ----------------------------------------------------

    /// Record a freshly minted code by hash, with the moment it stops working.
    /// The plaintext never comes here.
    pub async fn insert_invite(&self, code_hash: &str, expires_at: DateTime<Utc>) -> Result<i64> {
        let row = self
            .client()
            .await?
            .query_one(
                "INSERT INTO invite_codes(code_hash, created_at, expires_at)
                 VALUES($1, $2, $3) RETURNING id",
                &[&code_hash, &stamp(Utc::now()), &stamp(expires_at)],
            )
            .await?;
        Ok(row.try_get(0)?)
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
    pub async fn find_available_invite(
        &self,
        code_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT id FROM invite_codes
                  WHERE code_hash = $1
                    AND used_at IS NULL
                    AND (expires_at IS NULL OR expires_at > $2)
                    AND (reserved_until IS NULL OR reserved_until <= $2)",
                &[&code_hash, &stamp(now)],
            )
            .await?;
        row.map(|r| Ok(r.try_get(0)?)).transpose()
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
    pub async fn reserve_invite(
        &self,
        code_hash: &str,
        holder: &str,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "UPDATE invite_codes SET reserved_by = $2, reserved_until = $3
                  WHERE code_hash = $1
                    AND used_at IS NULL
                    AND (expires_at IS NULL OR expires_at > $4)
                    AND (reserved_until IS NULL OR reserved_until <= $4)
                RETURNING id",
                &[&code_hash, &holder, &stamp(until), &stamp(now)],
            )
            .await?;
        row.map(|r| Ok(r.try_get(0)?)).transpose()
    }

    /// Hand back a held code without spending it, so the person holding it can
    /// start again immediately rather than waiting out the reservation.
    ///
    /// Only the holder can release: a reservation that has already expired and
    /// been taken by somebody else must not be torn out from under them by a
    /// late failure path. Returns whether this holder was still the one holding
    /// it.
    pub async fn release_invite(&self, id: i64, holder: &str) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "UPDATE invite_codes SET reserved_by = NULL, reserved_until = NULL
                 WHERE id = $1 AND reserved_by = $2 AND used_at IS NULL",
                &[&id, &holder],
            )
            .await?;
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
    pub async fn consume_invite(&self, id: i64, label: &str, holder: &str) -> Result<()> {
        let changed = self
            .client()
            .await?
            .execute(
                "UPDATE invite_codes
                    SET used_at = $1, used_by_label = $2, reserved_by = NULL, reserved_until = NULL
                  WHERE id = $3 AND used_at IS NULL AND reserved_by = $4",
                &[&stamp(Utc::now()), &label, &id, &holder],
            )
            .await?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::InviteUnavailable)
        }
    }

    /// Every invite, newest first. Hashes are not selected.
    pub async fn list_invites(&self) -> Result<Vec<InviteRow>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT id, created_at, expires_at, used_at, used_by_label
                   FROM invite_codes ORDER BY id DESC",
                &[],
            )
            .await?;
        rows.iter().map(invite_row).collect()
    }

    /// Revoke an UNSPENT code by id. Returns false when there was no unspent
    /// code with that id, which covers both "already used" and "no such row":
    /// an operator command may distinguish them, so the caller reports what it
    /// sees rather than this returning a reason.
    ///
    /// Deleting rather than flagging: a revoked code has no history worth
    /// keeping, and a row that stays behind with `used_at` set would lie in
    /// `invite list` about a signup that never happened.
    pub async fn revoke_invite(&self, id: i64) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "DELETE FROM invite_codes WHERE id = $1 AND used_at IS NULL",
                &[&id],
            )
            .await?;
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
    /// nothing holding the row in between, which is exactly long enough for a
    /// signup to take the hold the check just said was absent. The condition has
    /// to travel WITH the delete.
    ///
    /// `false` covers spent, held, and never-there alike. The caller may then
    /// ask which, because by then nothing destructive is left to do and a race
    /// only changes the sentence the operator reads.
    pub async fn revoke_unheld_invite(&self, id: i64, now: DateTime<Utc>) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "DELETE FROM invite_codes
                  WHERE id = $1 AND used_at IS NULL
                    AND (reserved_until IS NULL OR reserved_until <= $2)",
                &[&id, &stamp(now)],
            )
            .await?;
        Ok(changed == 1)
    }

    /// Whether a signup session is holding this code RIGHT NOW.
    ///
    /// Diagnosis only, for the sentence the dashboard shows after
    /// [`Self::revoke_unheld_invite`] declined. NEVER a guard in front of a
    /// delete: see that method for why the condition has to travel with it.
    pub async fn invite_is_held(&self, id: i64, now: DateTime<Utc>) -> Result<bool> {
        Ok(self
            .client()
            .await?
            .query_opt(
                "SELECT 1 FROM invite_codes
                  WHERE id = $1 AND used_at IS NULL AND reserved_until > $2",
                &[&id, &stamp(now)],
            )
            .await?
            .is_some())
    }

    // ---- tenants ---------------------------------------------------------

    /// Whether this control plane has already recorded a tenant with `label`.
    /// The warden is asked the same question separately; this catches the case
    /// where a provision succeeded and only the record is being repeated.
    pub async fn label_exists(&self, label: &str) -> Result<bool> {
        Ok(self
            .client()
            .await?
            .query_opt("SELECT 1 FROM tenants WHERE label = $1", &[&label])
            .await?
            .is_some())
    }

    /// The label of the ACTIVE tenant for this mailbox, if any. One mailbox,
    /// one daemon: a second signup from the same Google account is refused
    /// politely rather than provisioned into a second, competing sync loop.
    pub async fn active_tenant_for_email(&self, account_email: &str) -> Result<Option<String>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT label FROM tenants WHERE account_email = $1 AND status = $2",
                &[&normalize_email(account_email), &STATUS_ACTIVE],
            )
            .await?;
        row.map(|r| Ok(r.try_get(0)?)).transpose()
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
    pub async fn active_tenant_email(&self, label: &str) -> Result<Option<String>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT account_email FROM tenants WHERE label = $1 AND status = $2",
                &[&label, &STATUS_ACTIVE],
            )
            .await?;
        row.map(|r| Ok(r.try_get(0)?)).transpose()
    }

    /// Record a provisioned tenant. Both unique constraints are mapped to
    /// their own errors rather than surfacing a Postgres message: these two
    /// races are expected (two tabs, two signups) and the pages above say
    /// different things about them.
    pub async fn insert_tenant(&self, label: &str, account_email: &str) -> Result<()> {
        let email = normalize_email(account_email);
        let res = self
            .client()
            .await?
            .execute(
                "INSERT INTO tenants(label, account_email, status, created_at)
                 VALUES($1, $2, $3, $4)",
                &[&label, &email, &STATUS_ACTIVE, &stamp(Utc::now())],
            )
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) => {
                // SQLSTATE 23505 plus the constraint NAME, which is the one
                // field Postgres fills in with something this file chose: the
                // label constraint and the partial email index are told apart
                // by it. A violation we do not recognize (and a unique index
                // added later without a line here) falls through as the account
                // error, which is the more conservative of the two: it does not
                // invite a retry that would provision a second daemon for one
                // mailbox. Copied out of the error before the match, so the
                // fall-through arm still owns `e`.
                let constraint = e
                    .as_db_error()
                    .filter(|db| *db.code() == SqlState::UNIQUE_VIOLATION)
                    .map(|db| db.constraint().unwrap_or_default().to_string());
                match constraint {
                    Some(name) if name == TENANTS_LABEL_KEY => Err(StoreError::LabelTaken),
                    Some(_) => Err(StoreError::AccountTaken),
                    None => Err(e.into()),
                }
            }
        }
    }

    /// Every tenant, newest first. For the operator CLI.
    pub async fn list_tenants(&self) -> Result<Vec<TenantRow>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT label, account_email, status, created_at FROM tenants ORDER BY id DESC",
                &[],
            )
            .await?;
        rows.iter().map(tenant_row).collect()
    }

    /// Record the Bifrost virtual-key id installed for this tenant, stamping
    /// when it was minted in the same RFC3339 shape as every other timestamp
    /// in this schema. THE ID ONLY: the key's value never comes here.
    /// Returns whether a tenant row with `label` existed to take it.
    pub async fn set_tenant_vk(&self, label: &str, vk_id: &str) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "UPDATE tenants SET bifrost_vk_id = $2, vk_minted_at = $3 WHERE label = $1",
                &[&label, &vk_id, &stamp(Utc::now())],
            )
            .await?;
        Ok(changed == 1)
    }

    /// The virtual-key id recorded for `label`. `None` covers both "no such
    /// tenant" and "tenant with no key"; the callers that care ask
    /// [`Self::label_exists`] first.
    pub async fn tenant_vk(&self, label: &str) -> Result<Option<String>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT bifrost_vk_id FROM tenants WHERE label = $1",
                &[&label],
            )
            .await?;
        Ok(row
            .map(|r| r.try_get::<_, Option<String>>(0))
            .transpose()?
            .flatten())
    }

    /// Forget the recorded virtual key, after `llm revoke` has revoked it in
    /// Bifrost. Returns whether there was a recorded key to forget.
    pub async fn clear_tenant_vk(&self, label: &str) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "UPDATE tenants SET bifrost_vk_id = NULL, vk_minted_at = NULL
                  WHERE label = $1 AND bifrost_vk_id IS NOT NULL",
                &[&label],
            )
            .await?;
        Ok(changed == 1)
    }

    /// Record the ASSISTANT virtual-key id, the same way and under the same
    /// rule as [`Self::set_tenant_vk`]: the id only, stamped RFC3339.
    /// Returns whether a tenant row with `label` existed to take it.
    pub async fn set_tenant_assistant_vk(&self, label: &str, vk_id: &str) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "UPDATE tenants SET bifrost_assistant_vk_id = $2, assistant_vk_minted_at = $3
                  WHERE label = $1",
                &[&label, &vk_id, &stamp(Utc::now())],
            )
            .await?;
        Ok(changed == 1)
    }

    /// The assistant virtual-key id recorded for `label`. `None` covers both
    /// "no such tenant" and "tenant with no key", like [`Self::tenant_vk`].
    pub async fn tenant_assistant_vk(&self, label: &str) -> Result<Option<String>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT bifrost_assistant_vk_id FROM tenants WHERE label = $1",
                &[&label],
            )
            .await?;
        Ok(row
            .map(|r| r.try_get::<_, Option<String>>(0))
            .transpose()?
            .flatten())
    }

    /// Forget the recorded assistant key, after a revoke has landed in
    /// Bifrost. Returns whether there was a recorded key to forget.
    pub async fn clear_tenant_assistant_vk(&self, label: &str) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "UPDATE tenants SET bifrost_assistant_vk_id = NULL, assistant_vk_minted_at = NULL
                  WHERE label = $1 AND bifrost_assistant_vk_id IS NOT NULL",
                &[&label],
            )
            .await?;
        Ok(changed == 1)
    }

    // ---- waitlist --------------------------------------------------------

    /// Record an address that asked for the hosted tier. `true` means this
    /// submission created the row.
    ///
    /// `ON CONFLICT DO NOTHING` rather than a SELECT then an INSERT: the form is
    /// public, so two submissions can race, and a UNIQUE column plus a
    /// tolerated conflict is the only shape where the loser is a no-op instead
    /// of an error. The caller answers the SAME thing either way, so the
    /// boolean is for counting, not for the page: a route that said "already on
    /// the list" would tell a stranger who else is.
    ///
    /// THE TIMING SIDE CHANNEL IS ACCEPTED, and named here so the acceptance is
    /// visible: an ignored conflict skips the heap write, so a duplicate answers
    /// measurably faster than a new address and a determined prober can ask
    /// whether one address is on the list. Closing it would mean writing on
    /// every submission (a row per guess) or padding the response, and neither
    /// is worth it for a list whose members are a marketing signup; the route's
    /// own rate bucket is what bounds the probing.
    pub async fn add_to_waitlist(&self, email: &str) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "INSERT INTO waitlist(email, created_at, status) VALUES($1, $2, $3)
                 ON CONFLICT DO NOTHING",
                &[
                    &normalize_email(email),
                    &stamp(Utc::now()),
                    &WAITLIST_PENDING,
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    /// Put an address straight onto the approved half, whether or not it ever
    /// asked. `Some(id)` is the row this call approved and is the caller's to
    /// mint for; `None` means it was approved already and there is nothing new
    /// to send.
    ///
    /// ONE STATEMENT, because the operator typing an address and the operator
    /// clicking Approve on that same address are the same race
    /// [`Self::approve_waitlist`] guards, and it has to hold across an INSERT
    /// the second one turns into an UPDATE. The upsert's `WHERE` is the guard:
    /// it promotes a pending row and refuses an approved one, so two presses
    /// mint exactly one invite between them, and `RETURNING` hands back the
    /// winner's id without a second lookup that another writer could
    /// invalidate.
    ///
    /// A direct invite is recorded as a waitlist row on purpose. The alternative
    /// is a second ledger with the same columns, and then two places to look for
    /// "did we already invite them", two things to page, and one of them
    /// silently missing the re-send button.
    pub async fn invite_directly(&self, email: &str, now: DateTime<Utc>) -> Result<Option<i64>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "INSERT INTO waitlist(email, created_at, status, approved_at)
                      VALUES($1, $2, $3, $2)
                 ON CONFLICT(email) DO UPDATE SET status = $3, approved_at = $2
                      WHERE waitlist.status = $4
                 RETURNING id",
                &[
                    &normalize_email(email),
                    &stamp(now),
                    &WAITLIST_APPROVED,
                    &WAITLIST_PENDING,
                ],
            )
            .await?;
        row.map(|r| Ok(r.try_get(0)?)).transpose()
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
    /// TWO STATEMENTS, ONE SNAPSHOT, and on a pool that takes saying so. Under
    /// the SQLite store the mutex was the guarantee: both halves ran under one
    /// lock, so an approval landing between them could not put a row in both
    /// halves or in neither. A pooled connection has no such lock, so the pair
    /// runs inside one REPEATABLE READ, READ ONLY transaction, which is the
    /// same promise written where a reader can see it.
    ///
    /// Ordered by id rather than by `created_at` because the id IS the arrival
    /// order (an identity column, one insert per submission) and two rows
    /// written in the same millisecond would otherwise tie.
    pub async fn list_waitlist(&self) -> Result<Vec<WaitlistRow>> {
        let mut client = self.client().await?;
        let tx = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        let mut rows = select_waitlist(
            &tx,
            &format!("SELECT {WAITLIST_COLUMNS} WHERE w.status = $1 ORDER BY w.id ASC LIMIT $2"),
            &[&WAITLIST_PENDING, &WAITLIST_PENDING_LIMIT],
        )
        .await?;
        rows.extend(
            select_waitlist(
                &tx,
                &format!(
                    "SELECT {WAITLIST_COLUMNS} WHERE w.status <> $1 ORDER BY w.id DESC LIMIT $2"
                ),
                &[&WAITLIST_PENDING, &WAITLIST_APPROVED_LIMIT],
            )
            .await?,
        );
        // Nothing was written, so committing and rolling back leave the same
        // database behind. It is closed EXPLICITLY anyway: a dropped
        // transaction queues its own `ROLLBACK` on the connection, which is a
        // second round trip on a path that just did two, and a snapshot held
        // open by a destructor is the shape that pins an old row version when
        // somebody later adds a write here.
        tx.commit().await?;
        Ok(rows)
    }

    /// One waitlist row by id, or `None` when there is no such row.
    pub async fn waitlist_entry(&self, id: i64) -> Result<Option<WaitlistRow>> {
        let row = self
            .client()
            .await?
            .query_opt(
                &format!("SELECT {WAITLIST_COLUMNS} WHERE w.id = $1"),
                &[&id],
            )
            .await?;
        row.as_ref().map(waitlist_row).transpose()
    }

    /// Move a row from pending to approved, atomically. `true` means THIS call
    /// made the transition.
    ///
    /// `status = 'pending'` in the WHERE clause is the whole guard: approving
    /// mints an invite and sends an email, and an operator double-clicking the
    /// button (or a replayed POST) must mint exactly one. The loser gets
    /// `Ok(false)` and says "already approved" rather than minting a second
    /// code nobody asked for.
    pub async fn approve_waitlist(&self, id: i64, now: DateTime<Utc>) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "UPDATE waitlist SET status = $1, approved_at = $2
                  WHERE id = $3 AND status = $4",
                &[&WAITLIST_APPROVED, &stamp(now), &id, &WAITLIST_PENDING],
            )
            .await?;
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
    /// `IS NOT DISTINCT FROM` rather than `=`, so a `None` expectation matches
    /// the NULL a row carries before its first invite; tokio-postgres binds
    /// `None` as NULL, and `= NULL` is NULL, which is not true.
    pub async fn set_waitlist_invite(
        &self,
        id: i64,
        invite_id: i64,
        expected_prior: Option<i64>,
    ) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "UPDATE waitlist SET invite_id = $2, notified_at = NULL
                  WHERE id = $1 AND invite_id IS NOT DISTINCT FROM $3",
                &[&id, &invite_id, &expected_prior],
            )
            .await?;
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
    pub async fn mark_waitlist_notified(
        &self,
        id: i64,
        invite_id: i64,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let changed = self
            .client()
            .await?
            .execute(
                "UPDATE waitlist SET notified_at = $3 WHERE id = $1 AND invite_id = $2",
                &[&id, &invite_id, &stamp(now)],
            )
            .await?;
        Ok(changed == 1)
    }
}

/// Run one of the listing statements above inside a transaction the caller
/// already opened, so both halves of a listing read the same snapshot.
async fn select_waitlist(
    tx: &Transaction<'_>,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
) -> Result<Vec<WaitlistRow>> {
    let rows = tx.query(sql, params).await?;
    rows.iter().map(waitlist_row).collect()
}

/// One `waitlist` row in the column order every statement above selects.
fn waitlist_row(r: &Row) -> Result<WaitlistRow> {
    Ok(WaitlistRow {
        id: r.try_get(0)?,
        email: r.try_get(1)?,
        created_at: parse_ts(r.try_get::<_, String>(2)?),
        status: r.try_get(3)?,
        approved_at: r.try_get::<_, Option<String>>(4)?.map(parse_ts),
        invite_id: r.try_get(5)?,
        notified_at: r.try_get::<_, Option<String>>(6)?.map(parse_ts),
        accepted_at: r.try_get::<_, Option<String>>(7)?.map(parse_ts),
        accepted_label: r.try_get(8)?,
    })
}

/// One `invite_codes` row in the column order [`ControlStore::list_invites`]
/// selects.
fn invite_row(r: &Row) -> Result<InviteRow> {
    Ok(InviteRow {
        id: r.try_get(0)?,
        created_at: parse_ts(r.try_get::<_, String>(1)?),
        expires_at: r.try_get::<_, Option<String>>(2)?.map(parse_ts),
        used_at: r.try_get::<_, Option<String>>(3)?.map(parse_ts),
        used_by_label: r.try_get(4)?,
    })
}

/// One `tenants` row in the column order [`ControlStore::list_tenants`]
/// selects.
fn tenant_row(r: &Row) -> Result<TenantRow> {
    Ok(TenantRow {
        label: r.try_get(0)?,
        account_email: r.try_get(1)?,
        status: r.try_get(2)?,
        created_at: parse_ts(r.try_get::<_, String>(3)?),
    })
}

/// The columns every waitlist statement selects, in the order
/// [`waitlist_row`] reads them.
///
/// The LEFT JOIN is what makes redemption visible without a second round trip
/// and without a column of its own: `invite_codes` already records who spent a
/// code and when, and `waitlist.invite_id` already points at the row. LEFT, not
/// inner, because a pending row has no invite and an approved row whose code
/// was revoked from the CLI points at nothing; both must still be listed.
const WAITLIST_COLUMNS: &str = "w.id, w.email, w.created_at, w.status, w.approved_at,
        w.invite_id, w.notified_at, i.used_at, i.used_by_label
   FROM waitlist w LEFT JOIN invite_codes i ON i.id = w.invite_id";

/// Bring an older database's tables up to the schema above.
///
/// `CREATE TABLE IF NOT EXISTS` leaves an existing table exactly as it found it,
/// so tables written before expiry and reservations existed need their new
/// columns added by hand. Adding a column is the only migration shape this
/// schema has ever needed; anything bigger would want a real tool.
///
/// Runs inside [`ControlStore::init`]'s advisory-locked transaction, so the
/// whole shape either arrives or does not: on Postgres, unlike on SQLite, DDL
/// is transactional.
async fn migrate(tx: &Transaction<'_>) -> Result<()> {
    add_missing_columns(tx, "invite_codes", &ADDED_COLUMNS).await?;
    add_missing_columns(tx, "tenants", &TENANT_ADDED_COLUMNS).await?;
    backfill_expiry(tx).await
}

/// `ADD COLUMN IF NOT EXISTS` per column, which is the whole of it on Postgres:
/// the catalog read the SQLite version needed (`PRAGMA table_info`, then a
/// membership test) is what the server does itself here, under the same lock as
/// the ALTER.
async fn add_missing_columns(
    tx: &Transaction<'_>,
    table: &str,
    columns: &[(&str, &str)],
) -> Result<()> {
    for (name, ty) in columns {
        // The table, name, and type are this file's own constants, never
        // input.
        tx.execute(
            &format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {name} {ty}"),
            &[],
        )
        .await?;
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
async fn backfill_expiry(tx: &Transaction<'_>) -> Result<()> {
    let rows = tx
        .query(
            "SELECT id, created_at FROM invite_codes
              WHERE expires_at IS NULL AND used_at IS NULL",
            &[],
        )
        .await?;
    for row in &rows {
        let id: i64 = row.try_get(0)?;
        let created_at: String = row.try_get(1)?;
        let expires = parse_ts(created_at) + chrono::Duration::days(DEFAULT_TTL_DAYS);
        tx.execute(
            "UPDATE invite_codes SET expires_at = $1 WHERE id = $2",
            &[&stamp(expires), &id],
        )
        .await?;
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
/// against a bound `now` by the database, which compares TEXT byte by byte
/// under the `COLLATE "C"` every timestamp column carries (see [`SCHEMA`]), so
/// two spellings of the same instant would order wrongly and a live code would
/// read as expired.
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

    /// What to do about a missing [`TEST_URL_VAR`], as one sentence with the
    /// two commands in it.
    ///
    /// A LOUD PANIC RATHER THAN A SKIP, decided deliberately: a suite that
    /// skips itself when the database is absent reports green while testing
    /// nothing, and the day that matters is the day somebody's CI forgets the
    /// service container.
    const NO_TEST_DATABASE: &str = "\
SQUELCH_TEST_PG_URL is not set, and these tests run against a real Postgres.

    docker run -d --name squelch-pg -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:16
    export SQUELCH_TEST_PG_URL=postgres://postgres:postgres@localhost:5432/postgres
";

    /// The database every test in this file connects to. One server, a schema
    /// per test.
    const TEST_URL_VAR: &str = "SQUELCH_TEST_PG_URL";

    /// How long a leftover test schema is kept before the next run drops it.
    /// Long enough that a slow test still owns its schema, short enough that a
    /// developer's database does not fill up with them.
    const SCHEMA_TTL_SECS: i64 = 3600;

    /// A store of its own, on a schema of its own.
    ///
    /// ISOLATION IS A SCHEMA, NOT A TEST-ONLY CONSTRUCTOR. The URL this hands
    /// to [`ControlStore::connect`] is the production one with a `search_path`
    /// appended, so every test exercises the real connect path — the pool, the
    /// advisory lock, the DDL, the migration — rather than a shortcut that
    /// exists only under `cfg(test)`.
    async fn store() -> ControlStore {
        ControlStore::connect(&fresh_schema().await).await.unwrap()
    }

    /// A fresh, empty schema, and the URL that points at it.
    ///
    /// The name embeds the second it was made in, which is what lets the NEXT
    /// run clean up after this one: there is no async `Drop`, so a test cannot
    /// reliably drop its own schema, and a reaper that runs on the way in is
    /// the shape that needs nothing to be remembered on the way out.
    async fn fresh_schema() -> String {
        let base = std::env::var(TEST_URL_VAR)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| panic!("{NO_TEST_DATABASE}"));
        let client = raw_client(&base).await;
        reap_old_schemas(&client).await;

        let mut suffix = [0u8; 8];
        getrandom::fill(&mut suffix).expect("the system random source");
        let name = format!(
            "sqct_{}_{}",
            Utc::now().timestamp(),
            suffix
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        client
            .batch_execute(&format!("CREATE SCHEMA {name}"))
            .await
            .expect("creating the test schema");
        schema_url(&base, &name)
    }

    /// The base URL with a `search_path` pointing at one schema.
    ///
    /// The `=` inside the option is PERCENT-ENCODED, because this is a query
    /// VALUE: an unescaped one would end the `options` parameter and the rest
    /// would be read as another key. The separator is `&` when the operator's
    /// URL already carries a query string (`?sslmode=disable` is a common one),
    /// and `?` when it does not.
    fn schema_url(base: &str, schema: &str) -> String {
        let sep = if base.contains('?') { '&' } else { '?' };
        format!("{base}{sep}options=-csearch_path%3D{schema}")
    }

    /// A connection outside the store, for the harness and for the assertions
    /// that read columns no method returns.
    ///
    /// The connection task is spawned and forgotten: it ends when the client is
    /// dropped, and a test process that exits with one still running has
    /// nothing to lose.
    async fn raw_client(url: &str) -> tokio_postgres::Client {
        let (client, connection) = tokio_postgres::connect(url, NoTls)
            .await
            .unwrap_or_else(|e| panic!("connecting to {TEST_URL_VAR}: {e}\n\n{NO_TEST_DATABASE}"));
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
    }

    /// Drop every `sqct_` schema older than [`SCHEMA_TTL_SECS`].
    ///
    /// Only names this harness could have written are touched: the prefix, then
    /// a timestamp that parses, then hex. A name that does not match that shape
    /// is left alone however much it looks like ours, because the one thing
    /// this must never do is drop a schema somebody's application lives in.
    async fn reap_old_schemas(client: &tokio_postgres::Client) {
        let rows = client
            .query(
                "SELECT nspname FROM pg_namespace WHERE nspname LIKE 'sqct\\_%'",
                &[],
            )
            .await
            .expect("listing test schemas");
        let cutoff = Utc::now().timestamp() - SCHEMA_TTL_SECS;
        for row in &rows {
            let name: String = row.get(0);
            let Some((ts, hex)) = name
                .strip_prefix("sqct_")
                .and_then(|rest| rest.split_once('_'))
            else {
                continue;
            };
            let Ok(ts) = ts.parse::<i64>() else { continue };
            if ts >= cutoff || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            // Best effort: another run reaping the same schema at the same
            // moment wins the race and this one has nothing to do.
            let _ = client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {name} CASCADE"))
                .await;
        }
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
    async fn held(s: &ControlStore, holder: &str) -> (String, i64) {
        let m = invites::mint().unwrap();
        let now = now();
        s.insert_invite(&m.code_hash, now + days(DEFAULT_TTL_DAYS))
            .await
            .unwrap();
        let id = s
            .reserve_invite(&m.code_hash, holder, now, now + days(1))
            .await
            .unwrap()
            .expect("a fresh code is available");
        (m.code_hash, id)
    }

    /// What the reservation columns say about a row, for the assertions that
    /// care that a hold was actually cleared rather than merely ignored.
    async fn reservation(s: &ControlStore, id: i64) -> (Option<String>, Option<String>) {
        let row = s
            .client()
            .await
            .unwrap()
            .query_one(
                "SELECT reserved_by, reserved_until FROM invite_codes WHERE id = $1",
                &[&id],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1))
    }

    #[tokio::test]
    async fn an_invite_is_spent_exactly_once() {
        let s = store().await;
        let (code_hash, id) = held(&s, "session-a").await;

        s.consume_invite(id, "ada", "session-a").await.unwrap();
        // Both the lookup and a second consume refuse.
        assert_eq!(
            s.find_available_invite(&code_hash, now()).await.unwrap(),
            None
        );
        assert!(matches!(
            s.consume_invite(id, "grace", "session-a").await,
            Err(StoreError::InviteUnavailable)
        ));
    }

    /// THE RACE F1 IS ABOUT: one code, two tabs. The second session is refused
    /// at the door instead of walking on to provision a second tenant with a
    /// code that only the first will manage to spend.
    #[tokio::test]
    async fn a_held_code_is_unavailable_to_every_other_session() {
        let s = store().await;
        let (code_hash, id) = held(&s, "session-a").await;
        let now = now();

        assert_eq!(
            s.reserve_invite(&code_hash, "session-b", now, now + days(1))
                .await
                .unwrap(),
            None,
            "a second session cannot take a held code"
        );
        assert_eq!(
            s.find_available_invite(&code_hash, now).await.unwrap(),
            None,
            "and the gate says so too"
        );
        // The holder still has it: nothing about the refusal moved the hold.
        let (by, _) = reservation(&s, id).await;
        assert_eq!(by.as_deref(), Some("session-a"));
    }

    /// A signup abandoned at Google costs the code nothing beyond the session's
    /// own lifetime: the hold lapses and the next attempt takes it.
    #[tokio::test]
    async fn a_lapsed_hold_frees_the_code() {
        let s = store().await;
        let (code_hash, id) = held(&s, "session-a").await;
        let later = now() + days(2);

        assert_eq!(
            s.find_available_invite(&code_hash, later).await.unwrap(),
            Some(id)
        );
        assert_eq!(
            s.reserve_invite(&code_hash, "session-b", later, later + days(1))
                .await
                .unwrap(),
            Some(id)
        );
        // ...and the session that lost it can no longer spend or release it.
        assert!(matches!(
            s.consume_invite(id, "ada", "session-a").await,
            Err(StoreError::InviteUnavailable)
        ));
        assert!(!s.release_invite(id, "session-a").await.unwrap());
    }

    /// A signup that fails hands the code back, so the same person can start
    /// again immediately rather than waiting out their own reservation.
    #[tokio::test]
    async fn releasing_frees_the_code_immediately() {
        let s = store().await;
        let (code_hash, id) = held(&s, "session-a").await;
        let now = now();

        assert!(s.release_invite(id, "session-a").await.unwrap());
        assert_eq!((None, None), reservation(&s, id).await);
        assert_eq!(
            s.find_available_invite(&code_hash, now).await.unwrap(),
            Some(id)
        );
        assert_eq!(
            s.reserve_invite(&code_hash, "session-b", now, now + days(1))
                .await
                .unwrap(),
            Some(id)
        );
        // Releasing twice is not an error, it is simply not this session's to
        // release any more.
        assert!(!s.release_invite(id, "session-a").await.unwrap());
    }

    /// THE INVARIANT F1 BUYS: whoever holds the code can always spend it, so a
    /// consume after a successful provision cannot fail. Only the holder can.
    #[tokio::test]
    async fn the_holder_and_only_the_holder_can_spend() {
        let s = store().await;
        let (_, id) = held(&s, "session-a").await;

        assert!(matches!(
            s.consume_invite(id, "grace", "session-b").await,
            Err(StoreError::InviteUnavailable)
        ));
        s.consume_invite(id, "ada", "session-a")
            .await
            .expect("the holder always wins");
        // Spending clears the hold: nothing is left pointing at a finished
        // session.
        assert_eq!((None, None), reservation(&s, id).await);
        let rows = s.list_invites().await.unwrap();
        assert_eq!(rows[0].used_by_label.as_deref(), Some("ada"));
    }

    /// An expired code is refused exactly the way an unknown one is, and it can
    /// no longer be held.
    #[tokio::test]
    async fn an_expired_code_is_simply_unavailable() {
        let s = store().await;
        let m = invites::mint().unwrap();
        let now = now();
        let id = s.insert_invite(&m.code_hash, now + days(30)).await.unwrap();

        assert_eq!(
            s.find_available_invite(&m.code_hash, now).await.unwrap(),
            Some(id)
        );
        let after = now + days(31);
        assert_eq!(
            s.find_available_invite(&m.code_hash, after).await.unwrap(),
            None
        );
        assert_eq!(
            s.reserve_invite(&m.code_hash, "session-a", after, after + days(1))
                .await
                .unwrap(),
            None
        );
    }

    /// The plaintext must never reach the database. Read the whole table back
    /// as text and look for it.
    #[tokio::test]
    async fn only_the_hash_is_at_rest() {
        let s = store().await;
        let m = invites::mint().unwrap();
        s.insert_invite(&m.code_hash, now() + days(30))
            .await
            .unwrap();

        let rows = s
            .client()
            .await
            .unwrap()
            .query("SELECT code_hash FROM invite_codes", &[])
            .await
            .unwrap();
        let stored: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        assert_eq!(stored, vec![m.code_hash.clone()]);
        assert!(!stored[0].contains(&invites::normalize(&m.code)));
        assert_eq!(stored[0], invites::hash(&m.code));
    }

    #[tokio::test]
    async fn a_wrong_or_unknown_code_is_simply_absent() {
        let s = store().await;
        let m = invites::mint().unwrap();
        s.insert_invite(&m.code_hash, now() + days(30))
            .await
            .unwrap();
        assert_eq!(
            s.find_available_invite(&invites::hash("ZZZZ-ZZZZ-ZZZZ-ZZZZ"), now())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn revoking_removes_only_unspent_codes() {
        let s = store().await;
        let (_, id_a) = held(&s, "session-a").await;
        let (_, id_b) = held(&s, "session-b").await;
        s.consume_invite(id_b, "ada", "session-b").await.unwrap();

        assert!(s.revoke_invite(id_a).await.unwrap());
        assert!(!s.revoke_invite(id_a).await.unwrap(), "already gone");
        assert!(
            !s.revoke_invite(id_b).await.unwrap(),
            "spent codes are kept"
        );
        let rows = s.list_invites().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id_b);
        assert_eq!(rows[0].used_by_label.as_deref(), Some("ada"));
    }

    /// A database written before expiry and reservations existed opens, gains
    /// the columns, and keeps the codes already in people's inboxes working
    /// until thirty days after they were ISSUED.
    ///
    /// The old shape is pre-seeded on a fresh schema and then
    /// [`ControlStore::connect`] is pointed at it, which is the same entry
    /// point a deploy uses: there is no test-only `init` to call.
    #[tokio::test]
    async fn an_older_store_is_migrated_and_its_codes_dated_from_issue() {
        let url = fresh_schema().await;
        let seed = raw_client(&url).await;
        seed.batch_execute(
            "CREATE TABLE invite_codes (
                 id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                 code_hash     TEXT NOT NULL UNIQUE,
                 created_at    TEXT NOT NULL,
                 used_at       TEXT,
                 used_by_label TEXT
             );",
        )
        .await
        .unwrap();
        // An 8-character code, minted 29 days ago, exactly as the old CLI
        // stored it.
        let old = invites::hash("ABCD-EFGH");
        let issued = parse_ts(stamp(Utc::now() - days(29)));
        seed.execute(
            "INSERT INTO invite_codes(code_hash, created_at) VALUES($1, $2)",
            &[&old, &stamp(issued)],
        )
        .await
        .unwrap();

        let s = ControlStore::connect(&url).await.unwrap();
        let row = s.list_invites().await.unwrap().remove(0);
        assert_eq!(
            row.expires_at,
            Some(issued + days(DEFAULT_TTL_DAYS)),
            "dated from issue, not from the migration"
        );
        assert!(
            s.find_available_invite(&old, now())
                .await
                .unwrap()
                .is_some(),
            "an already-issued code keeps working"
        );
        assert!(
            s.find_available_invite(&old, now() + days(2))
                .await
                .unwrap()
                .is_none(),
            "...until the backfilled expiry passes"
        );
    }

    #[tokio::test]
    async fn a_label_is_claimed_once() {
        let s = store().await;
        s.insert_tenant("ada", "ada@example.com").await.unwrap();
        assert!(s.label_exists("ada").await.unwrap());
        assert!(!s.label_exists("grace").await.unwrap());
        assert!(matches!(
            s.insert_tenant("ada", "someone@example.com").await,
            Err(StoreError::LabelTaken)
        ));
    }

    /// The raw `vk_minted_at` cell for `label`, for asserting the shape it is
    /// written in.
    async fn vk_minted_at(s: &ControlStore, label: &str) -> Option<String> {
        s.client()
            .await
            .unwrap()
            .query_one(
                "SELECT vk_minted_at FROM tenants WHERE label = $1",
                &[&label],
            )
            .await
            .unwrap()
            .get(0)
    }

    /// The vk id rides on the tenant row: set, rotate, clear, and the two
    /// nothing-there cases. Only ever the ID; the value has no path here.
    #[tokio::test]
    async fn the_vk_id_rides_on_the_tenant_row() {
        let s = store().await;
        s.insert_tenant("ada", "ada@example.com").await.unwrap();

        assert_eq!(s.tenant_vk("ada").await.unwrap(), None);
        assert_eq!(vk_minted_at(&s, "ada").await, None);
        assert!(s.set_tenant_vk("ada", "vk-1").await.unwrap());
        assert_eq!(s.tenant_vk("ada").await.unwrap(), Some("vk-1".to_string()));
        // The mint stamp is TEXT RFC3339 in the ONE shape every timestamp
        // column in this schema uses, not unix seconds: it round-trips through
        // `parse_ts`/`stamp` unchanged.
        let minted = vk_minted_at(&s, "ada")
            .await
            .expect("stamped alongside the id");
        assert_eq!(stamp(parse_ts(minted.clone())), minted, "{minted}");
        // A rotation overwrites; the store tracks only the installed key.
        assert!(s.set_tenant_vk("ada", "vk-2").await.unwrap());
        assert_eq!(s.tenant_vk("ada").await.unwrap(), Some("vk-2".to_string()));

        assert!(s.clear_tenant_vk("ada").await.unwrap());
        assert_eq!(s.tenant_vk("ada").await.unwrap(), None);
        assert_eq!(vk_minted_at(&s, "ada").await, None, "cleared with the id");
        assert!(
            !s.clear_tenant_vk("ada").await.unwrap(),
            "nothing left to forget"
        );

        // A label with no row takes nothing and answers nothing.
        assert!(!s.set_tenant_vk("ghost", "vk-9").await.unwrap());
        assert_eq!(s.tenant_vk("ghost").await.unwrap(), None);
    }

    /// The raw `assistant_vk_minted_at` cell for `label`.
    async fn assistant_vk_minted_at(s: &ControlStore, label: &str) -> Option<String> {
        s.client()
            .await
            .unwrap()
            .query_one(
                "SELECT assistant_vk_minted_at FROM tenants WHERE label = $1",
                &[&label],
            )
            .await
            .unwrap()
            .get(0)
    }

    /// The assistant vk id rides its own columns, independently of the triage
    /// one: setting, rotating, and clearing either leaves the other alone.
    #[tokio::test]
    async fn the_assistant_vk_id_rides_its_own_columns() {
        let s = store().await;
        s.insert_tenant("ada", "ada@example.com").await.unwrap();

        assert_eq!(s.tenant_assistant_vk("ada").await.unwrap(), None);
        assert_eq!(assistant_vk_minted_at(&s, "ada").await, None);
        assert!(s.set_tenant_assistant_vk("ada", "vk-a1").await.unwrap());
        assert_eq!(
            s.tenant_assistant_vk("ada").await.unwrap(),
            Some("vk-a1".to_string())
        );
        // Same stamp convention as every other timestamp in this schema.
        let minted = assistant_vk_minted_at(&s, "ada")
            .await
            .expect("stamped alongside the id");
        assert_eq!(stamp(parse_ts(minted.clone())), minted, "{minted}");
        assert!(s.set_tenant_assistant_vk("ada", "vk-a2").await.unwrap());
        assert_eq!(
            s.tenant_assistant_vk("ada").await.unwrap(),
            Some("vk-a2".to_string())
        );

        // The two pointers are independent: the triage key is untouched by
        // anything the assistant one does, and vice versa.
        assert!(s.set_tenant_vk("ada", "vk-t1").await.unwrap());
        assert!(s.clear_tenant_assistant_vk("ada").await.unwrap());
        assert_eq!(s.tenant_assistant_vk("ada").await.unwrap(), None);
        assert_eq!(
            assistant_vk_minted_at(&s, "ada").await,
            None,
            "cleared with the id"
        );
        assert_eq!(s.tenant_vk("ada").await.unwrap(), Some("vk-t1".to_string()));
        assert!(!s.clear_tenant_assistant_vk("ada").await.unwrap());
        assert!(s.clear_tenant_vk("ada").await.unwrap());

        assert!(!s.set_tenant_assistant_vk("ghost", "vk-9").await.unwrap());
        assert_eq!(s.tenant_assistant_vk("ghost").await.unwrap(), None);
    }

    /// A tenants table written before the vk columns existed opens, gains
    /// them, and takes a key id like any other row.
    #[tokio::test]
    async fn an_older_tenants_table_gains_the_vk_columns() {
        let url = fresh_schema().await;
        let seed = raw_client(&url).await;
        seed.batch_execute(
            "CREATE TABLE tenants (
                 id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                 label         TEXT NOT NULL UNIQUE,
                 account_email TEXT NOT NULL,
                 status        TEXT NOT NULL,
                 created_at    TEXT NOT NULL
             );",
        )
        .await
        .unwrap();
        seed.execute(
            "INSERT INTO tenants(label, account_email, status, created_at)
             VALUES('ada', 'ada@example.com', 'active', $1)",
            &[&stamp(Utc::now())],
        )
        .await
        .unwrap();

        let s = ControlStore::connect(&url).await.unwrap();
        assert_eq!(s.tenant_vk("ada").await.unwrap(), None, "migrated, keyless");
        assert!(s.set_tenant_vk("ada", "vk-1").await.unwrap());
        assert_eq!(s.tenant_vk("ada").await.unwrap(), Some("vk-1".to_string()));
        // ...including the assistant pair, which arrived after the triage one.
        assert_eq!(s.tenant_assistant_vk("ada").await.unwrap(), None);
        assert!(s.set_tenant_assistant_vk("ada", "vk-a1").await.unwrap());
        assert_eq!(
            s.tenant_assistant_vk("ada").await.unwrap(),
            Some("vk-a1".to_string())
        );
    }

    /// A table from the triage-only era — vk columns present, assistant
    /// columns not — gains exactly the missing pair and keeps its data.
    #[tokio::test]
    async fn a_triage_era_tenants_table_gains_the_assistant_columns() {
        let url = fresh_schema().await;
        let seed = raw_client(&url).await;
        seed.batch_execute(
            "CREATE TABLE tenants (
                 id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                 label         TEXT NOT NULL UNIQUE,
                 account_email TEXT NOT NULL,
                 status        TEXT NOT NULL,
                 created_at    TEXT NOT NULL,
                 bifrost_vk_id TEXT,
                 vk_minted_at  TEXT
             );",
        )
        .await
        .unwrap();
        seed.execute(
            "INSERT INTO tenants(label, account_email, status, created_at, bifrost_vk_id, vk_minted_at)
             VALUES('ada', 'ada@example.com', 'active', $1, 'vk-old', $1)",
            &[&stamp(Utc::now())],
        )
        .await
        .unwrap();

        let s = ControlStore::connect(&url).await.unwrap();
        assert_eq!(
            s.tenant_vk("ada").await.unwrap(),
            Some("vk-old".to_string()),
            "kept"
        );
        assert_eq!(s.tenant_assistant_vk("ada").await.unwrap(), None);
        assert!(s.set_tenant_assistant_vk("ada", "vk-a1").await.unwrap());
        assert_eq!(
            s.tenant_assistant_vk("ada").await.unwrap(),
            Some("vk-a1".to_string())
        );
    }

    /// What a console login asks the store, both times it asks: the label's
    /// mailbox, in the one spelling the column holds, and nothing at all for a
    /// label that was never provisioned.
    #[tokio::test]
    async fn a_label_names_the_mailbox_that_owns_it() {
        let s = store().await;
        s.insert_tenant("ada", "Ada@Example.com").await.unwrap();
        assert_eq!(
            s.active_tenant_email("ada").await.unwrap().as_deref(),
            Some("ada@example.com")
        );
        assert_eq!(s.active_tenant_email("grace").await.unwrap(), None);
        // A row that is not active is not a mailbox anybody signs in to.
        s.client()
            .await
            .unwrap()
            .execute(
                "UPDATE tenants SET status = 'stopped' WHERE label = 'ada'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(s.active_tenant_email("ada").await.unwrap(), None);
    }

    /// The public form can be submitted twice, from two tabs, with two
    /// capitalizations. The operator sees ONE person to approve.
    #[tokio::test]
    async fn an_address_joins_the_waitlist_once() {
        let s = store().await;
        assert!(s.add_to_waitlist("Ada@Example.com").await.unwrap());
        assert!(!s.add_to_waitlist("ada@example.com").await.unwrap());
        assert!(!s.add_to_waitlist("  ADA@EXAMPLE.COM  ").await.unwrap());

        let rows = s.list_waitlist().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].email, "ada@example.com");
        assert_eq!(rows[0].status, WAITLIST_PENDING);
        assert_eq!(rows[0].approved_at, None);
        assert_eq!(rows[0].invite_id, None);
        assert_eq!(rows[0].notified_at, None);
    }

    /// THE RACE THE ADMIN PAGE IS ABOUT: one row, two clicks. Only the first
    /// transition wins, so only one invite is ever minted for one person.
    #[tokio::test]
    async fn a_waitlist_row_is_approved_exactly_once() {
        let s = store().await;
        s.add_to_waitlist("ada@example.com").await.unwrap();
        let id = s.list_waitlist().await.unwrap()[0].id;
        let now = now();

        assert!(s.approve_waitlist(id, now).await.unwrap());
        assert!(
            !s.approve_waitlist(id, now + days(1)).await.unwrap(),
            "the second click mints nothing"
        );
        let row = s.waitlist_entry(id).await.unwrap().expect("still there");
        assert_eq!(row.status, WAITLIST_APPROVED);
        assert_eq!(row.approved_at, Some(now), "and keeps the first stamp");

        assert!(
            !s.approve_waitlist(id + 1, now).await.unwrap(),
            "no such row"
        );
        assert!(s.waitlist_entry(id + 1).await.unwrap().is_none());
    }

    /// The invite pointer and the delivery stamp round-trip in the same
    /// RFC3339 shape every other timestamp column here uses, and a re-send
    /// clears the old stamp so a row cannot show "invited" for a code that has
    /// not gone out.
    #[tokio::test]
    async fn the_invite_and_its_delivery_stamp_ride_on_the_row() {
        let s = store().await;
        s.add_to_waitlist("ada@example.com").await.unwrap();
        let id = s.list_waitlist().await.unwrap()[0].id;
        let now = now();
        s.approve_waitlist(id, now).await.unwrap();

        assert!(s.set_waitlist_invite(id, 7, None).await.unwrap());
        let row = s.waitlist_entry(id).await.unwrap().unwrap();
        assert_eq!(row.invite_id, Some(7));
        assert_eq!(row.notified_at, None, "nothing sent yet");

        assert!(s.mark_waitlist_notified(id, 7, now).await.unwrap());
        assert_eq!(
            s.waitlist_entry(id).await.unwrap().unwrap().notified_at,
            Some(now)
        );

        // A fresh invite for the same person: new pointer, no stamp.
        assert!(s.set_waitlist_invite(id, 9, Some(7)).await.unwrap());
        let row = s.waitlist_entry(id).await.unwrap().unwrap();
        assert_eq!(row.invite_id, Some(9));
        assert_eq!(row.notified_at, None, "the old delivery is not this one");

        // A send for the code the row has MOVED OFF stamps nothing: its
        // delivery is not the one this row is waiting on.
        assert!(!s.mark_waitlist_notified(id, 7, now).await.unwrap());
        assert_eq!(
            s.waitlist_entry(id).await.unwrap().unwrap().notified_at,
            None,
            "the row still wants to hear about the code it names"
        );
        assert!(s.mark_waitlist_notified(id, 9, now).await.unwrap());

        assert!(
            !s.set_waitlist_invite(id + 1, 7, None).await.unwrap(),
            "no such row"
        );
        assert!(!s.mark_waitlist_notified(id + 1, 7, now).await.unwrap());
    }

    /// The reservation check has to travel WITH the delete, because two
    /// statements leave a gap a signup can take the hold in.
    #[tokio::test]
    async fn a_held_invite_survives_the_admin_revoke() {
        let s = store().await;
        let now = now();
        let id = s
            .insert_invite("a".repeat(64).as_str(), now + days(30))
            .await
            .unwrap();

        // Held by a signup that is off at Google right now.
        assert!(
            s.reserve_invite(&"a".repeat(64), "holder", now, now + days(1))
                .await
                .is_ok()
        );
        assert!(
            !s.revoke_unheld_invite(id, now).await.unwrap(),
            "the hold refuses the delete"
        );
        assert!(
            s.invite_is_held(id, now).await.unwrap(),
            "and it is still there"
        );

        // Once the hold lapses, the same call takes it.
        assert!(s.revoke_unheld_invite(id, now + days(2)).await.unwrap());
        assert!(s.list_invites().await.unwrap().is_empty());
    }

    /// The compare-and-swap that decides which of two racing sends gets to mail
    /// its code: the one whose expectation still matches the row.
    #[tokio::test]
    async fn a_stale_expectation_loses_the_pointer() {
        let s = store().await;
        s.add_to_waitlist("ada@example.com").await.unwrap();
        let id = s.list_waitlist().await.unwrap()[0].id;
        s.approve_waitlist(id, now()).await.unwrap();

        // Both callers read the same empty pointer. The first wins it.
        assert!(s.set_waitlist_invite(id, 11, None).await.unwrap());
        assert!(
            !s.set_waitlist_invite(id, 12, None).await.unwrap(),
            "the second read a pointer that has since moved"
        );
        assert_eq!(
            s.waitlist_entry(id).await.unwrap().unwrap().invite_id,
            Some(11),
            "and the loser left the row alone"
        );

        // A caller that read the CURRENT pointer replaces it, which is the
        // re-send path.
        assert!(s.set_waitlist_invite(id, 13, Some(11)).await.unwrap());
        assert_eq!(
            s.waitlist_entry(id).await.unwrap().unwrap().invite_id,
            Some(13)
        );
    }

    /// What the operator reads top to bottom: everyone still waiting, oldest
    /// first (the longest wait is the next thing to do), then the most recent
    /// approvals as history.
    #[tokio::test]
    async fn the_listing_puts_the_longest_wait_first() {
        let s = store().await;
        for who in ["a@example.com", "b@example.com", "c@example.com"] {
            s.add_to_waitlist(who).await.unwrap();
        }
        let ids: Vec<i64> = s
            .list_waitlist()
            .await
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        s.approve_waitlist(ids[0], now()).await.unwrap();
        s.approve_waitlist(ids[1], now()).await.unwrap();

        let rows = s.list_waitlist().await.unwrap();
        let emails: Vec<&str> = rows.iter().map(|r| r.email.as_str()).collect();
        assert_eq!(
            emails,
            vec!["c@example.com", "b@example.com", "a@example.com"],
            "pending oldest-first, then approved newest-first"
        );
    }

    /// One mailbox, one daemon, however the address is capitalized.
    #[tokio::test]
    async fn a_mailbox_gets_one_tenant() {
        let s = store().await;
        s.insert_tenant("ada", "Ada@Example.com").await.unwrap();
        assert_eq!(
            s.active_tenant_for_email("ada@example.com").await.unwrap(),
            Some("ada".to_string())
        );
        assert_eq!(
            s.active_tenant_for_email("ADA@EXAMPLE.COM").await.unwrap(),
            Some("ada".to_string())
        );
        assert!(matches!(
            s.insert_tenant("grace", "ADA@example.com").await,
            Err(StoreError::AccountTaken)
        ));
        assert_eq!(
            s.active_tenant_for_email("other@example.com")
                .await
                .unwrap(),
            None
        );
    }
}
