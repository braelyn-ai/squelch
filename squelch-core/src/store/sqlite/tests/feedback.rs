//! Human triage-correction tests.

use super::super::*;
use super::support::*;

#[test]
fn correcting_triage_applies_records_and_survives_retriage() {
    let (store, acct) = store();
    let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let id = inbound_triaged(acct, "g1", "t1", "acme@x.com", t0, false).ingest(&store);

    // The pipeline called it noise; the human says it is a deadline.
    let fb = store
        .correct_triage(
            acct,
            id,
            TriageAxis::Tier,
            "deadline",
            Some("this is a bill"),
            t0,
        )
        .unwrap()
        .expect("message exists");
    assert_eq!(fb.dimension, "tier");
    assert_eq!(fb.from_value.as_deref(), Some("noise"));
    assert_eq!(fb.to_value, "deadline");
    assert_eq!(fb.sender, "acme@x.com");
    assert_eq!(fb.note.as_deref(), Some("this is a bill"));
    // The snapshot carries the FEATURES, not just the label — without them
    // the row is near-useless for refining anything.
    assert_eq!(fb.original["tier"], "noise");
    assert!(fb.original.get("importance").is_some());
    assert!(fb.original.get("reason").is_some());

    // The correction actually moved the row.
    let listed = store.list_triage_feedback(acct, 10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].to_value, "deadline");

    // THE INVARIANT THAT MATTERS: a later re-triage must not silently
    // overwrite the human. retriage_reset requeues rows for the LLM passes;
    // a human-stamped row must not come back into the queue.
    let reset = store.retriage_reset(acct, Some(id), 90).unwrap();
    assert_eq!(reset, 0, "a human-corrected row must not be requeued");
}

#[test]
fn sealing_by_hand_clears_the_category_and_specialist_rows() {
    // Sealing a message the pipeline already categorized and extracted must
    // keep both invariants true — sealed rows carry a NULL category, and the
    // specialist tables hold no sealed rows.
    let (store, acct) = store();
    let t0 = Utc::now();
    let id = inbound_triaged(acct, "g1", "t1", "deals@shop.com", t0, false).ingest(&store);

    // Pretend the pipeline categorized + extracted it as marketing.
    store
        .correct_triage(acct, id, TriageAxis::Category, "marketing", None, t0)
        .unwrap()
        .unwrap();
    store
        .marketing_apply(&crate::store::MarketingApplied {
            message_id: id,
            account_id: acct,
            brand: Some("Shop".into()),
            offer: Some("30% off".into()),
            discount: Some("30% off".into()),
            code: Some("SAVE30".into()),
            expires_at: None,
            received_at: t0,
            extractor_model_used: "m".into(),
        })
        .unwrap();
    assert_eq!(store.marketing_offers(acct, 30, 10).unwrap().len(), 1);

    // Now the human says: actually this is auth.
    store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
        .unwrap()
        .unwrap();

    // The extracted row is gone, so it cannot show in the Marketing zone.
    assert_eq!(store.marketing_offers(acct, 30, 10).unwrap().len(), 0);
    // And it now reads as sealed auth mail.
    let sealed = store.sealed_messages(acct).unwrap();
    assert_eq!(sealed.len(), 1);
    assert_eq!(sealed[0].id, id);
}

