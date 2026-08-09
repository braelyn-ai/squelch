//! Shipment, receipt, banking, calendar and bill auto-close tests.

use super::super::*;
use super::support::*;
use crate::types::Sensitivity;
use chrono::TimeZone;

#[test]
fn banking_apply_writes_row_stamps_marker_and_auto_resolves() {
    let (store, acct) = store();
    let id = triaged_row(acct, "g-stmt", "t1", None, false, Sensitivity::Normal)
        .category("banking_statement")
        .ingest(&store);

    store
        .banking_apply(&BankingApplied {
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
        })
        .unwrap();

    // The banking row landed with the extracted fields.
    let b = store.list_banking(acct).unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].kind, "statement");
    assert_eq!(b[0].institution.as_deref(), Some("Chase"));
    assert_eq!(b[0].amount, Some(1234.56));
    assert_eq!(b[0].account_hint.as_deref(), Some("…1234"));

    // The triage row was stamped (leaves the queue) AND auto-resolved.
    let (status, resolved_at, marker) = triage_extract_status(&store, id);
    assert_eq!(status, "done", "banking statement auto-resolves");
    assert!(resolved_at.is_some(), "resolved_at stamped");
    assert_eq!(marker.as_deref(), Some("claude-haiku-4-5"));
    assert!(
        store
            .extract_queue(acct, &["banking_statement"], 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn invoice_row_is_not_auto_resolved_and_stays_standing() {
    // Parity guard: an invoice is categorized but has NO extractor, so it is
    // never queued and never auto-resolved — it stays 'new' (standing).
    let (store, acct) = store();
    let id = triaged_row(acct, "g-inv", "t1", None, false, Sensitivity::Normal)
        .category("invoice")
        .ingest(&store);

    assert!(
        store
            .extract_queue(acct, &["banking_statement", "transaction_alert"], 10)
            .unwrap()
            .is_empty(),
        "invoice is never in the extract queue"
    );
    let (status, resolved_at, _) = triage_extract_status(&store, id);
    assert_eq!(status, "new", "invoice stays standing (not auto-resolved)");
    assert!(resolved_at.is_none());
}

#[test]
fn receipt_ingest_auto_resolves_and_lists_and_stays_out_of_bands() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);

    let id = receipt_triaged(acct, "g-r1", "t-r1", Some(3.49)).ingest(&store);

    // 1. The receipt row exists with its amount + clean sender.
    let receipts = store.list_receipts(acct, 30).unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].amount, Some(3.49));
    assert_eq!(receipts[0].currency.as_deref(), Some("USD"));
    assert_eq!(receipts[0].from_addr, "no-reply@baywheels.com");
    assert_eq!(receipts[0].from_name.as_deref(), Some("Bay Wheels"));

    // 2. AUTO-RESOLVE: the triage row is status='done' with resolved_at set.
    let done = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Done), None)
        .unwrap();
    assert_eq!(done.len(), 1, "receipt is auto-resolved to done");
    assert_eq!(done[0].update.id, id);
    assert!(done[0].resolved_at.is_some());

    // 3. It is ABSENT from the New band (never inbox clutter) even though it
    //    was never surfaced (surfaced_at IS NULL).
    let fresh = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::New))
        .unwrap();
    assert!(
        fresh.is_empty(),
        "auto-done receipt must not be in the New band"
    );

    // 4. Bands counts agree: new == 0, standing == 0.
    let stats = store
        .stats(acct, Utc::now() - chrono::Duration::days(30))
        .unwrap();
    assert_eq!(stats.bands.new, 0, "receipt excluded from new count");
    assert_eq!(stats.bands.standing, 0);
}

#[test]
fn receipt_with_no_amount_still_lists() {
    let (store, acct) = store();
    receipt_triaged(acct, "g-r2", "t-r2", None).ingest(&store);
    let receipts = store.list_receipts(acct, 30).unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].amount, None,
        "a receipt with no total is still a receipt"
    );
}

