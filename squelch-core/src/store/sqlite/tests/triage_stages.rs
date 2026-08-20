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
    let stmt = triaged_row(acct, "g-stmt", "t1", None, false, Sensitivity::Normal).ingest(&store);
    apply_category(&store, acct, stmt, "banking_statement", false);
    let alert = triaged_row(acct, "g-alert", "t2", None, false, Sensitivity::Normal).ingest(&store);
    apply_category(&store, acct, alert, "transaction_alert", false);

    // invoice + general -> NOT queued (no extractor).
    let inv = triaged_row(acct, "g-inv", "t3", None, false, Sensitivity::Normal)
        .category("invoice")
        .ingest(&store);
    let genr = triaged_row(acct, "g-gen", "t4", None, false, Sensitivity::Normal).ingest(&store);
    apply_category(&store, acct, genr, "general", false);

    // A sealed row: category stays NULL (stage1_apply is guarded), excluded.
    let sealed = triaged_row(acct, "g-seal", "t5", None, false, Sensitivity::Sealed).ingest(&store);
    apply_category(&store, acct, sealed, "banking_statement", false); // no-op on sealed

    // A banking-categorized message that ALSO produced a receipt: excluded
    // (a receipt and a banking row must never double-create).
    let dual = triaged_row(acct, "g-dual", "t6", None, false, Sensitivity::Normal).ingest(&store);
    apply_category(&store, acct, dual, "banking_statement", false);
    store
        .upsert_receipt(
            acct,
            dual,
            "orders@shop.com",
            None,
            &crate::triage::ReceiptInfo {
                amount: Some(5.0),
                currency: Some("USD".into()),
            },
            Utc::now(),
        )
        .unwrap();

    let cats = ["banking_statement", "transaction_alert"];
    let q = store.extract_queue(acct, &cats, 20).unwrap();
    let ids: Vec<i64> = q.iter().map(|r| r.message_id).collect();
    assert_eq!(
        q.len(),
        2,
        "only the two banking rows without a receipt: {ids:?}"
    );
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
        .extract_bump_usage(
            acct,
            "2026-07-23",
            "extract_banking",
            UsageTokens {
                input: 500,
                output: 20,
                cache_creation: 700,
                cache_read: 3000,
            },
        )
        .unwrap();
    store
        .extract_bump_usage(
            acct,
            "2026-07-23",
            "extract_banking",
            UsageTokens {
                input: 300,
                output: 10,
                cache_creation: 300,
                cache_read: 1000,
            },
        )
        .unwrap();
    let conn = store.lock().unwrap();
    let (calls, in_tok, out_tok, cache_w, cache_r): (i64, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT calls, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens
             FROM stage2_usage
             WHERE account_id=?1 AND day=?2 AND category='extract_banking'",
            params![acct, "2026-07-23"],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(in_tok, 800);
    assert_eq!(out_tok, 30);
    assert_eq!(cache_w, 1000);
    assert_eq!(cache_r, 4000);
}

#[test]
fn stage1_queue_takes_every_normal_row_including_rule_decided_ones() {
    let (store, acct) = store();

    // Normal, non-rule row -> enters the Stage-1 LLM queue.
    let normal = triaged_row(acct, "g-n", "t-n", None, false, Sensitivity::Normal).ingest(&store);
    // A Squelch/Surface rule row ALSO enters it. The rule settles what the user
    // sees; it does not settle the category, the deadline, or the revisit
    // schedule, and those are worth having on a row nobody looks at.
    let ruled = triaged_row(acct, "g-r", "t-r", Some(7), true, Sensitivity::Normal).ingest(&store);
    // Sealed -> never queued for any LLM, rule or no rule.
    triaged_row(acct, "g-s", "t-s", None, false, Sensitivity::Sealed).ingest(&store);

    let q = store.stage1_queue(acct, 10).unwrap();
    let ids: Vec<i64> = q.iter().map(|r| r.message_id).collect();
    assert_eq!(q.len(), 2, "both normal rows need Stage-1: {ids:?}");
    assert!(ids.contains(&normal));
    assert!(
        ids.contains(&ruled),
        "a rule row is not a reason to skip a model"
    );
    assert!(q.iter().all(|r| r.sensitivity == Sensitivity::Normal));
}

// ---- the SHIPMENTS extractor's own queue --------------------------------

#[test]
fn ship_extract_queue_keeps_receipt_bearing_rows_and_drops_sealed_and_sent() {
    let (store, acct) = store();

    // A plain order confirmation with the loose signal -> pending.
    let plain = triaged_row(acct, "g-ship", "t1", None, false, Sensitivity::Normal)
        .ship_extract(true)
        .ingest(&store);

    // THE WHOLE REASON THIS QUEUE EXISTS: an order confirmation that also
    // produced a RECEIPT. `extract_queue` drops those, and most order
    // confirmations are exactly that shape, so it could never serve shipments.
    let with_receipt = triaged_row(acct, "g-both", "t2", None, false, Sensitivity::Normal)
        .ship_extract(true)
        .ingest(&store);
    store
        .upsert_receipt(
            acct,
            with_receipt,
            "orders@shop.com",
            None,
            &crate::triage::ReceiptInfo {
                amount: Some(42.0),
                currency: Some("USD".into()),
            },
            Utc::now(),
        )
        .unwrap();

    // No shipping signal at ingest -> NULL, never queued.
    let quiet = triaged_row(acct, "g-quiet", "t3", None, false, Sensitivity::Normal).ingest(&store);
    // Sealed and sent carry the signal flag but must never queue.
    let sealed = triaged_row(acct, "g-seal", "t4", None, false, Sensitivity::Sealed)
        .ship_extract(true)
        .ingest(&store);
    let sent = triaged_row(acct, "g-sent", "t5", None, false, Sensitivity::Normal)
        .is_sent(true)
        .ship_extract(true)
        .ingest(&store);

    let q = store.ship_extract_queue(acct, 20).unwrap();
    let ids: Vec<i64> = q.iter().map(|r| r.message_id).collect();
    assert_eq!(q.len(), 2, "the two signal-bearing normal rows: {ids:?}");
    assert!(ids.contains(&plain));
    assert!(
        ids.contains(&with_receipt),
        "a receipt-bearing order confirmation MUST still queue"
    );
    assert!(!ids.contains(&quiet), "no shipping signal, no queue");
    assert!(!ids.contains(&sealed), "sealed mail never reaches an LLM");
    assert!(!ids.contains(&sent), "the user's own outbox is not tracked");
    // Queued before Stage-1 ever ran, so the category is still NULL -> "".
    assert_eq!(q[0].category, "");
    assert_eq!(q[0].sensitivity, Sensitivity::Normal);
}