#[test]
fn sealing_by_hand_deletes_the_shipment_the_message_fed() {
    // Shipments are keyed by tracking number, not message, so the generic
    // specialist scrub misses them — sealing the row's latest feeder must
    // delete the shipment outright, or its item name and thread keep serving
    // sealed-derived content on the Sitrep card.
    use crate::triage::{ShipmentInfo, ShipmentStatus};
    let (store, acct) = store();
    let t0 = Utc::now();
    let id = inbound_triaged(acct, "g1", "t1", "pharmacy@rx.com", t0, false).ingest(&store);

    store
        .upsert_shipment(
            acct,
            id,
            &ShipmentInfo {
                carrier: "ups".into(),
                tracking_number: "1Z999AA10123456784".into(),
                item_name: "Prescription refill".into(),
                status: ShipmentStatus::Shipped,
                tracking_url: None,
            },
            t0,
        )
        .unwrap();
    assert_eq!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .len(),
        1
    );

    // The human says: actually this is auth.
    store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
        .unwrap()
        .unwrap();

    // The shipment row is gone from every listing, delivered included.
    assert!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn sealing_by_hand_nulls_the_ship_trigger_and_deletes_the_staged_order() {
    // A staged order is nothing but mail-derived content (order reference, item
    // name, thread) and the trigger is a standing instruction to send this mail
    // to a model. Sealing must clear both, or the row keeps queueing itself.
    let (store, acct) = store();
    let t0 = Utc::now();
    let id = triaged(acct, "g1", "t1")
        .from("orders@shop.com")
        .ship_extract(true)
        .ingest(&store);
    assert_eq!(store.ship_extract_queue(acct, 10).unwrap().len(), 1);
    {
        let conn = store.lock().unwrap();
        conn.execute(
            "INSERT INTO shipment_orders(account_id, order_ref, item_name, thread_id,
                                         last_message_id, first_seen, last_update)
             VALUES(?1, 'ORD-9', 'Prescription refill', 't1', ?2, ?3, ?3)",
            params![acct, id, t0.to_rfc3339()],
        )
        .unwrap();
    }

    store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
        .unwrap()
        .unwrap();

    assert!(
        store.ship_extract_queue(acct, 10).unwrap().is_empty(),
        "a sealed row must never re-enter the shipments queue"
    );
    let conn = store.lock().unwrap();
    let marker: Option<String> = conn
        .query_row(
            "SELECT ship_extract_model FROM triage WHERE message_id=?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(marker, None, "the trigger is nulled, not stamped");
    let staged: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM shipment_orders WHERE account_id=?1",
            params![acct],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(staged, 0, "the staged order goes with the seal");
}

#[test]
fn sealing_by_hand_deletes_a_poll_advanced_shipment_too() {
    // The delete is keyed on `last_message_id`, and carrier polls deliberately
    // never touch that pointer — so a row the carrier has since advanced still
    // seals with the mail that fed it. If a poll ever adopted the pointer (or
    // nulled it), this row would survive the seal carrying sealed-derived
    // content on a Sitrep card.
    use crate::triage::{CarrierTrack, ShipmentInfo, ShipmentStatus};
    let (store, acct) = store();
    let t0 = Utc::now();
    let id = inbound_triaged(acct, "g1", "t1", "pharmacy@rx.com", t0, false).ingest(&store);

    let sid = store
        .upsert_shipment(
            acct,
            id,
            &ShipmentInfo {
                carrier: "ups".into(),
                tracking_number: "1Z999AA10123456784".into(),
                item_name: "Prescription refill".into(),
                status: ShipmentStatus::Shipped,
                tracking_url: None,
            },
            t0,
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
            t0 + chrono::Duration::minutes(5),
        )
        .unwrap();
    assert_eq!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .len(),
        1
    );

    store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
        .unwrap()
        .unwrap();

    assert!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn sealing_by_hand_retracts_the_notification_event() {
    // A message can notify FIRST and be sealed by hand after, so sealing has
    // to retract the pre-seal snapshot every client cursor replays forever.
    //
    // Retraction is REDACTION, not deletion: `events.id` is the rowid, so
    // deleting the newest row would free that id for the next append, and
    // every durable cursor past it would skip the reused event forever.
    let (store, acct) = store();
    let t0 = Utc::now();
    let id = inbound_triaged(acct, "g1", "t1", "noreply@bank.com", t0, false).ingest(&store);
    let other = inbound_triaged(acct, "g2", "t2", "alice@x.com", t0, false).ingest(&store);
    let sealed_ev = store
        .append_event(&NewEvent {
            deadline: Some("2026-08-01T17:00:00Z".to_string()),
            ..new_event(acct, id)
        })
        .unwrap()
        .unwrap();
    let keep = store
        .append_event(&new_event(acct, other))
        .unwrap()
        .unwrap();
    let before = store.event_by_id(acct, sealed_ev).unwrap().unwrap();
    assert!(!before.sender.is_empty() && !before.one_line.is_empty());
    assert!(before.deadline.is_some());
    let keep_before = store.event_by_id(acct, keep).unwrap().unwrap();

    store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
        .unwrap()
        .unwrap();

    // The row SURVIVES — the id must never be recyclable...
    let after = store
        .event_by_id(acct, sealed_ev)
        .unwrap()
        .expect("the id must stay taken; a freed rowid is a skipped notification later");
    // ...carrying nothing about the mail any more.
    assert_eq!(after.sender, "", "sealed content must not survive the seal");
    assert_eq!(after.one_line, "");
    assert_eq!(after.deadline, None);
    // Structure is untouched: the row is still addressable and orderable.
    assert_eq!(after.id, before.id);
    assert_eq!(after.message_id, before.message_id);
    assert_eq!(after.thread_id, before.thread_id);
    assert_eq!(after.created_at, before.created_at);

    // Replay sees both rows, in order, and only the sealed one is blanked.
    let left = store.events_after(acct, 0, 100).unwrap();
    assert_eq!(
        left.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![sealed_ev, keep],
        "redaction must not renumber or drop the log"
    );
    assert_eq!(left[0].one_line, "");
    assert_eq!(
        (left[1].sender.as_str(), left[1].one_line.as_str()),
        (keep_before.sender.as_str(), keep_before.one_line.as_str()),
        "and only THAT message's event is redacted"
    );

    // The next append gets a FRESH id rather than the sealed row's — which is
    // the whole reason this is an UPDATE.
    let third = inbound_triaged(acct, "g3", "t3", "bob@x.com", t0, false).ingest(&store);
    let next = store
        .append_event(&new_event(acct, third))
        .unwrap()
        .unwrap();
    assert!(next > keep, "ids must keep moving forward, got {next}");

    assert_eq!(
        store.sealed_messages(acct).unwrap().len(),
        1,
        "it WAS sealed"
    );
}