#[test]
fn calendar_ingest_auto_resolves_and_lists_and_stays_out_of_bands() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);

    let id = calendar_triaged(
        acct,
        "g-cal1",
        crate::triage::CalendarKind::Invite,
        Utc::now(),
    )
    .ingest(&store);

    // 1. The calendar row exists with its extracted fields.
    let items = store.list_calendar_updates(acct, 24).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].message_id, id);
    // The joined thread is what the rail clicks through to; without it the
    // row can only jump to the mail page.
    assert_eq!(items[0].thread_id, "t-g-cal1");
    assert_eq!(items[0].kind, "invite");
    assert_eq!(items[0].event_title.as_deref(), Some("Design review"));
    assert_eq!(
        items[0].starts_at,
        Some(Utc.with_ymd_and_hms(2026, 7, 22, 10, 0, 0).unwrap())
    );
    assert_eq!(items[0].organizer.as_deref(), Some("Sam Doe"));

    // 2. AUTO-RESOLVE: the triage row is status='done' with resolved_at set
    //    (same mechanism as receipts — squelch-internal only; nothing is
    //    written back to Gmail).
    let done = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Done), None)
        .unwrap();
    assert_eq!(done.len(), 1, "calendar update is auto-resolved to done");
    assert_eq!(done[0].update.id, id);
    assert!(done[0].resolved_at.is_some());

    // 3. ABSENT from the New band (never inbox clutter).
    let fresh = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::New))
        .unwrap();
    assert!(
        fresh.is_empty(),
        "auto-done calendar update must not be in New"
    );
    let stats = store
        .stats(acct, Utc::now() - chrono::Duration::days(30))
        .unwrap();
    assert_eq!(stats.bands.new, 0);
    assert_eq!(stats.bands.standing, 0);
}

#[test]
fn calendar_list_windows_on_received_at_hours() {
    // The window is mail-ARRIVAL time (received_at), not event start.
    let (store, acct) = store();
    let now = Utc::now();

    calendar_triaged(
        acct,
        "g-cal-new",
        crate::triage::CalendarKind::Update,
        now - chrono::Duration::hours(2),
    )
    .ingest(&store);
    calendar_triaged(
        acct,
        "g-cal-old",
        crate::triage::CalendarKind::Cancellation,
        now - chrono::Duration::hours(30),
    )
    .ingest(&store);

    // Default-ish 24h window: only the 2h-old row.
    let items = store.list_calendar_updates(acct, 24).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, "update");
    // Wider window: both, newest-received first.
    let items = store.list_calendar_updates(acct, 48).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].kind, "update", "newest-received first");
    assert_eq!(items[1].kind, "cancellation");
}

#[test]
fn calendar_upsert_is_idempotent_per_message() {
    let (store, acct) = store();
    let t = calendar_triaged(
        acct,
        "g-cal-i",
        crate::triage::CalendarKind::Invite,
        Utc::now(),
    );
    let id1 = t.ingest(&store);
    let id2 = t.ingest(&store);
    assert_eq!(id1, id2);
    assert_eq!(
        store.list_calendar_updates(acct, 24).unwrap().len(),
        1,
        "re-ingest updates the same row"
    );
}

#[test]
fn receipt_matching_merchant_and_amount_closes_open_bill() {
    let (store, acct) = store();
    let now = Utc::now();

    // An open PG&E bill for $84.20, received 10 days ago.
    let bill_id = bill_triaged(
        acct,
        "g-bill1",
        "billing@pge.com",
        Some("PG&E"),
        Some(84.20),
        now - chrono::Duration::days(10),
        now + chrono::Duration::days(5),
    )
    .ingest(&store);

    // The payment receipt: different mailbox + subdomain, name spelled
    // "PGE", same amount.
    receipt_from(
        acct,
        "g-pay1",
        "receipts@billing.pge.com",
        Some("PGE"),
        Some(84.20),
        now,
    )
    .ingest(&store);

    // The bill's triage row is resolved through the standard transition
    // (done + resolved_at), so it leaves the standing/obligations band.
    let (status, resolved_at) = triage_status(&store, acct, bill_id);
    assert_eq!(status, "done", "matched bill auto-closes");
    assert!(resolved_at.is_some(), "done stamps resolved_at");
    assert_eq!(
        store
            .stats(acct, Utc::now() - chrono::Duration::days(30))
            .unwrap()
            .bands
            .standing,
        0
    );

    // The WHY is on the audit trail, targeting the bill's message id.
    let audits = auto_close_audits(&store, acct);
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].actor, "ingest");
    assert_eq!(
        audits[0].target.as_deref(),
        Some(bill_id.to_string().as_str())
    );
}

#[test]
fn receipt_amount_mismatch_does_not_close_bill() {
    let (store, acct) = store();
    let now = Utc::now();

    let bill_id = bill_triaged(
        acct,
        "g-bill2",
        "billing@pge.com",
        Some("PG&E"),
        Some(84.20),
        now - chrono::Duration::days(10),
        now + chrono::Duration::days(5),
    )
    .ingest(&store);
    // Same merchant, WRONG amount (a small partial charge, not the bill).
    receipt_from(
        acct,
        "g-pay2",
        "receipts@pge.com",
        Some("PG&E"),
        Some(12.00),
        now,
    )
    .ingest(&store);

    let (status, _) = triage_status(&store, acct, bill_id);
    assert_eq!(status, "new", "amount mismatch must NOT close the bill");
    assert!(auto_close_audits(&store, acct).is_empty());
}

