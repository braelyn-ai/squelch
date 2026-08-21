//! The one-time move off the SQLite control store, end to end: a real legacy
//! file on disk, a real Postgres schema, and the importer between them.
//!
//! This runs ONCE, in production, against the file on the Railway volume, and
//! there is no second chance at it: the ids in that file are pointed at by
//! every soft pointer the store has (a funnel row's `invite_id` names an invite,
//! `invite_codes.used_by_label` names a tenant), and by the pairing codes and
//! links already in people's inboxes. So the properties asserted here are the
//! ones a dry run cannot rehearse:
//!
//! - every row lands, with its ID UNCHANGED and every column equal — including
//!   the NULLs, which is where a column-order slip shows up first,
//! - the soft pointers still resolve on the other side,
//! - EVERY SEQUENCE IS RE-ARMED, so the next signup takes `MAX(id) + 1` rather
//!   than id 1, which already names somebody else. Asserted by INSERTING and
//!   reading the id back, because `pg_sequences.last_value` reads NULL after a
//!   `setval(.., false)` until the first `nextval` and would prove nothing,
//! - a SECOND import is refused by name and by count, and leaves the store
//!   exactly as it found it. The window between "the new deployment is live"
//!   and "the import has run" is minutes wide, and a signup landing inside it
//!   is what this refusal is for.
//!
//! The fixture's ids are DELIBERATELY NON-CONTIGUOUS (tenant 7, invites 3 and
//! 9, the waiting row 5): a file whose ids happened to be 1..n would pass an
//! importer that renumbered as it went.
//!
//! THE FILE SAYS `waitlist` AND THE TARGET SAYS `users`. That table absorbed the
//! waitlist after this file's last writer was retired, so the importer reads one
//! name and writes the other, minting the `analytics_id` the new shape requires
//! and the old one never had. Everything else about the row is carried across
//! unchanged, ids included, which is what these assertions are checking.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use squelch_control::import::{self, ImportError};

/// The throwaway Postgres schema each test here imports into.
mod common;

/// The legacy schema, as the last SQLite build of this crate wrote it:
/// `INTEGER PRIMARY KEY AUTOINCREMENT` ids, no collations, no identity columns.
/// Spelled out rather than derived from anything, because the file this
/// importer meets in production was written by a binary that no longer exists.
const LEGACY_SCHEMA: &str = "
CREATE TABLE tenants (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    label         TEXT NOT NULL UNIQUE,
    account_email TEXT NOT NULL,
    status        TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    bifrost_vk_id           TEXT,
    vk_minted_at            TEXT,
    bifrost_assistant_vk_id TEXT,
    assistant_vk_minted_at  TEXT
);
CREATE TABLE invite_codes (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    code_hash      TEXT NOT NULL UNIQUE,
    created_at     TEXT NOT NULL,
    expires_at     TEXT,
    used_at        TEXT,
    used_by_label  TEXT,
    reserved_by    TEXT,
    reserved_until TEXT
);
CREATE TABLE waitlist (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    email       TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL,
    status      TEXT NOT NULL,
    approved_at TEXT,
    invite_id   INTEGER,
    notified_at TEXT
);
";

/// One tenant row of the fixture, in the legacy column order.
struct Tenant {
    id: i64,
    label: &'static str,
    account_email: &'static str,
    status: &'static str,
    created_at: &'static str,
    bifrost_vk_id: Option<&'static str>,
    vk_minted_at: Option<&'static str>,
    bifrost_assistant_vk_id: Option<&'static str>,
    assistant_vk_minted_at: Option<&'static str>,
}

/// One invite row of the fixture, in the legacy column order.
struct Invite {
    id: i64,
    code_hash: &'static str,
    created_at: &'static str,
    expires_at: Option<&'static str>,
    used_at: Option<&'static str>,
    used_by_label: Option<&'static str>,
    reserved_by: Option<&'static str>,
    reserved_until: Option<&'static str>,
}

/// One waitlist row of the fixture, in the legacy column order.
struct Waiting {
    id: i64,
    email: &'static str,
    created_at: &'static str,
    status: &'static str,
    approved_at: Option<&'static str>,
    invite_id: Option<i64>,
    notified_at: Option<&'static str>,
}

