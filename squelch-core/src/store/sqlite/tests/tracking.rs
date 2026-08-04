//! Read-tracking store tests: minted trackers, recorded opens, and the join
//! that gives one message its open history.

use super::super::*;
use super::support::*;
use crate::store::sqlite::tracking::MAX_OPENS_PER_TOKEN;

/// A tracker + the sent message it points at, the shape every tracked send ends
/// up in once its echo has landed.
fn tracked(store: &SqliteStore, acct: AccountId, token: &str) -> i64 {
    let message_id = triaged(acct, "sent-1", "thread-77").seed(store);
    store.insert_send_tracker(acct, token, None, 1_000).unwrap();
    assert!(store.set_send_tracker_message(acct, token, message_id).unwrap());
    message_id
}

#[test]
fn tracker_and_opens_round_trip_through_the_join() {
    let (store, acct) = store();
    let message_id = tracked(&store, acct, "tok-abc");

    assert!(store.message_opens(acct, message_id).unwrap().is_empty());

    assert!(store.record_open(acct, "tok-abc", 1_100, Some("Apple Mail/16.0"), "unknown").unwrap());
    assert!(
        store
            .record_open(acct, "tok-abc", 1_050, Some("… GoogleImageProxy"), "proxied")
            .unwrap()
    );
    assert!(store.record_open(acct, "tok-abc", 1_200, None, "unknown").unwrap());

    let opens = store.message_opens(acct, message_id).unwrap();
    // Oldest first, regardless of insert order, and every column survives.
    assert_eq!(
        opens,
        vec![
            MessageOpen {
                opened_at: 1_050,
                user_agent: Some("… GoogleImageProxy".to_string()),
                classification: "proxied".to_string(),
            },
            MessageOpen {
                opened_at: 1_100,
                user_agent: Some("Apple Mail/16.0".to_string()),
                classification: "unknown".to_string(),
            },
            MessageOpen {
                opened_at: 1_200,
                user_agent: None,
                classification: "unknown".to_string(),
            },
        ]
    );
}

/// THE GUARD: an open can only be recorded against a tracker that exists. A
/// stranger guessing at tokens leaves nothing behind.
#[test]
fn an_unknown_token_records_nothing() {
    let (store, acct) = store();
    let message_id = tracked(&store, acct, "tok-abc");

    assert!(!store.record_open(acct, "tok-nope", 1_100, None, "unknown").unwrap());
    assert!(!store.record_open(acct, "", 1_100, None, "unknown").unwrap());
    assert!(store.message_opens(acct, message_id).unwrap().is_empty());
}

/// Trackers are account-scoped, so another account's token is as good as an
/// unknown one — for recording AND for reading back.
#[test]
fn trackers_do_not_cross_accounts() {
    let (store, mine) = store();
    let theirs = store.ensure_account("other@example.com").unwrap();
    let message_id = tracked(&store, mine, "tok-abc");

    assert!(!store.record_open(theirs, "tok-abc", 1_100, None, "unknown").unwrap());
    assert!(!store.set_send_tracker_message(theirs, "tok-abc", 999).unwrap());
    assert!(store.tracked_message(theirs, "tok-abc").unwrap().is_none());
    assert!(store.message_opens(theirs, message_id).unwrap().is_empty());
}

/// An open recorded before the echo backfilled the message id is kept, and the
/// join picks it up the moment the id lands.
#[test]
fn opens_survive_a_late_message_id_backfill() {
    let (store, acct) = store();
    store.insert_send_tracker(acct, "tok-late", None, 1_000).unwrap();
    assert!(store.record_open(acct, "tok-late", 1_100, None, "unknown").unwrap());
    // Nothing to point a notification at yet.
    assert!(store.tracked_message(acct, "tok-late").unwrap().is_none());

    let message_id = triaged(acct, "sent-late", "thread-9").seed(&store);
    assert!(store.set_send_tracker_message(acct, "tok-late", message_id).unwrap());

    assert_eq!(store.message_opens(acct, message_id).unwrap().len(), 1);
    let tracked = store.tracked_message(acct, "tok-late").unwrap().unwrap();
    assert_eq!(tracked.message_id, message_id);
    assert_eq!(tracked.thread_id, "thread-9");
}

/// Two sends in the same thread each get their own tracker, and neither's opens
/// bleed into the other's history.
#[test]
fn each_tracker_reports_only_its_own_message() {
    let (store, acct) = store();
    let first = tracked(&store, acct, "tok-1");
    let second = triaged(acct, "sent-2", "thread-77").seed(&store);
    store.insert_send_tracker(acct, "tok-2", Some(second), 2_000).unwrap();

    store.record_open(acct, "tok-1", 1_100, None, "unknown").unwrap();
    store.record_open(acct, "tok-2", 2_100, None, "unknown").unwrap();
    store.record_open(acct, "tok-2", 2_200, None, "unknown").unwrap();

    assert_eq!(store.message_opens(acct, first).unwrap().len(), 1);
    assert_eq!(store.message_opens(acct, second).unwrap().len(), 2);
}

/// A live pixel URL is a capability anyone holding it can refetch forever — the
/// recipient, a forward, a scanner. The cap is what keeps that from being an
/// unbounded write channel into the user's database.
#[test]
fn one_token_stops_recording_at_the_cap() {
    let (store, acct) = store();
    let message_id = tracked(&store, acct, "tok-flood");

    for i in 0..(MAX_OPENS_PER_TOKEN as i64 + 25) {
        store.record_open(acct, "tok-flood", 1_000 + i, None, "unknown").unwrap();
    }

    let opens = store.message_opens(acct, message_id).unwrap();
    assert_eq!(opens.len(), MAX_OPENS_PER_TOKEN);
    // The cap drops the tail, so the earliest opens — the ones that carry the
    // signal — are what survive.
    assert_eq!(opens[0].opened_at, 1_000);
    assert!(!store.record_open(acct, "tok-flood", 9_999, None, "unknown").unwrap());
}

/// The cap is per token, not per table: a busy tracker must not silence a
/// different message's opens.
#[test]
fn the_cap_does_not_leak_across_tokens() {
    let (store, acct) = store();
    let loud = tracked(&store, acct, "tok-loud");
    let quiet = triaged(acct, "sent-2", "thread-77").seed(&store);
    store.insert_send_tracker(acct, "tok-quiet", Some(quiet), 2_000).unwrap();

    for i in 0..(MAX_OPENS_PER_TOKEN as i64 + 5) {
        store.record_open(acct, "tok-loud", 1_000 + i, None, "unknown").unwrap();
    }
    assert!(store.record_open(acct, "tok-quiet", 2_000, None, "unknown").unwrap());

    assert_eq!(store.message_opens(acct, loud).unwrap().len(), MAX_OPENS_PER_TOKEN);
    assert_eq!(store.message_opens(acct, quiet).unwrap().len(), 1);
}
