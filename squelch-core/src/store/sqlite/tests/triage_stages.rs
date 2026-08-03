//! Stage-1 / Stage-2 / extract queue, budget and usage-ledger tests.

use super::super::*;
use super::support::*;
// The REAL marker the heuristic-only fallback stamps, not a local mirror of it.
use crate::triage::stage1_llm::HEURISTIC_ONLY;
use crate::types::{Sensitivity, Tier};

#[test]
fn extract_queue_selects_banking_only_excludes_others_sealed_and_receipts() {
    let (store, acct) = store();

    // banking_statement + transaction_alert -> queued.
    let stmt = triaged_row(
        acct, "g-stmt", "t1", None, false, Sensitivity::Normal,
    )
    .ingest(&store);
    apply_category(&store, acct, stmt, "banking_statement", false);
    let alert = triaged_row(
        acct, "g-alert", "t2", None, false, Sensitivity::Normal,
    )
    .ingest(&store);
    apply_category(&store, acct, alert, "transaction_alert", false);

    // invoice + general -> NOT queued (no extractor).
    let inv = triaged_row(acct, "g-inv", "t3", None, false, Sensitivity::Normal)
        .category("invoice")
        .ingest(&store);
    let genr = triaged_row(
        acct, "g-gen", "t4", None, false, Sensitivity::Normal,
    )
    .ingest(&store);
    apply_category(&store, acct, genr, "general", false);

    // A sealed row: category stays NULL (stage1_apply is guarded), excluded.
    let sealed = triaged_row(
        acct, "g-seal", "t5", None, false, Sensitivity::Sealed,
    )
    .ingest(&store);
    apply_category(&store, acct, sealed, "banking_statement", false); // no-op on sealed

    // A banking-categorized message that ALSO produced a receipt: excluded
    // (a receipt and a banking row must never double-create).
    let dual = triaged_row(
        acct, "g-dual", "t6", None, false, Sensitivity::Normal,
    )
    .ingest(&store);
    apply_category(&store, acct, dual, "banking_statement", false);
    store
        .upsert_receipt(
            acct,
            dual,
            "orders@shop.com",
            None,
            &crate::triage::ReceiptInfo { amount: Some(5.0), currency: Some("USD".into()) },
            Utc::now(),
        )
        .unwrap();

    let cats = ["banking_statement", "transaction_alert"];
    let q = store.extract_queue(acct, &cats, 20).unwrap();
    let ids: Vec<i64> = q.iter().map(|r| r.message_id).collect();
    assert_eq!(q.len(), 2, "only the two banking rows without a receipt: {ids:?}");
    assert!(ids.contains(&stmt));
    assert!(ids.contains(&alert));
    assert!(!ids.contains(&inv), "invoice has no extractor");
    assert!(!ids.contains(&genr), "general has no extractor");
    assert!(!ids.contains(&sealed), "sealed row carries a NULL category");
    assert!(!ids.contains(&dual), "receipt-bearing row is excluded");
}