#[test]
fn ship_extract_mark_removes_the_row_from_the_queue() {
    let (store, acct) = store();
    let id = triaged_row(acct, "g-ship", "t1", None, false, Sensitivity::Normal)
        .ship_extract(true)
        .ingest(&store);
    assert_eq!(store.ship_extract_queue(acct, 10).unwrap().len(), 1);

    store
        .ship_extract_mark(acct, id, "claude-haiku-4-5")
        .unwrap();
    assert!(
        store.ship_extract_queue(acct, 10).unwrap().is_empty(),
        "a stamped marker takes the row out of the queue"
    );

    // A sealed row is guarded: the marker never lands on it.
    let sealed = triaged_row(acct, "g-seal", "t2", None, false, Sensitivity::Sealed).ingest(&store);
    store.ship_extract_mark(acct, sealed, "stale-skip").unwrap();
    let conn = store.lock().unwrap();
    let marker: Option<String> = conn
        .query_row(
            "SELECT ship_extract_model FROM triage WHERE message_id=?1",
            params![sealed],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(marker, None, "sensitivity guard holds on the mark");
}

#[test]
fn retriage_reset_repends_shipping_rows_and_scrubs_marketing_and_staged_orders() {
    let (store, acct) = store();

    let shipping = triaged_row(acct, "g-ship", "t1", None, false, Sensitivity::Normal)
        .ship_extract(true)
        .ingest(&store);
    let quiet = triaged_row(acct, "g-quiet", "t2", None, false, Sensitivity::Normal).ingest(&store);

    // The shipments extractor already ruled on the shipping row, and left a
    // staged order behind; the quiet row picked up a marketing extraction.
    store.ship_extract_mark(acct, shipping, "claude-x").unwrap();
    {
        let conn = store.lock().unwrap();
        conn.execute(
            "INSERT INTO shipment_orders(account_id, order_ref, item_name, thread_id,
                                         last_message_id, first_seen, last_update)
             VALUES(?1, 'ORD-1', 'Anker charger', 't1', ?2, ?3, ?3)",
            params![acct, shipping, Utc::now().to_rfc3339()],
        )
        .unwrap();
    }
    apply_category(&store, acct, quiet, "marketing", false);
    store
        .marketing_apply(&crate::store::MarketingApplied {
            message_id: quiet,
            account_id: acct,
            brand: Some("Shop".into()),
            offer: Some("30% off".into()),
            discount: None,
            code: None,
            expires_at: None,
            received_at: Utc::now(),
            extractor_model_used: "m".into(),
        })
        .unwrap();
    assert_eq!(store.marketing_offers(acct, 30, 10).unwrap().len(), 1);

    store.retriage_reset(acct, None, 7).unwrap();

    // The shipping row is PENDING again; the never-signalled row stays NULL —
    // blanking it would be indistinguishable from "had a signal, un-ruled".
    let markers = |mid: i64| -> Option<String> {
        let conn = store.lock().unwrap();
        conn.query_row(
            "SELECT ship_extract_model FROM triage WHERE message_id=?1",
            params![mid],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(markers(shipping).as_deref(), Some("pending"));
    assert_eq!(markers(quiet), None, "a NULL trigger stays NULL");
    assert_eq!(store.ship_extract_queue(acct, 10).unwrap().len(), 1);

    // Staged orders are re-derivable, so they go...
    {
        let conn = store.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM shipment_orders WHERE account_id=?1",
                params![acct],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "staged orders are dropped with the other specialists");
    }
    // ...and so does marketing, which the reset used to leave stranded.
    assert_eq!(
        store.marketing_offers(acct, 30, 10).unwrap().len(),
        0,
        "marketing rows must not survive a re-triage that drops their category"
    );
}

#[test]
fn retriage_reset_keeps_the_shipment_rows_it_cannot_recover() {
    // `shipments` is identity-keyed by tracking number and carries carrier-poll
    // state no re-run can rebuild, so unlike every other specialist table it
    // must SURVIVE a re-triage.
    use crate::triage::{ShipmentInfo, ShipmentStatus};
    let (store, acct) = store();
    let id = triaged_row(acct, "g-ship", "t1", None, false, Sensitivity::Normal)
        .ship_extract(true)
        .ingest(&store);
    store
        .upsert_shipment(
            acct,
            id,
            &ShipmentInfo {
                carrier: "ups".into(),
                tracking_number: "1Z999AA10123456784".into(),
                item_name: "Anker charger".into(),
                status: ShipmentStatus::Shipped,
                tracking_url: None,
            },
            Utc::now(),
        )
        .unwrap();

    store.retriage_reset(acct, None, 7).unwrap();
    assert_eq!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .len(),
        1,
        "a tracked package must survive a re-triage"
    );
}

