//! Migration + init upgrade-path tests.

use super::super::migrate::migrate;
use super::super::*;
use super::support::*;
use crate::types::Sensitivity;

#[test]
fn migrate_adds_unsub_columns_to_a_preexisting_messages_table() {
    // Simulate an existing install whose `messages` predates the unsubscribe
    // columns. The migration must add them, and be idempotent on re-open.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE messages(
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             gmail_msg_id TEXT NOT NULL, body TEXT);",
    )
    .unwrap();
    migrate(&conn).unwrap();
    migrate(&conn).unwrap(); // idempotent
    let mut stmt = conn.prepare("PRAGMA table_info(messages)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "list_unsubscribe"));
    assert!(cols.iter().any(|c| c == "list_unsub_one_click"));
}

#[test]
fn migrate_adds_auth_pass_null_to_a_preexisting_messages_table() {
    // An install predating the email-authentication column. The migration adds
    // it, and every historical row rests at NULL — no backfill, which is what
    // makes pre-existing mail read as "no tracking-pixel bypass".
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE messages(
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             gmail_msg_id TEXT NOT NULL, body TEXT);
         INSERT INTO messages(id, account_id, gmail_msg_id, body) VALUES (1, 1, 'g1', 'old');",
    )
    .unwrap();
    migrate(&conn).unwrap();
    migrate(&conn).unwrap(); // idempotent
    let mut stmt = conn.prepare("PRAGMA table_info(messages)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "auth_pass"));
    let existing: Option<i64> = conn
        .query_row("SELECT auth_pass FROM messages WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(existing, None, "pre-existing mail is never backfilled");
}

#[test]
fn migrate_adds_to_addrs_null_so_old_sent_mail_enters_the_backfill_queue() {
    // An install predating the display-recipients column. The migration adds it,
    // every historical row rests at NULL — which IS the backfill queue predicate
    // (`is_sent=1 AND to_addrs IS NULL`), since only a network pass can fill it.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE messages(
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             gmail_msg_id TEXT NOT NULL, body TEXT, is_sent INTEGER NOT NULL DEFAULT 0);
         INSERT INTO messages(id, account_id, gmail_msg_id, body, is_sent)
             VALUES (1, 1, 'g1', 'old', 1);",
    )
    .unwrap();
    migrate(&conn).unwrap();
    migrate(&conn).unwrap(); // idempotent
    let mut stmt = conn.prepare("PRAGMA table_info(messages)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "to_addrs"));
    let existing: Option<String> = conn
        .query_row("SELECT to_addrs FROM messages WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(existing, None, "old sent mail is queued, never invented");
}

#[test]
fn migrate_adds_field_reasons_to_a_preexisting_triage_table() {
    // An install whose `triage` has no field_reasons. `model_used`/`status`
    // are present because the two-stage backfill in migrate() reads them.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE triage(
             message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             importance INTEGER NOT NULL DEFAULT 0, reason TEXT,
             model_used TEXT, status TEXT NOT NULL DEFAULT 'new');",
    )
    .unwrap();
    migrate(&conn).unwrap();
    migrate(&conn).unwrap(); // idempotent
    let mut stmt = conn.prepare("PRAGMA table_info(triage)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "field_reasons"));
}

#[test]
fn migrate_adds_two_stage_columns_so_ingest_and_both_queues_work() {
    // A `triage` table with no stage1_model_used / needs_stage2 /
    // field_reasons. The migration must add them so ingest and both queue
    // SELECTs, which NAME those columns, don't fail with "no such column".
    register_vec_extension();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE triage(
             message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             importance INTEGER NOT NULL DEFAULT 0, tier TEXT NOT NULL DEFAULT 'noise',
             sensitivity TEXT NOT NULL DEFAULT 'normal', sealed_kind TEXT,
             one_line TEXT NOT NULL DEFAULT '', reason TEXT NOT NULL DEFAULT '',
             deadline TEXT, matched_rule_id INTEGER, model_used TEXT,
             status TEXT NOT NULL DEFAULT 'new', surfaced_at TEXT, resolved_at TEXT,
             created_at TEXT NOT NULL);",
    )
    .unwrap();
    // init applies SCHEMA (IF NOT EXISTS preserves the existing triage
    // shape) then migrate(), which adds the two columns + backfill.
    let store = SqliteStore::init(conn).unwrap();
    let acct = store.ensure_account("me@example.com").unwrap();

    let id = triaged_row(acct, "g-x", "t-x", None, false, Sensitivity::Normal).ingest(&store);
    // Both queue SELECTs run cleanly on the upgraded table.
    let s1 = store.stage1_queue(acct, 10).unwrap();
    assert_eq!(s1.len(), 1, "the fresh normal row enters Stage-1");
    assert_eq!(s1[0].message_id, id);
    assert!(store.stage2_queue(acct, 10).unwrap().is_empty());
}