#[test]
fn extract_bump_usage_records_its_own_ledger_category() {
    let (store, acct) = store();
    store
        .extract_bump_usage(acct, "2026-07-23", "extract_banking", 500, 20)
        .unwrap();
    store
        .extract_bump_usage(acct, "2026-07-23", "extract_banking", 300, 10)
        .unwrap();
    let conn = store.lock().unwrap();
    let (calls, in_tok, out_tok): (i64, i64, i64) = conn
        .query_row(
            "SELECT calls, input_tokens, output_tokens FROM stage2_usage
             WHERE account_id=?1 AND day=?2 AND category='extract_banking'",
            params![acct, "2026-07-23"],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(in_tok, 800);
    assert_eq!(out_tok, 30);
}

#[test]
fn stage1_queue_selects_normal_unrefined_excludes_rule_and_sealed() {
    let (store, acct) = store();

    // Normal, non-rule row -> enters the Stage-1 LLM queue.
    let normal = triaged_row(
        acct, "g-n", "t-n", None, false, Sensitivity::Normal,
    )
    .ingest(&store);
    // Explicit rule (confident) -> decided; NO Stage-1 model spend.
    triaged_row(acct, "g-r", "t-r", Some(7), true, Sensitivity::Normal).ingest(&store);
    // Sealed -> never queued for any LLM.
    triaged_row(acct, "g-s", "t-s", None, false, Sensitivity::Sealed).ingest(&store);

    let q = store.stage1_queue(acct, 10).unwrap();
    assert_eq!(q.len(), 1, "only the normal, non-rule row needs Stage-1");
    assert_eq!(q[0].message_id, normal);
    assert_eq!(q[0].sensitivity, Sensitivity::Normal);
}

#[test]
fn retriage_reset_requeues_llm_rows_but_never_rule_or_sealed() {
    let (store, acct) = store();

    let normal = triaged_row(
        acct, "g-n", "t-n", None, false, Sensitivity::Normal,
    )
    .ingest(&store);
    triaged_row(acct, "g-r", "t-r", Some(7), true, Sensitivity::Normal).ingest(&store);
    triaged_row(acct, "g-s", "t-s", None, false, Sensitivity::Sealed).ingest(&store);

    // Simulate the LLM having classified the normal row (leaves the queue).
    {
        let conn = store.lock().unwrap();
        conn.execute(
            "UPDATE triage SET stage1_model_used='claude-x', needs_stage2=1,
                    extractor_model_used='claude-x'
             WHERE message_id=?1",
            rusqlite::params![normal],
        )
        .unwrap();
    }
    assert_eq!(store.stage1_queue(acct, 10).unwrap().len(), 0);

    // Window re-triage: only the LLM-classified normal row resets.
    let n = store.retriage_reset(acct, None, 7).unwrap();
    assert_eq!(n, 1, "rule + sealed rows must never reset");
    let q = store.stage1_queue(acct, 10).unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].message_id, normal);
    // The escalation + extractor markers were cleared too.
    {
        let conn = store.lock().unwrap();
        let (needs, ext): (i64, Option<String>) = conn
            .query_row(
                "SELECT needs_stage2, extractor_model_used FROM triage WHERE message_id=?1",
                rusqlite::params![normal],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(needs, 0);
        assert_eq!(ext, None);
    }

    // Single-message scope on a sealed message: resets nothing.
    let sealed_reset = store.retriage_reset(acct, Some(999_999), 7).unwrap();
    assert_eq!(sealed_reset, 0);
}

#[test]
fn explicit_rule_row_skips_both_llm_queues() {
    let (store, acct) = store();
    // A Squelch/Surface rule row is final: not in Stage-1, not in Stage-2.
    triaged_row(acct, "g-r", "t-r", Some(9), true, Sensitivity::Normal).ingest(&store);
    assert!(store.stage1_queue(acct, 10).unwrap().is_empty());
    assert!(store.stage2_queue(acct, 10).unwrap().is_empty());
}

#[test]
fn filtered_rule_row_goes_straight_to_stage2() {
    let (store, acct) = store();
    // A Filtered rule (matched_rule set, NOT confident) skips Stage-1 and
    // escalates directly to Stage-2 for want_text evaluation.
    let id = triaged_row(
        acct, "g-f", "t-f", Some(3), false, Sensitivity::Normal,
    )
    .ingest(&store);
    assert!(store.stage1_queue(acct, 10).unwrap().is_empty(), "no Stage-1 spend");
    let s2 = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(s2.len(), 1);
    assert_eq!(s2[0].message_id, id);
}

#[test]
fn stage1_apply_confident_false_escalates_true_does_not() {
    let (store, acct) = store();
    let a = triaged_row(acct, "g-a", "t-a", None, false, Sensitivity::Normal).ingest(&store);
    let b = triaged_row(acct, "g-b", "t-b", None, false, Sensitivity::Normal).ingest(&store);

    let applied = |mid: i64, needs_stage2: bool| Stage1Applied {
        message_id: mid,
        account_id: acct,
        importance: 60,
        tier: Tier::Noise,
        one_line: "refined".into(),
        reason: "stage-1".into(),
        field_reasons: crate::types::FieldReasons::default(),
        stage1_model_used: "claude-haiku-4-5".into(),
        needs_stage2,
        deadline: None,
        category: Some("general".into()),
    };
    store.stage1_apply(&applied(a, false)).unwrap(); // confident -> final
    store.stage1_apply(&applied(b, true)).unwrap(); // not confident -> escalate

    // Both left the Stage-1 queue.
    assert!(store.stage1_queue(acct, 10).unwrap().is_empty());
    // Only `b` is now in the Stage-2 queue.
    let s2 = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(s2.len(), 1);
    assert_eq!(s2[0].message_id, b);
}

