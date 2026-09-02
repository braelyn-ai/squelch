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

// ---- the shipments trigger: column, backfill, and the stale-marker sweep ----

/// A DB shaped like an install that predates `ship_extract_model`: the two
/// tables the one-shots read, with no shipping columns on either.
fn preship_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE messages(
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             gmail_msg_id TEXT NOT NULL, from_addr TEXT NOT NULL DEFAULT '',
             subject TEXT NOT NULL DEFAULT '', body TEXT NOT NULL DEFAULT '',
             received_at TEXT NOT NULL, is_sent INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE triage(
             message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             sensitivity TEXT NOT NULL DEFAULT 'normal',
             category TEXT, extractor_model_used TEXT,
             model_used TEXT, status TEXT NOT NULL DEFAULT 'new');",
    )
    .unwrap();
    conn
}

/// Insert a message + its triage row, `days_ago` old.
fn preship_msg(conn: &Connection, id: i64, subject: &str, body: &str, days_ago: i64, sent: bool) {
    let at = (Utc::now() - chrono::Duration::days(days_ago)).to_rfc3339();
    conn.execute(
        "INSERT INTO messages(id, account_id, gmail_msg_id, from_addr, subject, body,
                              received_at, is_sent)
         VALUES(?1, 1, ?2, 'orders@shop.com', ?3, ?4, ?5, ?6)",
        params![id, format!("g{id}"), subject, body, at, sent as i64],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO triage(message_id, account_id) VALUES(?1, 1)",
        params![id],
    )
    .unwrap();
}

