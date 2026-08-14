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

    let en_route = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(en_route.len(), 1);
    assert_eq!(en_route[0].status, "out_for_delivery");
    assert_eq!(en_route[0].item_name, "Wireless Headphones");

    // Deliver it.
    let delivered_at = t0 + chrono::Duration::minutes(2);
    store
        .upsert_shipment(
            acct,
            mid,
            &ship(ShipmentStatus::Delivered, ""),
            delivered_at,
        )
        .unwrap();
    // A LATE stale "shipped" email (from another thread) must NOT regress the
    // delivered shipment — and must not become the row's click target or bump
    // its clock either.
    let stale_mid = store
        .upsert_message(&triaged(acct, "g-stale", "t-stale").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            stale_mid,
            &ship(ShipmentStatus::Shipped, ""),
            t0 + chrono::Duration::minutes(3),
        )
        .unwrap();

    // En-route list now excludes it (delivered).
    assert!(
        store
            .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .is_empty()
    );
    // include_delivered surfaces it, still delivered (no regress).
    let all = store
        .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, "delivered", "delivered never regresses");
    // The rejected stale email moved neither the click target nor the clock.
    assert_eq!(
        all[0].thread_id.as_deref(),
        Some("t1"),
        "a rejected status must not steal the click target"
    );
    assert_eq!(
        all[0].last_update, delivered_at,
        "a rejected status must not bump last_update"
    );
}

#[test]
fn list_shipments_serves_thread_and_left_join_keeps_pointerless_rows() {
    use crate::triage::{ShipmentInfo, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            mid,
            &ShipmentInfo {
                carrier: "ups".into(),
                tracking_number: "1Z999AA10123456784".into(),
                item_name: "Headphones".into(),
                status: ShipmentStatus::Shipped,
                tracking_url: None,
            },
            Utc::now(),
        )
        .unwrap();

    // The join serves the feeding message's thread so the card can open it.
    let listed = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].thread_id.as_deref(), Some("t1"));

    // A row written by an older daemon has no message pointer. The join MUST
    // stay a LEFT JOIN: the shipment still lists, just with no thread — an
    // inner join would silently drop it and this assert is what notices.
    store
        .lock()
        .unwrap()
        .execute("UPDATE shipments SET last_message_id = NULL", [])
        .unwrap();
    let listed = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(listed.len(), 1, "pointerless rows must still list");
    assert_eq!(listed[0].thread_id, None);
}

// ---- carrier polling ---------------------------------------------------

/// A shipment on `carrier` with a distinct tracking number, at `status`.
fn shipped(
    carrier: &str,
    number: &str,
    status: crate::triage::ShipmentStatus,
) -> crate::triage::ShipmentInfo {
    crate::triage::ShipmentInfo {
        carrier: carrier.into(),
        tracking_number: number.into(),
        item_name: "Headphones".into(),
        status,
        tracking_url: None,
    }
}

/// `(last_message_id, poll_failures)` straight from the row — neither is on the
/// wire type.
fn shipment_internals(store: &SqliteStore, id: i64) -> (Option<i64>, i64) {
    store
        .lock()
        .unwrap()
        .query_row(
            "SELECT last_message_id, poll_failures FROM shipments WHERE id=?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[test]
fn carrier_track_advances_status_and_leaves_the_click_target_alone() {
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now();
    let sid = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();

    let eta = t0 + chrono::Duration::hours(4);
    let polled = t0 + chrono::Duration::minutes(5);
    let changed = store
        .apply_carrier_track(
            acct,
            sid,
            &CarrierTrack {
                status: Some(ShipmentStatus::OutForDelivery),
                carrier_status_raw: "Out For Delivery".into(),
                eta: Some(eta),
                delivered_at: None,
            },
            polled,
        )
        .unwrap();
    assert!(changed, "shipped -> out_for_delivery is a status change");

    let listed = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(listed[0].status, "out_for_delivery");
    assert_eq!(
        listed[0].carrier_status_raw.as_deref(),
        Some("Out For Delivery")
    );
    assert_eq!(listed[0].eta, Some(eta));
    assert_eq!(listed[0].last_polled_at, Some(polled));
    assert_eq!(listed[0].last_update, polled, "a visible change moves it");

    // A poll has NO message behind it, so the click target stays the last
    // accepted email — and the sealing delete keys on exactly that pointer.
    assert_eq!(listed[0].thread_id.as_deref(), Some("t1"));
    assert_eq!(shipment_internals(&store, sid).0, Some(mid));
}

#[test]
fn carrier_replaces_the_email_inferred_status_in_both_directions() {
    // The carrier is ground truth, so unlike the email merge it may regress —
    // both of these are moves `ShipmentStatus::merge` would refuse.
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now();

    let track = |status, raw: &str| CarrierTrack {
        status: Some(status),
        carrier_status_raw: raw.into(),
        eta: None,
        delivered_at: None,
    };

    // A delay the mail announced, cleared by the carrier's own scan.
    let excepted = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Exception),
            t0,
        )
        .unwrap();
    store
        .apply_carrier_track(
            acct,
            excepted,
            &track(ShipmentStatus::Shipped, "In Transit"),
            t0,
        )
        .unwrap();

    // A package that missed its truck, walked back to shipped.
    let ofd = store
        .upsert_shipment(
            acct,
            mid,
            &shipped(
                "usps",
                "9400111899223817428490",
                ShipmentStatus::OutForDelivery,
            ),
            t0,
        )
        .unwrap();
    store
        .apply_carrier_track(acct, ofd, &track(ShipmentStatus::Shipped, "In Transit"), t0)
        .unwrap();

    let by_id: std::collections::HashMap<i64, String> = store
        .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
        .unwrap()
        .into_iter()
        .map(|s| (s.id, s.status))
        .collect();
    assert_eq!(by_id[&excepted], "shipped", "carrier clears the exception");
    assert_eq!(
        by_id[&ofd], "shipped",
        "carrier walks out_for_delivery back"
    );
}

#[test]
fn a_delivered_shipment_survives_any_carrier_status() {
    // Carrier scan data lags an email-announced delivery, so Delivered stays
    // terminal against a poll too.
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now();
    let sid = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Delivered),
            t0,
        )
        .unwrap();

    let changed = store
        .apply_carrier_track(
            acct,
            sid,
            &CarrierTrack {
                status: Some(ShipmentStatus::Shipped),
                carrier_status_raw: "In Transit".into(),
                eta: None,
                delivered_at: None,
            },
            t0 + chrono::Duration::minutes(1),
        )
        .unwrap();
    assert!(!changed, "delivered is terminal, so nothing changed");
    assert_eq!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()[0]
            .status,
        "delivered"
    );
}

#[test]
fn an_unmappable_carrier_status_records_the_raw_string_only() {
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now();
    let sid = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();

    let changed = store
        .apply_carrier_track(
            acct,
            sid,
            &CarrierTrack {
                status: None,
                carrier_status_raw: "Held At Customs".into(),
                eta: None,
                delivered_at: None,
            },
            t0 + chrono::Duration::minutes(1),
        )
        .unwrap();
    assert!(!changed);
    let listed = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(
        listed[0].status, "shipped",
        "an unmapped status is not guessed at"
    );
    assert_eq!(
        listed[0].carrier_status_raw.as_deref(),
        Some("Held At Customs"),
        "what the carrier said is still recorded"
    );
}