#[test]
fn stage1_mark_processed_preserves_needs_stage2_seed() {
    let (store, acct) = store();
    // Ambiguous seed (confident=false => needs_stage2 seed = 1).
    let amb = triaged_row(
        acct, "g-amb", "t-amb", None, false, Sensitivity::Normal,
    )
    .ingest(&store);
    // Confident seed (confident=true => needs_stage2 seed = 0).
    let sure = triaged_row(
        acct, "g-sure", "t-sure", None, true, Sensitivity::Normal,
    )
    .ingest(&store);

    // Heuristic-only fallback stamps the marker but PRESERVES the seed.
    store.stage1_mark_processed(acct, amb, HEURISTIC_ONLY).unwrap();
    store.stage1_mark_processed(acct, sure, HEURISTIC_ONLY).unwrap();

    assert!(store.stage1_queue(acct, 10).unwrap().is_empty());
    let s2 = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(s2.len(), 1, "only the ambiguous seed escalates");
    assert_eq!(s2[0].message_id, amb);
}

#[test]
fn stage1_usage_ledger_is_a_separate_category() {
    let (store, acct) = store();
    store.stage1_bump_usage(acct, "2026-07-09", 100, 20).unwrap();
    store.stage2_bump_usage(acct, "2026-07-09", 500, 90).unwrap();

    let s1 = store.stage1_usage_since(acct, "2026-07-01").unwrap();
    assert_eq!(s1.calls, 1);
    assert_eq!(s1.input_tokens, 100);
    assert_eq!(s1.output_tokens, 20);
    let s2 = store.stage2_usage_since(acct, "2026-07-01").unwrap();
    assert_eq!(s2.calls, 1);
    assert_eq!(s2.input_tokens, 500);

    let rows1 = store.list_usage_stage1(acct, 30).unwrap();
    assert_eq!(rows1.len(), 1);
    assert_eq!(rows1[0].input_tokens, 100);
    // The stage-2 list is unaffected by the stage-1 row.
    let rows2 = store.list_usage(acct, 30).unwrap();
    assert_eq!(rows2.len(), 1);
    assert_eq!(rows2[0].input_tokens, 500);
}

#[test]
fn list_usage_by_category_surfaces_extractors_nobody_named() {
    let (store, acct) = store();
    store.stage1_bump_usage(acct, "2026-07-09", 100, 20).unwrap();
    store.stage2_bump_usage(acct, "2026-07-09", 500, 90).unwrap();
    // An extractor category, and a category invented right here: the point of
    // enumerating is that a ledger writer added LATER still reports, without
    // anyone editing the reader.
    store
        .extract_bump_usage(acct, "2026-07-09", "extract_banking", 40, 8)
        .unwrap();
    store
        .extract_bump_usage(acct, "2026-07-10", "extract_something_new", 7, 3)
        .unwrap();

    let all = store.list_usage_by_category(acct, 30).unwrap();
    let names: Vec<&str> = all.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "extract_banking",
            "extract_something_new",
            "stage1",
            "stage2"
        ],
        "every ledger category, sorted by name"
    );

    let banking = &all.iter().find(|(c, _)| c == "extract_banking").unwrap().1;
    assert_eq!(banking.len(), 1);
    assert_eq!(banking[0].input_tokens, 40);
    assert_eq!(banking[0].output_tokens, 8);
    // Categories stay isolated — the extractor rows do not leak into stage-1.
    let s1 = &all.iter().find(|(c, _)| c == "stage1").unwrap().1;
    assert_eq!(s1[0].input_tokens, 100);
}

#[test]
fn stage2_queue_selects_only_normal_unprocessed_rows() {
    let (store, acct) = store();

    // A queued (normal, model_used NULL) row.
    let q1 = seed_triage_row(&store, acct, "g-normal", "t-1", Sensitivity::Normal);
    // A sealed row must be excluded.
    seed_triage_row(&store, acct, "g-sealed", "t-2", Sensitivity::Sealed);
    // A processed row (model_used set) must be excluded.
    let done = seed_triage_row(&store, acct, "g-done", "t-3", Sensitivity::Normal);
    store
        .stage2_mark_processed(acct, done, "claude-haiku-4-5")
        .unwrap();

    let rows = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(rows.len(), 1, "only the normal, unprocessed row is queued");
    assert_eq!(rows[0].message_id, q1);
    assert_eq!(rows[0].sensitivity, Sensitivity::Normal);
    assert!(rows[0].rule_want_text.is_none());
}

#[test]
fn stage2_queue_surfaces_matched_rule_want_text() {
    let (store, acct) = store();
    let rule_id = store
        .set_sender_rule(
            acct,
            "*@shop.com",
            "only discounts, clearance, new collections",
            Disposition::Filtered,
        )
        .unwrap();
    let id = store.upsert_message(&triaged(acct, "g1", "t1").msg()).unwrap();
    store
        .set_triage(
            id, acct, 30, Tier::Noise, Sensitivity::Normal, None, "filtered",
            "matched filtered rule", None,
        )
        .unwrap();
    // Attach the matched rule id (set_triage leaves matched_rule_id NULL).
    {
        let conn = store.lock().unwrap();
        conn.execute(
            "UPDATE triage SET matched_rule_id=?2 WHERE message_id=?1",
            params![id, rule_id],
        )
        .unwrap();
    }

    let rows = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].rule_want_text.as_deref(),
        Some("only discounts, clearance, new collections")
    );
}

