//! Attention band, surfacing and stats tests.

use super::super::*;
use super::support::*;
use crate::types::{SealedKind, Tier};

/// The two stamps are DIFFERENT FACTS. Surfacing a row (a list door) must not
/// make it look opened, or the share stat would measure scrolling.
///
/// `id` is unused past the seed: the open is addressed BY THREAD, which is the
/// whole point of `mark_thread_opened` - the client never hands back a list of
/// message ids the daemon gave it.
#[test]
fn opening_and_surfacing_are_separate_stamps() {
    let (store, acct) = store();
    let now = Utc::now();
    let id = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, now);

    store.mark_surfaced(acct, &[id]).unwrap();
    let rate = store
        .share_open_rate(acct, now - chrono::Duration::days(1))
        .unwrap();
    assert_eq!(rate.received, 1);
    assert_eq!(rate.opened, 0, "a surfaced row is not an opened one");

    assert_eq!(store.mark_thread_opened(acct, "t1").unwrap(), 1);
    let rate = store
        .share_open_rate(acct, now - chrono::Duration::days(1))
        .unwrap();
    assert_eq!(rate.opened, 1);

    // FIRST OPEN ONLY: re-reading a thread says nothing new, and a moving
    // stamp would make the rate drift with re-reads. The client fires the
    // route on every open precisely because the daemon settles this in SQL.
    assert_eq!(
        store.mark_thread_opened(acct, "t1").unwrap(),
        0,
        "a second open transitions nothing"
    );
}

/// What each side of the rate counts. Sealed mail is in the denominator (it
/// arrived and nobody had to open it, which is the point) and can never be in
/// the numerator; sent mail is in neither.
#[test]
fn the_open_rate_counts_received_mail_and_never_opens_a_sealed_row() {
    let (store, acct) = store();
    let now = Utc::now();
    let since = now - chrono::Duration::days(30);

    let opened = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, now);
    let _unopened = ingest_normal(&store, acct, "g2", "t2", Tier::Noise, 10, now);
    let sealed = triaged(acct, "g3", "t3")
        .received_at(now)
        .sealed(SealedKind::Otp)
        .seed(&store);

    store.mark_opened(acct, &[opened]).unwrap();
    // A sealed thread is never opened in the reader, so the client never says
    // so; the SQL guard is what makes that true rather than merely customary,
    // and it holds on BOTH doors into the stamp.
    assert_eq!(
        store.mark_opened(acct, &[sealed]).unwrap(),
        0,
        "a sealed row is never stamped opened"
    );
    assert_eq!(
        store.mark_thread_opened(acct, "t3").unwrap(),
        0,
        "and not by thread either"
    );

    let rate = store.share_open_rate(acct, since).unwrap();
    assert_eq!(rate.received, 3, "sealed mail counts as mail that arrived");
    assert_eq!(rate.opened, 1);
    assert!(rate.oldest_received_at.is_some());
}

/// THE DAY THE COLUMN SHIPPED. An established mailbox has years of mail and, the
/// instant `opened_at` exists, zero opens — so a window reaching past the
/// ledger's own arrival would divide a full denominator by an empty numerator
/// and report that this person opens almost none of their mail.
///
/// The caller's sample floors cannot catch it: they measure how old the MAIL is.
/// This is what makes the answer "no evidence yet" instead.
#[test]
fn the_window_never_reaches_further_back_than_the_ledger() {
    let (store, acct) = store();
    let now = Utc::now();

    // A mailbox with a year of mail behind it, none of it ever opened here.
    for i in 0..5 {
        ingest_normal(
            &store,
            acct,
            &format!("g{i}"),
            &format!("t{i}"),
            Tier::Signal,
            50,
            now - chrono::Duration::days(300 + i),
        );
    }

    // The ledger starts NOW (what the migration stamps on an existing account).
    store
        .set_app_setting(acct, OPEN_LEDGER_SINCE_KEY, &now.to_rfc3339())
        .unwrap();

    // Ask for a year. Get the ledger's answer, which is that it has seen
    // nothing at all — NOT "five received, none opened".
    let rate = store
        .share_open_rate(acct, now - chrono::Duration::days(365))
        .unwrap();
    assert_eq!(
        rate.received, 0,
        "mail that arrived before the ledger is not evidence about the ledger"
    );
    assert!(
        rate.oldest_received_at.is_none(),
        "and there is no history to date"
    );
}

