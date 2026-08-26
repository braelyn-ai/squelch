//! Additive schema migrations for pre-existing DBs.

use super::*;

/// Add `column` (`decl` = type + constraints) to `table` unless it is already
/// there. There is no schema-version counter, so presence is detected via
/// `PRAGMA table_info`, which keeps this idempotent across opens.
///
/// Returns `true` ONLY on the call that actually adds the column, which is what
/// lets a caller run a one-time backfill on the open that introduces it — never
/// on a fresh DB (schema.sql already carries it) and never again after.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<bool> {
    let cols = table_columns(conn, table)?;
    let any = !cols.is_empty();
    let present = cols.iter().any(|c| c == column);
    // An empty `table_info` means the table does not exist yet; skipping then
    // keeps this seam per-table independent for tests that build partial schemas.
    if any && !present {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
        return Ok(true);
    }
    Ok(false)
}

/// A table's column names, EMPTY when the table does not exist (SQLite answers
/// an absent `PRAGMA table_info` with no rows rather than an error).
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

/// Does `table` carry ALL of `columns`? Guards the blocks that READ columns
/// rather than add them: the migration unit tests build partial schemas, and a
/// SELECT naming a column that install never had is a hard SQL error, not a
/// no-op.
fn has_columns(conn: &Connection, table: &str, columns: &[&str]) -> Result<bool> {
    let present = table_columns(conn, table)?;
    Ok(columns.iter().all(|c| present.iter().any(|p| p == c)))
}

/// Do ALL of `tables` exist? The migration unit tests build partial schemas, so
/// every block that touches more than the table it just altered guards on this
/// rather than assuming a full DB.
fn tables_exist(conn: &Connection, tables: &[&str]) -> Result<bool> {
    let placeholders = (1..=tables.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ({placeholders})"
    ))?;
    let binds: Vec<&dyn rusqlite::ToSql> =
        tables.iter().map(|t| t as &dyn rusqlite::ToSql).collect();
    let n: i64 = stmt.query_row(binds.as_slice(), |r| r.get(0))?;
    Ok(n as usize == tables.len())
}