/// DEFECT: re-triage cleared the staged orders keyed by `last_message_id` but
/// left DONATED item names standing. Extraction attaches a name to a shipment
/// row ANOTHER message feeds and records that in `item_name_msg`; sealing scrubs
/// those by provenance, and re-triage did not. So a re-extraction that returned
/// no item name (or said the mail was not a shipment at all) kept showing the old
/// name forever. Both tables, both scrubbed, and the ROW still survives.
#[test]
fn retriage_reset_clears_a_donated_item_name_in_both_shipment_tables() {
    use crate::triage::{ShipmentInfo, ShipmentStatus};
    let (store, acct) = store();

    // The DONOR is an ordinary LLM-classified row, so it is in the reset scope.
    let donor = triaged_row(acct, "g-donor", "t1", None, false, Sensitivity::Normal)
        .ship_extract(true)
        .ingest(&store);
    // The FEEDER carries a FILTERED rule, which is the marker that still sits
    // outside the reset scope ('rule'), so it never resets — which is the whole
    // point: the rows below survive the reset and must still lose the donated
    // text.
    let feeder =
        triaged_row(acct, "g-feeder", "t2", Some(7), false, Sensitivity::Normal).ingest(&store);

    let sid = store
        .upsert_shipment(
            acct,
            feeder,
            &ShipmentInfo {
                carrier: "ups".into(),
                tracking_number: "1Z999AA10123456784".into(),
                item_name: String::new(),
                status: ShipmentStatus::Shipped,
                tracking_url: None,
            },
            Utc::now(),
        )
        .unwrap();
    {
        let conn = store.lock().unwrap();
        // The donation: the donor's extraction named a package another mail feeds.
        conn.execute(
            "UPDATE shipments SET item_name='Anker charger', item_name_msg=?2,
                 item_name_source='llm'
             WHERE id=?1",
            params![sid, donor],
        )
        .unwrap();
        // The same hole one table over: a staged order the donor named but a
        // later mail feeds, so the delete-by-`last_message_id` cannot reach it.
        conn.execute(
            "INSERT INTO shipment_orders(account_id, order_ref, item_name, item_name_msg,
                                         thread_id, last_message_id, first_seen, last_update)
             VALUES(?1, 'ORD-9', 'Anker charger', ?2, 't2', ?3, ?4, ?4)",
            params![acct, donor, feeder, Utc::now().to_rfc3339()],
        )
        .unwrap();
    }

    store.retriage_reset(acct, None, 7).unwrap();

    let listed = store
        .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(listed.len(), 1, "the package itself must survive");
    assert_eq!(listed[0].item_name, "", "the donated name is gone");

    let conn = store.lock().unwrap();
    let (ship_prov, ship_source): (Option<i64>, String) = conn
        .query_row(
            "SELECT item_name_msg, item_name_source FROM shipments WHERE id=?1",
            params![sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(ship_prov, None, "and so is its provenance");
    assert_eq!(
        ship_source, "regex",
        "BOTH halves of the provenance reset: an 'llm' marker with no name left \
         would lock the row out of taking a regex name on re-extraction"
    );
    let (order_name, order_prov): (String, Option<i64>) = conn
        .query_row(
            "SELECT item_name, item_name_msg FROM shipment_orders WHERE account_id=?1",
            params![acct],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        order_name, "",
        "the staged order loses the donated name too"
    );
    assert_eq!(order_prov, None);
}

/// DEFECT: a re-triage of anything older than the pass's age cutoff requeued the
/// row and then watched the very next tick stamp it processed with no model call
/// — "stale-skipped 1" in the log, an unchanged verdict on screen, and no way to
/// ask again that would work any better. The cutoff exists so a fresh install
/// does not spend its cap on a backlog nobody asked about; a re-triage IS asking.
#[test]
fn a_re_triaged_row_carries_the_stamp_that_overrides_the_stale_skip() {
    let (store, acct) = store();
    let now = Utc::now();

    // Old mail — past every pass's cutoff — that the LLM already ruled on, and
    // that also carries a shipping signal, so it sits in two queues at once.
    let old = triaged_row(acct, "g-old", "t-old", None, false, Sensitivity::Normal)
        .received_at(now - chrono::Duration::days(120))
        .ship_extract(true)
        .ingest(&store);
    {
        let conn = store.lock().unwrap();
        conn.execute(
            "UPDATE triage SET stage1_model_used='claude-x', model_used='claude-x',
                    extractor_model_used='claude-x', ship_extract_model='claude-x',
                    category='banking_statement'
             WHERE message_id=?1",
            rusqlite::params![old],
        )
        .unwrap();
    }

    assert_eq!(store.retriage_reset(acct, Some(old), 7).unwrap(), 1);

    // Every queue the row re-enters carries the request, so every pass's stale
    // skip yields to it.
    let s1 = store.stage1_queue(acct, 10).unwrap();
    assert_eq!(s1.len(), 1);
    assert!(
        crate::triage::retriage_forced(s1[0].retriage_at, Utc::now()),
        "stage-1 must see the hand request on the row it just requeued"
    );
    let ship = store.ship_extract_queue(acct, 10).unwrap();
    assert_eq!(ship.len(), 1);
    assert!(crate::triage::retriage_forced(
        ship[0].retriage_at,
        Utc::now()
    ));
    let ext = store
        .extract_queue(acct, &["banking_statement"], 10)
        .unwrap();
    assert_eq!(ext.len(), 1);
    assert!(crate::triage::retriage_forced(
        ext[0].retriage_at,
        Utc::now()
    ));

    // AND STAGE-2, which the row only reaches after Stage-1 re-runs and escalates
    // it. The stamp has to survive that hop: a re-triage that redoes Stage-1 and
    // then skips the escalation it asked for is half a re-triage.
    {
        let conn = store.lock().unwrap();
        conn.execute(
            "UPDATE triage SET stage1_model_used='claude-x', needs_stage2=1
             WHERE message_id=?1",
            rusqlite::params![old],
        )
        .unwrap();
    }
    let s2 = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(s2.len(), 1);
    assert!(
        crate::triage::retriage_forced(s2[0].retriage_at, Utc::now()),
        "the stamp must outlive the Stage-1 apply that escalates the row"
    );
}

/// A row nobody asked about keeps a NULL stamp, so the age cutoff still decides
/// for it. Without this the fix would read as "re-triage forces everything".
#[test]
fn an_untouched_row_carries_no_re_triage_stamp() {
    let (store, acct) = store();
    triaged_row(acct, "g-plain", "t-plain", None, false, Sensitivity::Normal).ingest(&store);
    let q = store.stage1_queue(acct, 10).unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].retriage_at, None);
    assert!(!crate::triage::retriage_forced(
        q[0].retriage_at,
        Utc::now()
    ));
}

/// `batch_per_cycle` is a real ceiling, so a hand-requested row that sorted
/// purely by age would wait behind the backlog for ticks — which reads exactly
/// like the skip it is not.
#[test]
fn a_hand_re_triaged_row_jumps_the_backlog() {
    let (store, acct) = store();
    let now = Utc::now();

    let old = triaged_row(acct, "g-old", "t-old", None, false, Sensitivity::Normal)
        .received_at(now - chrono::Duration::days(120))
        .ingest(&store);
    {
        let conn = store.lock().unwrap();
        conn.execute(
            "UPDATE triage SET stage1_model_used='claude-x' WHERE message_id=?1",
            rusqlite::params![old],
        )
        .unwrap();
    }
    // Newer, never-classified mail already waiting in the queue.
    for i in 0..3 {
        triaged_row(
            acct,
            &format!("g-new{i}"),
            &format!("t-new{i}"),
            None,
            false,
            Sensitivity::Normal,
        )
        .received_at(now - chrono::Duration::minutes(i))
        .ingest(&store);
    }

    store.retriage_reset(acct, Some(old), 7).unwrap();

    let batch = store.stage1_queue(acct, 1).unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(
        batch[0].message_id, old,
        "the row a human asked for goes first, not last"
    );
}

