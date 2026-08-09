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

    assert!(
        store
            .set_attention_status(acct, id, AttentionStatus::Done)
            .unwrap()
    );
    let done = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Done), None)
        .unwrap();
    assert_eq!(done.len(), 1);
    assert!(done[0].resolved_at.is_some(), "done stamps resolved_at");

    // Reopen clears resolved_at.
    assert!(
        store
            .set_attention_status(acct, id, AttentionStatus::Open)
            .unwrap()
    );
    let open = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Open), None)
        .unwrap();
    assert_eq!(open.len(), 1);
    assert!(open[0].resolved_at.is_none(), "reopen clears resolved_at");

    // Unknown id => false.
    assert!(
        !store
            .set_attention_status(acct, 999, AttentionStatus::Done)
            .unwrap()
    );
}

#[test]
fn resolve_sender_clears_every_open_thread_from_that_address() {
    // THE BUG: unsubscribing resolved only the thread the reader had open, so a
    // sender with nine emails in the window kept eight of them — which looks
    // exactly like the unsubscribe not having worked.
    let (store, acct) = store();
    let now = Utc::now();
    let a = triaged(acct, "g1", "t1")
        .from("news@shop.com")
        .received_at(now)
        .seed(&store);
    let b = triaged(acct, "g2", "t2")
        .from("NEWS@Shop.com")
        .received_at(now)
        .seed(&store);
    let other = triaged(acct, "g3", "t3")
        .from("real@person.com")
        .received_at(now)
        .seed(&store);
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
    assert!(
        done.iter().all(|u| u.update.id != other),
        "another sender is untouched"
    );
    assert!(
        done.iter().all(|u| u.update.id != sealed),
        "sealed is never touched"
    );

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
        &store,
        acct,
        "g1",
        "thr-dup",
        Tier::PastDue,
        80,
        Utc::now() - chrono::Duration::days(2),
    );
    let newer = ingest_normal(&store, acct, "g2", "thr-dup", Tier::PastDue, 90, Utc::now());
    let other = ingest_normal(
        &store,
        acct,
        "g3",
        "thr-solo",
        Tier::Deadline,
        70,
        Utc::now(),
    );

    // One row for the duplicated thread, and it is the band-sort-first message
    // (higher importance wins the representative slot).
    let standing = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::Standing))
        .unwrap();
    assert_eq!(standing.len(), 2, "two threads, two rows: {standing:#?}");
    assert_eq!(
        standing[0].update.id, newer,
        "representative is band-sort-first"
    );
    assert!(
        standing.iter().all(|u| u.update.id != older),
        "sibling hidden"
    );

    // Header counts agree with the collapsed list.
    let stats = store
        .stats(acct, Utc::now() - chrono::Duration::days(30))
        .unwrap();
    assert_eq!(
        stats.bands.standing, 2,
        "standing counts threads, not messages"
    );
    assert_eq!(stats.bands.new, 2, "new counts threads, not messages");

    // Done on the representative resolves the WHOLE thread: the sibling must
    // not reappear in any band.
    assert!(
        store
            .set_attention_status(acct, newer, AttentionStatus::Done)
            .unwrap()
    );
    let standing2 = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::Standing))
        .unwrap();
    assert_eq!(
        standing2.len(),
        1,
        "resolved thread fully gone: {standing2:#?}"
    );
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
    assert!(
        store
            .attention_updates(acct, since, None, None, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .attention_updates(acct, since, None, None, Some(SitrepBand::New))
            .unwrap()
            .is_empty()
    );

    // mark_surfaced refuses to stamp a sealed row.
    let n = store.mark_surfaced(acct, &[sealed]).unwrap();
    assert_eq!(n, 0);
    // set_attention_status refuses a sealed row.
    assert!(
        !store
            .set_attention_status(acct, sealed, AttentionStatus::Done)
            .unwrap()
    );

    // Stats: sealed row contributes to `sealed`, never to any band, and
    // never advances last_surfaced_at.
    let stats = store
        .stats(acct, Utc::now() - chrono::Duration::days(30))
        .unwrap();
    assert_eq!(stats.sealed, 1);
    assert_eq!(stats.bands.new, 0);
    assert_eq!(stats.bands.standing, 0);
    assert_eq!(stats.bands.open, 0);
    assert!(stats.last_surfaced_at.is_none());
}

// ---- standing band: live correspondence ---------------------------------

/// Land a contacts row through the Sent-history merge — the path the harvest
/// uses — so the known-contact half of the standing band has something to match.
fn contact(store: &SqliteStore, acct: AccountId, addr: &str, sent_count: i64) {
    store
        .merge_harvested_contacts(
            acct,
            &[ContactEntry {
                addr: addr.to_string(),
                display_name: None,
                sent_count,
                last_sent_at: None,
            }],
        )
        .unwrap();
}

/// A dateless signal-tier inbound from `from` — the shape that carries a real
/// ask but no deadline, so tier alone never lands it in the standing band.
fn dateless(store: &SqliteStore, acct: AccountId, gmail: &str, thread: &str, from: &str) -> i64 {
    triaged(acct, gmail, thread)
        .from(from)
        .received_at(Utc::now())
        .importance(70)
        .tier(Tier::Signal)
        .seed(store)
}