#[test]
fn migrate_rebuilds_stage2_usage_with_category_pk() {
    // A pre-category stage2_usage table (PK account_id+day) must be rebuilt
    // — ALTER ADD COLUMN can't extend a PK, and the bump upsert's
    // ON CONFLICT(account_id, day, category) needs the new unique index.
    // Existing rows land under category='stage2'; bumps work after.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE stage2_usage (
             account_id INTEGER NOT NULL, day TEXT NOT NULL,
             calls INTEGER NOT NULL DEFAULT 0,
             input_tokens INTEGER NOT NULL DEFAULT 0,
             output_tokens INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY(account_id, day));
         INSERT INTO stage2_usage VALUES (1, '2026-07-01', 3, 100, 50);
         CREATE TABLE triage(message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             model_used TEXT, status TEXT NOT NULL DEFAULT 'new',
             tier TEXT NOT NULL DEFAULT 'noise', deadline TEXT);
         CREATE TABLE deadlines(id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             message_id INTEGER NOT NULL, kind TEXT NOT NULL, due_at TEXT NOT NULL,
             past_due INTEGER NOT NULL DEFAULT 0, source TEXT NOT NULL);
         CREATE TABLE messages(id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             received_at TEXT NOT NULL);",
    )
    .unwrap();
    migrate(&conn).unwrap();
    // Old row preserved under the default category.
    let (cat, calls): (String, i64) = conn
        .query_row(
            "SELECT category, calls FROM stage2_usage WHERE account_id=1 AND day='2026-07-01'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cat, "stage2");
    assert_eq!(calls, 3);
    // The category-keyed upsert now works (this was the failing bump).
    conn.execute(
        "INSERT INTO stage2_usage(account_id, day, category, calls, input_tokens, output_tokens)
         VALUES (1, '2026-07-01', 'stage1', 1, 10, 5)
         ON CONFLICT(account_id, day, category) DO UPDATE SET calls = calls + 1",
        [],
    )
    .unwrap();
    // Idempotent: migrate again is a no-op.
    migrate(&conn).unwrap();
}

#[test]
fn migrate_adds_cache_token_columns_to_a_preexisting_usage_table() {
    // A category-keyed stage2_usage that predates the cache columns gains them
    // additively — old rows read 0, and the cache-carrying bump upsert works.
    // Also covers the rebuild path: the pre-category shape from the test above
    // is rebuilt WITHOUT the cache columns, so they must be added after.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE stage2_usage (
             account_id INTEGER NOT NULL, day TEXT NOT NULL,
             category TEXT NOT NULL DEFAULT 'stage2',
             calls INTEGER NOT NULL DEFAULT 0,
             input_tokens INTEGER NOT NULL DEFAULT 0,
             output_tokens INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY(account_id, day, category));
         INSERT INTO stage2_usage VALUES (1, '2026-08-01', 'stage2', 2, 100, 50);",
    )
    .unwrap();
    migrate(&conn).unwrap();
    let (cache_w, cache_r): (i64, i64) = conn
        .query_row(
            "SELECT cache_creation_tokens, cache_read_tokens FROM stage2_usage
             WHERE account_id=1 AND day='2026-08-01' AND category='stage2'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((cache_w, cache_r), (0, 0), "history reads as zero cache");
    // The widened bump upsert (this was the failing INSERT pre-migration).
    conn.execute(
        "INSERT INTO stage2_usage(account_id, day, category, calls, input_tokens, output_tokens,
                                  cache_creation_tokens, cache_read_tokens)
         VALUES (1, '2026-08-01', 'stage2', 1, 10, 5, 700, 9000)
         ON CONFLICT(account_id, day, category) DO UPDATE SET
             cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
             cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens",
        [],
    )
    .unwrap();
    // Idempotent: migrate again is a no-op.
    migrate(&conn).unwrap();
}