fn ship_marker(conn: &Connection, id: i64) -> Option<String> {
    conn.query_row(
        "SELECT ship_extract_model FROM triage WHERE message_id=?1",
        params![id],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn migrate_backfills_ship_extract_only_for_recent_signal_bearing_mail() {
    let conn = preship_db();
    // Recent + shipping signal -> pending.
    preship_msg(
        &conn,
        1,
        "Your order has shipped",
        "Tracking number 1Z999AA10123456784",
        2,
        false,
    );
    // Recent, no shipping signal at all -> untouched.
    preship_msg(&conn, 2, "Lunch tomorrow?", "See you at noon", 2, false);
    // Recent, but a RETURN -> excluded by the same precedence detection uses.
    preship_msg(
        &conn,
        3,
        "Your return label",
        "Print the return label and drop off your package",
        2,
        false,
    );
    // Signal-bearing but OUTSIDE the 30-day window -> untouched.
    preship_msg(
        &conn,
        4,
        "Your package was delivered",
        "Delivered to your porch",
        90,
        false,
    );
    // Signal-bearing SENT mail -> the user's own outbox is never tracked.
    preship_msg(
        &conn,
        5,
        "Re: your package shipped",
        "thanks, tracking looks right",
        2,
        true,
    );

    migrate(&conn).unwrap();
    assert_eq!(ship_marker(&conn, 1).as_deref(), Some("pending"));
    assert_eq!(ship_marker(&conn, 2), None, "no shipping signal");
    assert_eq!(ship_marker(&conn, 3), None, "a return is not a shipment");
    assert_eq!(ship_marker(&conn, 4), None, "outside the 30-day window");
    assert_eq!(ship_marker(&conn, 5), None, "sent mail is never queued");

    // Idempotent: the one-shot fires only on the open that ADDS the column, so
    // a second migrate must not re-pend a row an extractor has since ruled on.
    conn.execute(
        "UPDATE triage SET ship_extract_model='claude-x' WHERE message_id=1",
        [],
    )
    .unwrap();
    migrate(&conn).unwrap();
    assert_eq!(
        ship_marker(&conn, 1).as_deref(),
        Some("claude-x"),
        "the backfill must not fire twice"
    );
}

#[test]
fn migrate_ship_extract_backfill_honors_its_hard_row_cap() {
    // 600 signal-bearing messages, newest first by id. The cap is on the
    // CANDIDATE select, so exactly 500 rows can ever be pended, and they are the
    // newest ones (the mail most likely to still be in flight).
    let conn = preship_db();
    for i in 1..=600i64 {
        // id 600 is newest (0 days old), id 1 is oldest (~20 days old).
        preship_msg(
            &conn,
            i,
            "Your order has shipped",
            "on its way",
            (600 - i) / 30,
            false,
        );
    }
    migrate(&conn).unwrap();
    let pended: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM triage WHERE ship_extract_model='pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        pended, 500,
        "the hard cap bounds what a migration can spend"
    );
    assert_eq!(
        ship_marker(&conn, 600).as_deref(),
        Some("pending"),
        "newest mail is kept"
    );
    assert_eq!(ship_marker(&conn, 1), None, "oldest mail falls off the cap");
}

#[test]
fn migrate_clears_stale_skip_no_extractor_only_where_an_extractor_exists() {
    // Rows stamped 'skip-no-extractor' while the dispatch bug hid the marketing
    // extractor are permanently processed and would never re-queue. The sweep
    // frees exactly those; a category that genuinely has no extractor keeps its
    // honest verdict, and a real model marker is never touched.
    let conn = preship_db();
    conn.execute_batch(
        "INSERT INTO triage(message_id, account_id, category, extractor_model_used) VALUES
             (1, 1, 'marketing', 'skip-no-extractor'),
             (2, 1, 'invoice', 'skip-no-extractor'),
             (3, 1, 'banking_statement', 'claude-haiku-4-5'),
             (4, 1, NULL, 'skip-no-extractor');",
    )
    .unwrap();
    migrate(&conn).unwrap();

    let ext = |mid: i64| -> Option<String> {
        conn.query_row(
            "SELECT extractor_model_used FROM triage WHERE message_id=?1",
            params![mid],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(ext(1), None, "marketing has an extractor now: re-queue it");
    assert_eq!(
        ext(2).as_deref(),
        Some("skip-no-extractor"),
        "invoice really has no extractor"
    );
    assert_eq!(
        ext(3).as_deref(),
        Some("claude-haiku-4-5"),
        "a real model marker is untouched"
    );
    assert_eq!(
        ext(4).as_deref(),
        Some("skip-no-extractor"),
        "a NULL category has no extractor"
    );
}

#[test]
fn migrate_adds_order_ref_and_init_creates_the_orders_staging_table() {
    // The column seam covers `shipments.order_ref`; `shipment_orders` and its
    // indexes ride in on schema.sql, which `init` runs on EVERY open — the
    // reason migrate needs no CREATE TABLE of its own.
    register_vec_extension();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE shipments (
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             tracking_number TEXT NOT NULL, carrier TEXT NOT NULL,
             item_name TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'shipped',
             tracking_url TEXT, last_message_id INTEGER,
             first_seen TEXT NOT NULL, last_update TEXT NOT NULL,
             UNIQUE(account_id, tracking_number));",
    )
    .unwrap();
    let store = SqliteStore::init(conn).unwrap();
    let conn = store.lock().unwrap();
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(shipments)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "order_ref"), "order_ref added");
    // The user-clear column lands on the same seam, NULL on every historical row
    // (nobody has cleared anything yet), so no backfill and nothing hidden.
    assert!(cols.iter().any(|c| c == "cleared_at"), "cleared_at added");
    let staged: i64 = conn
        .query_row("SELECT COUNT(*) FROM shipment_orders", [], |r| r.get(0))
        .unwrap();
    assert_eq!(staged, 0, "the staging table exists on an upgraded DB");
}

