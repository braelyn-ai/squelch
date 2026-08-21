//! The one-time move off the SQLite control store, run as
//! `squelch-control import-sqlite <path>`.
//!
//! THIS MODULE IS TEMPORARY AND SAYS SO. It exists to carry the rows that are
//! already on the Railway volume into the Postgres this crate now talks to, and
//! it is the only reason `rusqlite` is still a dependency. When the volume is
//! retired, this file and that dependency go together.
//!
//! THE FILE IS OPENED READ-ONLY, which is what makes the cutover reversible: if
//! anything about the new store is wrong, the old image is redeployed and the
//! file it finds is byte-for-byte the one it left. Nothing here writes to
//! SQLite, not even a journal.
//!
//! IT REFUSES A TARGET THAT HAS ROWS. The window between "the new deployment is
//! live" and "the import has run" is minutes, and a signup landing inside it
//! writes a tenant row into the empty Postgres. Importing on top of that would
//! either collide on an id or silently interleave two id spaces, so the answer
//! is to stop and let a human reconcile the handful of rows by hand. A designed
//! failure, not a limitation.
//!
//! PRIVACY: this module reads addresses (every tenant's mailbox, every waitlist
//! entry) and logs none of them. What it returns is COUNTS, and the CLI prints
//! counts.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::store::{ControlStore, StoreError};

/// The three tables, in the order they are imported and re-armed. Order does
/// not matter to the database (there are no foreign keys, on purpose: the
/// pointers between these tables are soft), but it decides the order of the
/// counts an operator reads.
const TABLES: [&str; 3] = ["tenants", "invite_codes", "waitlist"];

/// Why an import stopped.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The SQLite file would not open, or would not read. Covers "no such
    /// file", "not a database", and a file written by a binary older than the
    /// last column this crate added (which reads as "no such column": the
    /// importer opens READ-ONLY and cannot migrate its way out of that, and the
    /// fix is to open the file once with the old binary).
    #[error("reading the SQLite control store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The Postgres side. Its `Display` carries no DETAIL line; see
    /// [`StoreError`].
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The target is not empty. Named table and count, no rows: a count is what
    /// an operator needs to decide whether this is the signup they saw come in
    /// or a second import of the same file.
    #[error(
        "refusing to import: the target `{table}` already holds {rows} row(s). \
         Nothing was written. Reconcile by hand, or point this at an empty database"
    )]
    NotEmpty { table: &'static str, rows: i64 },
}

type Result<T> = std::result::Result<T, ImportError>;

/// What an import moved, per table.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImportReport {
    pub tenants: usize,
    pub invite_codes: usize,
    pub waitlist: usize,
}

/// One tenant row, in the SQLite column order.
struct Tenant {
    id: i64,
    label: String,
    account_email: String,
    status: String,
    created_at: String,
    bifrost_vk_id: Option<String>,
    vk_minted_at: Option<String>,
    bifrost_assistant_vk_id: Option<String>,
    assistant_vk_minted_at: Option<String>,
}

/// One invite row, in the SQLite column order.
struct Invite {
    id: i64,
    code_hash: String,
    created_at: String,
    expires_at: Option<String>,
    used_at: Option<String>,
    used_by_label: Option<String>,
    reserved_by: Option<String>,
    reserved_until: Option<String>,
}

/// One waitlist row, in the SQLite column order.
struct Waiting {
    id: i64,
    email: String,
    created_at: String,
    status: String,
    approved_at: Option<String>,
    invite_id: Option<i64>,
    notified_at: Option<String>,
}