#[test]
fn retriage_reset_requeues_llm_rows_but_never_filtered_or_sealed() {
    let (store, acct) = store();

    let normal = triaged_row(acct, "g-n", "t-n", None, false, Sensitivity::Normal).ingest(&store);
    // A FILTERED rule row keeps the 'rule' marker (its verdict is pending a
    // Stage-2 want_text read) and stays outside the reset scope. A
    // Squelch/Surface row no longer does: it is an ordinary model-classified row
    // whose rule simply reapplies on the way back through.
    triaged_row(acct, "g-f", "t-f", Some(7), false, Sensitivity::Normal).ingest(&store);
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
    assert_eq!(n, 1, "filtered + sealed rows must never reset");
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

/// A Squelch/Surface rule still classifies, but it does NOT escalate: the row
/// enters Stage-1 and stops there, because a second opinion cannot change a
/// visibility the account owner has already settled.
#[test]
fn explicit_rule_row_classifies_once_and_never_escalates() {
    let (store, acct) = store();
    let id = triaged_row(acct, "g-r", "t-r", Some(9), true, Sensitivity::Normal).ingest(&store);
    assert_eq!(
        store.stage1_queue(acct, 10).unwrap().len(),
        1,
        "the rule row gets its model verdict"
    );
    assert!(
        store.stage2_queue(acct, 10).unwrap().is_empty(),
        "a rule row is seeded un-escalated"
    );
    let needs: i64 = store
        .lock()
        .unwrap()
        .query_row(
            "SELECT needs_stage2 FROM triage WHERE account_id=?1 AND message_id=?2",
            params![acct, id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(needs, 0);
}

#[test]
fn filtered_rule_row_goes_straight_to_stage2() {
    let (store, acct) = store();
    // A Filtered rule (matched_rule set, NOT confident) skips Stage-1 and
    // escalates directly to Stage-2 for want_text evaluation.
    let id = triaged_row(acct, "g-f", "t-f", Some(3), false, Sensitivity::Normal).ingest(&store);
    assert!(
        store.stage1_queue(acct, 10).unwrap().is_empty(),
        "no Stage-1 spend"
    );
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
        stage1_model_used: "claude-opus-5".into(),
        needs_stage2,
        escalation_reason: needs_stage2.then_some("boundary"),
        deadline: None,
        category: Some("general".into()),
    };
    store.stage1_apply(&applied(a, false)).unwrap(); // router found nothing -> final
    store.stage1_apply(&applied(b, true)).unwrap(); // router escalated

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
    let amb = triaged_row(acct, "g-amb", "t-amb", None, false, Sensitivity::Normal).ingest(&store);
    // Confident seed (confident=true => needs_stage2 seed = 0).
    let sure =
        triaged_row(acct, "g-sure", "t-sure", None, true, Sensitivity::Normal).ingest(&store);

    // Heuristic-only fallback stamps the marker but PRESERVES the seed.
    store
        .stage1_mark_processed(acct, amb, HEURISTIC_ONLY)
        .unwrap();
    store
        .stage1_mark_processed(acct, sure, HEURISTIC_ONLY)
        .unwrap();

    assert!(store.stage1_queue(acct, 10).unwrap().is_empty());
    let s2 = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(s2.len(), 1, "only the ambiguous seed escalates");
    assert_eq!(s2[0].message_id, amb);
}

#[test]
fn stage1_usage_ledger_is_a_separate_category() {
    let (store, acct) = store();
    store
        .stage1_bump_usage(
            acct,
            "2026-07-09",
            UsageTokens {
                input: 100,
                output: 20,
                cache_creation: 40,
                cache_read: 900,
            },
        )
        .unwrap();
    store
        .stage2_bump_usage(
            acct,
            "2026-07-09",
            UsageTokens {
                input: 500,
                output: 90,
                ..Default::default()
            },
        )
        .unwrap();

    let s1 = store.stage1_usage_since(acct, "2026-07-01").unwrap();
    assert_eq!(s1.calls, 1);
    assert_eq!(s1.input_tokens, 100);
    assert_eq!(s1.output_tokens, 20);
    assert_eq!(s1.cache_creation_tokens, 40);
    assert_eq!(s1.cache_read_tokens, 900);
    let s2 = store.stage2_usage_since(acct, "2026-07-01").unwrap();
    assert_eq!(s2.calls, 1);
    assert_eq!(s2.input_tokens, 500);

    let rows1 = store.list_usage_stage1(acct, 30).unwrap();
    assert_eq!(rows1.len(), 1);
    assert_eq!(rows1[0].input_tokens, 100);
    assert_eq!(rows1[0].cache_creation_tokens, 40);
    assert_eq!(rows1[0].cache_read_tokens, 900);
    // The stage-2 list is unaffected by the stage-1 row.
    let rows2 = store.list_usage(acct, 30).unwrap();
    assert_eq!(rows2.len(), 1);
    assert_eq!(rows2[0].input_tokens, 500);
}

#[test]
fn list_usage_by_category_surfaces_extractors_nobody_named() {
    let (store, acct) = store();
    store
        .stage1_bump_usage(
            acct,
            "2026-07-09",
            UsageTokens {
                input: 100,
                output: 20,
                ..Default::default()
            },
        )
        .unwrap();
    store
        .stage2_bump_usage(
            acct,
            "2026-07-09",
            UsageTokens {
                input: 500,
                output: 90,
                ..Default::default()
            },
        )
        .unwrap();
    // An extractor category, and a category invented right here: the point of
    // enumerating is that a ledger writer added LATER still reports, without
    // anyone editing the reader.
    store
        .extract_bump_usage(
            acct,
            "2026-07-09",
            "extract_banking",
            UsageTokens {
                input: 40,
                output: 8,
                ..Default::default()
            },
        )
        .unwrap();
    store
        .extract_bump_usage(
            acct,
            "2026-07-10",
            "extract_something_new",
            UsageTokens {
                input: 7,
                output: 3,
                ..Default::default()
            },
        )
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
    let id = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    store
        .set_triage(
            id,
            acct,
            30,
            Tier::Noise,
            Sensitivity::Normal,
            None,
            "filtered",
            "matched filtered rule",
            None,
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
    let id = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    store
        .set_triage(
            id,
            acct,
            30,
            Tier::Noise,
            Sensitivity::Normal,
            None,
            "filtered",
            "matched filtered rule",
            None,
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
    assert_eq!(
        rows[0].rule_want_text.as_deref(),
        Some("WANT_BRAVO only invoices")
    );

    // And the BUILT prompt contains exactly that one rule's text — none of
    // the other two rules leak in.
    let ctx = RowContext::from_queued(&rows[0], 4000);
    let prompt = build_user_message(&ctx);
    assert!(
        prompt.contains("WANT_BRAVO only invoices"),
        "matched want must appear"
    );
    assert!(
        !prompt.contains("WANT_ALPHA"),
        "non-matched rule must not leak"
    );
    assert!(
        !prompt.contains("WANT_CHARLIE"),
        "non-matched rule must not leak"
    );
    assert_eq!(
        prompt.matches("WANT_").count(),
        1,
        "exactly one rule's want_text in the prompt"
    );

    // NO-MATCH case: a row with matched_rule_id NULL carries zero rule text.
    let id2 = store
        .upsert_message(&triaged(acct, "g2", "t2").msg())
        .unwrap();
    store
        .set_triage(
            id2,
            acct,
            40,
            Tier::Noise,
            Sensitivity::Normal,
            None,
            "ambiguous",
            "no rule matched",
            None,
        )
        .unwrap();
    let rows2 = store.stage2_queue(acct, 10).unwrap();
    let unmatched = rows2.iter().find(|r| r.message_id == id2).unwrap();
    assert!(
        unmatched.rule_want_text.is_none(),
        "no rule => no want_text"
    );
    let prompt2 = build_user_message(&RowContext::from_queued(unmatched, 4000));
    assert!(
        !prompt2.contains("WANT_"),
        "unmatched row prompt has zero rule text"
    );
    assert!(prompt2.contains("standing_instruction_for_this_sender: none"));
}

#[test]
fn stage2_budget_increment_and_exhaustion() {
    let (store, acct) = store();
    let day = "2026-07-09";

    assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 0);
    assert_eq!(
        store.stage2_increment_budget(acct, "t-abc", day).unwrap(),
        1
    );
    assert_eq!(
        store.stage2_increment_budget(acct, "t-abc", day).unwrap(),
        2
    );
    assert_eq!(store.stage2_budget_used(acct, "t-abc", day).unwrap(), 2);

    // A different thread and a different day are independent counters.
    assert_eq!(store.stage2_budget_used(acct, "t-other", day).unwrap(), 0);
    assert_eq!(
        store
            .stage2_budget_used(acct, "t-abc", "2026-07-10")
            .unwrap(),
        0
    );

    // The global sentinel is a separate scope in the same table.
    assert_eq!(
        store
            .stage2_increment_budget(acct, "__global__", day)
            .unwrap(),
        1
    );
    assert_eq!(
        store.stage2_budget_used(acct, "__global__", day).unwrap(),
        1
    );
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
        store
            .stage2_increment_budget(acct, sender_key, day)
            .unwrap();
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

    store
        .stage2_bump_usage(
            acct,
            day,
            UsageTokens {
                input: 1200,
                output: 60,
                cache_creation: 500,
                cache_read: 4000,
            },
        )
        .unwrap();
    store
        .stage2_bump_usage(
            acct,
            day,
            UsageTokens {
                input: 800,
                output: 40,
                cache_creation: 100,
                cache_read: 2000,
            },
        )
        .unwrap();
    let u = store.stage2_usage_today(acct, day).unwrap();
    assert_eq!(u.calls, 2);
    assert_eq!(u.input_tokens, 2000);
    assert_eq!(u.output_tokens, 100);
    assert_eq!(u.cache_creation_tokens, 600);
    assert_eq!(u.cache_read_tokens, 6000);

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

    store
        .stage2_bump_usage(
            acct,
            "2026-07-07",
            UsageTokens {
                input: 100,
                output: 10,
                ..Default::default()
            },
        )
        .unwrap();
    store
        .stage2_bump_usage(
            acct,
            "2026-07-08",
            UsageTokens {
                input: 200,
                output: 20,
                ..Default::default()
            },
        )
        .unwrap();
    store
        .stage2_bump_usage(
            acct,
            "2026-07-09",
            UsageTokens {
                input: 300,
                output: 30,
                ..Default::default()
            },
        )
        .unwrap();
    store
        .stage2_bump_usage(
            acct,
            "2026-07-09",
            UsageTokens {
                input: 100,
                output: 10,
                ..Default::default()
            },
        )
        .unwrap();

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
    assert_eq!(
        store.get_app_setting(a, "k").unwrap().as_deref(),
        Some("v1")
    );
    store.set_app_setting(a, "k", "v2").unwrap();
    assert_eq!(
        store.get_app_setting(a, "k").unwrap().as_deref(),
        Some("v2")
    );

    // Per-account scoped: b's key is independent.
    assert!(store.get_app_setting(b, "k").unwrap().is_none());
}

#[test]
fn stage2_cap_overrides_reads_and_precedence() {
    use crate::config::{
        APP_SETTING_GLOBAL_DAILY_CAP, APP_SETTING_SENDER_DAILY_CAP, APP_SETTING_THREAD_DAILY_CAP,
    };
    let (store, acct) = store();

    // No rows => all None (caller falls back to config/env then default).
    assert_eq!(
        store.stage2_cap_overrides(acct).unwrap(),
        Default::default()
    );

    // A set thread cap surfaces; the others stay None (so the effective cap
    // is the override where present, config/default elsewhere — precedence).
    store
        .set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "5")
        .unwrap();
    let o = store.stage2_cap_overrides(acct).unwrap();
    assert_eq!(o.thread_daily_cap, Some(5));
    assert_eq!(o.sender_daily_cap, None);
    assert_eq!(o.global_daily_cap, None);

    // Set the remaining two.
    store
        .set_app_setting(acct, APP_SETTING_SENDER_DAILY_CAP, "9")
        .unwrap();
    store
        .set_app_setting(acct, APP_SETTING_GLOBAL_DAILY_CAP, "300")
        .unwrap();
    let o = store.stage2_cap_overrides(acct).unwrap();
    assert_eq!(o.thread_daily_cap, Some(5));
    assert_eq!(o.sender_daily_cap, Some(9));
    assert_eq!(o.global_daily_cap, Some(300));

    // A malformed OR out-of-range stored value is ignored (treated as absent),
    // so a corrupt row can never remove the cap entirely.
    store
        .set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "not-a-number")
        .unwrap();
    assert_eq!(
        store.stage2_cap_overrides(acct).unwrap().thread_daily_cap,
        None
    );
    store
        .set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "0")
        .unwrap();
    assert_eq!(
        store.stage2_cap_overrides(acct).unwrap().thread_daily_cap,
        None
    );
    store
        .set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "100001")
        .unwrap();
    assert_eq!(
        store.stage2_cap_overrides(acct).unwrap().thread_daily_cap,
        None
    );
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
    store
        .set_app_setting(acct, APP_SETTING_THREAD_DAILY_CAP, "1")
        .unwrap();

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
    assert_eq!(
        calls, 1,
        "override cap of 1 must bind below the config default of 3"
    );
}