#[test]
fn receipt_without_amount_never_closes_an_amounted_bill() {
    // The bill has a verifiable amount but the receipt parsed none: the one
    // number we could check is missing — refuse (a false close hides an
    // unpaid bill).
    let (store, acct) = store();
    let now = Utc::now();

    let bill_id = bill_triaged(
        acct,
        "g-bill3",
        "billing@pge.com",
        Some("PG&E"),
        Some(84.20),
        now - chrono::Duration::days(3),
        now + chrono::Duration::days(12),
    )
    .ingest(&store);
    receipt_from(acct, "g-pay3", "receipts@pge.com", Some("PG&E"), None, now).ingest(&store);

    let (status, _) = triage_status(&store, acct, bill_id);
    assert_eq!(status, "new");
    assert!(auto_close_audits(&store, acct).is_empty());
}

#[test]
fn merchant_name_normalization_matches_across_domains() {
    // Different domains entirely; identity carried by the normalized
    // display name ("PG&E" == "PGE" after case/punctuation folding).
    let (store, acct) = store();
    let now = Utc::now();

    let bill_id = bill_triaged(
        acct,
        "g-bill4",
        "billing@pacificgas.com",
        Some("PG&E"),
        Some(84.20),
        now - chrono::Duration::days(7),
        now + chrono::Duration::days(7),
    )
    .ingest(&store);
    receipt_from(
        acct,
        "g-pay4",
        "no-reply@pge.com",
        Some("pge"),
        Some(84.20),
        now,
    )
    .ingest(&store);

    let (status, _) = triage_status(&store, acct, bill_id);
    assert_eq!(status, "done", "normalized names establish the merchant");
}

#[test]
fn already_done_bill_is_not_touched() {
    let (store, acct) = store();
    let now = Utc::now();

    let bill_id = bill_triaged(
        acct,
        "g-bill5",
        "billing@pge.com",
        Some("PG&E"),
        Some(84.20),
        now - chrono::Duration::days(10),
        now + chrono::Duration::days(5),
    )
    .ingest(&store);
    // The user already dismissed it.
    assert!(
        store
            .set_attention_status(acct, bill_id, AttentionStatus::Done)
            .unwrap()
    );

    receipt_from(
        acct,
        "g-pay5",
        "receipts@pge.com",
        Some("PG&E"),
        Some(84.20),
        now,
    )
    .ingest(&store);

    // Still done, and the auto-closer left no audit row (it never fired —
    // a done bill is not an open candidate, so no double-resolution).
    let (status, resolved_at) = triage_status(&store, acct, bill_id);
    assert_eq!(status, "done");
    assert!(resolved_at.is_some());
    assert!(auto_close_audits(&store, acct).is_empty());
}

#[test]
fn receipt_with_no_matching_bill_does_nothing() {
    let (store, acct) = store();
    let now = Utc::now();

    // An open Comcast bill; the receipt is from an unrelated merchant.
    let bill_id = bill_triaged(
        acct,
        "g-bill6",
        "billing@comcast.com",
        Some("Comcast"),
        Some(89.99),
        now - chrono::Duration::days(5),
        now + chrono::Duration::days(10),
    )
    .ingest(&store);
    let receipt_id = receipt_from(
        acct,
        "g-pay6",
        "no-reply@baywheels.com",
        Some("Bay Wheels"),
        Some(3.49),
        now,
    )
    .ingest(&store);

    let (status, _) = triage_status(&store, acct, bill_id);
    assert_eq!(status, "new", "unrelated bill stays open");
    assert!(auto_close_audits(&store, acct).is_empty());
    // The receipt itself is still auto-resolved + listed as usual.
    let (rstatus, _) = triage_status(&store, acct, receipt_id);
    assert_eq!(rstatus, "done");
    assert_eq!(store.list_receipts(acct, 30).unwrap().len(), 1);
}

#[test]
fn amountless_bill_closes_on_merchant_match_within_tight_window() {
    // The bill parsed no amount: merchant identity + the tight recency
    // window carry the match alone.
    let (store, acct) = store();
    let now = Utc::now();

    let bill_id = bill_triaged(
        acct,
        "g-bill7",
        "billing@pge.com",
        Some("PG&E"),
        None,
        now - chrono::Duration::days(10),
        now + chrono::Duration::days(5),
    )
    .ingest(&store);
    receipt_from(
        acct,
        "g-pay7",
        "receipts@pge.com",
        Some("PG&E"),
        Some(84.20),
        now,
    )
    .ingest(&store);

    let (status, _) = triage_status(&store, acct, bill_id);
    assert_eq!(status, "done");
    assert_eq!(auto_close_audits(&store, acct).len(), 1);
}