#[test]
fn a_poll_with_nothing_new_advances_last_polled_at_but_not_last_update() {
    // last_update is the Sitrep sort key: polling every en-route shipment must
    // not reshuffle the cards.
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now();
    let sid = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();

    let track = CarrierTrack {
        status: Some(ShipmentStatus::Shipped),
        carrier_status_raw: "In Transit".into(),
        eta: None,
        delivered_at: None,
    };
    let first = t0 + chrono::Duration::minutes(1);
    store.apply_carrier_track(acct, sid, &track, first).unwrap();
    let after_first = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap()
        .remove(0);
    assert_eq!(after_first.last_update, first, "the raw string was new");

    // Same answer an hour later: nothing the user can see moved.
    let second = t0 + chrono::Duration::hours(1);
    let changed = store
        .apply_carrier_track(acct, sid, &track, second)
        .unwrap();
    assert!(!changed);
    let after_second = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap()
        .remove(0);
    assert_eq!(
        after_second.last_update, first,
        "an unchanged poll must not churn the sort order"
    );
    assert_eq!(
        after_second.last_polled_at,
        Some(second),
        "the attempt still lands"
    );
}

#[test]
fn list_pollable_shipments_filters_delivered_carrier_age_and_failures() {
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let now = Utc::now();
    let cutoff = now - chrono::Duration::days(30);

    let land = |carrier: &str, number: &str, status, seen| {
        store
            .upsert_shipment(acct, mid, &shipped(carrier, number, status), seen)
            .unwrap()
    };
    let live = land("ups", "1Z999AA10123456784", ShipmentStatus::Shipped, now);
    land("ups", "1Z999AA10123456785", ShipmentStatus::Delivered, now);
    land("amazon", "TBA303392911000", ShipmentStatus::Shipped, now);
    land("unknown", "555000111222", ShipmentStatus::Shipped, now);
    let stale = land(
        "fedex",
        "123456789012",
        ShipmentStatus::Shipped,
        now - chrono::Duration::days(45),
    );
    let failing = land("dhl", "1234567890", ShipmentStatus::Shipped, now);

    // Three permanent failures retires it at a cap of 3.
    for i in 1..=3 {
        store
            .record_poll_outcome(acct, failing, now + chrono::Duration::minutes(i), true)
            .unwrap();
    }

    let pollable: Vec<i64> = store
        .list_pollable_shipments(acct, cutoff, 3)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(
        pollable,
        vec![live],
        "delivered, API-less carriers, over-age and failure-capped rows are all out"
    );

    // Widening the window readmits the stale one; raising the cap readmits the
    // failing one — proving each exclusion was its own predicate.
    let widened: Vec<i64> = store
        .list_pollable_shipments(acct, now - chrono::Duration::days(60), 4)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert!(widened.contains(&stale) && widened.contains(&failing));

    // A carrier answer resets the counter, so the retired row polls again.
    store
        .apply_carrier_track(
            acct,
            failing,
            &CarrierTrack {
                status: Some(ShipmentStatus::Shipped),
                carrier_status_raw: "In Transit".into(),
                eta: None,
                delivered_at: None,
            },
            now + chrono::Duration::hours(1),
        )
        .unwrap();
    assert_eq!(shipment_internals(&store, failing).1, 0);
    assert!(
        store
            .list_pollable_shipments(acct, cutoff, 3)
            .unwrap()
            .iter()
            .any(|s| s.id == failing),
        "a success un-retires the shipment"
    );
}

#[test]
fn record_poll_outcome_stamps_every_attempt_but_counts_only_permanent_failures() {
    use crate::triage::ShipmentStatus;
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now();
    let sid = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();

    // A transient error rotates the row through the queue without spending a
    // life against the retirement cap.
    let transient = t0 + chrono::Duration::minutes(1);
    store
        .record_poll_outcome(acct, sid, transient, false)
        .unwrap();
    assert_eq!(shipment_internals(&store, sid).1, 0);
    let listed = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(listed[0].last_polled_at, Some(transient));
    assert_eq!(
        listed[0].last_update, t0,
        "a failed poll shows the user nothing"
    );

    store
        .record_poll_outcome(acct, sid, t0 + chrono::Duration::minutes(2), true)
        .unwrap();
    store
        .record_poll_outcome(acct, sid, t0 + chrono::Duration::minutes(3), true)
        .unwrap();
    assert_eq!(shipment_internals(&store, sid).1, 2);
}

/// DEFECT: retirement was a one-way door. `poll_failures` was zeroed ONLY by a
/// successful poll — which a retired row can never have, because the cap is
/// exactly what keeps it out of the poll queue. Pre-manifest 404s are ordinary
/// (a retailer mails the waybill before the handover), so a LIVE parcel could
/// cross the cap, leave the queue AND the client lists, and never come back:
/// not on a later email, not on the delivery notice, not on a retriage.
///
/// Mail is the second witness. An update the no-regress state machine ACCEPTS
/// is fresh evidence the number is real, so the failure count starts over.
#[test]
fn a_new_email_revives_a_retired_shipment() {
    use crate::triage::ShipmentStatus;
    let (store, acct) = store();
    let first = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now();
    // An AMBIGUOUS shape, so this also exercises the read-side suppression the
    // same counter drives: a retired row of this shape is invisible to both
    // doors, not merely unpolled.
    let sid = store
        .upsert_shipment(
            acct,
            first,
            &shipped("fedex", "123456789012", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();
    fail_polls(&store, acct, sid, 5);
    assert!(
        store
            .list_pollable_shipments(acct, t0 - chrono::Duration::days(30), 5)
            .unwrap()
            .is_empty(),
        "retired out of the poll queue"
    );
    assert!(
        store
            .list_shipments(acct, false, suppress_at(5))
            .unwrap()
            .is_empty(),
        "and out of the lists"
    );

    // The parcel was real all along, and here is the mail that says so.
    let second = store
        .upsert_message(&triaged(acct, "g2", "t1").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            second,
            &shipped("fedex", "123456789012", ShipmentStatus::OutForDelivery),
            t0 + chrono::Duration::hours(1),
        )
        .unwrap();

    assert_eq!(
        shipment_internals(&store, sid).1,
        0,
        "an accepted email clears the carrier's rejections"
    );
    assert!(
        store
            .list_pollable_shipments(acct, t0 - chrono::Duration::days(30), 5)
            .unwrap()
            .iter()
            .any(|s| s.id == sid),
        "the revived row is pollable again"
    );
    assert_eq!(
        store
            .list_shipments(acct, false, suppress_at(5))
            .unwrap()
            .len(),
        1
    );

    // A REJECTED update is not evidence: a stale "shipped" arriving after the
    // delivery is exactly the mail the merge threw away, and it must not
    // resurrect a genuine phantom.
    store
        .upsert_shipment(
            acct,
            second,
            &shipped("fedex", "123456789012", ShipmentStatus::Delivered),
            t0 + chrono::Duration::hours(2),
        )
        .unwrap();
    fail_polls(&store, acct, sid, 5);
    store
        .upsert_shipment(
            acct,
            first,
            &shipped("fedex", "123456789012", ShipmentStatus::Shipped),
            t0 + chrono::Duration::hours(3),
        )
        .unwrap();
    assert_eq!(
        shipment_internals(&store, sid).1,
        5,
        "a regress the state machine rejected is not evidence of anything"
    );
}

#[test]
fn delivered_at_is_stamped_by_either_path_and_never_overwritten() {
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now();

    // POLL PATH: the carrier's own timestamp wins over the poll clock.
    let polled = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();
    let carrier_stamp = t0 + chrono::Duration::hours(2);
    store
        .apply_carrier_track(
            acct,
            polled,
            &CarrierTrack {
                status: Some(ShipmentStatus::Delivered),
                carrier_status_raw: "Delivered".into(),
                eta: None,
                delivered_at: Some(carrier_stamp),
            },
            t0 + chrono::Duration::hours(3),
        )
        .unwrap();

    // POLL PATH, no carrier timestamp: the poll clock is the fallback.
    let fallback = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("usps", "9400111899223817428490", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();
    let fallback_poll = t0 + chrono::Duration::hours(4);
    store
        .apply_carrier_track(
            acct,
            fallback,
            &CarrierTrack {
                status: Some(ShipmentStatus::Delivered),
                carrier_status_raw: "Delivered".into(),
                eta: None,
                delivered_at: None,
            },
            fallback_poll,
        )
        .unwrap();

    // EMAIL PATH: a delivered email stamps it too.
    let emailed = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("fedex", "123456789012", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();
    let email_stamp = t0 + chrono::Duration::hours(5);
    store
        .upsert_shipment(
            acct,
            mid,
            &shipped("fedex", "123456789012", ShipmentStatus::Delivered),
            email_stamp,
        )
        .unwrap();

    let stamped = |id: i64| {
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap()
            .delivered_at
    };
    assert_eq!(stamped(polled), Some(carrier_stamp));
    assert_eq!(stamped(fallback), Some(fallback_poll));
    assert_eq!(stamped(emailed), Some(email_stamp));

    // A later poll re-reporting the delivery must not move the stamp.
    store
        .apply_carrier_track(
            acct,
            polled,
            &CarrierTrack {
                status: Some(ShipmentStatus::Delivered),
                carrier_status_raw: "Delivered, Front Door".into(),
                eta: None,
                delivered_at: Some(t0 + chrono::Duration::hours(9)),
            },
            t0 + chrono::Duration::hours(9),
        )
        .unwrap();
    assert_eq!(
        stamped(polled),
        Some(carrier_stamp),
        "the first stamp holds"
    );
}

#[test]
fn a_shipment_carries_no_poll_state_until_it_is_polled() {
    use crate::triage::ShipmentStatus;
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Shipped),
            Utc::now(),
        )
        .unwrap();
    let s = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap()
        .remove(0);
    assert_eq!(s.carrier_status_raw, None);
    assert_eq!(s.eta, None);
    assert_eq!(s.delivered_at, None);
    assert_eq!(s.last_polled_at, None, "NULL means never polled");
}

// ---- read-side suppression of capped ambiguous rows ---------------------

/// Drive a shipment's `poll_failures` to `n` with permanent poll failures.
fn fail_polls(store: &SqliteStore, acct: AccountId, shipment_id: i64, n: u32) {
    for _ in 0..n {
        store
            .record_poll_outcome(acct, shipment_id, Utc::now(), true)
            .unwrap();
    }
}

#[test]
fn a_capped_ambiguous_row_is_suppressed_but_a_prefixed_one_is_not() {
    use crate::triage::ShipmentStatus;
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    // Same failure history, different SHAPES: only the bare digit-run could be
    // an eBay item id, so only it is hideable.
    let phantom = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("fedex", "123456789012", ShipmentStatus::Shipped),
            Utc::now(),
        )
        .unwrap();
    let real = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Shipped),
            Utc::now(),
        )
        .unwrap();
    fail_polls(&store, acct, phantom, 5);
    fail_polls(&store, acct, real, 5);

    let listed = store.list_shipments(acct, false, suppress_at(5)).unwrap();
    let ids: Vec<i64> = listed.iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![real], "only the ambiguous shape is suppressed");
    assert_eq!(
        listed[0].poll_failures, 5,
        "the counter is on the wire type"
    );

    // SUPPRESSION IS A READ FILTER, not a delete: the row is still there for a
    // repair pass to fix or remove.
    assert_eq!(
        store
            .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .len(),
        2,
        "the suppressed row is still stored"
    );
}