#[test]
fn unsealing_records_the_correction_and_frees_the_message() {
    // The other direction: over-sealing is a real failure mode (seal.rs
    // carries explicit guards against it), so it must be correctable.
    let (store, acct) = store();
    let t0 = Utc::now();
    let id = inbound_triaged(acct, "g1", "t1", "news@x.com", t0, false).ingest(&store);
    store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
        .unwrap()
        .unwrap();
    assert_eq!(store.sealed_messages(acct).unwrap().len(), 1);

    let fb = store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "normal", None, t0)
        .unwrap()
        .unwrap();
    assert_eq!(fb.dimension, "sensitivity");
    assert_eq!(fb.from_value.as_deref(), Some("sealed"));
    assert_eq!(fb.to_value, "normal");
    assert_eq!(store.sealed_messages(acct).unwrap().len(), 0);
}

#[test]
fn sealing_by_hand_discards_the_reply_draft() {
    // A draft is only ever SAVED against non-sealed mail, so a post-hoc seal has
    // to take the composition with it: the reply quotes mail the user has just
    // decided is auth. The account's new-message draft is keyed to nothing and
    // must survive.
    let (store, acct) = store();
    let t0 = Utc::now();
    let id = inbound_triaged(acct, "g1", "t1", "noreply@bank.com", t0, false).ingest(&store);
    let reply = store
        .upsert_draft(
            acct,
            Some(id),
            DraftFields {
                to_addr: "noreply@bank.com",
                subject: "Re: code",
                body: "was this you?",
                ..Default::default()
            },
            t0,
        )
        .unwrap();
    store
        .upsert_draft(
            acct,
            None,
            DraftFields {
                to_addr: "bob@example.com",
                subject: "Hello",
                body: "hi",
                ..Default::default()
            },
            t0,
        )
        .unwrap();
    assert_eq!(store.list_drafts(acct).unwrap().len(), 2);

    store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
        .unwrap()
        .unwrap();

    // DELETED, not merely filtered: the row is gone from the table.
    let left = store.list_drafts(acct).unwrap();
    assert_eq!(left.len(), 1);
    assert!(
        left[0].reply_to_message_id.is_none(),
        "the new-message draft stands"
    );
    let n: i64 = store
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM drafts WHERE id = ?1",
            params![reply.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "the seal scrubs the row, it does not just hide it");
}