/// A tenant provisioned long enough ago to have a triage key and not an
/// assistant one: the NULL pair is half this row's job.
const TENANTS: [Tenant; 1] = [Tenant {
    id: 7,
    label: "ada",
    account_email: "ada@example.com",
    status: "active",
    created_at: "2026-07-02T10:30:00.000Z",
    bifrost_vk_id: Some("vk-tenant-ada"),
    vk_minted_at: Some("2026-07-02T10:30:01.000Z"),
    bifrost_assistant_vk_id: None,
    assistant_vk_minted_at: None,
}];

/// The spent code that provisioned `ada`, and a live one somebody is holding at
/// Google's consent screen right now. Between them every nullable column on the
/// table is NULL in one row and set in the other.
const INVITES: [Invite; 2] = [
    Invite {
        id: 3,
        code_hash: "0000000000000000000000000000000000000000000000000000000000000003",
        created_at: "2026-07-01T09:00:00.000Z",
        expires_at: Some("2026-07-31T09:00:00.000Z"),
        used_at: Some("2026-07-02T10:30:00.000Z"),
        used_by_label: Some("ada"),
        reserved_by: None,
        reserved_until: None,
    },
    Invite {
        id: 9,
        code_hash: "0000000000000000000000000000000000000000000000000000000000000009",
        created_at: "2026-08-10T09:00:00.000Z",
        expires_at: Some("2026-09-09T09:00:00.000Z"),
        used_at: None,
        used_by_label: None,
        reserved_by: Some("a-signup-session"),
        reserved_until: Some("2026-08-10T09:10:00.000Z"),
    },
];

/// An approved applicant whose email did not go out (`notified_at` NULL is the
/// repairable row), pointing at invite 9 — the pointer this whole exercise
/// exists to keep meaning the same thing.
const WAITING: [Waiting; 1] = [Waiting {
    id: 5,
    email: "grace@example.com",
    created_at: "2026-08-05T08:00:00.000Z",
    status: "approved",
    approved_at: Some("2026-08-10T09:00:00.000Z"),
    invite_id: Some(9),
    notified_at: None,
}];

/// The highest id in the fixture, and therefore the id the next invite must
/// take. Spelled here so the assertion reads as the arithmetic it is.
const NEXT_INVITE_ID: i64 = 10;

/// A legacy SQLite file, removed when the test that made it ends.
///
/// A guard rather than a `remove_file` at the bottom of each test: the file is
/// written to the shared temp directory, and a test that fails an assertion
/// would otherwise leave it there for the next run to inherit.
struct Fixture(PathBuf);

impl Fixture {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write the fixture to a file of its own. `name` keeps two tests running in
/// the same process off each other's path.
fn legacy_file(name: &str) -> Fixture {
    let path = std::env::temp_dir().join(format!(
        "squelch-control-import-{}-{name}.sqlite3",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    let conn = Connection::open(&path).expect("creating the legacy fixture");
    conn.execute_batch(LEGACY_SCHEMA)
        .expect("the legacy schema");
    for t in &TENANTS {
        conn.execute(
            "INSERT INTO tenants(id, label, account_email, status, created_at, bifrost_vk_id,
                                 vk_minted_at, bifrost_assistant_vk_id, assistant_vk_minted_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                t.id,
                t.label,
                t.account_email,
                t.status,
                t.created_at,
                t.bifrost_vk_id,
                t.vk_minted_at,
                t.bifrost_assistant_vk_id,
                t.assistant_vk_minted_at
            ],
        )
        .expect("a fixture tenant");
    }
    for i in &INVITES {
        conn.execute(
            "INSERT INTO invite_codes(id, code_hash, created_at, expires_at, used_at,
                                      used_by_label, reserved_by, reserved_until)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                i.id,
                i.code_hash,
                i.created_at,
                i.expires_at,
                i.used_at,
                i.used_by_label,
                i.reserved_by,
                i.reserved_until
            ],
        )
        .expect("a fixture invite");
    }
    for w in &WAITING {
        conn.execute(
            "INSERT INTO waitlist(id, email, created_at, status, approved_at, invite_id,
                                  notified_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                w.id,
                w.email,
                w.created_at,
                w.status,
                w.approved_at,
                w.invite_id,
                w.notified_at
            ],
        )
        .expect("a fixture waitlist row");
    }
    drop(conn);
    Fixture(path)
}