#[test]
fn an_ambiguous_row_below_the_cap_still_lists() {
    use crate::triage::ShipmentStatus;
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let sid = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("fedex", "123456789012", ShipmentStatus::Shipped),
            Utc::now(),
        )
        .unwrap();
    fail_polls(&store, acct, sid, 4);

    let listed = store.list_shipments(acct, false, suppress_at(5)).unwrap();
    assert_eq!(listed.len(), 1, "cap-1 failures is not yet a phantom");
    assert_eq!(listed[0].poll_failures, 4);
}

#[test]
fn a_successful_poll_unsuppresses_an_ambiguous_row() {
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let sid = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("fedex", "123456789012", ShipmentStatus::Shipped),
            Utc::now(),
        )
        .unwrap();
    fail_polls(&store, acct, sid, 5);
    assert!(
        store
            .list_shipments(acct, false, suppress_at(5))
            .unwrap()
            .is_empty(),
        "capped out"
    );

    // The carrier acknowledging the number is proof it was real all along, and
    // the success zeroes `poll_failures` — so the row comes straight back.
    store
        .apply_carrier_track(
            acct,
            sid,
            &CarrierTrack {
                status: Some(ShipmentStatus::OutForDelivery),
                carrier_status_raw: "Out For Delivery".into(),
                eta: None,
                delivered_at: None,
            },
            Utc::now(),
        )
        .unwrap();
    let listed = store.list_shipments(acct, false, suppress_at(5)).unwrap();
    assert_eq!(listed.len(), 1, "a successful poll brings the row back");
    assert_eq!(listed[0].poll_failures, 0);
}

// ---- staleness + user clear --------------------------------------------

/// One en-route row whose `last_update` is `age_days` old, plus its id.
fn aged_shipment(store: &SqliteStore, acct: AccountId, number: &str, age_days: i64) -> i64 {
    let mid = store
        .upsert_message(&triaged(acct, &format!("g-{number}"), "t-stale").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", number, crate::triage::ShipmentStatus::Shipped),
            Utc::now() - chrono::Duration::days(age_days),
        )
        .unwrap()
}

/// `last_update` advances ONLY on a user-visible change, so "older than N days"
/// is literally "nothing has happened to this package in N days" — which is what
/// the timeout is for. 0 turns the whole filter off.
#[test]
fn a_shipment_goes_stale_after_the_window_and_zero_disables_it() {
    let (store, acct) = store();
    let old = aged_shipment(&store, acct, "1Z999AA10123456784", 8);
    let recent = aged_shipment(&store, acct, "1Z999AA10123456785", 6);

    let listed = store.list_shipments(acct, false, stale_after(7)).unwrap();
    let ids: Vec<i64> = listed.iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![recent], "8 days out is hidden, 6 days out is not");

    assert_eq!(
        store
            .list_shipments(acct, false, stale_after(0))
            .unwrap()
            .len(),
        2,
        "stale_after_days = 0 disables the filter entirely"
    );
    assert_eq!(
        store
            .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .len(),
        2,
        "and the default test policy hides nothing either"
    );

    // HIDDEN IS NOT RETIRED: the stale row is still in the poll queue, because a
    // poll is exactly what would bring it back.
    assert!(
        store
            .list_pollable_shipments(acct, Utc::now() - chrono::Duration::days(45), 5)
            .unwrap()
            .iter()
            .any(|s| s.id == old),
        "a stale row keeps being polled"
    );
}