#[test]
fn migrate_adds_shipment_provenance_and_backfills_it_from_the_pointer() {
    // An install predating the provenance columns. `created_by_message_id` and
    // `item_name_msg` are both backfilled to `last_message_id` — for a
    // historical row that is the best available truth, and it is the pointer
    // sealing and reaping already (wrongly) relied on. `order_merchant` is
    // backfilled from the creating message's sender domain.
    register_vec_extension();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE messages(
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             gmail_msg_id TEXT NOT NULL, thread_id TEXT NOT NULL DEFAULT '',
             from_addr TEXT NOT NULL DEFAULT '', subject TEXT NOT NULL DEFAULT '',
             body TEXT NOT NULL DEFAULT '', received_at TEXT NOT NULL DEFAULT '',
             is_sent INTEGER NOT NULL DEFAULT 0);
         INSERT INTO messages(id, account_id, gmail_msg_id, from_addr)
             VALUES(7, 1, 'g7', 'orders@mail.shopa.com');
         CREATE TABLE shipments (
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             tracking_number TEXT NOT NULL, carrier TEXT NOT NULL,
             item_name TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'shipped',
             tracking_url TEXT, last_message_id INTEGER, order_ref TEXT,
             first_seen TEXT NOT NULL, last_update TEXT NOT NULL,
             UNIQUE(account_id, tracking_number));
         INSERT INTO shipments(id, account_id, tracking_number, carrier, item_name,
                               last_message_id, order_ref, first_seen, last_update)
             VALUES(1, 1, '1Z999AA10123456784', 'ups', 'Standing desk', 7, '1042', '', ''),
                   (2, 1, '9400111899223817428490', 'usps', '', 7, NULL, '', '');",
    )
    .unwrap();

    let store = SqliteStore::init(conn).unwrap();
    let conn = store.lock().unwrap();
    let row = |id: i64| -> (Option<i64>, Option<i64>, Option<String>) {
        conn.query_row(
            "SELECT created_by_message_id, item_name_msg, order_merchant
             FROM shipments WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    // `item_name_source` lands on the same seam with NO backfill: every
    // historical name came from the ingest detector, so 'regex' is the truth and
    // the extractor is free to replace any of them.
    let source = |id: i64| -> String {
        conn.query_row(
            "SELECT item_name_source FROM shipments WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(source(1), "regex");
    assert_eq!(source(2), "regex");
    assert_eq!(
        row(1),
        (Some(7), Some(7), Some("shopa.com".to_string())),
        "provenance backfills from the pointer, merchant from the sender"
    );
    assert_eq!(
        row(2),
        (Some(7), None, None),
        "no name and no order reference means nothing to attribute"
    );
}

#[test]
fn migrate_rebuilds_the_staging_table_onto_the_merchant_scoped_key() {
    // `shipment_orders` grew its UNIQUE key from (account, order_ref) to
    // (account, merchant, order_ref). ALTER cannot change a unique index, and the
    // staging upsert's ON CONFLICT names the new triple, so an un-namespaced
    // table must be rebuilt or every staging write fails outright.
    register_vec_extension();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE shipment_orders (
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             order_ref TEXT NOT NULL, item_name TEXT NOT NULL DEFAULT '',
             thread_id TEXT NOT NULL DEFAULT '', last_message_id INTEGER,
             first_seen TEXT NOT NULL, last_update TEXT NOT NULL,
             UNIQUE(account_id, order_ref));
         INSERT INTO shipment_orders(account_id, order_ref, item_name, first_seen, last_update)
             VALUES(1, '1042', 'Cat bed', '', '');",
    )
    .unwrap();

    let store = SqliteStore::init(conn).unwrap();
    let conn = store.lock().unwrap();
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(shipment_orders)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "order_merchant"));
    assert!(cols.iter().any(|c| c == "item_name_msg"));

    // Two shops CAN now stage the same reference — the point of the rebuild —
    // which the old unique key made impossible.
    for merchant in ["shopa.com", "shopb.com"] {
        conn.execute(
            "INSERT INTO shipment_orders(account_id, order_ref, order_merchant,
                                         first_seen, last_update)
             VALUES(1, '1042', ?1, '', '')
             ON CONFLICT(account_id, order_merchant, order_ref) DO NOTHING",
            params![merchant],
        )
        .unwrap();
    }
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM shipment_orders WHERE order_ref = '1042'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2, "the rebuilt key namespaces by merchant");

    // The index the DROP took with it is back.
    let idx: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='index' AND name='idx_shipment_orders_msg'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "the rebuild re-creates the dropped index");
}

#[test]
fn migrate_adds_reminder_columns_and_the_due_index_to_preexisting_triage() {
    // A `triage` table predating "remind me later". Both columns must land (the
    // reminder SQL NAMES them, so a missing one is a hard error rather than a
    // dark feature), and so must the partial index the sync sweep reads — it
    // cannot live in schema.sql, which runs BEFORE this seam and would fail on
    // "no such column: remind_at". Idempotent on re-open, index included.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE triage(
             message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             model_used TEXT, status TEXT NOT NULL DEFAULT 'new');
         INSERT INTO triage(message_id, account_id) VALUES (1, 1);",
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
    assert!(cols.iter().any(|c| c == "remind_at"), "remind_at added");
    assert!(cols.iter().any(|c| c == "reminded_at"), "reminded_at added");

    let indexed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='index' AND name='idx_triage_remind_at'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        indexed, 1,
        "the sweep's index is created here, not in schema.sql"
    );

    // NOT backfilled: `reminded_at` is a standing-band arm, so a stamped
    // historical row would drag the whole mailbox into the band.
    let (remind, reminded): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT remind_at, reminded_at FROM triage WHERE message_id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(remind, None);
    assert_eq!(reminded, None);
}