#[test]
fn stage2_prompt_carries_only_the_matched_rules_want_text() {
    // DETERMINISM: a Stage-2 prompt carries AT MOST the one want_text whose
    // rule id equals the row's matched_rule_id, never the others'. The queue
    // LEFT JOINs exactly `sr.id = t.matched_rule_id`, so the full rule list
    // is never fed to the prompt.
    use crate::triage::stage2::{RowContext, build_user_message};

    let (store, acct) = store();

    // Three distinct Filtered rules, each with a unique, greppable want_text.
    let wants = [
        "WANT_ALPHA only closures",
        "WANT_BRAVO only invoices",
        "WANT_CHARLIE only shipments",
    ];
    let patterns = ["*@alpha.com", "*@bravo.com", "*@charlie.com"];
    let mut rule_ids = Vec::new();
    for (pat, want) in patterns.iter().zip(wants.iter()) {
        rule_ids.push(
            store
                .set_sender_rule(acct, pat, want, Disposition::Filtered)
                .unwrap(),
        );
    }

    // A queued row whose Stage-1 match landed on rule #2 (bravo). We stamp
    // matched_rule_id exactly as Stage-1 would (it selects a single rule id).
    let matched_id = rule_ids[1];
    let id = store.upsert_message(&triaged(acct, "g1", "t1").msg()).unwrap();
    store
        .set_triage(
            id, acct, 30, Tier::Noise, Sensitivity::Normal, None, "filtered",
            "matched filtered rule", None,
        )
        .unwrap();
    {
        let conn = store.lock().unwrap();
        conn.execute(
            "UPDATE triage SET matched_rule_id=?2 WHERE message_id=?1",
            params![id, matched_id],
        )
        .unwrap();
    }

    let rows = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(rows.len(), 1);
    // Only the matched rule's want_text surfaces from the store.
    assert_eq!(rows[0].rule_want_text.as_deref(), Some("WANT_BRAVO only invoices"));

    // And the BUILT prompt contains exactly that one rule's text — none of
    // the other two rules leak in.
    let ctx = RowContext::from_queued(&rows[0], 4000);
    let prompt = build_user_message(&ctx);
    assert!(prompt.contains("WANT_BRAVO only invoices"), "matched want must appear");
    assert!(!prompt.contains("WANT_ALPHA"), "non-matched rule must not leak");
    assert!(!prompt.contains("WANT_CHARLIE"), "non-matched rule must not leak");
    assert_eq!(
        prompt.matches("WANT_").count(),
        1,
        "exactly one rule's want_text in the prompt"
    );

    // NO-MATCH case: a row with matched_rule_id NULL carries zero rule text.
    let id2 = store.upsert_message(&triaged(acct, "g2", "t2").msg()).unwrap();
    store
        .set_triage(
            id2, acct, 40, Tier::Noise, Sensitivity::Normal, None, "ambiguous",
            "no rule matched", None,
        )
        .unwrap();
    let rows2 = store.stage2_queue(acct, 10).unwrap();
    let unmatched = rows2.iter().find(|r| r.message_id == id2).unwrap();
    assert!(unmatched.rule_want_text.is_none(), "no rule => no want_text");
    let prompt2 = build_user_message(&RowContext::from_queued(unmatched, 4000));
    assert!(!prompt2.contains("WANT_"), "unmatched row prompt has zero rule text");
    assert!(prompt2.contains("standing_instruction_for_this_sender: none"));
}

#[test]
fn stage2_budget_increment_and_exhaustion() {
    let (store, acct) = store();
    let day = "2026-07-09";

    assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 0);
    assert_eq!(store.stage2_increment_budget(acct, "t-abc", day).unwrap(), 1);
    assert_eq!(store.stage2_increment_budget(acct, "t-abc", day).unwrap(), 2);
    assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 2);

    // A different thread and a different day are independent counters.
    assert_eq!(store.stage2_budget_used(acct, "t-other", day).unwrap(), 0);
    assert_eq!(store.stage2_budget_used(acct, "t-abc", "2026-07-10").unwrap(), 0);

    // The global sentinel is a separate scope in the same table.
    assert_eq!(store.stage2_increment_budget(acct, "__global__", day).unwrap(), 1);
    assert_eq!(store.stage2_budget_used(acct, "__global__", day).unwrap(), 1);
    // The per-thread counter is unaffected by the global increment.
    assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 2);
}