/// The clear, and the whole revival design: there is no un-clear call anywhere in
/// this test, only an update that moves `last_update` past the stamp.
#[test]
fn a_cleared_shipment_hides_until_a_poll_actually_moves_it() {
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now() - chrono::Duration::hours(3);
    let sid = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();
    // One real poll first, so the row already carries the carrier's words and a
    // repeat of the same answer is genuinely a no-change poll.
    let in_transit = CarrierTrack {
        status: Some(ShipmentStatus::Shipped),
        carrier_status_raw: "In Transit".into(),
        eta: None,
        delivered_at: None,
    };
    store
        .apply_carrier_track(acct, sid, &in_transit, t0 + chrono::Duration::hours(1))
        .unwrap();

    let cleared_at = t0 + chrono::Duration::hours(2);
    assert!(store.clear_shipment(acct, sid, cleared_at).unwrap());
    assert!(
        store
            .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .is_empty(),
        "a cleared row leaves the listing"
    );
    // STILL POLLED. This is the load-bearing half: filtering the poll queue on
    // `cleared_at` would make the clear permanent.
    assert!(
        store
            .list_pollable_shipments(acct, t0 - chrono::Duration::days(45), 5)
            .unwrap()
            .iter()
            .any(|s| s.id == sid),
        "a cleared row keeps being polled"
    );

    // A poll that CONFIRMS what the row already says moves nothing user-visible,
    // so `last_update` does not advance and the row stays hidden.
    store
        .apply_carrier_track(
            acct,
            sid,
            &in_transit,
            cleared_at + chrono::Duration::minutes(30),
        )
        .unwrap();
    assert!(
        store
            .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .is_empty(),
        "a poll that changed nothing must not un-hide the row"
    );

    // A poll that MOVES it does, with no un-clear call anywhere: the comparison
    // in the listing is the whole revival mechanism.
    store
        .apply_carrier_track(
            acct,
            sid,
            &CarrierTrack {
                status: Some(ShipmentStatus::OutForDelivery),
                carrier_status_raw: "Out For Delivery".into(),
                eta: None,
                delivered_at: None,
            },
            cleared_at + chrono::Duration::minutes(45),
        )
        .unwrap();
    let listed = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(listed.len(), 1, "an update revives the row by itself");
    assert_eq!(listed[0].status, "out_for_delivery");
}

/// The other revival path: a new email the state machine ACCEPTS advances
/// `last_update` too, so it un-hides exactly the same way a poll does.
#[test]
fn a_new_accepted_email_revives_a_cleared_shipment() {
    use crate::triage::ShipmentStatus;
    let (store, acct) = store();
    let first = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now() - chrono::Duration::hours(2);
    let sid = store
        .upsert_shipment(
            acct,
            first,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();
    store.clear_shipment(acct, sid, t0).unwrap();
    assert!(
        store
            .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .is_empty()
    );

    let second = store
        .upsert_message(&triaged(acct, "g2", "t1").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            second,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::OutForDelivery),
            t0 + chrono::Duration::hours(1),
        )
        .unwrap();

    let listed = store
        .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(listed.len(), 1, "the ship notice brings the package back");
    assert_eq!(listed[0].id, sid, "the same row, not a second one");
}

/// Idempotence, restamping, and the unknown-id answer the endpoint's 404 rests on.
#[test]
fn clearing_is_idempotent_restamps_and_reports_an_unknown_id() {
    use crate::triage::ShipmentStatus;
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    let t0 = Utc::now() - chrono::Duration::hours(3);
    let sid = store
        .upsert_shipment(
            acct,
            mid,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::Shipped),
            t0,
        )
        .unwrap();

    assert!(store.clear_shipment(acct, sid, t0).unwrap());
    assert!(
        store.clear_shipment(acct, sid, t0).unwrap(),
        "clearing twice is a no-op success, not an error"
    );
    assert!(
        !store.clear_shipment(acct, sid + 999, Utc::now()).unwrap(),
        "an unknown id is false, never an error — the door turns this into a 404"
    );

    // RESTAMPING MATTERS: revive the row, then clear it again. The second clear
    // must hide it against the LATER stamp, which a "only if NULL" write would
    // not do.
    let second = store
        .upsert_message(&triaged(acct, "g2", "t1").msg())
        .unwrap();
    let moved = t0 + chrono::Duration::hours(1);
    store
        .upsert_shipment(
            acct,
            second,
            &shipped("ups", "1Z999AA10123456784", ShipmentStatus::OutForDelivery),
            moved,
        )
        .unwrap();
    assert_eq!(
        store
            .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .len(),
        1
    );
    assert!(store.clear_shipment(acct, sid, moved).unwrap());
    assert!(
        store
            .list_shipments(acct, false, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .is_empty(),
        "the re-clear restamped and hid the revived row again"
    );
}

// ---- one-shot re-detect cleanup ----------------------------------------

/// A shipment row on `(carrier, number)` whose feeder is `msg` — the shape the
/// re-detect pass re-judges.
fn shipment_over_mail(
    store: &SqliteStore,
    acct: AccountId,
    msg: &NewMessage,
    carrier: &str,
    number: &str,
) -> i64 {
    let mid = store.upsert_message(msg).unwrap();
    store
        .upsert_shipment(
            acct,
            mid,
            &shipped(carrier, number, crate::triage::ShipmentStatus::Shipped),
            Utc::now(),
        )
        .unwrap()
}

#[test]
fn redetect_deletes_the_ebay_phantom_and_keeps_the_real_row() {
    let (store, acct) = store();
    // The live bug: an eBay item id minted as a "fedex" shipment.
    let phantom = shipment_over_mail(
        &store,
        acct,
        &triaged(acct, "g-ebay", "t-ebay")
            .from("ebay@ebay.com")
            .subject("Your package is now with its carrier!")
            .body(
                "Your package is now with its carrier! Shipping via USPS. \
                 See https://www.ebay.com/itm/123456789012, item 234567890123.",
            )
            .msg(),
        "fedex",
        "123456789012",
    );
    // A real UPS notice, which the tightened detector still yields.
    let real = shipment_over_mail(
        &store,
        acct,
        &triaged(acct, "g-ups", "t-ups")
            .from("mcinfo@ups.com")
            .subject("Your UPS package has shipped")
            .body("Tracking number: 1Z999AA10123456784. Track your package.")
            .msg(),
        "ups",
        "1Z999AA10123456784",
    );

    assert_eq!(store.shipments_redetect_cleanup(acct).unwrap(), 1);
    let ids: Vec<i64> = store
        .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec![real], "only the phantom goes");
    assert!(!ids.contains(&phantom));

    // IDEMPOTENT: a second pass over the repaired store deletes nothing.
    assert_eq!(store.shipments_redetect_cleanup(acct).unwrap(), 0);
    assert_eq!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn redetect_deletes_a_row_whose_feeder_now_yields_a_different_number() {
    let (store, acct) = store();
    // The mail yields the IMpb; the stored row is one of its item ids, which the
    // old first-match-only scan had picked instead.
    shipment_over_mail(
        &store,
        acct,
        &triaged(acct, "g-ebay", "t-ebay")
            .from("ebay@ebay.com")
            .subject("Your package is now with its carrier!")
            .body("Item 234567890123 shipped via USPS. Tracking number 9400111899223817428490.")
            .msg(),
        "fedex",
        "234567890123",
    );
    assert_eq!(store.shipments_redetect_cleanup(acct).unwrap(), 1);
    assert!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .is_empty(),
        "a row the feeder no longer yields is deleted"
    );
}

#[test]
fn redetect_leaves_pointerless_rows_alone() {
    use crate::triage::ShipmentStatus;
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g1", "t1").msg())
        .unwrap();
    // An ambiguous number that would NOT re-detect from any mail — but with no
    // feeder message there is no evidence to judge it on, so it stays.
    store
        .upsert_shipment(
            acct,
            mid,
            &shipped("fedex", "123456789012", ShipmentStatus::Shipped),
            Utc::now(),
        )
        .unwrap();
    store
        .lock()
        .unwrap()
        .execute("UPDATE shipments SET last_message_id = NULL", [])
        .unwrap();

    assert_eq!(store.shipments_redetect_cleanup(acct).unwrap(), 0);
    assert_eq!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .len(),
        1,
        "no feeder message, no judgement"
    );
}

// ---- the shipments EXTRACTOR apply (identity merge rules) ---------------

use crate::triage::extract::shipments::ShipmentsApplied;

/// A message + triage row queued for the shipments extractor.
fn ship_queued_msg(store: &SqliteStore, acct: AccountId, gmail: &str, thread: &str) -> i64 {
    triaged_row(acct, gmail, thread, None, false, Sensitivity::Normal)
        .ship_extract(true)
        .ingest(store)
}