#[test]
fn migrate_adds_notify_eligible_at_null_to_a_preexisting_triage_table() {
    // A `triage` table predating the notify-eligibility stamp. The column has
    // to land through the migration seam rather than schema.sql, because
    // schema.sql's `CREATE TABLE IF NOT EXISTS` is a no-op against a table that
    // already exists — and `ingest_message` NAMES the column in both halves of
    // its upsert, so a missing one is a hard "no such column" on the very first
    // message rather than a quietly dark feature.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE triage(
             message_id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             model_used TEXT, status TEXT NOT NULL DEFAULT 'new');
         INSERT INTO triage(message_id, account_id) VALUES (1, 1);",
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
        cols.iter().any(|c| c == "notify_eligible_at"),
        "notify_eligible_at added"
    );

    // NOT BACKFILLED, and the NULL is the whole safety property. A stamp
    // invented for mail already in the database would make the first tick after
    // an upgrade eligible to push a month of archived mail at a phone; historical
    // rows simply never notify, which is exactly what they do today.
    let stamp: Option<String> = conn
        .query_row(
            "SELECT notify_eligible_at FROM triage WHERE message_id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stamp, None, "every pre-existing row rests at NULL");
}

#[test]
fn migrate_adds_sealed_kind_null_to_a_preexisting_events_table() {
    // An `events` table predating the sealed routing tag. The column has to land
    // through the migration seam rather than schema.sql, because schema.sql's
    // `CREATE TABLE IF NOT EXISTS` is a no-op against a table that already
    // exists — and both event SELECTs plus `append_event` NAME the column, so a
    // missing one is a hard "no such column" on the very first notification.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE events(
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             message_id INTEGER NOT NULL UNIQUE, thread_id TEXT NOT NULL,
             kind TEXT NOT NULL, tier TEXT NOT NULL, importance INTEGER NOT NULL,
             sender TEXT NOT NULL, one_line TEXT NOT NULL, deadline TEXT,
             created_at TEXT NOT NULL);
         INSERT INTO events(id, account_id, message_id, thread_id, kind, tier, importance,
                            sender, one_line, created_at)
             VALUES (1, 1, 1, 't1', 'surfaced', 'signal', 70, 'a@x.com', 'old',
                     '2026-08-01T00:00:00Z');",
    )
    .unwrap();
    migrate(&conn).unwrap();
    migrate(&conn).unwrap(); // idempotent
    let mut stmt = conn.prepare("PRAGMA table_info(events)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "sealed_kind"), "sealed_kind added");

    // NOT BACKFILLED, and NULL is the honest reading rather than merely the
    // convenient one: no historical event came from a sealed message, because
    // the old emission path required sensitivity='normal'. There is no kind to
    // recover, and a client reads NULL as "an ordinary event" — which is what
    // every one of these rows is.
    let kind: Option<String> = conn
        .query_row("SELECT sealed_kind FROM events WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(kind, None, "every pre-existing event rests at NULL");
}

/// The LEDGER needs no migration seam at all — it is a new TABLE, and
/// `schema.sql` is all `CREATE ... IF NOT EXISTS` and runs in full on every
/// open. This asserts the thing that claim depends on: an install that predates
/// the table gets it, with its UNIQUE and its index, from `init` alone.
#[test]
fn init_creates_notify_decisions_on_a_db_that_predates_it() {
    let (store, acct) = store();
    let conn = store.lock().unwrap();

    let mut stmt = conn.prepare("PRAGMA table_info(notify_decisions)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    for expected in [
        "id",
        "account_id",
        "message_id",
        "lane",
        "decision",
        "notify_importance",
        "model_used",
        "latency_ms",
        "created_at",
    ] {
        assert!(cols.iter().any(|c| c == expected), "missing {expected}");
    }

    // The UNIQUE is the append-only rule made structural, so it is worth an
    // assertion of its own: a second row for the same (message, lane) is what
    // `INSERT OR IGNORE` relies on to be a no-op instead of a duplicate.
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO notify_decisions(account_id, message_id, lane, decision, created_at)
         VALUES(?1, 1, 'fast', 'sent', ?2)",
        params![acct, now],
    )
    .unwrap();
    let dup = conn.execute(
        "INSERT INTO notify_decisions(account_id, message_id, lane, decision, created_at)
         VALUES(?1, 1, 'fast', 'expired', ?2)",
        params![acct, now],
    );
    assert!(dup.is_err(), "UNIQUE(message_id, lane) is enforced");

    let index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='index' AND name='idx_notify_decisions_account_created'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(index, 1, "the eval read's index ships with the table");
}