#[test]
fn stage2_usage_since_sums_window_inclusively() {
    let (store, acct) = store();

    // Empty ledger => zeros.
    assert_eq!(
        store.stage2_usage_since(acct, "2026-07-01").unwrap(),
        Stage2Usage::default()
    );

    store
        .stage2_bump_usage(
            acct,
            "2026-07-05",
            UsageTokens {
                input: 100,
                output: 10,
                ..Default::default()
            },
        )
        .unwrap();
    store
        .stage2_bump_usage(
            acct,
            "2026-07-08",
            UsageTokens {
                input: 200,
                output: 20,
                ..Default::default()
            },
        )
        .unwrap();
    store
        .stage2_bump_usage(
            acct,
            "2026-07-08",
            UsageTokens {
                input: 300,
                output: 30,
                ..Default::default()
            },
        )
        .unwrap();

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
            to_addrs: None,
            cc_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
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
        .set_triage(
            stale_id,
            acct,
            40,
            Tier::Noise,
            Sensitivity::Normal,
            None,
            "amb",
            "",
            None,
        )
        .unwrap();
    let mut fresh = triaged(acct, "g-fresh", "t-fresh").msg();
    fresh.received_at = now;
    let fresh_id = store.upsert_message(&fresh).unwrap();
    store
        .set_triage(
            fresh_id,
            acct,
            40,
            Tier::Noise,
            Sensitivity::Normal,
            None,
            "amb",
            "",
            None,
        )
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
            id,
            acct,
            40,
            Tier::Noise,
            Sensitivity::Normal,
            None,
            "amb",
            "",
            None,
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
    assert!(
        store.stage2_apply(&applied).unwrap(),
        "the guard matched the normal row"
    );

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
    assert!(
        !store.stage2_apply(&applied).unwrap(),
        "sealed row: apply reports false"
    );
    // The sealed row's triage must be unchanged (guarded by sensitivity),
    // and the verdict's deadline must NOT have been written either.
    assert!(
        store.deadlines(acct, Some(365)).unwrap().is_empty(),
        "no deadline row"
    );
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
        escalation_reason: None,
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
        .correct_triage(
            acct,
            id,
            TriageAxis::Sensitivity,
            "sealed",
            None,
            Utc::now(),
        )
        .unwrap()
        .unwrap();

    assert!(
        !store.stage1_apply(&applied).unwrap(),
        "sealed mid-pass: apply reports false"
    );
    assert!(
        store.deadlines(acct, Some(365)).unwrap().is_empty(),
        "no deadline row"
    );

    // Control: the same apply on a live row reports true.
    let live = seed_triage_row(&store, acct, "g-live", "t2", Sensitivity::Normal);
    let mut ok = applied.clone();
    ok.message_id = live;
    assert!(
        store.stage1_apply(&ok).unwrap(),
        "normal row: apply reports true"
    );
}