#[test]
fn triage_debug_carries_the_thread_id() {
    // The debug read joins `messages` already; the thread id rides along so a
    // client holding one triage row can ask for the whole conversation without
    // a second lookup.
    let (store, acct) = store();
    let t0 = Utc::now();
    let id = inbound_triaged(acct, "g1", "thread-abc", "alice@x.com", t0, false).ingest(&store);

    let debug = store.triage_debug(acct, id).unwrap().expect("triage row");
    assert_eq!(debug.message_id, id);
    assert_eq!(debug.thread_id, "thread-abc");

    // Sealed rows stay out of this door, thread id or not.
    store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
        .unwrap()
        .unwrap();
    assert!(store.triage_debug(acct, id).unwrap().is_none());
}

#[test]
fn correcting_an_unknown_message_is_none_not_an_error() {
    let (store, acct) = store();
    let t0 = Utc::now();
    assert!(
        store
            .correct_triage(acct, 9999, TriageAxis::Category, "invoice", None, t0)
            .unwrap()
            .is_none()
    );
}

// ---- sealing scrubs the text a message DONATED elsewhere ----------------

use crate::triage::extract::shipments::ShipmentsApplied;
use crate::types::Sensitivity;

/// A shipments-extractor verdict carrying nothing — the fields each test needs
/// are filled in over the top.
fn ship_verdict(acct: AccountId, mid: i64, thread: &str) -> ShipmentsApplied {
    ShipmentsApplied {
        message_id: mid,
        account_id: acct,
        thread_id: thread.into(),
        is_shipment: true,
        tracking_number: None,
        order_ref: None,
        item_name: None,
        carrier: "unknown".into(),
        status: None,
        received_at: Utc::now(),
        extractor_model_used: "claude-haiku-4-5".into(),
    }
}

/// A message queued for the shipments extractor, from a chosen sender.
fn ship_msg(store: &SqliteStore, acct: AccountId, gmail: &str, thread: &str, from: &str) -> i64 {
    triaged_row(acct, gmail, thread, None, false, Sensitivity::Normal)
        .from(from)
        .ship_extract(true)
        .ingest(store)
}

