//! Additive schema migrations for pre-existing DBs.

use super::*;

/// Add `column` (`decl` = type + constraints) to `table` unless it is already
/// there. There is no schema-version counter, so presence is detected via
/// `PRAGMA table_info`, which keeps this idempotent across opens.
///
/// Returns `true` ONLY on the call that actually adds the column, which is what
/// lets a caller run a one-time backfill on the open that introduces it — never
/// on a fresh DB (schema.sql already carries it) and never again after.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt.query_map([], |r| r.get::<_, String>(1))?;
    let mut present = false;
    let mut any = false;
    for c in cols {
        any = true;
        if c? == column {
            present = true;
            break;
        }
    }
    // An empty `table_info` means the table does not exist yet; skipping then
    // keeps this seam per-table independent for tests that build partial schemas.
    if any && !present {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), [])?;
        return Ok(true);
    }
    Ok(false)
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
    // Per-property triage reasons (JSON object). NULL on pre-existing rows.
    add_column_if_missing(conn, "triage", "field_reasons", "TEXT")?;

    // Two-stage triage markers: `stage1_model_used` gates the Stage-1 LLM queue
    // (NULL == still needs Stage-1), `needs_stage2` is the escalation flag.
    let added_stage1 = add_column_if_missing(conn, "triage", "stage1_model_used", "TEXT")?;
    add_column_if_missing(conn, "triage", "needs_stage2", "INTEGER NOT NULL DEFAULT 0")?;

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

    // CATEGORIZE-THEN-EXTRACT markers. NULL is the correct resting value for
    // both (no category => never queued for extraction), so no backfill.
    add_column_if_missing(conn, "triage", "category", "TEXT")?;
    add_column_if_missing(conn, "triage", "extractor_model_used", "TEXT")?;

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

    // Guarded on table existence — migration unit tests build partial schemas.
    let cleanup_tables_exist: bool = {
        let mut stmt = conn.prepare(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
             AND name IN ('triage','deadlines','messages')",
        )?;
        let n: i64 = stmt.query_row([], |r| r.get(0))?;
        n == 3
    };
    if !cleanup_tables_exist {
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