// ---- SCHEDULED RE-EVALUATION -------------------------------------------

use crate::triage::revisit::{RevisitRequest, RevisitSource};

fn req(at: DateTime<Utc>, why: &str, source: RevisitSource) -> RevisitRequest {
    RevisitRequest {
        at,
        why: why.into(),
        source,
    }
}

#[test]
fn a_revisit_is_invisible_until_its_date_then_comes_due() {
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-r", "t-r", Sensitivity::Normal);
    let now = Utc::now();
    let due = now + chrono::Duration::days(3);

    store
        .revisits_schedule(
            acct,
            id,
            &[req(due, "dinner has passed", RevisitSource::Model)],
            now,
        )
        .unwrap();

    // Before the date: nothing to do.
    assert!(
        store.revisit_queue(acct, now, 6, 10).unwrap().is_empty(),
        "a future revisit must not fire early"
    );

    // After it: due, carrying the PRIOR verdict so the re-score has something
    // to revise.
    let q = store
        .revisit_queue(acct, due + chrono::Duration::minutes(1), 6, 10)
        .unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].message_id, id);
    assert_eq!(q[0].reason, "dinner has passed");
    assert_eq!(q[0].source, "model");
    assert_eq!(q[0].prior_importance, 40);
    assert_eq!(q[0].prior_one_line, "ambiguous");
}