#[test]
fn mailing_list_storm_capped_at_thread_daily_cap() {
    // A mailing-list storm — 30 messages in ONE thread — must cost AT MOST
    // `thread_daily_cap` API calls. Models the check-BEFORE-increment
    // discipline stage2_pass runs per row, with the global cap set high so
    // the per-thread cap binds.
    let (store, acct) = store();
    let day = "2026-07-09";
    let thread = "t-listserv";
    let thread_daily_cap: u32 = 3; // matches Stage2Config default

    let mut calls = 0u32;
    for _ in 0..30 {
        let used = store.stage2_budget_used(acct, thread, day).unwrap();
        if used >= thread_daily_cap {
            continue; // capped: row stays queued, no call
        }
        // "Make the call": increment BEFORE the attempt.
        store.stage2_increment_budget(acct, thread, day).unwrap();
        calls += 1;
    }

    assert_eq!(
        calls, thread_daily_cap,
        "30-message storm on one thread must cost at most thread_daily_cap calls"
    );
    assert_eq!(
        store.stage2_budget_used(acct, thread, day).unwrap(),
        thread_daily_cap,
        "counter must not exceed the cap"
    );
}

#[test]
fn one_sender_across_many_threads_capped_at_sender_daily_cap() {
    // A chatty sender fanning 10 messages across 10 DIFFERENT threads must
    // cost AT MOST `sender_daily_cap` calls. Models the per-sender
    // check-BEFORE-increment the pass runs (keyed by sender:<addr>), with the
    // per-thread and global caps set high so the per-sender cap binds.
    let (store, acct) = store();
    let day = "2026-07-09";
    let sender_key = "sender:chatty@example.com";
    let sender_daily_cap: u32 = 5; // matches Stage2Config default

    let mut calls = 0u32;
    for i in 0..10 {
        // Each message is in its OWN thread — the per-thread cap never binds.
        let _thread = format!("t-{i}");
        let used = store.stage2_budget_used(acct, sender_key, day).unwrap();
        if used >= sender_daily_cap {
            continue; // sender capped: row stays queued, no call
        }
        store.stage2_increment_budget(acct, sender_key, day).unwrap();
        calls += 1;
    }

    assert_eq!(
        calls, sender_daily_cap,
        "10 messages from one sender across 10 threads cost at most sender_daily_cap"
    );
    assert_eq!(
        store.stage2_budget_used(acct, sender_key, day).unwrap(),
        sender_daily_cap
    );
}

#[test]
fn stage2_usage_ledger_bumps_and_reads() {
    // Bumping the ledger accumulates calls + tokens per day, and reading
    // returns the running totals (zeroed for an untouched day).
    let (store, acct) = store();
    let day = "2026-07-09";

    // Untouched day reads as zeros.
    let z = store.stage2_usage_today(acct, day).unwrap();
    assert_eq!(z, Stage2Usage::default());

    store.stage2_bump_usage(acct, day, 1200, 60).unwrap();
    store.stage2_bump_usage(acct, day, 800, 40).unwrap();
    let u = store.stage2_usage_today(acct, day).unwrap();
    assert_eq!(u.calls, 2);
    assert_eq!(u.input_tokens, 2000);
    assert_eq!(u.output_tokens, 100);

    // A different day is an independent row.
    assert_eq!(
        store.stage2_usage_today(acct, "2026-07-10").unwrap(),
        Stage2Usage::default()
    );
}

#[test]
fn list_usage_returns_recent_days_newest_first() {
    let (store, acct) = store();

    // Empty ledger => no rows.
    assert!(store.list_usage(acct, 30).unwrap().is_empty());

    store.stage2_bump_usage(acct, "2026-07-07", 100, 10).unwrap();
    store.stage2_bump_usage(acct, "2026-07-08", 200, 20).unwrap();
    store.stage2_bump_usage(acct, "2026-07-09", 300, 30).unwrap();
    store.stage2_bump_usage(acct, "2026-07-09", 100, 10).unwrap();

    // Newest-first, sparse (only days with a row).
    let rows = store.list_usage(acct, 30).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].day, "2026-07-09");
    assert_eq!(rows[0].calls, 2);
    assert_eq!(rows[0].input_tokens, 400);
    assert_eq!(rows[0].output_tokens, 40);
    assert_eq!(rows[2].day, "2026-07-07");

    // `days` caps the row count (still newest-first).
    let capped = store.list_usage(acct, 2).unwrap();
    assert_eq!(capped.len(), 2);
    assert_eq!(capped[0].day, "2026-07-09");
    assert_eq!(capped[1].day, "2026-07-08");
}