/// Copy every row of the SQLite control store at `path` into `store`.
///
/// ONE POSTGRES TRANSACTION, so the outcome is the whole store or none of it.
/// An import that half-landed would leave an operator diffing two databases
/// during a cutover window, which is the one moment nobody has time for it.
///
/// The whole file is read into memory first, deliberately: it is a few hundred
/// rows on the largest day this will ever run, and reading it before the
/// transaction opens keeps the write side short and keeps a rusqlite error from
/// happening with a Postgres transaction held open.
///
/// IDS ARE PRESERVED, AND THAT IS THE POINT OF THE WHOLE EXERCISE. Every
/// pointer between these tables is soft — `waitlist.invite_id` names an invite
/// row, `invite_codes.used_by_label` names a tenant — so renumbering on the way
/// in would silently repoint half the waitlist at the wrong codes.
pub async fn import_sqlite(store: &ControlStore, path: &Path) -> Result<ImportReport> {
    let (tenants, invites, waiting) = read_sqlite(path)?;

    let mut client = store.client().await?;
    let tx = client.transaction().await?;

    // The refusal, before anything is written. Checked inside the transaction
    // so "empty" is a fact about the database this import writes to, not about
    // the database as it was a moment before.
    for table in TABLES {
        let rows: i64 = tx
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await?
            .get(0);
        if rows > 0 {
            return Err(ImportError::NotEmpty { table, rows });
        }
    }

    // `OVERRIDING SYSTEM VALUE` is required, not decoration: the id columns are
    // `GENERATED ALWAYS AS IDENTITY`, which refuses a supplied id unless the
    // statement says out loud that it means to override. That is the trade the
    // schema makes — an application cannot set an id by accident, and this one
    // place says it is not an accident.
    for t in &tenants {
        tx.execute(
            "INSERT INTO tenants(id, label, account_email, status, created_at,
                                 bifrost_vk_id, vk_minted_at,
                                 bifrost_assistant_vk_id, assistant_vk_minted_at)
             OVERRIDING SYSTEM VALUE
             VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &t.id,
                &t.label,
                &t.account_email,
                &t.status,
                &t.created_at,
                &t.bifrost_vk_id,
                &t.vk_minted_at,
                &t.bifrost_assistant_vk_id,
                &t.assistant_vk_minted_at,
            ],
        )
        .await?;
    }
    for i in &invites {
        tx.execute(
            "INSERT INTO invite_codes(id, code_hash, created_at, expires_at,
                                      used_at, used_by_label, reserved_by, reserved_until)
             OVERRIDING SYSTEM VALUE
             VALUES($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &i.id,
                &i.code_hash,
                &i.created_at,
                &i.expires_at,
                &i.used_at,
                &i.used_by_label,
                &i.reserved_by,
                &i.reserved_until,
            ],
        )
        .await?;
    }
    for w in &waiting {
        tx.execute(
            "INSERT INTO waitlist(id, email, created_at, status, approved_at,
                                  invite_id, notified_at)
             OVERRIDING SYSTEM VALUE
             VALUES($1, $2, $3, $4, $5, $6, $7)",
            &[
                &w.id,
                &w.email,
                &w.created_at,
                &w.status,
                &w.approved_at,
                &w.invite_id,
                &w.notified_at,
            ],
        )
        .await?;
    }

    // RE-ARM EVERY SEQUENCE, and this is the step that would be forgotten.
    // Inserting an explicit id does not advance the identity sequence, so
    // without this the next signup takes id 1 — an id that already names
    // somebody else's tenant — and every soft pointer in the store starts
    // meaning two things at once. `setval(.., MAX(id) + 1, false)` says "the
    // next value handed out is exactly this", so ids continue where the file
    // left off and no id is ever reused.
    for table in TABLES {
        tx.execute(
            &format!(
                "SELECT setval(pg_get_serial_sequence('{table}', 'id'),
                               COALESCE((SELECT MAX(id) FROM {table}), 0) + 1, false)"
            ),
            &[],
        )
        .await?;
    }

    tx.commit().await?;
    Ok(ImportReport {
        tenants: tenants.len(),
        invite_codes: invites.len(),
        waitlist: waiting.len(),
    })
}

/// Read all three tables out of the file.
///
/// `SQLITE_OPEN_READ_ONLY` and nothing else: no create, so a mistyped path is
/// an error rather than a new empty database that imports zero rows and looks
/// like a success.
fn read_sqlite(path: &Path) -> Result<(Vec<Tenant>, Vec<Invite>, Vec<Waiting>)> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    let tenants = {
        let mut stmt = conn.prepare(
            "SELECT id, label, account_email, status, created_at, bifrost_vk_id, vk_minted_at,
                    bifrost_assistant_vk_id, assistant_vk_minted_at
               FROM tenants ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Tenant {
                id: r.get(0)?,
                label: r.get(1)?,
                account_email: r.get(2)?,
                status: r.get(3)?,
                created_at: r.get(4)?,
                bifrost_vk_id: r.get(5)?,
                vk_minted_at: r.get(6)?,
                bifrost_assistant_vk_id: r.get(7)?,
                assistant_vk_minted_at: r.get(8)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let invites = {
        let mut stmt = conn.prepare(
            "SELECT id, code_hash, created_at, expires_at, used_at, used_by_label,
                    reserved_by, reserved_until
               FROM invite_codes ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Invite {
                id: r.get(0)?,
                code_hash: r.get(1)?,
                created_at: r.get(2)?,
                expires_at: r.get(3)?,
                used_at: r.get(4)?,
                used_by_label: r.get(5)?,
                reserved_by: r.get(6)?,
                reserved_until: r.get(7)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let waiting = {
        let mut stmt = conn.prepare(
            "SELECT id, email, created_at, status, approved_at, invite_id, notified_at
               FROM waitlist ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Waiting {
                id: r.get(0)?,
                email: r.get(1)?,
                created_at: r.get(2)?,
                status: r.get(3)?,
                approved_at: r.get(4)?,
                invite_id: r.get(5)?,
                notified_at: r.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    Ok((tenants, invites, waiting))
}

/// Postgres failures reach this module through the store's own error type, so
/// there is ONE place that documents why these are logged with `%e` and never
/// `{:?}`.
impl From<tokio_postgres::Error> for ImportError {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Store(StoreError::from(e))
    }
}