/// The same, from a named SENDER — the merchant namespace an order reference
/// lives in is that sender's registrable domain.
fn ship_queued_from(
    store: &SqliteStore,
    acct: AccountId,
    gmail: &str,
    thread: &str,
    from: &str,
) -> i64 {
    triaged_row(acct, gmail, thread, None, false, Sensitivity::Normal)
        .from(from)
        .ship_extract(true)
        .ingest(store)
}

/// A NEGATIVE extractor verdict for `(mid, thread)`; the positive helpers below
/// build on it, so every test starts from the same explicit baseline.
fn ship_verdict(acct: AccountId, mid: i64, thread: &str) -> ShipmentsApplied {
    ShipmentsApplied {
        message_id: mid,
        account_id: acct,
        thread_id: thread.into(),
        is_shipment: false,
        tracking_number: None,
        order_ref: None,
        item_name: None,
        carrier: "unknown".into(),
        status: None,
        received_at: Utc::now(),
        extractor_model_used: "claude-haiku-4-5".into(),
    }
}

/// A shipment row exactly as the REGEX detector would have written it.
fn detected(carrier: &str, number: &str, item_name: &str) -> crate::triage::ShipmentInfo {
    crate::triage::ShipmentInfo {
        carrier: carrier.into(),
        tracking_number: number.into(),
        item_name: item_name.into(),
        status: crate::triage::ShipmentStatus::Shipped,
        tracking_url: None,
    }
}

/// `(tracking_number, item_name, order_ref, status)` for every shipment row,
/// ordered by id — `order_ref` is not on the wire type.
fn shipment_rows(
    store: &SqliteStore,
    acct: AccountId,
) -> Vec<(String, String, Option<String>, String)> {
    let conn = store.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT tracking_number, item_name, order_ref, status FROM shipments
             WHERE account_id=?1 ORDER BY id",
        )
        .unwrap();
    stmt.query_map(params![acct], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })
    .unwrap()
    .collect::<std::result::Result<Vec<_>, _>>()
    .unwrap()
}

/// `(order_ref, item_name, last_message_id)` for every staged order.
fn staged_orders(store: &SqliteStore, acct: AccountId) -> Vec<(String, String, Option<i64>)> {
    let conn = store.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT order_ref, item_name, last_message_id FROM shipment_orders
             WHERE account_id=?1 ORDER BY id",
        )
        .unwrap();
    stmt.query_map(params![acct], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn ship_marker(store: &SqliteStore, mid: i64) -> Option<String> {
    store
        .lock()
        .unwrap()
        .query_row(
            "SELECT ship_extract_model FROM triage WHERE message_id=?1",
            params![mid],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn ship_extract_apply_replaces_the_ebay_phantom_with_the_real_impb() {
    // THE live bug end-to-end: the regex detector minted a 12-digit eBay ITEM id
    // as a shipment; the model reads the same mail, names the real IMpb number
    // and the item, and the phantom goes.
    let (store, acct) = store();
    let mid = ship_queued_msg(&store, acct, "g-ebay", "t-ebay");
    store
        .upsert_shipment(
            acct,
            mid,
            &detected("fedex", "123456789012", "package now with its carrier!"),
            Utc::now(),
        )
        .unwrap();

    let wrote = store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("9400111899223817428490".into()),
            order_ref: Some("234567890123".into()),
            item_name: Some("Double Take mirror".into()),
            carrier: "usps".into(),
            status: Some(crate::triage::ShipmentStatus::Shipped),
            ..ship_verdict(acct, mid, "t-ebay")
        })
        .unwrap();
    assert!(wrote);

    let rows = shipment_rows(&store, acct);
    assert_eq!(rows.len(), 1, "the phantom is gone: {rows:?}");
    assert_eq!(rows[0].0, "9400111899223817428490");
    assert_eq!(rows[0].1, "Double Take mirror");
    assert_eq!(rows[0].2.as_deref(), Some("234567890123"));

    // The real row carries the carrier's URL, and the row leaves the queue.
    let listed = store
        .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert!(
        listed[0]
            .tracking_url
            .as_deref()
            .unwrap()
            .contains("tools.usps.com")
    );
    assert_eq!(
        ship_marker(&store, mid).as_deref(),
        Some("claude-haiku-4-5")
    );
    assert!(store.ship_extract_queue(acct, 10).unwrap().is_empty());
}

#[test]
fn ship_extract_apply_stages_an_order_then_promotes_it_with_its_name() {
    // The order confirmation lands days before the ship notice, so the purchase
    // is staged under the retailer's reference and promoted when a number shows.
    let (store, acct) = store();
    let order_msg = ship_queued_msg(&store, acct, "g-ord", "t-ord");
    let wrote = store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            order_ref: Some("112-3456789-1234567".into()),
            item_name: Some("Anker USB-C charger".into()),
            ..ship_verdict(acct, order_msg, "t-ord")
        })
        .unwrap();
    assert!(wrote, "staging a purchase is a write");
    assert_eq!(
        staged_orders(&store, acct),
        vec![(
            "112-3456789-1234567".to_string(),
            "Anker USB-C charger".to_string(),
            Some(order_msg)
        )]
    );
    assert!(
        shipment_rows(&store, acct).is_empty(),
        "no tracking number, no shipments row"
    );

    // The ship notice: same order reference, a real number, and NO item name of
    // its own — the staged name is the only one anyone has.
    let ship_msg = ship_queued_msg(&store, acct, "g-shipnote", "t-ord");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            order_ref: Some("112-3456789-1234567".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, ship_msg, "t-ord")
        })
        .unwrap();

    let rows = shipment_rows(&store, acct);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "1Z999AA10123456784");
    assert_eq!(
        rows[0].1, "Anker USB-C charger",
        "the staged name is donated"
    );
    assert_eq!(rows[0].2.as_deref(), Some("112-3456789-1234567"));
    assert!(
        staged_orders(&store, acct).is_empty(),
        "the promoted staging row is deleted"
    );
}

#[test]
fn ship_extract_apply_negative_verdict_retires_only_the_phantom_it_fed() {
    // A false verdict retires the ambiguous row THIS message minted, and nothing
    // else: another message's package is none of its business.
    let (store, acct) = store();
    let mine = ship_queued_msg(&store, acct, "g-mine", "t1");
    let theirs = ship_queued_msg(&store, acct, "g-theirs", "t2");
    store
        .upsert_shipment(
            acct,
            mine,
            &detected("fedex", "123456789012", ""),
            Utc::now(),
        )
        .unwrap();
    store
        .upsert_shipment(
            acct,
            theirs,
            &detected("ups", "1Z999AA10123456784", "Headphones"),
            Utc::now(),
        )
        .unwrap();

    let wrote = store
        .shipments_extract_apply(&ship_verdict(acct, mine, "t1"))
        .unwrap();
    assert!(!wrote, "a negative verdict writes no shipment row");

    let rows = shipment_rows(&store, acct);
    assert_eq!(rows.len(), 1, "only my phantom goes: {rows:?}");
    assert_eq!(rows[0].0, "1Z999AA10123456784");
}

#[test]
fn ship_extract_apply_negative_verdict_spares_a_real_number_it_fed() {
    // THE FALSE-NEGATIVE GUARD: the model can be wrong, and a `1Z…` number is
    // self-identifying — no retailer id impersonates it — so the row stays even
    // though this very message fed it.
    let (store, acct) = store();
    let mid = ship_queued_msg(&store, acct, "g-ups", "t1");
    for number in [
        "1Z999AA10123456784",
        "TBA303392911000",
        "9400111899223817428490",
    ] {
        store
            .upsert_shipment(acct, mid, &detected("ups", number, ""), Utc::now())
            .unwrap();
    }

    store
        .shipments_extract_apply(&ship_verdict(acct, mid, "t1"))
        .unwrap();
    assert_eq!(
        shipment_rows(&store, acct).len(),
        3,
        "self-identifying shapes survive a false verdict"
    );
}