/// Additive, idempotent column migrations for pre-existing DBs. New tables and
/// indexes are handled by `CREATE ... IF NOT EXISTS` in `schema.sql`; only new
/// COLUMNS on an existing table need this seam.
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "messages", "list_unsubscribe", "TEXT")?;
    add_column_if_missing(
        conn,
        "messages",
        "list_unsub_one_click",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // Email-authentication verdict. NULL on every pre-existing row and NOT
    // backfilled: NULL is the safe reading (no tracking-pixel bypass), and a
    // re-sync refills it through the message upsert.
    add_column_if_missing(conn, "messages", "auth_pass", "INTEGER")?;
    // Display recipients of sent mail. NULL on every pre-existing row, which is
    // exactly the recipients-backfill queue predicate (`is_sent=1 AND to_addrs
    // IS NULL`); the backfill runs from the sync engine, over the network, so it
    // cannot live in this synchronous seam.
    add_column_if_missing(conn, "messages", "to_addrs", "TEXT")?;
    // Per-property triage reasons (JSON object). NULL on pre-existing rows.
    add_column_if_missing(conn, "triage", "field_reasons", "TEXT")?;

    // Two-stage triage markers: `stage1_model_used` gates the Stage-1 LLM queue
    // (NULL == still needs Stage-1), `needs_stage2` is the escalation flag.
    let added_stage1 = add_column_if_missing(conn, "triage", "stage1_model_used", "TEXT")?;
    add_column_if_missing(conn, "triage", "needs_stage2", "INTEGER NOT NULL DEFAULT 0")?;
    // WHICH router arm set `needs_stage2` (see `crate::triage::router`). NULL
    // means "did not escalate", and also covers every historical row, which
    // escalated under the old self-report rule and cannot be attributed now.
    add_column_if_missing(conn, "triage", "escalation_reason", "TEXT")?;
    // Re-evaluation counter (see `crate::triage::revisit`). Historical rows
    // start at 0, which is honest: they have never been revisited.
    add_column_if_missing(
        conn,
        "triage",
        "revisit_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // WHEN A HUMAN LAST ASKED for this row to be re-triaged. NULL on every
    // pre-existing row and NOT backfilled: nobody has asked, and a backfilled
    // stamp would force the whole mailbox through the LLM passes on the next
    // tick — the exact opposite of what the age-based stale skip is for.
    add_column_if_missing(conn, "triage", "retriage_at", "TEXT")?;

    // "REMIND ME LATER": the pending stamp and the fired one. NULL on every
    // pre-existing row and NOT backfilled — nobody has asked to be reminded of
    // anything, and a backfilled `reminded_at` would drag the whole mailbox into
    // the standing band (that column is one of its arms).
    add_column_if_missing(conn, "triage", "remind_at", "TEXT")?;
    add_column_if_missing(conn, "triage", "reminded_at", "TEXT")?;
    // WHEN THE USER OPENED THIS MAIL. NULL on every
    // pre-existing row and NOT backfilled, and the absence is honest rather
    // than convenient: nothing recorded opens before this column existed, and
    // inventing them would put a number in front of a user that their own
    // mailbox does not support. `share_open_rate` refuses to answer until the
    // column has been there long enough to mean something.
    if add_column_if_missing(conn, "triage", "opened_at", "TEXT")? {
        stamp_open_ledger_start(conn)?;
    }

    // AND THE SWEEP'S INDEX, HERE rather than in schema.sql, for the reason
    // spelled out at that file's `idx_triage_remind_at` note: it runs before this
    // function, so an index over a just-migrated column takes the open down. The
    // partial index is deliberate — the sweep asks only "what is due?", and every
    // row in a real mailbox has `remind_at IS NULL`, so a full index would be
    // almost entirely dead weight scanned on every poll tick.
    if has_columns(conn, "triage", &["remind_at"])? {
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_triage_remind_at
             ON triage(account_id, remind_at) WHERE remind_at IS NOT NULL",
            [],
        )?;
    }

    // stage2_usage grew `category` INSIDE ITS PRIMARY KEY. ALTER ADD COLUMN
    // cannot change a PK, and the bump upsert's ON CONFLICT(account_id, day,
    // category) needs that exact unique index, so an old table must be REBUILT —
    // otherwise every ledger bump fails and LLM spend goes unrecorded.
    let usage_has_category: bool = {
        let mut stmt = conn.prepare("PRAGMA table_info(stage2_usage)")?;
        let cols = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        cols.is_empty() || cols.iter().any(|c| c == "category")
    };
    if !usage_has_category {
        conn.execute_batch(
            "ALTER TABLE stage2_usage RENAME TO stage2_usage_old;
             CREATE TABLE stage2_usage (
                 account_id    INTEGER NOT NULL,
                 day           TEXT NOT NULL,
                 category      TEXT NOT NULL DEFAULT 'stage2',
                 calls         INTEGER NOT NULL DEFAULT 0,
                 input_tokens  INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY(account_id, day, category)
             );
             INSERT INTO stage2_usage(account_id, day, category, calls, input_tokens, output_tokens)
                 SELECT account_id, day, 'stage2', calls, input_tokens, output_tokens
                 FROM stage2_usage_old;
             DROP TABLE stage2_usage_old;",
        )?;
    }

    // Prompt-cache token columns, AFTER the category rebuild so they land on the
    // rebuilt table too. Zero is correct history: pre-existing rows never
    // recorded cache traffic, and zero cache tokens cost nothing at read time.
    add_column_if_missing(
        conn,
        "stage2_usage",
        "cache_creation_tokens",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "stage2_usage",
        "cache_read_tokens",
        "INTEGER NOT NULL DEFAULT 0",
    )?;

    // CATEGORIZE-THEN-EXTRACT markers. NULL is the correct resting value for
    // both (no category => never queued for extraction), so no backfill.
    add_column_if_missing(conn, "triage", "category", "TEXT")?;
    add_column_if_missing(conn, "triage", "extractor_model_used", "TEXT")?;

    // SHIPMENTS-EXTRACTOR trigger, plus the order reference the extractor writes
    // onto a shipment. Both rest at NULL on historical rows; the trigger gets a
    // bounded backfill at the bottom of this function (an unqueued column would
    // leave the whole existing mailbox invisible to the new extractor).
    let added_ship_extract = add_column_if_missing(conn, "triage", "ship_extract_model", "TEXT")?;
    add_column_if_missing(conn, "shipments", "order_ref", "TEXT")?;
    // AND ITS INDEX, HERE rather than in schema.sql. That file runs in full on
    // every open, BEFORE this function, so an index over a just-migrated column
    // would fail with "no such column: order_ref" on every pre-existing DB and
    // take the whole store's open down with it. `IF NOT EXISTS` keeps this
    // idempotent, and it covers fresh DBs too (schema.sql declares the column,
    // the ALTER above no-ops, this still runs).
    if tables_exist(conn, &["shipments"])? {
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_shipments_order_ref
             ON shipments(account_id, order_ref)",
            [],
        )?;
    }
    // `shipment_orders` and its index DO ride in on schema.sql: `init` runs that
    // whole file (all `CREATE ... IF NOT EXISTS`) on EVERY open, so a
    // pre-existing DB grows new TABLES there with no seam. Only new columns on
    // an existing table — and anything that references one — need this function.

    // SHIPMENT IDENTITY PROVENANCE. Three columns, each fixing a different way
    // mail-derived state used to be attributed to the wrong message.
    //
    // `created_by_message_id` — IMMUTABLE. `last_message_id` moves to whichever
    // mail most recently advanced the row, so the extractor's phantom reaping,
    // keyed on it, could delete a package established weeks earlier by another
    // email. Backfilled to `last_message_id`: for a historical row that is the
    // best available truth (often the only message the row ever saw).
    if add_column_if_missing(conn, "shipments", "created_by_message_id", "INTEGER")? {
        conn.execute(
            "UPDATE shipments SET created_by_message_id = last_message_id
             WHERE created_by_message_id IS NULL",
            [],
        )?;
    }
    // `item_name_msg` — which message's extraction supplied the CURRENT name.
    // Sealing scrubs by this, so a name donated onto a row another mail feeds
    // cannot outlive the seal of the mail it came from. Backfilled to
    // `last_message_id` wherever a name exists — again the best truth available,
    // and the pointer sealing already used.
    if add_column_if_missing(conn, "shipments", "item_name_msg", "INTEGER")? {
        conn.execute(
            "UPDATE shipments SET item_name_msg = last_message_id
             WHERE item_name_msg IS NULL AND COALESCE(item_name, '') != ''",
            [],
        )?;
    }
    // `item_name_source` — which MECHANISM supplied the name, the sibling of
    // `item_name_msg`'s which MESSAGE. 'regex' is correct history and needs no
    // backfill: every pre-existing name came from the ingest detector, so the
    // shipments extractor may replace any of them, and the detector's own
    // longer-wins heuristic still applies among them.
    add_column_if_missing(
        conn,
        "shipments",
        "item_name_source",
        "TEXT NOT NULL DEFAULT 'regex'",
    )?;
    // `order_merchant` — the namespace `order_ref` was always missing. Backfilled
    // in Rust below (it needs the registrable-domain rule, not a SQL expression);
    // a row left NULL simply matches no future order mail, which is the safe
    // direction — a missed name donation, never a cross-merchant one.
    let added_order_merchant = add_column_if_missing(conn, "shipments", "order_merchant", "TEXT")?;
    if tables_exist(conn, &["shipments"])? {
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_shipments_order_merchant
             ON shipments(account_id, order_merchant, order_ref)",
            [],
        )?;
    }
    if added_order_merchant {
        backfill_order_merchant(conn)?;
    }

    // `shipment_orders` GREW ITS UNIQUE KEY: (account_id, order_ref) became
    // (account_id, order_merchant, order_ref). ALTER ADD COLUMN cannot change a
    // unique index, and the staging upsert's ON CONFLICT names the new triple, so
    // an old table must be REBUILT or every staging write fails.
    //
    // DROP + CREATE, discarding rows, rather than a copy: staging is write-only
    // scaffolding introduced in the same unreleased slice as this migration
    // (nothing has shipped that writes it), an un-namespaced row cannot be
    // assigned a merchant after the fact, and a staged order is pure
    // mail-derived content that the next order mail from that seller recreates.
    // Guarded on the column being absent, so it runs at most once per DB. The
    // DROP takes the table's indexes with it, hence the re-CREATE — schema.sql
    // already ran this open and will not run again.
    if tables_exist(conn, &["shipment_orders"])?
        && !has_columns(conn, "shipment_orders", &["order_merchant"])?
    {
        conn.execute_batch(
            "DROP TABLE shipment_orders;
             CREATE TABLE shipment_orders (
                 id INTEGER PRIMARY KEY,
                 account_id INTEGER NOT NULL,
                 order_ref TEXT NOT NULL,
                 order_merchant TEXT NOT NULL DEFAULT '',
                 item_name TEXT NOT NULL DEFAULT '',
                 thread_id TEXT NOT NULL DEFAULT '',
                 last_message_id INTEGER,
                 item_name_msg INTEGER,
                 first_seen TEXT NOT NULL,
                 last_update TEXT NOT NULL,
                 UNIQUE(account_id, order_merchant, order_ref)
             );
             CREATE INDEX IF NOT EXISTS idx_shipment_orders_msg
                 ON shipment_orders(account_id, last_message_id);",
        )?;
    }
    // A `shipment_orders` that already carries `order_merchant` but predates
    // `item_name_msg` still needs the column (and the same name-provenance
    // backfill as `shipments`).
    if add_column_if_missing(conn, "shipment_orders", "item_name_msg", "INTEGER")? {
        conn.execute(
            "UPDATE shipment_orders SET item_name_msg = last_message_id
             WHERE item_name_msg IS NULL AND COALESCE(item_name, '') != ''",
            [],
        )?;
    }

    // Recipient-autocomplete columns. NULL is fine on old rows: the Sent
    // harvest fills both for all of history, and ongoing seeding stamps
    // last_sent_at from then on.
    add_column_if_missing(conn, "contacts", "last_sent_at", "TEXT")?;
    add_column_if_missing(conn, "contacts", "display_name", "TEXT")?;

    // Carrier-polling columns. NULL/0 is correct history and needs no backfill:
    // a pre-existing row has never been polled, so nothing beyond what its mail
    // said is known about it, and it owes zero failures.
    add_column_if_missing(conn, "shipments", "carrier_status_raw", "TEXT")?;
    add_column_if_missing(conn, "shipments", "eta", "TEXT")?;
    add_column_if_missing(conn, "shipments", "delivered_at", "TEXT")?;
    add_column_if_missing(conn, "shipments", "last_polled_at", "TEXT")?;
    add_column_if_missing(
        conn,
        "shipments",
        "poll_failures",
        "INTEGER NOT NULL DEFAULT 0",
    )?;

    // USER CLEAR. NULL is correct history for every existing row — nobody has
    // cleared anything yet — and needs no backfill. Read-side only; see the
    // column comment in schema.sql.
    add_column_if_missing(conn, "shipments", "cleared_at", "TEXT")?;

    // The cid an inline image part declared. NULL on every pre-existing row and
    // NOT backfillable from here — the Content-ID lives in the RFC822, which the
    // store never kept — so already-synced mail keeps painting its inline images
    // as tiles below the body until a re-sync re-ingests it.
    add_column_if_missing(conn, "attachments", "content_id", "TEXT")?;

    // Adding `stage1_model_used` leaves it NULL on every historical row — exactly
    // the Stage-1 queue predicate — so without this backfill the whole mailbox
    // re-classifies through the paid model. Rows already classified or already
    // seen are marked 'migrated' to stay out of the queue; the residual
    // (`status='new' AND model_used IS NULL`) correctly does re-enter Stage-1,
    // whose apply recomputes `needs_stage2` from model confidence.
    //
    // Guarded by `added_stage1` so it fires exactly once, at introduction: a
    // later run would wrongly 'migrate' rows legitimately queued for Stage-1 that
    // a read door had promoted past 'new'.
    if added_stage1 {
        conn.execute(
            "UPDATE triage SET stage1_model_used = 'migrated'
             WHERE stage1_model_used IS NULL
               AND (model_used IS NOT NULL OR status != 'new')",
            [],
        )?;
    }

    // ---- ONE-SHOTS AT THE `ship_extract_model` INTRODUCTION ----------------
    //
    // BOTH are gated on `added_ship_extract` rather than on an `app_settings`
    // done-flag. `app_settings` is keyed PER ACCOUNT and this seam runs per DB,
    // before any account is known, so a flag here would need an account loop that
    // buys nothing: "the column was absent" is already an exactly-once,
    // DB-scoped, self-recording trigger, and it is the idiom the `added_stage1`
    // backfill above established.
    if added_ship_extract {
        ship_extract_backfill(conn)?;
        clear_stale_no_extractor_markers(conn)?;
    }

    // Blind recipients on a local draft. Pre-existing rows had none, and '' is
    // the honest reading, which is also the column default: no backfill.
    add_column_if_missing(conn, "drafts", "bcc_addr", "TEXT NOT NULL DEFAULT ''")?;

    // STRANDED FAN-OUTS. A group send settles its recipients from a background
    // task, and no task survives the process. So any recipient still 'pending'
    // when the store opens belongs to a batch that died — a restart, a crash, a
    // kill mid-send — and leaving it would make a months-old send read as one
    // still in progress.
    //
    // Failing it is the honest reading and the safe one: the mail either went or
    // did not, we cannot tell which from here, and a recipient recorded as
    // unreached is a prompt to check rather than a silent claim of delivery. Safe
    // to run unconditionally because nothing is in flight at open.
    if tables_exist(conn, &["group_send_recipients"])? {
        conn.execute(
            "UPDATE group_send_recipients
             SET status = 'failed', error = 'the daemon restarted mid-send'
             WHERE status = 'pending'",
            [],
        )?;
    }

    // ---- ONE-SHOT: the normalized sent-recipient index ---------------------
    //
    // `message_recipients` arrives empty on an existing DB while `to_addrs` has
    // been filling for months, so without this a send group opens on a mailbox
    // that looks like it has never written to anyone. Derived history is the
    // half of the feature that can speak about the past, and this is what gives
    // it something to read.
    //
    // Self-triggering and idempotent: the candidate predicate is "sent, has
    // recipients, has no index rows", so a completed pass matches nothing and an
    // interrupted one resumes where it stopped. No done-flag and no `added_*`
    // gate — a plain re-run is free.
    backfill_message_recipients(conn)?;

    // Guarded on table existence — migration unit tests build partial schemas.
    if !tables_exist(conn, &["triage", "deadlines", "messages"])? {
        return Ok(());
    }
    // A model-sourced deadline more than 45 days BEFORE its own message's
    // receipt is a hallucinated year, so clear it (the mail stays surfaced, just
    // dateless), demote a stranded past_due tier, and drop the deadlines row.
    // Deterministic parses (source not stage1/stage2) are untouched — explicit
    // year text in an email is data. Idempotent: the predicate cannot re-match
    // once cleared.
    conn.execute(
        "UPDATE triage SET deadline = NULL,
                tier = CASE WHEN tier = 'past_due' THEN 'deadline' ELSE tier END
         WHERE deadline IS NOT NULL
           AND message_id IN (
             SELECT d.message_id FROM deadlines d
             JOIN messages m ON m.id = d.message_id
             WHERE d.source IN ('stage1', 'stage2')
               AND julianday(m.received_at) - julianday(d.due_at) > 45
           )",
        [],
    )?;
    conn.execute(
        "DELETE FROM deadlines
         WHERE source IN ('stage1', 'stage2')
           AND id IN (
             SELECT d.id FROM deadlines d
             JOIN messages m ON m.id = d.message_id
             WHERE julianday(m.received_at) - julianday(d.due_at) > 45
           )",
        [],
    )?;
    Ok(())
}