#[test]
fn app_settings_get_set_roundtrip_and_scoping() {
    let store = SqliteStore::open_in_memory().unwrap();
    let a = store.ensure_account("a@example.com").unwrap();
    let b = store.ensure_account("b@example.com").unwrap();

    // Unset key reads None.
    assert!(store.get_app_setting(a, "k").unwrap().is_none());

    // Set, read back, and overwrite (upsert).
    store.set_app_setting(a, "k", "v1").unwrap();
    assert_eq!(store.get_app_setting(a, "k").unwrap().as_deref(), Some("v1"));
    store.set_app_setting(a, "k", "v2").unwrap();
    assert_eq!(store.get_app_setting(a, "k").unwrap().as_deref(), Some("v2"));

    // Per-account scoped: b's key is independent.
    assert!(store.get_app_setting(b, "k").unwrap().is_none());
}

#[test]
fn stage2_cap_overrides_reads_and_precedence() {
    use crate::config::{
        APP_SETTING_GLOBAL_DAILY_CAP, APP_SETTING_SENDER_DAILY_CAP,
        APP_SETTING_THREAD_DAILY_CAP,
    };
    let (store, acct) = store();

    // No rows => all None (caller falls back to config/env then default).
    assert_eq!(store.stage2_cap_overrides(acct).unwrap(), Default::default());

    // A set thread cap surfaces; the others stay None (so the effective cap
    // is the override where present, config/default elsewhere — precedence).
    store.set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "5").unwrap();
    let o = store.stage2_cap_overrides(acct).unwrap();
    assert_eq!(o.thread_daily_cap, Some(5));
    assert_eq!(o.sender_daily_cap, None);
    assert_eq!(o.global_daily_cap, None);

    // Set the remaining two.
    store.set_app_setting(acct, APP_SETTING_SENDER_DAILY_CAP, "9").unwrap();
    store.set_app_setting(acct, APP_SETTING_GLOBAL_DAILY_CAP, "300").unwrap();
    let o = store.stage2_cap_overrides(acct).unwrap();
    assert_eq!(o.thread_daily_cap, Some(5));
    assert_eq!(o.sender_daily_cap, Some(9));
    assert_eq!(o.global_daily_cap, Some(300));

    // A malformed OR out-of-range stored value is ignored (treated as absent),
    // so a corrupt row can never remove the cap entirely.
    store.set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "not-a-number").unwrap();
    assert_eq!(store.stage2_cap_overrides(acct).unwrap().thread_daily_cap, None);
    store.set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "0").unwrap();
    assert_eq!(store.stage2_cap_overrides(acct).unwrap().thread_daily_cap, None);
    store.set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "100001").unwrap();
    assert_eq!(store.stage2_cap_overrides(acct).unwrap().thread_daily_cap, None);
}

#[test]
fn override_cap_binds_below_config_default() {
    // Precedence is override > config/env > default: a runtime override of 1
    // caps a thread the config default (3) would have allowed 3 calls on.
    use crate::config::APP_SETTING_THREAD_DAILY_CAP;
    let (store, acct) = store();
    let day = "2026-07-09";
    let thread = "t-override";
    let config_default_cap: u32 = 3; // Stage2Config default

    // Client lowers the per-thread cap to 1 at runtime.
    store.set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "1").unwrap();

    // Effective cap = override (1), NOT the config default (3) — precedence.
    let overrides = store.stage2_cap_overrides(acct).unwrap();
    let effective = overrides.thread_daily_cap.unwrap_or(config_default_cap);
    assert_eq!(effective, 1);

    // Same check-before-increment loop the pass runs, using the effective cap.
    let mut calls = 0u32;
    for _ in 0..10 {
        let used = store.stage2_budget_used(acct, thread, day).unwrap();
        if used >= effective {
            continue;
        }
        store.stage2_increment_budget(acct, thread, day).unwrap();
        calls += 1;
    }
    assert_eq!(calls, 1, "override cap of 1 must bind below the config default of 3");
}