#[test]
fn ship_extract_apply_names_a_lone_thread_shipment() {
    // NO IDENTITY AT ALL: the only safe inference is "this thread's one package
    // is the one being named".
    let (store, acct) = store();
    let feeder = ship_queued_msg(&store, acct, "g-feed", "t-solo");
    store
        .upsert_shipment(
            acct,
            feeder,
            &detected("ups", "1Z999AA10123456784", ""),
            Utc::now(),
        )
        .unwrap();

    let follow_up = ship_queued_msg(&store, acct, "g-follow", "t-solo");
    let wrote = store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            item_name: Some("Double Take mirror".into()),
            ..ship_verdict(acct, follow_up, "t-solo")
        })
        .unwrap();
    assert!(wrote);
    assert_eq!(shipment_rows(&store, acct)[0].1, "Double Take mirror");
}

#[test]
fn ship_extract_apply_leaves_a_two_shipment_thread_unnamed() {
    // Two packages in one thread and no identity to tell them apart: naming
    // either would be a coin flip, so nothing is written.
    let (store, acct) = store();
    let a = ship_queued_msg(&store, acct, "g-a", "t-pair");
    let b = ship_queued_msg(&store, acct, "g-b", "t-pair");
    store
        .upsert_shipment(
            acct,
            a,
            &detected("ups", "1Z999AA10123456784", ""),
            Utc::now(),
        )
        .unwrap();
    store
        .upsert_shipment(
            acct,
            b,
            &detected("ups", "1Z12345E0205271688", ""),
            Utc::now(),
        )
        .unwrap();

    let third = ship_queued_msg(&store, acct, "g-c", "t-pair");
    let wrote = store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            item_name: Some("Double Take mirror".into()),
            ..ship_verdict(acct, third, "t-pair")
        })
        .unwrap();
    assert!(!wrote, "an ambiguous thread is left alone");
    assert!(
        shipment_rows(&store, acct).iter().all(|r| r.1.is_empty()),
        "neither row may be named"
    );
}

#[test]
fn ship_extract_apply_never_walks_a_delivered_row_back() {
    // The extractor's status still flows through `ShipmentStatus::merge`, so a
    // late "shipped" mail cannot un-deliver a package.
    let (store, acct) = store();
    let mid = ship_queued_msg(&store, acct, "g-del", "t1");
    store
        .upsert_shipment(
            acct,
            mid,
            &crate::triage::ShipmentInfo {
                status: crate::triage::ShipmentStatus::Delivered,
                ..detected("ups", "1Z999AA10123456784", "")
            },
            Utc::now(),
        )
        .unwrap();

    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            carrier: "ups".into(),
            status: Some(crate::triage::ShipmentStatus::Shipped),
            ..ship_verdict(acct, mid, "t1")
        })
        .unwrap();
    assert_eq!(shipment_rows(&store, acct)[0].3, "delivered");
}

#[test]
fn ship_extract_apply_writes_nothing_for_a_row_sealed_mid_pass() {
    // TOCTOU: the queue handed out a normal row and the user sealed it while the
    // model was thinking. The guarded marker matches nothing, so nothing derived
    // from sealed mail may land.
    let (store, acct) = store();
    let mid = ship_queued_msg(&store, acct, "g-seal", "t1");
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE triage SET sensitivity='sealed' WHERE message_id=?1",
            params![mid],
        )
        .unwrap();

    let wrote = store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            order_ref: Some("112-3456789-1234567".into()),
            item_name: Some("Anker USB-C charger".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, mid, "t1")
        })
        .unwrap();
    assert!(!wrote);
    assert!(shipment_rows(&store, acct).is_empty(), "no shipment row");
    assert!(staged_orders(&store, acct).is_empty(), "no staged order");
    assert_eq!(
        ship_marker(&store, mid).as_deref(),
        Some("pending"),
        "the marker itself is guarded too"
    );
}

#[test]
fn ship_extract_apply_item_name_beats_a_longer_regex_name() {
    // `upsert_shipment_conn`'s longer-name-wins heuristic picks between two REGEX
    // guesses; against the extractor it would keep subject-line junk purely for
    // being longer, so the extractor's name is written over the top.
    let (store, acct) = store();
    let mid = ship_queued_msg(&store, acct, "g-junk", "t1");
    store
        .upsert_shipment(
            acct,
            mid,
            &detected(
                "ups",
                "1Z999AA10123456784",
                "package is now with its carrier and on its way to you",
            ),
            Utc::now(),
        )
        .unwrap();

    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            item_name: Some("Anker USB-C charger".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, mid, "t1")
        })
        .unwrap();
    assert_eq!(shipment_rows(&store, acct)[0].1, "Anker USB-C charger");
    assert_eq!(
        name_and_source(&store, acct).1,
        "llm",
        "and the row now remembers WHICH MECHANISM named it"
    );
}

// ---- item-name provenance: which MECHANISM named the package ------------

/// The one shipment row's `(item_name, item_name_source)`.
fn name_and_source(store: &SqliteStore, acct: AccountId) -> (String, String) {
    store
        .lock()
        .unwrap()
        .query_row(
            "SELECT item_name, item_name_source FROM shipments WHERE account_id=?1",
            params![acct],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[test]
fn a_regex_name_never_overwrites_an_extractor_name() {
    // The mirror of `ship_extract_apply_item_name_beats_a_longer_regex_name`:
    // once the model has named the goods, a LATER email's subject-lifted phrase
    // cannot take the card back, however much longer it is.
    let (store, acct) = store();
    let mid = ship_queued_msg(&store, acct, "g-1", "t1");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            item_name: Some("Anker USB-C charger".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, mid, "t1")
        })
        .unwrap();
    assert_eq!(
        name_and_source(&store, acct),
        ("Anker USB-C charger".into(), "llm".into())
    );

    let later = store
        .upsert_message(&triaged(acct, "g-2", "t1").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            later,
            &detected(
                "ups",
                "1Z999AA10123456784",
                "Wireless Noise Cancelling Headphones Over Ear",
            ),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        name_and_source(&store, acct),
        ("Anker USB-C charger".into(), "llm".into()),
        "a longer REGEX name does not outrank the extractor"
    );
}

#[test]
fn a_longer_extractor_name_wins_within_the_llm_source_and_a_shorter_one_loses() {
    // Longer-wins still applies BETWEEN two model answers — it is only the
    // cross-source comparison the provenance column exists to stop.
    let (store, acct) = store();
    let first = ship_queued_msg(&store, acct, "g-1", "t1");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            item_name: Some("USB-C charger".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, first, "t1")
        })
        .unwrap();

    let second = ship_queued_msg(&store, acct, "g-2", "t1");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            item_name: Some("Anker 735 USB-C charger".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, second, "t1")
        })
        .unwrap();
    assert_eq!(name_and_source(&store, acct).0, "Anker 735 USB-C charger");

    let third = ship_queued_msg(&store, acct, "g-3", "t1");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            item_name: Some("charger".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, third, "t1")
        })
        .unwrap();
    assert_eq!(
        name_and_source(&store, acct),
        ("Anker 735 USB-C charger".into(), "llm".into()),
        "a vaguer model answer does not walk the name back"
    );
}