#[test]
fn migrate_cleans_year_slipped_stage_deadlines() {
    // A stage-sourced deadline far before its message's receipt is nulled
    // (tier demoted from past_due) and its deadlines row deleted;
    // deterministic-source rows are untouched.
    let store = SqliteStore::open_in_memory().unwrap();
    let conn = store.lock().unwrap();
    conn.execute_batch(
        "INSERT INTO accounts(id, email, created_at) VALUES (1, 'a@b', '2026-01-01T00:00:00Z');
         INSERT INTO messages(id, account_id, gmail_msg_id, thread_id, from_addr, subject,
                              received_at, snippet)
             VALUES (1, 1, 'g1', 't1', 'x@y', 's', '2026-07-23T00:00:00Z', ''),
                    (2, 1, 'g2', 't2', 'x@y', 's', '2026-07-23T00:00:00Z', '');
         INSERT INTO triage(message_id, account_id, tier, deadline, created_at, status)
             VALUES (1, 1, 'past_due', '2025-07-24T00:00:00Z', '2026-07-23T00:00:00Z', 'new'),
                    (2, 1, 'past_due', '2025-07-24T00:00:00Z', '2026-07-23T00:00:00Z', 'new');
         INSERT INTO deadlines(account_id, message_id, kind, due_at, past_due, source)
             VALUES (1, 1, 'bill', '2025-07-24T00:00:00Z', 1, 'stage1'),
                    (1, 2, 'bill', '2025-07-24T00:00:00Z', 1, 'subject');",
    )
    .unwrap();
    migrate(&conn).unwrap();
    // Stage-sourced year-slip: deadline nulled, tier demoted, row gone.
    let (dl1, tier1): (Option<String>, String) = conn
        .query_row(
            "SELECT deadline, tier FROM triage WHERE message_id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(dl1, None);
    assert_eq!(tier1, "deadline");
    let n1: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM deadlines WHERE message_id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n1, 0);
    // Deterministic source ('subject'): untouched (explicit text is data).
    let dl2: Option<String> = conn
        .query_row("SELECT deadline FROM triage WHERE message_id=2", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(dl2.is_some(), "deterministic-source deadline must survive");
}

#[test]
fn migrate_backfill_keeps_only_processed_history_out_of_stage1() {
    // Old-semantics rows on a pre-two-stage `triage` table; the backfill must
    // mark ONLY genuinely-processed history 'migrated' (out of the Stage-1
    // queue) and leave genuinely-unprocessed recent rows to re-queue.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE triage(
             message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             model_used TEXT, status TEXT NOT NULL DEFAULT 'new');
         INSERT INTO triage(message_id, account_id, model_used, status) VALUES
             (1, 1, NULL, 'open'),       -- finalized/seen (past 'new')
             (2, 1, 'claude-x', 'new'),  -- old stage-2 processed
             (3, 1, NULL, 'new');        -- genuinely new & unprocessed",
    )
    .unwrap();
    migrate(&conn).unwrap();
    migrate(&conn).unwrap(); // idempotent: backfill fires ONCE at column add

    let get = |mid: i64| -> (Option<String>, i64) {
        conn.query_row(
            "SELECT stage1_model_used, needs_stage2 FROM triage WHERE message_id=?1",
            [mid],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)),
        )
        .unwrap()
    };
    assert_eq!(
        get(1).0.as_deref(),
        Some("migrated"),
        "seen/finalized row out of Stage-1"
    );
    assert_eq!(
        get(2).0.as_deref(),
        Some("migrated"),
        "old stage-2 processed row out"
    );
    assert_eq!(
        get(3).0,
        None,
        "genuinely-new unprocessed row re-enters Stage-1"
    );
    assert_eq!(
        get(3).1,
        0,
        "residual row's needs_stage2 rests at 0 (Stage-1 recomputes)"
    );
}