/// The window is a floor on `received_at`, and the oldest row in it is what
/// says how much history the answer rests on.
#[test]
fn the_open_rate_reports_the_reach_of_its_own_evidence() {
    let (store, acct) = store();
    let now = Utc::now();
    // A ledger that has been running for longer than anything in this test, so
    // what is being pinned here is the WINDOW's arithmetic and not the clamp's
    // (which `the_window_never_reaches_further_back_than_the_ledger` owns).
    store
        .set_app_setting(
            acct,
            OPEN_LEDGER_SINCE_KEY,
            &(now - chrono::Duration::days(400)).to_rfc3339(),
        )
        .unwrap();

    let old = now - chrono::Duration::days(60);
    ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, old);
    ingest_normal(&store, acct, "g2", "t2", Tier::Signal, 80, now);

    // A window that reaches past the older row sees both, and dates itself to
    // the older one.
    let wide = store
        .share_open_rate(acct, now - chrono::Duration::days(90))
        .unwrap();
    assert_eq!(wide.received, 2);
    assert!(wide.oldest_received_at.unwrap() - old < chrono::Duration::seconds(2));

    // A narrower one sees only the recent row, and says so.
    let narrow = store
        .share_open_rate(acct, now - chrono::Duration::days(7))
        .unwrap();
    assert_eq!(narrow.received, 1);
    assert!(narrow.oldest_received_at.unwrap() - now < chrono::Duration::seconds(2));

    // An empty window has no evidence at all, which the caller reads as "no
    // number" rather than as a rate of zero.
    let empty = store
        .share_open_rate(acct, now + chrono::Duration::days(1))
        .unwrap();
    assert_eq!(empty.received, 0);
    assert!(empty.oldest_received_at.is_none());
}

#[test]
fn mark_surfaced_is_stamp_once_and_promotes_new_to_open() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(1);
    let id = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, Utc::now());

    // Pre-stamp: status new, surfaced_at NULL.
    let before = store
        .attention_updates(acct, since, None, None, None, false)
        .unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].status, AttentionStatus::New);
    assert!(before[0].surfaced_at.is_none());

    // First surface: stamps + promotes.
    let n = store.mark_surfaced(acct, &[id]).unwrap();
    assert_eq!(n, 1, "first surface counts as a transition");
    let after = store
        .attention_updates(acct, since, None, None, None, false)
        .unwrap();
    assert_eq!(after[0].status, AttentionStatus::Open);
    let stamp = after[0].surfaced_at.expect("surfaced_at set");

    // Second surface: idempotent, surfaced_at unchanged, no transition.
    let n2 = store.mark_surfaced(acct, &[id]).unwrap();
    assert_eq!(n2, 0, "second surface transitions nothing");
    let after2 = store
        .attention_updates(acct, since, None, None, None, false)
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
        .attention_updates(acct, since, None, None, Some(SitrepBand::Standing), false)
        .unwrap();
    assert_eq!(standing.len(), 1);
    assert_eq!(standing[0].update.id, bill);

    // NEW: everything (nothing surfaced yet).
    let new = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::New), false)
        .unwrap();
    assert_eq!(new.len(), 3);

    // Surface fresh + aged -> they become 'open'; bill stays new.
    store.mark_surfaced(acct, &[fresh, aged]).unwrap();

    // NEW now only the bill.
    let new2 = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::New), false)
        .unwrap();
    assert_eq!(new2.len(), 1);
    assert_eq!(new2[0].update.id, bill);

    // OPEN band sorted by age*importance: aged (14d*60) before fresh (0d*70).
    let open = store
        .attention_updates(acct, since, None, None, Some(SitrepBand::Open), false)
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
        .attention_updates(acct, since, None, Some(AttentionStatus::Done), None, false)
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
        .attention_updates(acct, since, None, Some(AttentionStatus::Open), None, false)
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
        .attention_updates(acct, since, None, Some(AttentionStatus::Open), None, false)
        .unwrap();
    assert!(
        !open.iter().any(|u| u.update.id == a || u.update.id == b),
        "both of that sender's threads are resolved"
    );

    let done = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Done), None, false)
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
        .attention_updates(acct, since, None, None, Some(SitrepBand::Standing), false)
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
        .attention_updates(acct, since, None, None, Some(SitrepBand::Standing), false)
        .unwrap();
    assert_eq!(
        standing2.len(),
        1,
        "resolved thread fully gone: {standing2:#?}"
    );
    assert_eq!(standing2[0].update.id, other);

    // The unrelated thread was untouched.
    let done = store
        .attention_updates(acct, since, None, Some(AttentionStatus::Done), None, false)
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
            .attention_updates(acct, since, None, None, None, false)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .attention_updates(acct, since, None, None, Some(SitrepBand::New), false)
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
        .attention_updates(acct, since, None, None, Some(SitrepBand::Standing), false)
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
fn standing_admits_the_mail_the_user_sent_themselves() {
    // A self-addressed message is ordinary INBOX mail by the time it lands here:
    // Gmail hands the same id to both label walks and the INBOX copy wins, so
    // `is_sent` is 0 and `from_addr` is the user. The contact row `ensure_account`
    // seeds is the whole reason it stands rather than falling in the gap between
    // "not from a correspondent" and "not from anyone at all".
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);

    let note = dateless(&store, acct, "g1", "t1", "me@example.com");

    assert_eq!(standing_ids(&store, acct, since), vec![note]);
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
        .attention_updates(acct, since, None, None, None, false)
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

