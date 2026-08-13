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
    assert_eq!(store.list_shipments(acct, true).unwrap().len(), 1);

    // The human says: actually this is auth.
    store
        .correct_triage(acct, id, TriageAxis::Sensitivity, "sealed", None, t0)
        .unwrap()
        .unwrap();

    // The shipment row is gone from every listing, delivered included.
    assert!(store.list_shipments(acct, true).unwrap().is_empty());
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
            "noreply@bank.com",
            "Re: code",
            "was this you?",
            t0,
        )
        .unwrap();
    store
        .upsert_draft(acct, None, "bob@example.com", "Hello", "hi", t0)
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
