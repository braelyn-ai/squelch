//! Attention band, surfacing and stats tests.

use super::super::*;
use super::support::*;
use crate::types::{SealedKind, Tier};

#[test]
fn mark_surfaced_is_stamp_once_and_promotes_new_to_open() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(1);
    let id = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, Utc::now());

    // Pre-stamp: status new, surfaced_at NULL.
    let before = store
        .attention_updates(acct, since, None, None, None)
        .unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].status, AttentionStatus::New);
    assert!(before[0].surfaced_at.is_none());

    // First surface: stamps + promotes.
    let n = store.mark_surfaced(acct, &[id]).unwrap();
    assert_eq!(n, 1, "first surface counts as a transition");
    let after = store
        .attention_updates(acct, since, None, None, None)
        .unwrap();
    assert_eq!(after[0].status, AttentionStatus::Open);
    let stamp = after[0].surfaced_at.expect("surfaced_at set");

    // Second surface: idempotent, surfaced_at unchanged, no transition.
    let n2 = store.mark_surfaced(acct, &[id]).unwrap();
    assert_eq!(n2, 0, "second surface transitions nothing");
    let after2 = store
        .attention_updates(acct, since, None, None, None)
        .unwrap();
    assert_eq!(after2[0].surfaced_at, Some(stamp));
    assert_eq!(after2[0].status, AttentionStatus::Open);
}

#[test]
fn band_queries_bucket_correctly() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);

    // A past_due bill (standing), a fresh signal (new), an aged signal.
    let bill = ingest_normal(&store, acct, "g1", "t1", Tier::PastDue, 90, Utc::now());
    let fresh = ingest_normal(&store, acct, "g2", "t2", Tier::Signal, 70, Utc::now());
    let aged = ingest_normal(
        &store,
        acct,
        "g3",
        "t3",
        Tier::Signal,
        60,
        Utc::now() - chrono::Duration::days(14),
    );

    // STANDING: only the bill (tier past_due/deadline, not done).
    let standing = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::Standing))
        .unwrap();
    assert_eq!(standing.len(), 1);
    assert_eq!(standing[0].update.id, bill);

    // NEW: everything (nothing surfaced yet).
    let new = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::New))
        .unwrap();
    assert_eq!(new.len(), 3);

    // Surface fresh + aged -> they become 'open'; bill stays new.
    store.mark_surfaced(acct, &[fresh, aged]).unwrap();

    // NEW now only the bill.
    let new2 = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::New))
        .unwrap();
    assert_eq!(new2.len(), 1);
    assert_eq!(new2[0].update.id, bill);

    // OPEN band sorted by age*importance: aged (14d*60) before fresh (0d*70).
    let open = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::Open))
        .unwrap();
    assert_eq!(open.len(), 2);
    assert_eq!(open[0].update.id, aged, "older*importance floats to top");
    assert_eq!(open[1].update.id, fresh);
}

#[test]
fn set_attention_status_resolves_and_reopens() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(1);
    let id = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, Utc::now());

    assert!(store
        .set_attention_status(acct, id, AttentionStatus::Done)
        .unwrap());
    let done = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Done), None)
        .unwrap();
    assert_eq!(done.len(), 1);
    assert!(done[0].resolved_at.is_some(), "done stamps resolved_at");

    // Reopen clears resolved_at.
    assert!(store
        .set_attention_status(acct, id, AttentionStatus::Open)
        .unwrap());
    let open = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Open), None)
        .unwrap();
    assert_eq!(open.len(), 1);
    assert!(open[0].resolved_at.is_none(), "reopen clears resolved_at");

    // Unknown id => false.
    assert!(!store
        .set_attention_status(acct, 999, AttentionStatus::Done)
        .unwrap());
}

#[test]
fn resolve_sender_clears_every_open_thread_from_that_address() {
    // THE BUG: unsubscribing resolved only the thread the reader had open, so a
    // sender with nine emails in the window kept eight of them — which looks
    // exactly like the unsubscribe not having worked.
    let (store, acct) = store();
    let now = Utc::now();
    let a = triaged(acct, "g1", "t1").from("news@shop.com").received_at(now).seed(&store);
    let b = triaged(acct, "g2", "t2").from("NEWS@Shop.com").received_at(now).seed(&store);
    let other = triaged(acct, "g3", "t3").from("real@person.com").received_at(now).seed(&store);
    let sealed = triaged(acct, "g4", "t4")
        .from("news@shop.com")
        .received_at(now)
        .sealed(SealedKind::Otp)
        .seed(&store);

    // Case-insensitive on the address, and a different sender is untouched.
    assert_eq!(store.resolve_sender(acct, "  News@Shop.com ").unwrap(), 2);

    let since = now - chrono::Duration::days(1);
    let open = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Open), None)
        .unwrap();
    assert!(
        !open.iter().any(|u| u.update.id == a || u.update.id == b),
        "both of that sender's threads are resolved"
    );

    let done = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Done), None)
        .unwrap();
    assert!(done.iter().all(|u| u.update.id != other), "another sender is untouched");
    assert!(done.iter().all(|u| u.update.id != sealed), "sealed is never touched");

    // Idempotent: a second call moves nothing, so an already-done row keeps the
    // resolved_at (and the reason) it was first given.
    assert_eq!(store.resolve_sender(acct, "news@shop.com").unwrap(), 0);
    // A blank address must never resolve the whole account.
    assert_eq!(store.resolve_sender(acct, "   ").unwrap(), 0);
}