fn standing_ids(store: &SqliteStore, acct: AccountId, since: DateTime<Utc>) -> Vec<i64> {
    store
        .attention_updates(acct, since, None, None, Some(SitrepBand::Standing))
        .unwrap()
        .into_iter()
        .map(|u| u.update.id)
        .collect()
}

#[test]
fn standing_admits_dateless_mail_from_a_sender_the_user_has_written_to() {
    // THE MISSED ASK: a real correspondent wanted something, attached no date,
    // so no deadline tier — and the surfacing clock rotated it out of view.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    contact(&store, acct, "johanna@wvfc.org", 12);
    contact(&store, acct, "list@wvfc.org", 0);

    let known = dateless(&store, acct, "g1", "t1", "johanna@wvfc.org");
    let never_written_to = dateless(&store, acct, "g2", "t2", "list@wvfc.org");
    let stranger = dateless(&store, acct, "g3", "t3", "hello@bigbox.com");

    let standing = standing_ids(&store, acct, since);
    assert_eq!(standing, vec![known], "only the known contact stands");
    assert!(
        !standing.contains(&never_written_to),
        "sent_count 0 is not a correspondent"
    );
    assert!(
        !standing.contains(&stranger),
        "a stranger's dateless mail is not owed"
    );
    assert_eq!(
        store
            .stats(acct, Utc::now() - chrono::Duration::days(30))
            .unwrap()
            .bands
            .standing,
        1
    );
}

#[test]
fn standing_known_contact_match_folds_address_case() {
    // from_addr is stored as the header spelled it; the contact row is
    // lowercased by the harvest. Neither side may be assumed normalized.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    contact(&store, acct, "johanna@wvfc.org", 3);

    let shouty = dateless(&store, acct, "g1", "t1", "Johanna@WVFC.org");
    assert_eq!(standing_ids(&store, acct, since), vec![shouty]);
    assert_eq!(
        store
            .stats(acct, Utc::now() - chrono::Duration::days(30))
            .unwrap()
            .bands
            .standing,
        1
    );
}

#[test]
fn standing_admits_a_thread_the_user_has_written_in() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);

    // A thread carrying the user's own reply, plus one the user never joined.
    let reply = triaged(acct, "g-sent", "thr-live")
        .from("me@example.com")
        .is_sent(true)
        .importance(90)
        .tier(Tier::Signal)
        .seed(&store);
    let theirs = dateless(&store, acct, "g1", "thr-live", "stranger@bigbox.com");
    let quiet = dateless(&store, acct, "g2", "thr-quiet", "stranger@bigbox.com");

    let standing = standing_ids(&store, acct, since);
    assert_eq!(standing, vec![theirs], "writing in a thread makes it live");
    assert!(!standing.contains(&quiet), "an unjoined thread stays out");

    // SECURITY/UX: the evidence row is never itself listed, in any band.
    let all = store
        .attention_updates(acct, since, None, None, None)
        .unwrap();
    assert!(
        all.iter().all(|u| u.update.id != reply),
        "sent mail is never listed"
    );
    assert!(!standing.contains(&reply));
    assert_eq!(
        store
            .stats(acct, Utc::now() - chrono::Duration::days(30))
            .unwrap()
            .bands
            .standing,
        1
    );
}

#[test]
fn standing_drops_a_resolved_participated_thread() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    contact(&store, acct, "johanna@wvfc.org", 5);
    let known = dateless(&store, acct, "g1", "t1", "johanna@wvfc.org");

    assert_eq!(standing_ids(&store, acct, since), vec![known]);
    assert!(
        store
            .set_attention_status(acct, known, AttentionStatus::Done)
            .unwrap()
    );
    assert!(
        standing_ids(&store, acct, since).is_empty(),
        "done leaves the band"
    );
    assert_eq!(
        store
            .stats(acct, Utc::now() - chrono::Duration::days(30))
            .unwrap()
            .bands
            .standing,
        0
    );
}

#[test]
fn standing_correspondence_arms_do_not_cross_accounts() {
    // SECURITY: both EXISTS arms are account-scoped. One account's correspondence
    // is not evidence for another's: a shared work address the user writes to
    // from account B must not pull B-shaped mail into account A's band, and a
    // thread id colliding across mailboxes must not count as participation.
    let (store, a) = store();
    let b = store.ensure_account("other@example.com").unwrap();
    let since = Utc::now() - chrono::Duration::days(30);

    // Only account B has ever written to Johanna, or written in this thread.
    contact(&store, b, "johanna@wvfc.org", 12);
    triaged(b, "g-b-sent", "thr-shared")
        .from("other@example.com")
        .is_sent(true)
        .seed(&store);

    let a_from_b_contact = dateless(&store, a, "g-a1", "t-a1", "johanna@wvfc.org");
    let a_in_b_thread = dateless(&store, a, "g-a2", "thr-shared", "stranger@bigbox.com");

    let standing_a = standing_ids(&store, a, since);
    assert!(
        standing_a.is_empty(),
        "account B's correspondence must not widen account A's band: {standing_a:?}"
    );
    assert!(!standing_a.contains(&a_from_b_contact));
    assert!(!standing_a.contains(&a_in_b_thread));
    assert_eq!(
        store
            .stats(a, Utc::now() - chrono::Duration::days(30))
            .unwrap()
            .bands
            .standing,
        0,
        "header agrees"
    );

    // The same rows DO stand once account A itself is the correspondent.
    contact(&store, a, "johanna@wvfc.org", 4);
    assert_eq!(standing_ids(&store, a, since), vec![a_from_b_contact]);
    assert_eq!(
        store
            .stats(a, Utc::now() - chrono::Duration::days(30))
            .unwrap()
            .bands
            .standing,
        1
    );
}