// ---- categorize-then-extract: schema migration + extractor queue -------

#[test]
fn migrate_adds_category_and_extractor_columns_to_preexisting_triage() {
    // A `triage` table predating the categorize-then-extract feature. The
    // additive migration must add both columns so the queue/apply SQL that
    // NAMES them doesn't fail with "no such column". Idempotent on re-open.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE triage(
             message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             model_used TEXT, status TEXT NOT NULL DEFAULT 'new');",
    )
    .unwrap();
    migrate(&conn).unwrap();
    migrate(&conn).unwrap(); // idempotent
    let mut stmt = conn.prepare("PRAGMA table_info(triage)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(
        cols.iter().any(|c| c == "category"),
        "category column added"
    );
    assert!(
        cols.iter().any(|c| c == "extractor_model_used"),
        "extractor_model_used column added"
    );
}

#[test]
fn migrate_adds_item_name_source_with_regex_default_to_preexisting_shipments() {
    // A `shipments` table predating item-name provenance. The additive
    // migration adds the column and every historical row reads 'regex' — every
    // pre-existing name came from the ingest detector, so the LLM specialist
    // may replace any of them. Idempotent on re-open.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE shipments(
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             tracking_number TEXT NOT NULL, carrier TEXT NOT NULL,
             item_name TEXT NOT NULL DEFAULT '',
             status TEXT NOT NULL DEFAULT 'shipped',
             first_seen TEXT NOT NULL, last_update TEXT NOT NULL);
         INSERT INTO shipments(id, account_id, tracking_number, carrier, item_name,
                               first_seen, last_update)
             VALUES (1, 1, '1Z999AA10123456784', 'ups', 'Shipped Allbirds!',
                     '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z');",
    )
    .unwrap();
    migrate(&conn).unwrap();
    migrate(&conn).unwrap(); // idempotent
    let src: String = conn
        .query_row(
            "SELECT item_name_source FROM shipments WHERE id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(src, "regex", "historical names read as regex-sourced");
}

#[test]
fn init_upgrades_old_db_with_banking_table_and_extract_flow() {
    // OLD-shape triage (no category / extractor_model_used) and NO banking
    // table. init() applies SCHEMA (creates the banking table IF NOT EXISTS)
    // then migrate() (adds the two triage columns), so the whole extract flow
    // works on the upgraded DB.
    register_vec_extension();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE triage(
             message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             importance INTEGER NOT NULL DEFAULT 0, tier TEXT NOT NULL DEFAULT 'noise',
             sensitivity TEXT NOT NULL DEFAULT 'normal', sealed_kind TEXT,
             one_line TEXT NOT NULL DEFAULT '', reason TEXT NOT NULL DEFAULT '',
             deadline TEXT, matched_rule_id INTEGER, model_used TEXT,
             status TEXT NOT NULL DEFAULT 'new', surfaced_at TEXT, resolved_at TEXT,
             created_at TEXT NOT NULL);",
    )
    .unwrap();
    let store = SqliteStore::init(conn).unwrap();
    let acct = store.ensure_account("me@example.com").unwrap();

    let id = triaged_row(acct, "g-b", "t-b", None, false, Sensitivity::Normal)
        .category("banking_statement")
        .ingest(&store);

    // The banking table exists and the extract queue + apply work end to end.
    let cats = ["banking_statement", "transaction_alert"];
    let q = store.extract_queue(acct, &cats, 10).unwrap();
    assert_eq!(q.len(), 1);
    let applied = BankingApplied {
        message_id: id,
        account_id: acct,
        kind: "statement".into(),
        institution: Some("Chase".into()),
        amount: Some(1234.56),
        currency: Some("USD".into()),
        account_hint: Some("…1234".into()),
        received_at: Utc::now(),
        extractor_model_used: "claude-haiku-4-5".into(),
        auto_resolve: true,
    };
    store.banking_apply(&applied).unwrap();
    assert_eq!(store.list_banking(acct).unwrap().len(), 1);
}