#[test]
fn stage2_usage_since_sums_window_inclusively() {
    let (store, acct) = store();

    // Empty ledger => zeros.
    assert_eq!(
        store.stage2_usage_since(acct, "2026-07-01").unwrap(),
        Stage2Usage::default()
    );

    store.stage2_bump_usage(acct, "2026-07-05", 100, 10).unwrap();
    store.stage2_bump_usage(acct, "2026-07-08", 200, 20).unwrap();
    store.stage2_bump_usage(acct, "2026-07-08", 300, 30).unwrap();

    // since_day <= earliest => everything summed (2 days, 3 calls).
    let all = store.stage2_usage_since(acct, "2026-07-05").unwrap();
    assert_eq!(all.calls, 3);
    assert_eq!(all.input_tokens, 600);
    assert_eq!(all.output_tokens, 60);

    // Window boundary is inclusive on since_day and excludes older rows.
    let recent = store.stage2_usage_since(acct, "2026-07-08").unwrap();
    assert_eq!(recent.calls, 2);
    assert_eq!(recent.input_tokens, 500);
    assert_eq!(recent.output_tokens, 50);
}

#[test]
fn count_inbound_since_counts_only_received_in_window() {
    let (store, acct) = store();
    let now = Utc::now();

    let inbound = |gmail: &str, sent: bool, received: DateTime<Utc>| {
        let m = NewMessage {
            account_id: acct,
            gmail_msg_id: gmail.to_string(),
            thread_id: gmail.to_string(),
            from_addr: "x@y.com".to_string(),
            from_name: None,
            subject: "s".to_string(),
            received_at: received,
            snippet: String::new(),
            body: String::new(),
            body_html: None,
            is_sent: sent,
            list_unsubscribe: None,
            list_unsub_one_click: false,
        };
        store.upsert_message(&m).unwrap();
    };

    // Two recent inbound, one old inbound, one recent SENT (excluded).
    inbound("m1", false, now - chrono::Duration::days(1));
    inbound("m2", false, now - chrono::Duration::days(10));
    inbound("m3", false, now - chrono::Duration::days(30));
    inbound("m4", true, now - chrono::Duration::days(1));

    let since = now - chrono::Duration::days(14);
    assert_eq!(store.count_inbound_since(acct, since).unwrap(), 2);
}

#[test]
fn stale_skip_marks_processed_without_budget() {
    // A row older than the cutoff is stale-skipped: marked processed with
    // model_used='stale-skip' (keeping Stage-1 values), leaving the queue,
    // and spending no budget.
    let (store, acct) = store();
    let max_age_days: i64 = 7;
    let now = Utc::now();
    let cutoff = now - chrono::Duration::days(max_age_days);

    // A stale row (received 30d ago) and a fresh row (now).
    let mut stale = triaged(acct, "g-stale", "t-stale").msg();
    stale.received_at = now - chrono::Duration::days(30);
    let stale_id = store.upsert_message(&stale).unwrap();
    store
        .set_triage(stale_id, acct, 40, Tier::Noise, Sensitivity::Normal, None, "amb", "", None)
        .unwrap();
    let mut fresh = triaged(acct, "g-fresh", "t-fresh").msg();
    fresh.received_at = now;
    let fresh_id = store.upsert_message(&fresh).unwrap();
    store
        .set_triage(fresh_id, acct, 40, Tier::Noise, Sensitivity::Normal, None, "amb", "", None)
        .unwrap();

    // Apply the pass-loop decision: stale-skip old rows, keep fresh queued.
    let day = "2026-07-09";
    for row in store.stage2_queue(acct, 10).unwrap() {
        if row.received_at < cutoff {
            store
                .stage2_mark_processed(acct, row.message_id, "stale-skip")
                .unwrap();
        }
    }

    // Only the fresh row remains queued.
    let remaining = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].message_id, fresh_id);

    // No budget was spent on the stale skip.
    assert_eq!(store.stage2_budget_used(acct, "t-stale", day).unwrap(), 0);
    assert_eq!(
        store.stage2_budget_used(acct, "__global__", day).unwrap(),
        0
    );

    // The stale row's triage is stamped 'stale-skip' with Stage-1 values kept.
    let conn = store.lock().unwrap();
    let (imp, model): (i64, Option<String>) = conn
        .query_row(
            "SELECT importance, model_used FROM triage WHERE message_id=?1",
            params![stale_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(imp, 40, "stale-skip keeps Stage-1 importance");
    assert_eq!(model.as_deref(), Some("stale-skip"));
}

#[test]
fn stage2_queue_carries_received_at() {
    // The queue surfaces received_at so the pass can skip stale rows.
    let (store, acct) = store();
    let mut m = triaged(acct, "g1", "t1").msg();
    let when = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    m.received_at = when;
    let id = store.upsert_message(&m).unwrap();
    store
        .set_triage(
            id, acct, 40, Tier::Noise, Sensitivity::Normal, None, "amb", "", None,
        )
        .unwrap();
    let rows = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].received_at, when);
}