#[test]
fn standing_never_admits_sealed_mail_from_a_correspondent() {
    // SECURITY: participation widens the band's DEFINITION, never its clearance.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    contact(&store, acct, "johanna@wvfc.org", 5);

    let sealed_known = triaged(acct, "g1", "t1")
        .from("johanna@wvfc.org")
        .importance(90)
        .tier(Tier::Signal)
        .sealed(SealedKind::Otp)
        .seed(&store);
    triaged(acct, "g-sent", "thr-live")
        .from("me@example.com")
        .is_sent(true)
        .seed(&store);
    let sealed_thread = triaged(acct, "g2", "thr-live")
        .from("stranger@bigbox.com")
        .importance(90)
        .tier(Tier::Signal)
        .sealed(SealedKind::Otp)
        .seed(&store);

    let standing = standing_ids(&store, acct, since);
    assert!(
        standing.is_empty(),
        "sealed rows are absent from the band: {standing:?}"
    );
    assert!(!standing.contains(&sealed_known));
    assert!(!standing.contains(&sealed_thread));
    assert_eq!(
        store
            .stats(acct, Utc::now() - chrono::Duration::days(30))
            .unwrap()
            .bands
            .standing,
        0
    );
}

#[test]
fn stats_standing_count_matches_the_listed_standing_band() {
    // Header and list must agree over the widened definition, thread collapse
    // included.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    contact(&store, acct, "johanna@wvfc.org", 5);

    let bill = ingest_normal(
        &store,
        acct,
        "g-bill",
        "t-bill",
        Tier::PastDue,
        95,
        Utc::now(),
    );
    let known = dateless(&store, acct, "g-known", "t-known", "johanna@wvfc.org");
    // A live thread with TWO inbound rows: one band row, one counted thread.
    triaged(acct, "g-sent", "thr-live")
        .from("me@example.com")
        .is_sent(true)
        .seed(&store);
    dateless(&store, acct, "g-live1", "thr-live", "stranger@bigbox.com");
    let live_newer = triaged(acct, "g-live2", "thr-live")
        .from("stranger@bigbox.com")
        .received_at(Utc::now())
        .importance(88)
        .tier(Tier::Signal)
        .seed(&store);
    // Out: a stranger, a done row, a sealed row.
    dateless(&store, acct, "g-cold", "t-cold", "hello@bigbox.com");
    let done = dateless(&store, acct, "g-done", "t-done", "johanna@wvfc.org");
    store
        .set_attention_status(acct, done, AttentionStatus::Done)
        .unwrap();
    triaged(acct, "g-sealed", "t-sealed")
        .from("johanna@wvfc.org")
        .sealed(SealedKind::Otp)
        .seed(&store);

    let standing = standing_ids(&store, acct, since);
    assert_eq!(
        standing.len(),
        3,
        "bill + known contact + live thread: {standing:?}"
    );
    assert!(standing.contains(&bill));
    assert!(standing.contains(&known));
    assert!(
        standing.contains(&live_newer),
        "thread collapses to its sort-first row"
    );
    assert_eq!(
        store
            .stats(acct, Utc::now() - chrono::Duration::days(30))
            .unwrap()
            .bands
            .standing as usize,
        standing.len(),
        "header count equals the listed band"
    );
}

#[test]
fn stats_bands_and_last_surfaced_at() {
    let (store, acct) = store();

    let bill = ingest_normal(&store, acct, "g1", "t1", Tier::Deadline, 90, Utc::now());
    let sig = ingest_normal(&store, acct, "g2", "t2", Tier::Signal, 70, Utc::now());

    let s0 = store
        .stats(acct, Utc::now() - chrono::Duration::days(30))
        .unwrap();
    assert_eq!(s0.bands.standing, 1, "deadline tier counts as standing");
    assert_eq!(s0.bands.new, 2);
    assert_eq!(s0.bands.open, 0);
    assert!(s0.last_surfaced_at.is_none());

    store.mark_surfaced(acct, &[bill, sig]).unwrap();
    let s1 = store
        .stats(acct, Utc::now() - chrono::Duration::days(30))
        .unwrap();
    assert_eq!(s1.bands.new, 0, "both surfaced");
    assert_eq!(s1.bands.open, 2);
    assert_eq!(s1.bands.standing, 1, "surfacing doesn't change standing");
    assert!(s1.last_surfaced_at.is_some());
}