#[test]
fn a_longer_regex_name_still_beats_a_shorter_regex_name() {
    // PINNED: within the regex source the original longer-wins heuristic is
    // untouched — the provenance column only gates the cross-source case.
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g-1", "t1").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            mid,
            &detected("ups", "1Z999AA10123456784", "Headphones"),
            Utc::now(),
        )
        .unwrap();
    store
        .upsert_shipment(
            acct,
            mid,
            &detected("ups", "1Z999AA10123456784", "Wireless Headphones"),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        name_and_source(&store, acct),
        ("Wireless Headphones".into(), "regex".into())
    );
    // ... and a shorter one does not walk it back.
    store
        .upsert_shipment(
            acct,
            mid,
            &detected("ups", "1Z999AA10123456784", "Cans"),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(name_and_source(&store, acct).0, "Wireless Headphones");
}

#[test]
fn a_stored_filler_name_heals_to_empty_on_the_next_email() {
    // THE LIVE ROWS: four packages read "package now with its carrier!", written
    // by a strip that did not yet know that phrase was filler. An empty
    // extraction normally cannot overwrite a non-empty name — which would leave
    // them junk forever — so a stored name TODAY's strip refuses is treated as
    // no name at all, and the next email clears it. The client then shows its
    // own "Package via <carrier>" label.
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g-1", "t1").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            mid,
            &detected("ups", "1Z999AA10123456784", "package now with its carrier!"),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        name_and_source(&store, acct).0,
        "package now with its carrier!"
    );

    // A later delivery notice whose subject yields nothing: the junk goes, and
    // its provenance pointer goes with it — nobody donated an absence.
    let later = store
        .upsert_message(&triaged(acct, "g-2", "t1").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            later,
            &detected("ups", "1Z999AA10123456784", ""),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        name_and_source(&store, acct),
        (String::new(), "regex".into()),
        "the junk name is cleared, not preserved for being longer"
    );
    let name_msg: Option<i64> = store
        .lock()
        .unwrap()
        .query_row(
            "SELECT item_name_msg FROM shipments WHERE account_id=?1",
            params![acct],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name_msg, None, "no message donated the empty name");
}

#[test]
fn a_stored_filler_name_yields_to_a_shorter_real_one() {
    // Healing is not only about clearing: junk loses to a real name even when
    // longer-wins would have kept it.
    let (store, acct) = store();
    let mid = store
        .upsert_message(&triaged(acct, "g-1", "t1").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            mid,
            &detected("ups", "1Z999AA10123456784", "package now with its carrier!"),
            Utc::now(),
        )
        .unwrap();
    store
        .upsert_shipment(
            acct,
            mid,
            &detected("ups", "1Z999AA10123456784", "Cat bed"),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        name_and_source(&store, acct),
        ("Cat bed".into(), "regex".into())
    );
}

#[test]
fn an_extractor_name_is_never_healed_away_by_a_later_email() {
    // Healing is scoped to REGEX names. A name stamped 'llm' is the model's
    // considered answer about the goods, so even one the subject strip would
    // refuse survives an empty regex extraction — the healing rule is about
    // undoing the DETECTOR's old mistakes, not overruling the extractor.
    let (store, acct) = store();
    let mid = ship_queued_msg(&store, acct, "g-1", "t1");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            item_name: Some("package now with its carrier!".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, mid, "t1")
        })
        .unwrap();
    let later = store
        .upsert_message(&triaged(acct, "g-2", "t1").msg())
        .unwrap();
    store
        .upsert_shipment(
            acct,
            later,
            &detected("ups", "1Z999AA10123456784", ""),
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        name_and_source(&store, acct),
        ("package now with its carrier!".into(), "llm".into())
    );
}

#[test]
fn ship_extract_apply_order_only_names_the_shipment_that_already_landed() {
    // The ship notice arrived FIRST and recorded the reference; a later order
    // mail carrying the item name has nothing but that name to contribute — and
    // must not re-stage a purchase that is already tracked.
    let (store, acct) = store();
    let ship_msg = ship_queued_msg(&store, acct, "g-ship", "t-a");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            order_ref: Some("ORD-1".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, ship_msg, "t-a")
        })
        .unwrap();

    let order_msg = ship_queued_msg(&store, acct, "g-order", "t-b");
    let wrote = store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            order_ref: Some("ORD-1".into()),
            item_name: Some("Anker USB-C charger".into()),
            ..ship_verdict(acct, order_msg, "t-b")
        })
        .unwrap();
    assert!(wrote);
    assert_eq!(shipment_rows(&store, acct)[0].1, "Anker USB-C charger");
    assert!(
        staged_orders(&store, acct).is_empty(),
        "a tracked purchase is never re-staged"
    );
}

// ---- the reaping bounds: provenance, carrier evidence, positive evidence --

#[test]
fn a_second_email_moving_the_pointer_does_not_make_the_first_rows_package_a_phantom() {
    // `last_message_id` MOVES to the newest mail that advances a row, so keying
    // the reaping on it puts weeks-old packages in a later mail's blast radius.
    // Day 1: a real FedEx number (12 digits — an AMBIGUOUS shape, so the shape
    // gate does not save it). Day 4: a second notice covering both packages
    // re-upserts it, moving the pointer, and the extractor names the OTHER one.
    let (store, acct) = store();
    let day1 = ship_queued_msg(&store, acct, "g-day1", "t-ship");
    store
        .upsert_shipment(
            acct,
            day1,
            &detected("fedex", "123456789012", "Standing desk"),
            Utc::now(),
        )
        .unwrap();

    let day4 = ship_queued_msg(&store, acct, "g-day4", "t-ship");
    store
        .upsert_shipment(
            acct,
            day4,
            &detected("fedex", "123456789012", "Standing desk"),
            Utc::now(),
        )
        .unwrap();

    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("987654321098".into()),
            carrier: "fedex".into(),
            ..ship_verdict(acct, day4, "t-ship")
        })
        .unwrap();

    let numbers: Vec<String> = shipment_rows(&store, acct)
        .into_iter()
        .map(|r| r.0)
        .collect();
    assert!(
        numbers.contains(&"123456789012".to_string()),
        "day 1's package is not day 4's phantom: {numbers:?}"
    );
    assert!(numbers.contains(&"987654321098".to_string()));
}

#[test]
fn a_carrier_confirmed_row_is_never_reaped_as_a_phantom() {
    // A number a carrier ANSWERED about is a real package whatever the model now
    // says about the mail — even a flat "not a shipment" verdict on the very
    // message that created the row.
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();
    let mid = ship_queued_msg(&store, acct, "g-fedex", "t1");
    let sid = store
        .upsert_shipment(
            acct,
            mid,
            &detected("fedex", "123456789012", ""),
            Utc::now(),
        )
        .unwrap();
    store
        .apply_carrier_track(
            acct,
            sid,
            &CarrierTrack {
                status: Some(ShipmentStatus::OutForDelivery),
                carrier_status_raw: "Out For Delivery".into(),
                eta: None,
                delivered_at: None,
            },
            Utc::now(),
        )
        .unwrap();

    store
        .shipments_extract_apply(&ship_verdict(acct, mid, "t1"))
        .unwrap();
    assert_eq!(
        shipment_rows(&store, acct).len(),
        1,
        "carrier evidence outranks a model verdict"
    );
}

#[test]
fn an_extraction_with_no_tracking_number_deletes_nothing() {
    // ABSENCE OF EVIDENCE IS NOT EVIDENCE OF ABSENCE. "Shipped via FedEx, Order
    // #1042, Tracking number 123456789012" — the model puts the number in BOTH
    // fields and the extractor's contradiction rule drops it, leaving an
    // order-ref-only verdict. The row the detector minted is REAL, so neither
    // the order-ref branch nor the no-identity branch may touch it.
    let (store, acct) = store();
    let mid = ship_queued_msg(&store, acct, "g-fedex", "t-ship");
    store
        .upsert_shipment(
            acct,
            mid,
            &detected("fedex", "123456789012", ""),
            Utc::now(),
        )
        .unwrap();

    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            order_ref: Some("1042".into()),
            ..ship_verdict(acct, mid, "t-ship")
        })
        .unwrap();
    assert_eq!(
        shipment_rows(&store, acct).len(),
        1,
        "an order-ref-only verdict deletes nothing"
    );

    // And the same for a verdict carrying no identity at all.
    let follow_up = ship_queued_msg(&store, acct, "g-follow", "t-ship");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            ..ship_verdict(acct, follow_up, "t-ship")
        })
        .unwrap();
    assert_eq!(
        shipment_rows(&store, acct).len(),
        1,
        "a no-identity verdict deletes nothing"
    );
}