#[test]
fn migrate_adds_content_id_null_to_a_preexisting_attachments_table() {
    // An install predating the cid column. The migration adds it, and every
    // already-synced attachment rests at NULL: the Content-ID only exists in the
    // RFC822, which the store never kept, so there is nothing to backfill from
    // and those inline images stay tiles until a re-sync re-ingests the mail.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE attachments(
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             message_id INTEGER NOT NULL, filename TEXT NOT NULL,
             mime TEXT NOT NULL, size_bytes INTEGER NOT NULL, data BLOB,
             UNIQUE(account_id, message_id, filename, size_bytes));
         INSERT INTO attachments(id, account_id, message_id, filename, mime, size_bytes, data)
             VALUES (1, 1, 1, 'logo.png', 'image/png', 3, x'616263');",
    )
    .unwrap();
    migrate(&conn).unwrap();
    migrate(&conn).unwrap(); // idempotent
    let mut stmt = conn.prepare("PRAGMA table_info(attachments)").unwrap();
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(cols.iter().any(|c| c == "content_id"));
    let existing: Option<String> = conn
        .query_row("SELECT content_id FROM attachments WHERE id=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        existing, None,
        "pre-existing attachments are not backfilled"
    );
}

// ---- one-shot repairs of already-stored specialist rows ----------------

#[test]
fn migrate_relaunders_a_prompt_echo_institution_out_of_banking() {
    // THE ROW THAT SHIPPED. The extractor only ever CAPPED this field, so a
    // model that emitted a correct name and then kept generating left 80
    // characters of prompt scaffolding on a card that renders one line
    // ("Venmo=== TRUSTED"). The cap is now a shape check, and the rows already
    // written have to be re-judged by it — nothing else can reach them.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE banking(
             id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL,
             message_id INTEGER NOT NULL, kind TEXT NOT NULL,
             institution TEXT, amount REAL, currency TEXT, account_hint TEXT);
         INSERT INTO banking(id, account_id, message_id, kind, institution, amount, currency)
           VALUES
             (1, 1, 981, 'transaction_alert',
              'Venmo=== TRUSTED CONTEXT (from the account owner; authoritative) ===
owner refin', NULL, NULL),
             (2, 1, 1046, 'transaction_alert',
              'Visa=== END NOTE ===Continue extraction.ionation. Return JSON.999,', 39.99, 'USD'),
             (3, 1, 1200, 'statement', 'American Express', 4138.89, 'USD'),
             (4, 1, 1291, 'autopay', NULL, 91.43, 'USD');",
    )
    .unwrap();

    migrate(&conn).unwrap();
    migrate(&conn).unwrap(); // idempotent

    let read = |id: i64| -> Option<String> {
        conn.query_row(
            "SELECT institution FROM banking WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(read(1), None, "prompt echo cleared");
    assert_eq!(read(2), None, "degenerated answer cleared");
    assert_eq!(
        read(3).as_deref(),
        Some("American Express"),
        "a real institution is left alone"
    );
    assert_eq!(read(4), None, "an already-NULL institution stays NULL");

    // THE REST OF THE ROW SURVIVES. Clearing a bad label must not cost the
    // amount, the kind, or the card tail — the row is still a real transaction.
    let (kind, amount): (String, Option<f64>) = conn
        .query_row("SELECT kind, amount FROM banking WHERE id = 2", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(kind, "transaction_alert");
    assert_eq!(amount, Some(39.99));
}

#[test]
fn institution_relaundering_survives_a_db_without_a_banking_table() {
    // The migration suite builds deliberately partial old schemas, and an
    // install predating the banking table must not fault on a pass that reads
    // one.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE messages(id INTEGER PRIMARY KEY, account_id INTEGER);")
        .unwrap();
    migrate(&conn).unwrap();
}