// ---- reminders: "remind me about this later" -----------------------------

/// The pending-reminder schedule, in listing order.
fn pending_ids(store: &SqliteStore, acct: AccountId, since: DateTime<Utc>) -> Vec<i64> {
    store
        .attention_updates(acct, since, None, None, None, true)
        .unwrap()
        .into_iter()
        .map(|u| u.update.id)
        .collect()
}

/// Every row for this account, reminder fields included — the listing a client
/// actually reads, so the assertions run through the same columns it does.
fn all_updates(store: &SqliteStore, acct: AccountId, since: DateTime<Utc>) -> Vec<AttentionUpdate> {
    store
        .attention_updates(acct, since, None, None, None, false)
        .unwrap()
}

/// `(status, remind_at)` straight off the row. The listing collapses a thread to
/// one representative, so a hidden sibling can only be inspected here.
fn raw_triage(store: &SqliteStore, message_id: i64) -> (String, Option<String>) {
    store
        .lock()
        .unwrap()
        .query_row(
            "SELECT status, remind_at FROM triage WHERE message_id = ?1",
            params![message_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[test]
fn set_reminder_stamps_the_message_and_resolves_the_whole_thread() {
    // DEFERRING IS RESOLVING: the mail must leave every band immediately, and
    // the thread's other messages with it — a sibling left open puts the mail
    // the user just snoozed straight back in front of them.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let first = ingest_normal(&store, acct, "g1", "thr", Tier::Signal, 80, Utc::now());
    let sibling = ingest_normal(&store, acct, "g2", "thr", Tier::Signal, 70, Utc::now());
    let elsewhere = ingest_normal(&store, acct, "g3", "other", Tier::Signal, 60, Utc::now());

    let due = Utc::now() + chrono::Duration::days(3);
    assert!(store.set_reminder(acct, first, due).unwrap());

    let rows = all_updates(&store, acct, since);
    let by_id = |id: i64| rows.iter().find(|u| u.update.id == id).unwrap();
    assert_eq!(
        by_id(first).status,
        AttentionStatus::Done,
        "the reminded message is done"
    );
    assert!(by_id(first).resolved_at.is_some());
    assert_eq!(
        by_id(first).remind_at.unwrap().timestamp(),
        due.timestamp(),
        "the pending stamp roundtrips"
    );
    assert!(by_id(first).reminded_at.is_none(), "nothing has fired yet");
    assert_eq!(
        by_id(elsewhere).status,
        AttentionStatus::New,
        "another thread is untouched"
    );
    // The sibling is read from the row, not the listing: the thread collapses to
    // one representative, which is exactly why the done sweep has to be
    // thread-wide in the first place.
    assert_eq!(
        raw_triage(&store, sibling),
        ("done".to_string(), None),
        "done is thread-wide, and only the named message carries the reminder"
    );

    // And it is out of the bands until it comes due.
    assert!(standing_ids(&store, acct, since).is_empty());
    assert_eq!(store.stats(acct, since).unwrap().bands.standing, 0);
}

#[test]
fn set_reminder_replaces_a_reminder_that_already_fired() {
    // The two stamps are the pending and fired halves of ONE reminder, never a
    // history: re-arming must clear the old fired mark or the row would sit in
    // the standing band while ALSO being scheduled to come back.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let id = ingest_normal(&store, acct, "g1", "t1", Tier::Noise, 10, Utc::now());

    store
        .set_reminder(acct, id, Utc::now() - chrono::Duration::hours(1))
        .unwrap();
    assert_eq!(
        store.fire_due_reminders(acct, Utc::now()).unwrap(),
        vec![id]
    );
    assert!(all_updates(&store, acct, since)[0].reminded_at.is_some());

    store
        .set_reminder(acct, id, Utc::now() + chrono::Duration::days(2))
        .unwrap();
    let row = all_updates(&store, acct, since).remove(0);
    assert!(row.remind_at.is_some(), "re-armed");
    assert!(row.reminded_at.is_none(), "the fired stamp is cleared");
    assert!(
        standing_ids(&store, acct, since).is_empty(),
        "and it leaves the band it had re-entered"
    );
}

#[test]
fn fire_due_reminders_moves_only_the_due_and_only_the_named_columns() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let due = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, Utc::now());
    let later = ingest_normal(&store, acct, "g2", "t2", Tier::Signal, 80, Utc::now());
    let never = ingest_normal(&store, acct, "g3", "t3", Tier::Signal, 80, Utc::now());

    let due_at = Utc::now() - chrono::Duration::minutes(5);
    store.set_reminder(acct, due, due_at).unwrap();
    store
        .set_reminder(acct, later, Utc::now() + chrono::Duration::days(1))
        .unwrap();

    let fired = store.fire_due_reminders(acct, Utc::now()).unwrap();
    assert_eq!(fired, vec![due], "only the one whose moment has passed");

    let rows = all_updates(&store, acct, since);
    let by_id = |id: i64| rows.iter().find(|u| u.update.id == id).unwrap();
    let hit = by_id(due);
    assert_eq!(hit.status, AttentionStatus::Open, "back in play");
    assert!(hit.resolved_at.is_none(), "and no longer resolved");
    assert!(hit.remind_at.is_none(), "the pending stamp MOVED");
    assert_eq!(hit.reminded_at.unwrap().timestamp(), due_at.timestamp());
    assert_eq!(by_id(later).status, AttentionStatus::Done, "still deferred");
    assert!(by_id(later).remind_at.is_some());
    assert_eq!(
        by_id(never).status,
        AttentionStatus::New,
        "a row with no reminder is not touched"
    );

    // IDEMPOTENT WITHOUT A COOLDOWN: the fired row no longer matches, so a
    // second tick (or a second daemon) cannot fire it twice.
    assert!(
        store
            .fire_due_reminders(acct, Utc::now())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_fired_reminder_re_enters_standing_at_any_tier() {
    // THE POINT OF THE ARM: the user personally declared this mail owed
    // attention, so the triage model's "noise" verdict stops mattering. Asserted
    // on BOTH sides of the shared const — the list and the header count.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let noise = ingest_normal(&store, acct, "g1", "t1", Tier::Noise, 0, Utc::now());
    ingest_normal(&store, acct, "g2", "t2", Tier::Noise, 0, Utc::now());

    assert!(
        standing_ids(&store, acct, since).is_empty(),
        "noise is not standing on its own"
    );

    store
        .set_reminder(acct, noise, Utc::now() - chrono::Duration::minutes(1))
        .unwrap();
    store.fire_due_reminders(acct, Utc::now()).unwrap();

    assert_eq!(
        standing_ids(&store, acct, since),
        vec![noise],
        "a fired reminder outranks tier"
    );
    assert_eq!(
        store.stats(acct, since).unwrap().bands.standing as usize,
        1,
        "header count equals the listed band"
    );
}

#[test]
fn pending_reminders_lists_deferred_mail_soonest_first() {
    // A SCHEDULE, NOT A BAND: every row in it is done, so this listing is the
    // one place `done` must not mean `gone`.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let soon = ingest_normal(&store, acct, "g1", "t1", Tier::Noise, 0, Utc::now());
    let later = ingest_normal(&store, acct, "g2", "t2", Tier::PastDue, 99, Utc::now());
    let unscheduled = ingest_normal(&store, acct, "g3", "t3", Tier::Signal, 50, Utc::now());

    // The LOW-importance row is due first: due date beats the ranking sort, or
    // the schedule is not a schedule.
    store
        .set_reminder(acct, later, Utc::now() + chrono::Duration::days(9))
        .unwrap();
    store
        .set_reminder(acct, soon, Utc::now() + chrono::Duration::hours(2))
        .unwrap();

    assert_eq!(pending_ids(&store, acct, since), vec![soon, later]);
    assert!(
        !pending_ids(&store, acct, since).contains(&unscheduled),
        "a row with no reminder is not on the schedule"
    );
    assert!(
        pending_ids(&store, acct, since)
            .iter()
            .all(|id| all_updates(&store, acct, since)
                .iter()
                .any(|u| u.update.id == *id && u.status == AttentionStatus::Done)),
        "pending-reminder rows are done by construction"
    );

    // Firing the soonest one takes it off the schedule.
    store
        .fire_due_reminders(acct, Utc::now() + chrono::Duration::hours(3))
        .unwrap();
    assert_eq!(pending_ids(&store, acct, since), vec![later]);
}

#[test]
fn clear_reminder_unschedules_without_undoing_the_deferral() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let id = ingest_normal(&store, acct, "g1", "t1", Tier::Signal, 80, Utc::now());

    store
        .set_reminder(acct, id, Utc::now() + chrono::Duration::days(1))
        .unwrap();
    assert!(store.clear_reminder(acct, id).unwrap());

    let row = all_updates(&store, acct, since).remove(0);
    assert!(row.remind_at.is_none(), "un-scheduled");
    assert_eq!(
        row.status,
        AttentionStatus::Done,
        "clearing a reminder is not an undo"
    );
    assert!(pending_ids(&store, acct, since).is_empty());
    // Idempotent: a row with no reminder is a successful no-op.
    assert!(store.clear_reminder(acct, id).unwrap());
    // Missing id is the only false.
    assert!(!store.clear_reminder(acct, 999).unwrap());
    assert!(
        store
            .fire_due_reminders(acct, Utc::now())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reminders_never_touch_a_sealed_row() {
    // SECURITY: every reminder query excludes sealed rows in SQL, so a sealed
    // message is indistinguishable from a missing one — and the sweep re-guards
    // even though nothing should have been able to schedule one.
    let (store, acct) = store();
    let sealed = triaged(acct, "g-sealed", "t-sealed")
        .sealed(SealedKind::Otp)
        .seed(&store);

    let due = Utc::now() + chrono::Duration::days(1);
    assert!(
        !store.set_reminder(acct, sealed, due).unwrap(),
        "sealed reads as missing"
    );
    assert!(!store.clear_reminder(acct, sealed).unwrap());
    assert!(!store.set_reminder(acct, 999, due).unwrap(), "missing id");

    // Force a reminder onto the sealed row behind the store's back: the sweep's
    // own guard is what is under test, not `set_reminder`'s.
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE triage SET remind_at = ?1 WHERE message_id = ?2",
            params![
                (Utc::now() - chrono::Duration::days(1)).to_rfc3339(),
                sealed
            ],
        )
        .unwrap();
    assert!(
        store
            .fire_due_reminders(acct, Utc::now())
            .unwrap()
            .is_empty(),
        "the sweep will not surface a sealed row"
    );
}

#[test]
fn reminders_do_not_cross_accounts() {
    // SECURITY: every reminder statement is account-scoped, sweep included.
    let (store, mine) = store();
    let theirs = store.ensure_account("other@example.com").unwrap();
    let my_row = ingest_normal(&store, mine, "g1", "t1", Tier::Signal, 80, Utc::now());

    let past = Utc::now() - chrono::Duration::hours(1);
    assert!(
        !store.set_reminder(theirs, my_row, past).unwrap(),
        "another account cannot schedule my mail"
    );
    store.set_reminder(mine, my_row, past).unwrap();
    assert!(
        store
            .fire_due_reminders(theirs, Utc::now())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.fire_due_reminders(mine, Utc::now()).unwrap(),
        vec![my_row]
    );
}

#[test]
fn a_reminder_outlives_the_recency_window() {
    // THE FEATURE'S CORE PROMISE, against the window that would have killed it
    // on day 30: `since` here is the handler's own default (now - 30d), and the
    // mail is two months old — exactly the "remind me next month" case. Without
    // the exemption the schedule stops listing it, and the fired reminder
    // reopens a row no band, no count and no page can show.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let old = ingest_normal(
        &store,
        acct,
        "g-old",
        "t-old",
        Tier::Noise,
        0,
        Utc::now() - chrono::Duration::days(60),
    );
    assert!(
        all_updates(&store, acct, since).is_empty(),
        "the window still holds for mail nobody scheduled"
    );

    // (a) the pending lens lists it, 60-day-old receipt date and all.
    store
        .set_reminder(acct, old, Utc::now() + chrono::Duration::days(2))
        .unwrap();
    assert_eq!(pending_ids(&store, acct, since), vec![old]);

    // (b) once it fires: in the standing LIST and in the header COUNT, which is
    // the pair that has to agree row for row.
    store
        .fire_due_reminders(acct, Utc::now() + chrono::Duration::days(3))
        .unwrap();
    assert_eq!(standing_ids(&store, acct, since), vec![old]);
    assert_eq!(store.stats(acct, since).unwrap().bands.standing, 1);

    // (c) and the flat listing (no band) carries it too — the page the user
    // lands on after the reminder brings it back.
    assert_eq!(
        all_updates(&store, acct, since)
            .iter()
            .map(|u| u.update.id)
            .collect::<Vec<_>>(),
        vec![old]
    );
}

#[test]
fn a_reminder_cannot_be_set_on_the_users_own_sent_mail() {
    // Sent mail carries a triage row (neutral, tier=noise) and every listing
    // filters `m.is_sent = 0`, so a reminder on one would be unreachable
    // forever. Same indistinguishability rule as sealed: no row, no `true`.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let sent = triaged(acct, "g-sent", "t1").is_sent(true).seed(&store);
    let inbound = triaged(acct, "g-in", "t1").importance(80).seed(&store);

    let due = Utc::now() + chrono::Duration::days(1);
    assert!(
        !store.set_reminder(acct, sent, due).unwrap(),
        "the user's own message reads as missing"
    );
    // NOTHING happened: not the stamp, not the thread-wide done sweep that
    // would have resolved the real, inbound half of the conversation.
    assert_eq!(raw_triage(&store, sent), ("new".to_string(), None));
    assert_eq!(raw_triage(&store, inbound), ("new".to_string(), None));
    assert!(pending_ids(&store, acct, since).is_empty());
    assert!(
        store
            .fire_due_reminders(acct, Utc::now() + chrono::Duration::days(2))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_schedule_lists_every_reminder_even_two_in_one_thread() {
    // A SCHEDULE LISTS ONE ROW PER REMINDER, not one per conversation: the
    // band's thread collapse would hide the second one where it can neither be
    // seen nor cancelled, while it still comes due.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let first = ingest_normal(&store, acct, "g1", "thr", Tier::Signal, 80, Utc::now());
    let second = ingest_normal(&store, acct, "g2", "thr", Tier::Signal, 70, Utc::now());

    store
        .set_reminder(acct, first, Utc::now() + chrono::Duration::days(1))
        .unwrap();
    store
        .set_reminder(acct, second, Utc::now() + chrono::Duration::days(4))
        .unwrap();
    assert_eq!(
        pending_ids(&store, acct, since),
        vec![first, second],
        "both siblings are on the schedule, soonest first"
    );

    // And cancelling one leaves the other exactly where it was.
    assert!(store.clear_reminder(acct, first).unwrap());
    assert_eq!(pending_ids(&store, acct, since), vec![second]);

    // The bands still collapse the thread to one row — this is a schedule
    // exemption, not a change to the bands.
    store
        .fire_due_reminders(acct, Utc::now() + chrono::Duration::days(5))
        .unwrap();
    assert_eq!(standing_ids(&store, acct, since).len(), 1);
}

#[test]
fn resolving_a_fired_reminder_spends_it_for_good() {
    // The fired stamp holds a row in standing at ANY tier, so it must not
    // survive the user answering it: fire -> done -> reopen would otherwise put
    // a noise-tier newsletter back in the standing band forever, every single
    // time the row is ever reopened.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let noise = ingest_normal(&store, acct, "g1", "t1", Tier::Noise, 0, Utc::now());

    store
        .set_reminder(acct, noise, Utc::now() - chrono::Duration::minutes(1))
        .unwrap();
    store.fire_due_reminders(acct, Utc::now()).unwrap();
    assert_eq!(standing_ids(&store, acct, since), vec![noise], "it fired");

    store
        .set_attention_status(acct, noise, AttentionStatus::Done)
        .unwrap();
    assert!(all_updates(&store, acct, since)[0].reminded_at.is_none());

    // The undo path: the row comes back, the spent reminder does not.
    store
        .set_attention_status(acct, noise, AttentionStatus::Open)
        .unwrap();
    assert!(
        standing_ids(&store, acct, since).is_empty(),
        "a reopened row is judged on its tier again, not on a spent reminder"
    );
    assert_eq!(store.stats(acct, since).unwrap().bands.standing, 0);
}

#[test]
fn re_arming_clears_the_fired_stamp_on_the_whole_thread() {
    // The done sweep is thread-wide, and so is the clearing: a sibling left
    // wearing an old fired stamp sits in the standing band while the thread is
    // supposedly parked until the new date.
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(30);
    let first = ingest_normal(&store, acct, "g1", "thr", Tier::Noise, 0, Utc::now());
    let sibling = ingest_normal(&store, acct, "g2", "thr", Tier::Noise, 0, Utc::now());

    // The sibling's reminder fires, then the user re-parks the thread off the
    // other message.
    store
        .set_reminder(acct, sibling, Utc::now() - chrono::Duration::minutes(1))
        .unwrap();
    store.fire_due_reminders(acct, Utc::now()).unwrap();
    store
        .set_reminder(acct, first, Utc::now() + chrono::Duration::days(3))
        .unwrap();

    assert!(
        standing_ids(&store, acct, since).is_empty(),
        "the whole thread is parked, siblings included"
    );

    // And the sibling's spent stamp is really gone, not merely hidden behind
    // its `done`: reopening it (undo, or the next reminder firing on the thread)
    // must not drag a noise-tier row back into standing on a reminder that was
    // already answered.
    store
        .set_attention_status(acct, sibling, AttentionStatus::Open)
        .unwrap();
    assert!(
        standing_ids(&store, acct, since).is_empty(),
        "a re-armed thread carries no leftover fired stamp"
    );
    assert_eq!(store.stats(acct, since).unwrap().bands.standing, 0);
}

/// The per-sender probes SEEK, they do not SCAN. Every one of them compares an
/// address under COLLATE NOCASE, and SQLite serves a NOCASE comparison only from
/// an index declared with that collation: the BINARY primary key on `contacts`
/// cannot. Without `idx_contacts_addr_nocase` the standing band's contact arm
/// walked every contact once per message in the window, which cost the sitrep's
/// 10s poll 140ms on a thousand-message store and seconds on a bigger one, under
/// the mutex every request waits on. That queue WAS the hosted p95 (2026-08-27).
///
/// Pinned by PLAN rather than by timing: a timing test is flaky on CI and a plan
/// is exact. The second half drops the indexes and checks the plans degrade, so
/// a rename that silently detaches an index cannot pass either.
#[test]
fn sender_probes_seek_their_collated_indexes() {
    use super::super::attention::STANDING_BAND;

    let (store, _acct) = store();
    let conn = store.lock().unwrap();

    fn plan(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        // The plan does not depend on the values, but rusqlite still insists
        // every placeholder is bound.
        let nulls = vec![rusqlite::types::Value::Null; stmt.parameter_count()];
        stmt.query_map(rusqlite::params_from_iter(nulls), |r| r.get::<_, String>(3))
            .unwrap()
            .map(|line| line.unwrap())
            .collect()
    }
    /// Whether `table` (as the plan names it: the alias when there is one) is
    /// reached by an index SEEK on `col`, rather than a walk of the account's rows.
    fn seeks(plan: &[String], table: &str, col: &str) -> bool {
        plan.iter().any(|line| {
            line.starts_with(&format!("SEARCH {table} USING")) && line.contains(&format!("{col}=?"))
        })
    }

    // The standing band, exactly as `attention_updates` and `stats` spell it.
    let standing = format!(
        "SELECT COUNT(*) FROM triage t JOIN messages m ON m.id = t.message_id
         WHERE t.account_id = ?1 AND {STANDING_BAND}"
    );
    // `is_known_contact`, the ingest-time floor and `get_thread`'s bypass bit.
    let known = "SELECT COUNT(*) FROM contacts
                 WHERE account_id=?1 AND addr=?2 COLLATE NOCASE AND sent_count > 0";
    // The triage queues' per-candidate probes (`stage1_queue` and its siblings).
    let queue = "SELECT EXISTS(SELECT 1 FROM contacts c
                               WHERE c.account_id = m.account_id
                                 AND c.addr = m.from_addr COLLATE NOCASE
                                 AND c.sent_count > 0),
                        EXISTS(SELECT 1 FROM triage_feedback f
                               WHERE f.account_id = m.account_id
                                 AND f.sender = m.from_addr COLLATE NOCASE)
                 FROM messages m WHERE m.account_id = ?1";

    let p = plan(&conn, &standing);
    assert!(
        seeks(&p, "c", "addr"),
        "standing band walks contacts:\n{p:#?}"
    );
    let p = plan(&conn, known);
    assert!(
        seeks(&p, "contacts", "addr"),
        "is_known_contact walks contacts:\n{p:#?}"
    );
    let p = plan(&conn, queue);
    assert!(
        seeks(&p, "c", "addr"),
        "queue probe walks contacts:\n{p:#?}"
    );
    assert!(
        seeks(&p, "f", "sender"),
        "queue probe walks triage_feedback:\n{p:#?}"
    );

    // THE CANARY: with the collated indexes gone the same statements must fall
    // back to a walk, or the assertions above were never testing the index.
    conn.execute_batch(
        "DROP INDEX idx_contacts_addr_nocase;
         DROP INDEX idx_triage_feedback_sender_nocase;",
    )
    .unwrap();
    let p = plan(&conn, &standing);
    assert!(
        !seeks(&p, "c", "addr"),
        "the BINARY key served a NOCASE probe?\n{p:#?}"
    );
    let p = plan(&conn, known);
    assert!(
        !seeks(&p, "contacts", "addr"),
        "the BINARY key served a NOCASE probe?\n{p:#?}"
    );
    let p = plan(&conn, queue);
    assert!(
        !seeks(&p, "c", "addr"),
        "the BINARY key served a NOCASE probe?\n{p:#?}"
    );
    assert!(
        !seeks(&p, "f", "sender"),
        "the BINARY index served a NOCASE probe?\n{p:#?}"
    );
}