// ---- order references are merchant-scoped -------------------------------

#[test]
fn two_merchants_sharing_an_order_number_do_not_bind_to_one_package() {
    // "Order #1042" is unique only inside the shop that issued it. Without a
    // merchant namespace, shop B's confirmation renames shop A's in-flight
    // package — the user's desk becomes a cat bed.
    let (store, acct) = store();
    let a_ship = ship_queued_from(&store, acct, "g-a", "t-a", "orders@shopa.com");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            tracking_number: Some("1Z999AA10123456784".into()),
            order_ref: Some("1042".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, a_ship, "t-a")
        })
        .unwrap();

    let b_order = ship_queued_from(&store, acct, "g-b", "t-b", "orders@shopb.com");
    let wrote = store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            order_ref: Some("1042".into()),
            item_name: Some("Cat bed".into()),
            ..ship_verdict(acct, b_order, "t-b")
        })
        .unwrap();

    assert!(wrote, "shop B's purchase is staged under its own merchant");
    assert_eq!(
        shipment_rows(&store, acct)[0].1,
        "",
        "shop A's package keeps its own (absent) name"
    );
    assert_eq!(
        staged_orders(&store, acct),
        vec![("1042".to_string(), "Cat bed".to_string(), Some(b_order))],
        "shop B stages instead of donating"
    );
}

#[test]
fn an_ambiguous_order_ref_match_donates_to_neither_row() {
    // One order, two boxes: both packages carry reference 1042. A later order
    // mail naming the item cannot say WHICH box it means, and the previous
    // `query_row` silently took whichever row SQLite handed back first.
    let (store, acct) = store();
    for (gmail, number) in [
        ("g-box1", "1Z999AA10123456784"),
        ("g-box2", "1Z12345E0205271688"),
    ] {
        let mid = ship_queued_from(&store, acct, gmail, "t-split", "orders@shopa.com");
        store
            .shipments_extract_apply(&ShipmentsApplied {
                is_shipment: true,
                tracking_number: Some(number.into()),
                order_ref: Some("1042".into()),
                carrier: "ups".into(),
                ..ship_verdict(acct, mid, "t-split")
            })
            .unwrap();
    }

    let order_msg = ship_queued_from(&store, acct, "g-ord", "t-ord", "orders@shopa.com");
    let wrote = store
        .shipments_extract_apply(&ShipmentsApplied {
            is_shipment: true,
            order_ref: Some("1042".into()),
            item_name: Some("Anker USB-C charger".into()),
            ..ship_verdict(acct, order_msg, "t-ord")
        })
        .unwrap();

    assert!(!wrote, "an ambiguous reference writes nothing");
    assert!(
        shipment_rows(&store, acct).iter().all(|r| r.1.is_empty()),
        "neither box may be named"
    );
    assert!(
        staged_orders(&store, acct).is_empty(),
        "an already-tracked purchase is not re-staged either"
    );
}

// ---- the re-detect one-shot: evidence and atomicity ---------------------

/// The re-detect done-flag as the store records it.
fn redetect_flag(store: &SqliteStore, acct: AccountId) -> Option<String> {
    store
        .get_app_setting(acct, "shipments_redetect_v1")
        .unwrap()
}

/// A mail whose text yields NO tracking number at all, so any row hung off it
/// fails the re-detect keep test.
fn undetectable_mail(acct: AccountId, gmail: &str) -> TriagedBuilder {
    triaged(acct, gmail, &format!("t-{gmail}"))
        .from("ebay@ebay.com")
        .subject("Your package is now with its carrier!")
        .body("Your package is now with its carrier! See https://www.ebay.com/itm/123456789012.")
}

#[test]
fn the_redetect_one_shot_spares_extractor_and_carrier_evidence() {
    // The keep test is the REGEX detector, and extractor-written rows are
    // exactly what it cannot reproduce. So the one-shot judges only rows with no
    // evidence of their own: a carrier answer (or even a poll attempt) and an
    // order reference both put a row out of reach.
    use crate::triage::{CarrierTrack, ShipmentStatus};
    let (store, acct) = store();

    let plain = shipment_over_mail(
        &store,
        acct,
        &undetectable_mail(acct, "g-plain").msg(),
        "fedex",
        "123456789012",
    );
    let polled = shipment_over_mail(
        &store,
        acct,
        &undetectable_mail(acct, "g-polled").msg(),
        "fedex",
        "223456789012",
    );
    let ordered = shipment_over_mail(
        &store,
        acct,
        &undetectable_mail(acct, "g-ordered").msg(),
        "fedex",
        "323456789012",
    );
    store
        .apply_carrier_track(
            acct,
            polled,
            &CarrierTrack {
                status: Some(ShipmentStatus::Shipped),
                carrier_status_raw: "In Transit".into(),
                eta: None,
                delivered_at: None,
            },
            Utc::now(),
        )
        .unwrap();
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE shipments SET order_ref='1042', order_merchant='shopa.com' WHERE id=?1",
            params![ordered],
        )
        .unwrap();

    assert_eq!(
        store.shipments_redetect_cleanup(acct).unwrap(),
        1,
        "only the evidence-free phantom is reaped"
    );
    let surviving: Vec<i64> = store
        .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert!(!surviving.contains(&plain));
    assert!(surviving.contains(&polled), "carrier evidence is spared");
    assert!(surviving.contains(&ordered), "extractor evidence is spared");
}

#[test]
fn the_redetect_flag_and_its_deletions_commit_together() {
    // The pass CANNOT complete without being recorded: flag and deletions are
    // one transaction, and the store owns both. With the flag written by the
    // caller afterwards, a crash or an unwritable settings row meant a re-run —
    // and by then the extractor has written rows the regex cannot reproduce, so
    // the second pass would eat them.
    let (store, acct) = store();
    assert_eq!(redetect_flag(&store, acct), None);

    shipment_over_mail(
        &store,
        acct,
        &undetectable_mail(acct, "g-phantom").msg(),
        "fedex",
        "123456789012",
    );
    assert_eq!(store.shipments_redetect_cleanup(acct).unwrap(), 1);
    assert_eq!(
        redetect_flag(&store, acct).as_deref(),
        Some("done"),
        "the pass records itself in the same transaction as its deletions"
    );

    // An extractor-written row lands afterwards: one the regex will never yield.
    // The recorded completion is what stands between it and the reaper.
    let extracted = shipment_over_mail(
        &store,
        acct,
        &undetectable_mail(acct, "g-extracted").msg(),
        "fedex",
        "223456789012",
    );
    assert_eq!(
        store.shipments_redetect_cleanup(acct).unwrap(),
        0,
        "the one-shot is over"
    );
    assert!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .iter()
            .any(|s| s.id == extracted),
        "a recorded pass never runs again"
    );
}

#[test]
fn the_redetect_one_shot_records_itself_even_when_it_deletes_nothing() {
    // A pass with nothing to reap still HAPPENED. Leaving the flag unwritten
    // would arm the reaper for the next start, over a store the extractor has
    // been writing to since.
    let (store, acct) = store();
    assert_eq!(store.shipments_redetect_cleanup(acct).unwrap(), 0);
    assert_eq!(redetect_flag(&store, acct).as_deref(), Some("done"));
}