#[test]
fn thread_shows_one_row_and_done_resolves_the_whole_thread() {
    // THE DUPLICATE-THREAD BUG: two messages of one thread each carried a
    // triage row and the band showed the conversation twice. The bands must
    // collapse to one row per thread — the band-sort-first message — and
    // resolving that representative must resolve the siblings, or the hidden
    // row pops straight back in.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);

    let older = ingest_normal(
        &store, acct, "g1", "thr-dup", Tier::PastDue, 80,
        Utc::now() - chrono::Duration::days(2),
    );
    let newer = ingest_normal(&store, acct, "g2", "thr-dup", Tier::PastDue, 90, Utc::now());
    let other = ingest_normal(&store, acct, "g3", "thr-solo", Tier::Deadline, 70, Utc::now());

    // One row for the duplicated thread, and it is the band-sort-first message
    // (higher importance wins the representative slot).
    let standing = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::Standing))
        .unwrap();
    assert_eq!(standing.len(), 2, "two threads, two rows: {standing:#?}");
    assert_eq!(standing[0].update.id, newer, "representative is band-sort-first");
    assert!(standing.iter().all(|u| u.update.id != older), "sibling hidden");

    // Header counts agree with the collapsed list.
    let stats = store.stats(acct).unwrap();
    assert_eq!(stats.bands.standing, 2, "standing counts threads, not messages");
    assert_eq!(stats.bands.new, 2, "new counts threads, not messages");

    // Done on the representative resolves the WHOLE thread: the sibling must
    // not reappear in any band.
    assert!(store
        .set_attention_status(acct, newer, AttentionStatus::Done)
        .unwrap());
    let standing2 = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::Standing))
        .unwrap();
    assert_eq!(standing2.len(), 1, "resolved thread fully gone: {standing2:#?}");
    assert_eq!(standing2[0].update.id, other);

    // The unrelated thread was untouched.
    let done = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Done), None)
        .unwrap();
    assert!(done.iter().all(|u| u.update.id != other));
}

#[test]
fn sealed_rows_never_surface_through_the_ledger() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(1);

    let sealed = triaged(acct, "g1", "t1")
        .subject("Your verification code")
        .importance(90)
        .sealed(SealedKind::Otp)
        .seed(&store);

    // Never appears in attention_updates (any band).
    assert!(store
        .attention_updates(acct, since, None, None, None)
        .unwrap()
        .is_empty());
    assert!(store
        .attention_updates(acct, since, None, None, Some(SitrepBand::New))
        .unwrap()
        .is_empty());

    // mark_surfaced refuses to stamp a sealed row.
    let n = store.mark_surfaced(acct, &[sealed]).unwrap();
    assert_eq!(n, 0);
    // set_attention_status refuses a sealed row.
    assert!(!store
        .set_attention_status(acct, sealed, AttentionStatus::Done)
        .unwrap());

    // Stats: sealed row contributes to `sealed`, never to any band, and
    // never advances last_surfaced_at.
    let stats = store.stats(acct).unwrap();
    assert_eq!(stats.sealed, 1);
    assert_eq!(stats.bands.new, 0);
    assert_eq!(stats.bands.standing, 0);
    assert_eq!(stats.bands.open, 0);
    assert!(stats.last_surfaced_at.is_none());
}

#[test]
fn stats_bands_and_last_surfaced_at() {
    let (store, acct) = store();

    let bill = ingest_normal(&store, acct, "g1", "t1", Tier::Deadline, 90, Utc::now());
    let sig = ingest_normal(&store, acct, "g2", "t2", Tier::Signal, 70, Utc::now());

    let s0 = store.stats(acct).unwrap();
    assert_eq!(s0.bands.standing, 1, "deadline tier counts as standing");
    assert_eq!(s0.bands.new, 2);
    assert_eq!(s0.bands.open, 0);
    assert!(s0.last_surfaced_at.is_none());

    store.mark_surfaced(acct, &[bill, sig]).unwrap();
    let s1 = store.stats(acct).unwrap();
    assert_eq!(s1.bands.new, 0, "both surfaced");
    assert_eq!(s1.bands.open, 2);
    assert_eq!(s1.bands.standing, 1, "surfacing doesn't change standing");
    assert!(s1.last_surfaced_at.is_some());
}