/// The one shipment row's `(item_name, item_name_msg)`.
fn shipment_name(store: &SqliteStore, acct: AccountId) -> (String, Option<i64>) {
    let conn = store.lock().unwrap();
    conn.query_row(
        "SELECT item_name, item_name_msg FROM shipments WHERE account_id=?1",
        params![acct],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

#[test]
fn sealing_clears_an_item_name_donated_to_another_messages_row() {
    // The seal DELETE only reaches rows whose latest feeder is the sealed
    // message. The thread-adoption path writes an item name onto a row a
    // DIFFERENT message feeds, so without a name-provenance scrub the sealed
    // mail's words keep rendering on the Sitrep card the delete never saw.
    let (store, acct) = store();
    let feeder = ship_msg(&store, acct, "g-feed", "t-solo", "ship@ups.com");
    store
        .upsert_shipment(
            acct,
            feeder,
            &crate::triage::ShipmentInfo {
                carrier: "ups".into(),
                tracking_number: "1Z999AA10123456784".into(),
                item_name: String::new(),
                status: crate::triage::ShipmentStatus::Shipped,
                tracking_url: None,
            },
            Utc::now(),
        )
        .unwrap();

    // A follow-up in the same thread donates the name onto the feeder's row.
    let donor = ship_msg(&store, acct, "g-donor", "t-solo", "pharmacy@rx.com");
    assert!(
        store
            .shipments_extract_apply(&ShipmentsApplied {
                item_name: Some("Prescription refill".into()),
                ..ship_verdict(acct, donor, "t-solo")
            })
            .unwrap()
    );
    assert_eq!(shipment_name(&store, acct).0, "Prescription refill");

    // The human seals the DONOR, not the feeder.
    store
        .correct_triage(
            acct,
            donor,
            TriageAxis::Sensitivity,
            "sealed",
            None,
            Utc::now(),
        )
        .unwrap()
        .unwrap();

    let listed = store
        .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
        .unwrap();
    assert_eq!(
        listed.len(),
        1,
        "the package itself is not the donor's to delete"
    );
    assert_eq!(
        shipment_name(&store, acct),
        (String::new(), None),
        "sealed-derived text leaves the row it was donated to"
    );
    // AND ITS SOURCE MARKER: a row still stamped 'llm' with no name left would
    // be locked out of ever taking a regex-extracted name from a later email.
    let source: String = store
        .lock()
        .unwrap()
        .query_row(
            "SELECT item_name_source FROM shipments WHERE account_id=?1",
            params![acct],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(source, "regex", "the scrub resets the provenance too");

    // Proof it is not merely cosmetic: the next email's regex name lands.
    let later = ship_msg(&store, acct, "g-later", "t-solo", "ship@ups.com");
    store
        .upsert_shipment(
            acct,
            later,
            &crate::triage::ShipmentInfo {
                carrier: "ups".into(),
                tracking_number: "1Z999AA10123456784".into(),
                item_name: "Cat bed".into(),
                status: crate::triage::ShipmentStatus::Shipped,
                tracking_url: None,
            },
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        shipment_name(&store, acct),
        ("Cat bed".to_string(), Some(later))
    );
}

#[test]
fn promotion_carries_the_staged_messages_provenance_so_sealing_it_still_scrubs() {
    // A promoted staged order donates a name the ORDER CONFIRMATION wrote onto a
    // row the SHIP NOTICE feeds. Stamping the promotion with the ship notice
    // would make the confirmation's own seal a no-op.
    let (store, acct) = store();
    let order = ship_msg(&store, acct, "g-order", "t-ord", "orders@rx.com");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            order_ref: Some("ORD-9".into()),
            item_name: Some("Prescription refill".into()),
            ..ship_verdict(acct, order, "t-ord")
        })
        .unwrap();

    // Same merchant, so the promotion finds the staged row.
    let ship = ship_msg(&store, acct, "g-ship", "t-ord", "orders@rx.com");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            tracking_number: Some("1Z999AA10123456784".into()),
            order_ref: Some("ORD-9".into()),
            carrier: "ups".into(),
            ..ship_verdict(acct, ship, "t-ord")
        })
        .unwrap();
    assert_eq!(
        shipment_name(&store, acct),
        ("Prescription refill".to_string(), Some(order)),
        "the donated name still belongs to the order mail"
    );

    // Sealing the ORDER mail leaves the package (the ship notice feeds it) but
    // takes back the words the order mail contributed.
    store
        .correct_triage(
            acct,
            order,
            TriageAxis::Sensitivity,
            "sealed",
            None,
            Utc::now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .list_shipments(acct, true, KEEP_ALL_SHIPMENTS)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(shipment_name(&store, acct), (String::new(), None));
}

#[test]
fn sealing_scrubs_a_name_donated_to_a_staged_order_another_message_feeds() {
    // Same hole one table over: the staging upsert MOVES `last_message_id` to the
    // latest mail about that order, so the delete misses a name an earlier mail
    // wrote. Sealing has to reach it by provenance.
    let (store, acct) = store();
    let namer = ship_msg(&store, acct, "g-namer", "t-ord", "orders@rx.com");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            order_ref: Some("ORD-9".into()),
            item_name: Some("Prescription refill".into()),
            ..ship_verdict(acct, namer, "t-ord")
        })
        .unwrap();
    // A second, nameless mail about the same order moves the row's pointer.
    let later = ship_msg(&store, acct, "g-later", "t-ord", "orders@rx.com");
    store
        .shipments_extract_apply(&ShipmentsApplied {
            order_ref: Some("ORD-9".into()),
            ..ship_verdict(acct, later, "t-ord")
        })
        .unwrap();

    store
        .correct_triage(
            acct,
            namer,
            TriageAxis::Sensitivity,
            "sealed",
            None,
            Utc::now(),
        )
        .unwrap()
        .unwrap();

    let conn = store.lock().unwrap();
    let (name, name_msg): (String, Option<i64>) = conn
        .query_row(
            "SELECT item_name, item_name_msg FROM shipment_orders WHERE account_id=?1",
            params![acct],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (name.as_str(), name_msg),
        ("", None),
        "the sealed mail's product name leaves the staged row"
    );
}