/// Firing is once-only and charges the lifetime counter, so a message cannot be
/// re-evaluated forever.
#[test]
fn firing_is_idempotent_and_spends_the_lifetime_budget() {
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-r", "t-r", Sensitivity::Normal);
    let now = Utc::now();
    store
        .revisits_schedule(acct, id, &[req(now, "now", RevisitSource::Model)], now)
        .unwrap();

    let q = store.revisit_queue(acct, now, 6, 10).unwrap();
    let rid = q[0].revisit_id;
    store.revisit_mark_fired(acct, rid, now).unwrap();
    // Double-fire must not double-charge.
    store.revisit_mark_fired(acct, rid, now).unwrap();

    assert!(
        store.revisit_queue(acct, now, 6, 10).unwrap().is_empty(),
        "a fired revisit never comes due again"
    );

    let count: i64 = store
        .lock()
        .unwrap()
        .query_row(
            "SELECT revisit_count FROM triage WHERE account_id=?1 AND message_id=?2",
            params![acct, id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "fired twice, charged once");
}

fn revisit_count(store: &SqliteStore, acct: AccountId, message_id: i64) -> i64 {
    store
        .lock()
        .unwrap()
        .query_row(
            "SELECT revisit_count FROM triage WHERE account_id=?1 AND message_id=?2",
            params![acct, message_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// THE PASS ORDER IS LOAD-BEARING, so it is asserted here rather than left to
/// the sync engine to get right silently.
///
/// `revisits_schedule` deletes every PENDING revisit for the message — including
/// the one being processed — and only a row that still exists can be stamped
/// fired. Firing is what charges the lifetime budget, so scheduling first would
/// leave `revisit_count` at zero forever and the termination guarantee would
/// never engage on a single message.
#[test]
fn a_revisit_charges_its_budget_only_if_fired_before_the_reschedule() {
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-r", "t-r", Sensitivity::Normal);
    let now = Utc::now();
    store
        .revisits_schedule(acct, id, &[req(now, "due", RevisitSource::Model)], now)
        .unwrap();

    // The order the pass uses: fire, THEN rebuild the schedule.
    let rid = store.revisit_queue(acct, now, 6, 10).unwrap()[0].revisit_id;
    store.revisit_mark_fired(acct, rid, now).unwrap();
    let next = now + chrono::Duration::days(2);
    store
        .revisits_schedule(acct, id, &[req(next, "next", RevisitSource::Model)], now)
        .unwrap();
    assert_eq!(
        revisit_count(&store, acct, id),
        1,
        "the re-evaluation spent one of the message's lifetime budget"
    );

    // The other order, demonstrating what it costs: a pending revisit the
    // reschedule deleted can never be charged, however many times it is fired.
    let rid2 = store.revisit_queue(acct, next, 6, 10).unwrap()[0].revisit_id;
    store.revisits_schedule(acct, id, &[], now).unwrap();
    store.revisit_mark_fired(acct, rid2, next).unwrap();
    assert_eq!(
        revisit_count(&store, acct, id),
        1,
        "a deleted revisit charges nothing: fire first, or do not charge at all"
    );
    // A SHARPER EDGE on the same ordering, worth naming: `id` is the rowid, and
    // SQLite hands a deleted rowid back out to the next insert. Firing a stale
    // revisit_id after a reschedule does not merely miss — it can charge, and
    // consume, whichever revisit inherited the number. One re-evaluation per
    // message per pass (the sync loop's own guard) plus firing before
    // rescheduling is what keeps a stale id from ever being fired.
}

/// A row the pipeline or the user has already resolved is not a candidate for
/// re-evaluation. Receipts and banking mail are INGESTED done so they live only
/// in their rail, and a model-scheduled revisit must not spend a call to drag one
/// back out.
#[test]
fn a_done_row_is_never_re_evaluated() {
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-d", "t-d", Sensitivity::Normal);
    let now = Utc::now();
    store
        .revisits_schedule(acct, id, &[req(now, "check", RevisitSource::Model)], now)
        .unwrap();
    assert_eq!(store.revisit_queue(acct, now, 6, 10).unwrap().len(), 1);

    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE triage SET status='done' WHERE account_id=?1 AND message_id=?2",
            params![acct, id],
        )
        .unwrap();

    assert!(
        store.revisit_queue(acct, now, 6, 10).unwrap().is_empty(),
        "the queue must not hand out a resolved row"
    );
    // ...and the apply refuses it directly, for the row someone resolves DURING
    // the model call.
    let applied = Stage1Applied {
        message_id: id,
        account_id: acct,
        importance: 90,
        tier: Tier::Signal,
        one_line: "back from the dead".into(),
        reason: "re-evaluated".into(),
        field_reasons: crate::types::FieldReasons::default(),
        stage1_model_used: "claude-opus-5".into(),
        needs_stage2: false,
        escalation_reason: None,
        deadline: None,
        category: Some("general".into()),
    };
    assert!(
        !store.revisit_apply(&applied).unwrap(),
        "revisit_apply must refuse a resolved row"
    );
}

/// The staleness sweep has a COOLDOWN, and the pending-revisit check is not it: a
/// swept row schedules at `now` and fires in the same pass, so within one sync
/// tick it is pending no longer. Without the fired-since half of the window every
/// stale standing row re-sweeps every 45 seconds, spending a frontier-model call
/// each time until the daily cap runs out.
#[test]
fn the_staleness_sweep_waits_a_full_window_before_asking_again() {
    let (store, acct) = store();
    let now = Utc::now();
    let window = chrono::Duration::days(14);
    let id = triaged(acct, "g-stale", "t-stale")
        .tier(Tier::Deadline)
        .importance(80)
        .one_line("invoice, unpaid")
        .reason("stage-1")
        .received_at(now - chrono::Duration::days(20))
        .seed(&store);
    store
        .stage1_mark_processed(acct, id, "claude-opus-5")
        .unwrap();

    let older_than = now - window;
    assert_eq!(
        store
            .revisit_stale_standing(acct, older_than, 6, 10)
            .unwrap(),
        vec![id],
        "a standing row nobody has touched in a fortnight is swept"
    );

    // The sweep's own revisit, scheduled at `now` and fired in the same pass.
    store
        .revisits_schedule(acct, id, &[req(now, "stale", RevisitSource::FyeStale)], now)
        .unwrap();
    let rid = store.revisit_queue(acct, now, 6, 10).unwrap()[0].revisit_id;
    store.revisit_mark_fired(acct, rid, now).unwrap();

    assert!(
        store
            .revisit_stale_standing(acct, older_than, 6, 10)
            .unwrap()
            .is_empty(),
        "the next tick, 45 seconds later, must not sweep the same row again"
    );

    // A full window later it is fair game once more: the row is still sitting
    // there, and the question is worth asking again.
    let a_window_later = (now + window) - window + chrono::Duration::hours(1);
    assert_eq!(
        store
            .revisit_stale_standing(acct, a_window_later, 6, 10)
            .unwrap(),
        vec![id],
        "one full window later the sweep asks again"
    );
}

/// The termination guarantee: past the lifetime cap, a message stops being
/// re-evaluated even if something keeps scheduling it.
#[test]
fn the_lifetime_cap_ends_the_loop() {
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-r", "t-r", Sensitivity::Normal);
    let now = Utc::now();
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE triage SET revisit_count = 6 WHERE account_id=?1 AND message_id=?2",
            params![acct, id],
        )
        .unwrap();
    store
        .revisits_schedule(acct, id, &[req(now, "again", RevisitSource::Model)], now)
        .unwrap();
    assert!(
        store.revisit_queue(acct, now, 6, 10).unwrap().is_empty(),
        "at the cap, a scheduled revisit must not fire"
    );
}

/// THE INVARIANT THAT MATTERS MOST: a verdict the account owner fixed by hand is
/// never overwritten by a machine, however the schedule was arrived at. Enforced
/// twice on purpose — the queue will not hand the row out, and the apply refuses
/// it even if something else does.
#[test]
fn a_human_corrected_row_is_never_re_evaluated() {
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-r", "t-r", Sensitivity::Normal);
    let now = Utc::now();
    store
        .revisits_schedule(acct, id, &[req(now, "check", RevisitSource::Model)], now)
        .unwrap();
    assert_eq!(store.revisit_queue(acct, now, 6, 10).unwrap().len(), 1);

    store
        .correct_triage(acct, id, TriageAxis::Tier, "signal", None, now)
        .unwrap()
        .unwrap();

    assert!(
        store.revisit_queue(acct, now, 6, 10).unwrap().is_empty(),
        "the queue must not hand out a hand-corrected row"
    );

    // ...and the apply refuses it directly, too.
    let applied = Stage1Applied {
        message_id: id,
        account_id: acct,
        importance: 5,
        tier: Tier::Noise,
        one_line: "machine says noise".into(),
        reason: "re-evaluated".into(),
        field_reasons: crate::types::FieldReasons::default(),
        stage1_model_used: "claude-opus-5".into(),
        needs_stage2: false,
        escalation_reason: None,
        deadline: None,
        category: Some("general".into()),
    };
    assert!(
        !store.revisit_apply(&applied).unwrap(),
        "revisit_apply must refuse a hand-corrected row"
    );
}

/// Sealed mail never gets scheduled, because firing one would put it back in
/// front of a model.
#[test]
fn a_sealed_row_stores_no_schedule() {
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-s", "t-s", Sensitivity::Sealed);
    let now = Utc::now();
    store
        .revisits_schedule(acct, id, &[req(now, "check", RevisitSource::Model)], now)
        .unwrap();
    assert!(store.revisit_queue(acct, now, 6, 10).unwrap().is_empty());
    let n: i64 = store
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM triage_revisits WHERE account_id=?1 AND message_id=?2",
            params![acct, id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "nothing is stored for sealed mail at all");
}

/// Re-scheduling replaces what is PENDING but keeps what already fired: the
/// schedule doubles as the record of why a verdict changed.
#[test]
fn rescheduling_replaces_pending_and_keeps_history() {
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-r", "t-r", Sensitivity::Normal);
    let now = Utc::now();
    store
        .revisits_schedule(acct, id, &[req(now, "first", RevisitSource::Model)], now)
        .unwrap();
    let rid = store.revisit_queue(acct, now, 6, 10).unwrap()[0].revisit_id;
    store.revisit_mark_fired(acct, rid, now).unwrap();

    // A second pending one, then a reschedule that supersedes it.
    let later = now + chrono::Duration::days(5);
    store
        .revisits_schedule(acct, id, &[req(later, "second", RevisitSource::Model)], now)
        .unwrap();
    store
        .revisits_schedule(
            acct,
            id,
            &[req(later, "third", RevisitSource::Deadline)],
            now,
        )
        .unwrap();

    let conn = store.lock().unwrap();
    let fired: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM triage_revisits
             WHERE account_id=?1 AND message_id=?2 AND fired_at IS NOT NULL",
            params![acct, id],
            |r| r.get(0),
        )
        .unwrap();
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM triage_revisits
             WHERE account_id=?1 AND message_id=?2 AND fired_at IS NULL",
            params![acct, id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fired, 1, "history survives a reschedule");
    assert_eq!(pending, 1, "only the newest pending schedule stands");
}

/// A re-evaluation clears the Stage-2 marker, so a newly escalated verdict can
/// actually reach Stage-2 instead of being stranded behind the old one.
#[test]
fn a_revisit_that_escalates_can_reenter_stage2() {
    let (store, acct) = store();
    let id = seed_triage_row(&store, acct, "g-r", "t-r", Sensitivity::Normal);
    // Pretend the row already completed both stages.
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE triage SET stage1_model_used='claude-opus-5', model_used='claude-opus-5'
             WHERE account_id=?1 AND message_id=?2",
            params![acct, id],
        )
        .unwrap();
    assert!(store.stage2_queue(acct, 10).unwrap().is_empty());

    let applied = Stage1Applied {
        message_id: id,
        account_id: acct,
        importance: 55,
        tier: Tier::Signal,
        one_line: "still relevant".into(),
        reason: "re-evaluated".into(),
        field_reasons: crate::types::FieldReasons::default(),
        stage1_model_used: "claude-opus-5".into(),
        needs_stage2: true,
        escalation_reason: Some("boundary"),
        deadline: None,
        category: Some("general".into()),
    };
    assert!(store.revisit_apply(&applied).unwrap());

    let q = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(q.len(), 1, "the re-escalated row must reach Stage-2");
    assert_eq!(q[0].message_id, id);
}