/// ONE-SHOT at the `order_merchant` introduction: stamp each order-bearing
/// shipment with the registrable domain of the message that CREATED it, so
/// pre-existing rows stay reachable by a later order mail from the same shop.
///
/// Runs in Rust rather than SQL because the merchant key is the shared
/// registrable-domain rule ([`super::specialists::merchant_key`]) — a second,
/// drifting definition in SQL is exactly the bug the namespacing exists to stop.
/// Rows whose creator is unknown or has no derivable domain are LEFT NULL: they
/// then match no order mail at all, which loses a name donation but can never
/// bind one merchant's product to another's package.
/// Record, per existing account, the moment the open ledger started running.
///
/// WITHOUT THIS THE FIRST NUMBER IS A LIE, and it is a flattering one. An
/// established mailbox has years of mail and, the instant `opened_at` is added,
/// ZERO opens - so a rate computed over "the last 90 days of mail" divides a
/// real denominator by an empty numerator and reports that its owner opens
/// almost nothing. The sample floors in `sharing::share_stat` do not catch it:
/// they measure the age of the MAIL, which is ample, not the age of the LEDGER,
/// which is seconds.
///
/// So the ledger dates itself, and `Store::share_open_rate` refuses to look
/// back further than this. An account created AFTER this migration gets no row
/// and needs none: it has been recording since it existed, which is what its
/// own `created_at` says.
///
/// `INSERT OR IGNORE`, so a re-run cannot move a stamp that is already the
/// truth - which would restart the clock and hide the number for another month.
fn stamp_open_ledger_start(conn: &Connection) -> Result<()> {
    // BOTH TABLES OR NEITHER. In production this is never false: `schema.sql`
    // creates both and runs in full before this function does. It is false only
    // in the migration suite, which builds deliberately partial old schemas to
    // migrate FROM, and a stamp into a table that does not exist there would
    // fail a test about something else entirely.
    if !tables_exist(conn, &["app_settings", "accounts"])? {
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO app_settings(account_id, key, value)
         SELECT id, ?1, ?2 FROM accounts",
        rusqlite::params![OPEN_LEDGER_SINCE_KEY, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn backfill_order_merchant(conn: &Connection) -> Result<()> {
    if !has_columns(
        conn,
        "shipments",
        &["order_ref", "order_merchant", "created_by_message_id"],
    )? || !has_columns(conn, "messages", &["from_addr"])?
    {
        return Ok(());
    }
    let rows: Vec<(i64, String)> = conn
        .prepare(
            "SELECT s.id, m.from_addr FROM shipments s
             JOIN messages m ON m.id = s.created_by_message_id AND m.account_id = s.account_id
             WHERE s.order_ref IS NOT NULL AND s.order_merchant IS NULL",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, from_addr) in rows {
        let merchant = super::specialists::merchant_key(&from_addr);
        if !merchant.is_empty() {
            conn.execute(
                "UPDATE shipments SET order_merchant = ?2 WHERE id = ?1",
                params![id, merchant],
            )?;
        }
    }
    Ok(())
}

/// How far back the shipments-trigger backfill looks. A package older than this
/// has either arrived or been written off; extracting it spends model budget to
/// resurrect a card nobody wants.
const SHIP_BACKFILL_DAYS: i64 = 30;

/// HARD CAP on rows the backfill may pend, applied to the candidate SELECT
/// itself and not just to the matches. The extractor is a paid per-row model
/// call, so the worst case a migration can commit the user to is this many —
/// a busy 30-day mailbox stays bounded, and anything the cap misses still gets
/// picked up by the ongoing ingest trigger the next time that seller writes.
const SHIP_BACKFILL_MAX: usize = 500;

/// ONE-SHOT at the `ship_extract_model` introduction: pend the recent mail that
/// carries a loose shipping signal, so the shipments extractor has history to
/// work from instead of only new arrivals.
///
/// The signal test is the SAME Rust predicate ingest uses
/// ([`has_loose_shipping_signal`](crate::triage::shipment::has_loose_shipping_signal)),
/// run row by row rather than reimplemented as SQL LIKEs — a second, drifting
/// definition of "shipping mail" is exactly the bug this column exists to avoid.
fn ship_extract_backfill(conn: &Connection) -> Result<()> {
    // Column-level guard, not just table-level: this SELECT names the message
    // surfaces the signal test reads, and a partial-schema DB that lacks one
    // would fail the whole open rather than skip a backfill.
    if !has_columns(
        conn,
        "messages",
        &["from_addr", "subject", "body", "received_at", "is_sent"],
    )? || !tables_exist(conn, &["triage"])?
    {
        return Ok(());
    }
    let cutoff = (Utc::now() - chrono::Duration::days(SHIP_BACKFILL_DAYS)).to_rfc3339();
    // Newest first so the cap keeps the mail most likely to still be in flight.
    let candidates: Vec<(i64, String, String, String)> = conn
        .prepare(
            "SELECT m.id, m.from_addr, m.subject, m.body
             FROM messages m
             JOIN triage t ON t.message_id = m.id
             WHERE m.account_id = t.account_id
               AND m.is_sent = 0
               AND m.received_at >= ?1
               AND COALESCE(t.sensitivity, 'normal') = 'normal'
             ORDER BY m.received_at DESC
             LIMIT ?2",
        )?
        .query_map(params![cutoff, SHIP_BACKFILL_MAX as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    for (id, from_addr, subject, body) in candidates {
        if crate::triage::shipment::has_loose_shipping_signal(&from_addr, &subject, &body) {
            conn.execute(
                "UPDATE triage SET ship_extract_model = 'pending' WHERE message_id = ?1",
                params![id],
            )?;
        }
    }
    Ok(())
}

/// ONE-SHOT, same trigger: un-stick the rows the DISPATCH BUG stranded.
///
/// While `extractable_categories` queued a category that `extractor_for_category`
/// did not handle, the extract pass stamped those rows `'skip-no-extractor'` —
/// a PROCESSED marker. The dispatch is fixed, but the marker is permanent, so
/// those rows (the whole marketing corpus of that era) would never re-queue.
/// NULLing it puts them back in the extract queue, and only for categories that
/// really do have an extractor now: a `'skip-no-extractor'` on a genuinely
/// unhandled category is an honest verdict and stays.
fn clear_stale_no_extractor_markers(conn: &Connection) -> Result<()> {
    if !tables_exist(conn, &["triage"])? {
        return Ok(());
    }
    let stranded: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT COALESCE(category, '') FROM triage
             WHERE extractor_model_used = 'skip-no-extractor'",
        )?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for category in stranded {
        if crate::triage::extract::extractor_for_category(&category).is_some() {
            conn.execute(
                "UPDATE triage SET extractor_model_used = NULL
                 WHERE extractor_model_used = 'skip-no-extractor' AND category = ?1",
                params![category],
            )?;
        }
    }
    Ok(())
}

/// ONE-SHOT (and resumable): fill `message_recipients` from the `to_addrs`
/// display strings already on disk.
///
/// Local and parse-only — no network, unlike the recipients sweep in the sync
/// engine that produced `to_addrs` in the first place. Rows whose `to_addrs` is
/// NULL are not candidates here: they are that sweep's queue, and it writes the
/// index itself as it fills them.
///
/// An EMPTY `to_addrs` ("looked, and the headers named nobody") yields no rows
/// and therefore stays a candidate forever. That is a handful of rows parsing to
/// nothing on each open, which is cheaper than a done-flag per account for a
/// table this seam scans once.
fn backfill_message_recipients(conn: &Connection) -> Result<()> {
    if !tables_exist(conn, &["messages", "message_recipients"])?
        || !has_columns(conn, "messages", &["to_addrs", "is_sent"])?
    {
        return Ok(());
    }
    let rows: Vec<(i64, i64, String)> = conn
        .prepare(
            "SELECT m.id, m.account_id, m.to_addrs FROM messages m
             WHERE m.is_sent = 1
               AND m.to_addrs IS NOT NULL
               AND m.to_addrs != ''
               AND NOT EXISTS (
                   SELECT 1 FROM message_recipients r
                   WHERE r.account_id = m.account_id AND r.message_id = m.id)",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    for (message_id, account_id, to_addrs) in rows {
        for addr in crate::sync::ingest::parse_stored_recipients(&to_addrs) {
            conn.execute(
                "INSERT OR IGNORE INTO message_recipients(account_id, message_id, addr)
                 VALUES(?1,?2,?3)",
                params![account_id, message_id, addr],
            )?;
        }
    }
    Ok(())
}