#[test]
fn stage2_apply_updates_row_stamps_model_and_writes_deadline() {
    use crate::triage::DeadlineHit;
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g1", "t1", Sensitivity::Normal);

    let due = DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let applied = Stage2Applied {
        message_id: id,
        account_id: acct,
        importance: 88,
        tier: Tier::Deadline,
        one_line: "invoice due sep 1".into(),
        reason: "stage-2 (m): real bill".into(),
        field_reasons: crate::types::FieldReasons {
            importance: Some("stage-2: real bill".into()),
            deadline: Some("stage-2: invoice due sep 1".into()),
            tier: Some("stage-2: future deadline -> deadline".into()),
        },
        model_used: "claude-haiku-4-5".into(),
        deadline: Some(DeadlineHit {
            kind: "invoice".into(),
            amount: None,
            currency: None,
            due_at: due,
            past_due: false,
            source: "stage2".into(),
        }),
        category: Some("invoice".into()),
    };
    assert!(store.stage2_apply(&applied).unwrap(), "the guard matched the normal row");

    // Row left the queue (model_used stamped).
    assert!(store.stage2_queue(acct, 10).unwrap().is_empty());
    // A deadlines row was written.
    let ds = store.deadlines(acct, Some(365)).unwrap();
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].kind, "invoice");
    // The ranked update reflects the new tier/importance.
    let ups = store
        .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), None)
        .unwrap();
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].tier, Tier::Deadline);
    assert_eq!(ups[0].importance, 88);
}

#[test]
fn stage2_apply_never_touches_sealed_row() {
    use crate::triage::DeadlineHit;
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-sealed", "t1", Sensitivity::Sealed);
    let applied = Stage2Applied {
        message_id: id,
        account_id: acct,
        importance: 99,
        tier: Tier::Signal,
        one_line: "leak".into(),
        reason: "should not apply".into(),
        field_reasons: crate::types::FieldReasons::default(),
        model_used: "m".into(),
        deadline: Some(DeadlineHit {
            kind: "invoice".into(),
            amount: None,
            currency: None,
            due_at: Utc::now() + chrono::Duration::days(3),
            past_due: false,
            source: "stage2".into(),
        }),
        category: Some("general".into()),
    };
    // TOCTOU report: the guard matched nothing and the caller must know —
    // a bare Ok would let a message sealed mid-pass emit an event anyway.
    assert!(!store.stage2_apply(&applied).unwrap(), "sealed row: apply reports false");
    // The sealed row's triage must be unchanged (guarded by sensitivity),
    // and the verdict's deadline must NOT have been written either.
    assert!(store.deadlines(acct, Some(365)).unwrap().is_empty(), "no deadline row");
    let conn = store.lock().unwrap();
    let (imp, model): (i64, Option<String>) = conn
        .query_row(
            "SELECT importance, model_used FROM triage WHERE message_id=?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(imp, 40, "sealed row importance unchanged");
    assert!(model.is_none(), "sealed row model_used untouched");
}

#[test]
fn stage1_apply_reports_false_when_the_row_was_sealed_mid_pass() {
    use crate::triage::DeadlineHit;
    // TOCTOU: a human seals a row between the Stage-1 queue SELECT and its
    // apply. The guarded UPDATE matches nothing, and the apply must say so or
    // the engine emits a notification event for a now-sealed message.
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-race", "t1", Sensitivity::Normal);
    let applied = Stage1Applied {
        message_id: id,
        account_id: acct,
        importance: 90,
        tier: Tier::PastDue,
        one_line: "loud verdict".into(),
        reason: "stage-1".into(),
        field_reasons: crate::types::FieldReasons::default(),
        stage1_model_used: "claude-haiku-4-5".into(),
        needs_stage2: false,
        deadline: Some(DeadlineHit {
            kind: "bill".into(),
            amount: Some(10.0),
            currency: Some("USD".into()),
            due_at: Utc::now() - chrono::Duration::days(1),
            past_due: true,
            source: "stage1".into(),
        }),
        category: None,
    };

    // Sealed between queue and apply.
    store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, Utc::now())
        .unwrap()
        .unwrap();

    assert!(!store.stage1_apply(&applied).unwrap(), "sealed mid-pass: apply reports false");
    assert!(store.deadlines(acct, Some(365)).unwrap().is_empty(), "no deadline row");

    // Control: the same apply on a live row reports true.
    let live = seed_triage_row(&store, acct, "g-live", "t2", Sensitivity::Normal);
    let mut ok = applied.clone();
    ok.message_id = live;
    assert!(store.stage1_apply(&ok).unwrap(), "normal row: apply reports true");
}