#[test]
fn stale_bill_outside_recency_window_is_not_closed() {
    // Same merchant + amount, but the bill is 90 days old — outside even
    // the wide amount-verified window, so it stays open (stale history must
    // not be silently swept by a coincidental amount).
    let (store, acct) = store();
    let now = Utc::now();

    let bill_id = bill_triaged(
        acct,
        "g-bill8",
        "billing@pge.com",
        Some("PG&E"),
        Some(84.20),
        now - chrono::Duration::days(90),
        now - chrono::Duration::days(75),
    )
    .ingest(&store);
    receipt_from(
        acct,
        "g-pay8",
        "receipts@pge.com",
        Some("PG&E"),
        Some(84.20),
        now,
    )
    .ingest(&store);

    let (status, _) = triage_status(&store, acct, bill_id);
    assert_eq!(status, "new");
    assert!(auto_close_audits(&store, acct).is_empty());
}

#[test]
fn one_receipt_closes_only_the_earliest_due_of_identical_bills() {
    // Two open months of the same $15.49 subscription: one payment settles
    // ONE month — the earliest due. Closing both would hide the unpaid one.
    let (store, acct) = store();
    let now = Utc::now();

    let june = bill_triaged(
        acct,
        "g-bill-jun",
        "billing@streamco.com",
        Some("StreamCo"),
        Some(15.49),
        now - chrono::Duration::days(40),
        now - chrono::Duration::days(25),
    )
    .ingest(&store);
    let july = bill_triaged(
        acct,
        "g-bill-jul",
        "billing@streamco.com",
        Some("StreamCo"),
        Some(15.49),
        now - chrono::Duration::days(10),
        now + chrono::Duration::days(5),
    )
    .ingest(&store);
    receipt_from(
        acct,
        "g-pay-jun",
        "receipts@streamco.com",
        Some("StreamCo"),
        Some(15.49),
        now,
    )
    .ingest(&store);

    let (june_status, _) = triage_status(&store, acct, june);
    let (july_status, _) = triage_status(&store, acct, july);
    assert_eq!(june_status, "done", "earliest-due month is the one paid");
    assert_eq!(july_status, "new", "the newer month must stay open");
    assert_eq!(auto_close_audits(&store, acct).len(), 1);
}

#[test]
fn shipment_upsert_dedupes_and_state_machine_no_regress() {
    use crate::triage::{ShipmentInfo, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();

    let ship = |status, item: &str| ShipmentInfo {
        carrier: "ups".into(),
        tracking_number: "1Z999AA10123456784".into(),
        item_name: item.into(),
        status,
        tracking_url: Some("https://www.ups.com/track?tracknum=1Z999AA10123456784".into()),
    };

    // First sight: shipped.
    let t0 = Utc::now();
    let id1 = store
        .upsert_shipment(acct, mid, &ship(ShipmentStatus::Shipped, ""), t0)
        .unwrap();
    // Second email, same tracking number: out_for_delivery + a better item
    // name. Must UPDATE the same row (dedupe), advance status, adopt name.
    let id2 = store
        .upsert_shipment(
            acct,
            mid,
            &ship(ShipmentStatus::OutForDelivery, "Wireless Headphones"),
            t0 + chrono::Duration::minutes(1),
        )
        .unwrap();
    assert_eq!(id1, id2, "same tracking number dedupes to one row");

    let en_route = store.list_shipments(acct, false).unwrap();
    assert_eq!(en_route.len(), 1);
    assert_eq!(en_route[0].status, "out_for_delivery");
    assert_eq!(en_route[0].item_name, "Wireless Headphones");

    // Deliver it.
    store
        .upsert_shipment(
            acct,
            mid,
            &ship(ShipmentStatus::Delivered, ""),
            t0 + chrono::Duration::minutes(2),
        )
        .unwrap();
    // A LATE stale "shipped" email must NOT regress the delivered shipment.
    store
        .upsert_shipment(
            acct,
            mid,
            &ship(ShipmentStatus::Shipped, ""),
            t0 + chrono::Duration::minutes(3),
        )
        .unwrap();

    // En-route list now excludes it (delivered).
    assert!(store.list_shipments(acct, false).unwrap().is_empty());
    // include_delivered surfaces it, still delivered (no regress).
    let all = store.list_shipments(acct, true).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, "delivered", "delivered never regresses");
}