/// Whether a string is shaped like the UUIDv4 `mint_analytics_id` writes:
/// 8-4-4-4-12 lowercase hex, version nibble 4, variant in `89ab`. The app holds
/// an adopted id to this shape before it will adopt it, so it is a contract
/// rather than a convention.
fn is_uuid_shaped(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12] == parts.iter().map(|p| p.len()).collect::<Vec<_>>()[..]
        && parts
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
        && parts[2].starts_with('4')
        && matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b')
}

/// Every row of all three tables, whole, as text, in id order.
///
/// `to_jsonb(t)` rather than a column list, deliberately: a column this test
/// does not know about is still in the snapshot, so "the refusal changed
/// nothing" stays true of a table that grows a column later.
async fn snapshot(client: &tokio_postgres::Client) -> Vec<String> {
    let mut out = Vec::new();
    for table in ["tenants", "invite_codes", "users"] {
        let rows = client
            .query(
                &format!("SELECT to_jsonb(t)::text FROM {table} t ORDER BY id"),
                &[],
            )
            .await
            .expect("reading the imported rows");
        out.extend(rows.iter().map(|r| r.get::<_, String>(0)));
    }
    out
}

/// Ids preserved, values equal, NULLs still NULL, and the soft pointer between
/// two of the tables still resolving.
#[tokio::test]
async fn a_legacy_file_lands_row_for_row() {
    let (store, url) = common::fresh_store().await;
    let file = legacy_file("row-for-row");

    let report = import::import_sqlite(&store, file.path())
        .await
        .expect("the import");
    assert_eq!(report.tenants, TENANTS.len());
    assert_eq!(report.invite_codes, INVITES.len());
    assert_eq!(report.users, WAITING.len());

    let client = common::raw_client(&url).await;

    let rows = client
        .query("SELECT * FROM tenants ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), TENANTS.len());
    for (row, t) in rows.iter().zip(&TENANTS) {
        assert_eq!(row.get::<_, i64>("id"), t.id, "the id is the file's");
        assert_eq!(row.get::<_, String>("label"), t.label);
        assert_eq!(row.get::<_, String>("account_email"), t.account_email);
        assert_eq!(row.get::<_, String>("status"), t.status);
        assert_eq!(row.get::<_, String>("created_at"), t.created_at);
        assert_eq!(
            row.get::<_, Option<String>>("bifrost_vk_id").as_deref(),
            t.bifrost_vk_id
        );
        assert_eq!(
            row.get::<_, Option<String>>("vk_minted_at").as_deref(),
            t.vk_minted_at
        );
        assert_eq!(
            row.get::<_, Option<String>>("bifrost_assistant_vk_id")
                .as_deref(),
            t.bifrost_assistant_vk_id,
            "a NULL stays NULL"
        );
        assert_eq!(
            row.get::<_, Option<String>>("assistant_vk_minted_at")
                .as_deref(),
            t.assistant_vk_minted_at,
            "a NULL stays NULL"
        );
    }

    let rows = client
        .query("SELECT * FROM invite_codes ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), INVITES.len());
    for (row, i) in rows.iter().zip(&INVITES) {
        assert_eq!(row.get::<_, i64>("id"), i.id, "the id is the file's");
        assert_eq!(row.get::<_, String>("code_hash"), i.code_hash);
        assert_eq!(row.get::<_, String>("created_at"), i.created_at);
        assert_eq!(
            row.get::<_, Option<String>>("expires_at").as_deref(),
            i.expires_at
        );
        assert_eq!(
            row.get::<_, Option<String>>("used_at").as_deref(),
            i.used_at
        );
        assert_eq!(
            row.get::<_, Option<String>>("used_by_label").as_deref(),
            i.used_by_label
        );
        assert_eq!(
            row.get::<_, Option<String>>("reserved_by").as_deref(),
            i.reserved_by
        );
        assert_eq!(
            row.get::<_, Option<String>>("reserved_until").as_deref(),
            i.reserved_until
        );
    }

    let rows = client
        .query("SELECT * FROM users ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), WAITING.len());
    for (row, w) in rows.iter().zip(&WAITING) {
        assert_eq!(row.get::<_, i64>("id"), w.id, "the id is the file's");
        assert_eq!(row.get::<_, String>("email"), w.email);
        assert_eq!(row.get::<_, String>("created_at"), w.created_at);
        assert_eq!(row.get::<_, String>("status"), w.status);
        assert_eq!(
            row.get::<_, Option<String>>("approved_at").as_deref(),
            w.approved_at
        );
        assert_eq!(row.get::<_, Option<i64>>("invite_id"), w.invite_id);
        assert_eq!(
            row.get::<_, Option<String>>("notified_at").as_deref(),
            w.notified_at,
            "a NULL stays NULL"
        );
        // The one column the file had nothing to say about: minted on the way
        // in, so a migrated person is knowable to analytics exactly like
        // somebody who arrived after the funnel table existed.
        let aid: String = row.get("analytics_id");
        assert!(is_uuid_shaped(&aid), "{aid}");
        // ...and the funnel columns start empty, because the file recorded
        // nothing about them. The first init after this import is what fills
        // them in from the invite trail.
        assert_eq!(row.get::<_, Option<String>>("signed_up_at"), None);
        assert_eq!(row.get::<_, Option<String>>("first_paired_at"), None);
    }

    // THE SOFT POINTER, asked the way the admin page asks it: the id the funnel
    // row carries names a row that is actually here.
    let pointed_at: i64 = WAITING[0].invite_id.expect("the fixture points at one");
    let hit = client
        .query_one(
            "SELECT code_hash FROM invite_codes WHERE id = $1",
            &[&pointed_at],
        )
        .await
        .expect("the invite the funnel row names survived the import");
    assert_eq!(hit.get::<_, String>(0), INVITES[1].code_hash);
}

/// THE STEP THAT WOULD BE FORGOTTEN. Inserting explicit ids does not advance an
/// identity sequence, so without the re-arm the next signup takes id 1 — which
/// already names somebody — and every soft pointer in the store starts meaning
/// two things at once.
///
/// Asserted by WRITING, one table at a time, because `pg_sequences.last_value`
/// reads NULL after `setval(.., false)` until the first `nextval` and would say
/// nothing about whether the re-arm ran.
#[tokio::test]
async fn every_sequence_continues_where_the_file_left_off() {
    let (store, url) = common::fresh_store().await;
    let file = legacy_file("sequences");
    import::import_sqlite(&store, file.path())
        .await
        .expect("the import");

    let next_invite = store
        .insert_invite(
            "000000000000000000000000000000000000000000000000000000000000000a",
            chrono::Utc::now() + chrono::Duration::days(30),
        )
        .await
        .unwrap();
    assert_eq!(
        next_invite, NEXT_INVITE_ID,
        "the next invite takes MAX(id) + 1, not an id somebody already holds"
    );

    // The same, for the two tables whose inserts do not hand back an id. Both
    // are checked because the re-arm is a loop over three tables and a loop
    // that stopped early would still pass on the first of them.
    store
        .insert_tenant("grace", "grace@example.com")
        .await
        .unwrap();
    assert!(store.add_user_waiting("hopper@example.com").await.unwrap());

    let client = common::raw_client(&url).await;
    let tenant_id: i64 = client
        .query_one("SELECT id FROM tenants WHERE label = 'grace'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(tenant_id, TENANTS[0].id + 1);
    let user_id: i64 = client
        .query_one(
            "SELECT id FROM users WHERE email = 'hopper@example.com'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(user_id, WAITING[0].id + 1);
}

/// The designed failure: a target that already holds rows is refused by NAME
/// and by COUNT, and nothing is written. A count is what an operator needs to
/// tell "the one signup I watched come in" from "I have already run this".
#[tokio::test]
async fn a_second_import_is_refused_and_changes_nothing() {
    let (store, url) = common::fresh_store().await;
    let file = legacy_file("second-import");
    import::import_sqlite(&store, file.path())
        .await
        .expect("the first import");

    let client = common::raw_client(&url).await;
    let before = snapshot(&client).await;
    assert!(!before.is_empty(), "the first import landed");

    let err = import::import_sqlite(&store, file.path())
        .await
        .expect_err("a second import must refuse");
    assert!(
        matches!(err, ImportError::NotEmpty { table, rows }
                 if table == "tenants" && rows == TENANTS.len() as i64),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("tenants"), "the table is named: {msg}");
    assert!(msg.contains("1 row(s)"), "the count is given: {msg}");
    assert!(msg.contains("Nothing was written"), "{msg}");

    assert_eq!(
        snapshot(&client).await,
        before,
        "the refusal left every row exactly as it was"
    );
}