// ---- WHAT AN ESCALATION BUYS -------------------------------------------

/// THE SEAL INVARIANT, one level removed: a sealed sibling's subject is exactly
/// as forbidden to a model as its body. "It was only context for another row" is
/// not an exception, and this is the test that says so.
#[test]
fn thread_context_never_carries_a_sealed_sibling() {
    let (store, acct) = store();
    let escalated =
        triaged_row(acct, "g-esc", "t-shared", None, false, Sensitivity::Normal).ingest(&store);
    let normal_sib =
        triaged_row(acct, "g-sib", "t-shared", None, false, Sensitivity::Normal).ingest(&store);
    let sealed_sib =
        triaged_row(acct, "g-seal", "t-shared", None, false, Sensitivity::Sealed).ingest(&store);

    // Put the escalated row in the Stage-2 queue.
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE triage SET stage1_model_used='claude-opus-5', needs_stage2=1
             WHERE account_id=?1 AND message_id=?2",
            params![acct, escalated],
        )
        .unwrap();

    let q = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(q.len(), 1);
    let ids: Vec<String> = q[0].thread.iter().map(|s| s.subject.clone()).collect();
    assert_eq!(q[0].thread.len(), 1, "only the non-sealed sibling: {ids:?}");
    assert_ne!(escalated, normal_sib);
    assert_ne!(escalated, sealed_sib);
}

/// THE THREAD WINDOW TAKES THE NEWEST SIBLINGS, not the oldest.
///
/// The prompt states "the account owner HAS WRITTEN / has never written in it" as
/// a fact, and on a long thread the owner's reply is at the END. An oldest-first
/// window drops it, and the second pass is then told, as trusted context, the
/// exact opposite of the single most useful thing in the thread.
#[test]
fn thread_context_keeps_the_newest_siblings_including_the_owners_reply() {
    let (store, acct) = store();
    let now = Utc::now();
    // A long thread: twelve inbound messages, then the owner's own reply.
    for i in 0..12 {
        triaged_row(
            acct,
            &format!("g-old{i}"),
            "t-long",
            None,
            false,
            Sensitivity::Normal,
        )
        .subject(&format!("old {i}"))
        .received_at(now - chrono::Duration::days(20 - i))
        .ingest(&store);
    }
    triaged_row(acct, "g-reply", "t-long", None, false, Sensitivity::Normal)
        .subject("the owner replies")
        .is_sent(true)
        .received_at(now - chrono::Duration::hours(2))
        .ingest(&store);

    let escalated = triaged_row(acct, "g-esc", "t-long", None, false, Sensitivity::Normal)
        .subject("the row under judgement")
        .received_at(now)
        .ingest(&store);
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE triage SET stage1_model_used='claude-opus-5', needs_stage2=1
             WHERE account_id=?1 AND message_id=?2",
            params![acct, escalated],
        )
        .unwrap();

    let q = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(q.len(), 1);
    let thread = &q[0].thread;
    assert!(
        thread.iter().any(|s| s.is_sent),
        "the owner's reply is the last message and must survive the window: {:?}",
        thread.iter().map(|s| &s.subject).collect::<Vec<_>>()
    );
    assert!(
        thread
            .windows(2)
            .all(|w| w[0].received_at <= w[1].received_at),
        "still rendered oldest-first"
    );
}

/// An escalated row arrives knowing WHY it was escalated and what this sender's
/// verdicts have historically been worth.
#[test]
fn an_escalated_row_carries_its_reason_and_the_senders_record() {
    let (store, acct) = store();
    let id = triaged_row(acct, "g-a", "t-a", None, false, Sensitivity::Normal).ingest(&store);
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE triage SET stage1_model_used='claude-opus-5', needs_stage2=1,
                    escalation_reason='buried_bill'
             WHERE account_id=?1 AND message_id=?2",
            params![acct, id],
        )
        .unwrap();

    let q = store.stage2_queue(acct, 10).unwrap();
    assert_eq!(q[0].escalation_reason.as_deref(), Some("buried_bill"));
    // The row under judgement is NOT its own history: reporting "1 previous
    // message" for a first-time sender says the opposite of the truth.
    assert_eq!(
        q[0].sender_history.total, 0,
        "a first message from a sender has no history"
    );

    // A second message from the same sender DOES see the first.
    let second = triaged_row(acct, "g-b", "t-b", None, false, Sensitivity::Normal).ingest(&store);
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE triage SET stage1_model_used='claude-opus-5', needs_stage2=1
             WHERE account_id=?1 AND message_id=?2",
            params![acct, second],
        )
        .unwrap();
    let q = store.stage2_queue(acct, 10).unwrap();
    let row = q.iter().find(|r| r.message_id == second).unwrap();
    assert_eq!(row.sender_history.total, 1);
}
