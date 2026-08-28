//! Event log and push-device tests.

use super::super::*;
use super::support::*;
use crate::types::Tier;
use chrono::TimeZone;

#[test]
fn append_event_is_once_per_message_ever() {
    let (store, acct) = store();

    let first = store.append_event(&new_event(acct, 1)).unwrap();
    assert_eq!(
        first,
        Some(1),
        "first append inserts and returns the new id"
    );

    // A SECOND append for the same message — a re-ingest, or a Stage-2 verdict
    // landing on a row that already notified at ingest — is a silent no-op.
    let mut again = new_event(acct, 1);
    again.kind = EventKind::Urgent;
    again.one_line = "a louder verdict".into();
    assert_eq!(
        store.append_event(&again).unwrap(),
        None,
        "dedup on message_id"
    );

    let all = store.events_after(acct, 0, 100).unwrap();
    assert_eq!(all.len(), 1, "still exactly one row");
    assert_eq!(
        all[0].kind,
        EventKind::Surfaced,
        "the FIRST verdict is the one kept"
    );
    assert_eq!(all[0].one_line, "line 1");
}

#[test]
fn events_after_pages_in_id_order_and_scopes_by_account() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();

    let mut ids = Vec::new();
    for m in 1..=5 {
        ids.push(store.append_event(&new_event(acct, m)).unwrap().unwrap());
    }
    // Another account's event must never appear in this account's replay.
    store.append_event(&new_event(other, 99)).unwrap().unwrap();

    // From the zero cursor: everything, oldest first.
    let all = store.events_after(acct, 0, 100).unwrap();
    assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), ids);
    assert_eq!(
        all.iter().map(|e| e.message_id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );

    // Limit truncates from the FRONT (the oldest unseen), so a client that
    // pages never skips a row.
    let page = store.events_after(acct, 0, 2).unwrap();
    assert_eq!(
        page.iter().map(|e| e.id).collect::<Vec<_>>(),
        ids[..2].to_vec()
    );

    // Resuming from a cursor is exclusive of the cursor itself.
    let rest = store.events_after(acct, ids[2], 100).unwrap();
    assert_eq!(
        rest.iter().map(|e| e.id).collect::<Vec<_>>(),
        ids[3..].to_vec()
    );

    // Caught up.
    assert!(
        store
            .events_after(acct, *ids.last().unwrap(), 100)
            .unwrap()
            .is_empty()
    );
    // Account scoping.
    assert_eq!(store.events_after(other, 0, 100).unwrap().len(), 1);
}

#[test]
fn event_by_id_round_trips_the_snapshot_and_scopes_by_account() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();

    let due = Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap();
    let mut ev = new_event(acct, 42);
    ev.kind = EventKind::Urgent;
    ev.tier = Tier::PastDue;
    ev.importance = 95;
    ev.deadline = Some(due.to_rfc3339());
    let id = store.append_event(&ev).unwrap().unwrap();

    // Every field a client renders from comes back verbatim — the row alone
    // is enough to build the notification (the iOS NSE has no second call).
    let got = store.event_by_id(acct, id).unwrap().expect("event");
    assert_eq!(got.id, id);
    assert_eq!(got.kind, EventKind::Urgent);
    assert_eq!(got.message_id, 42);
    assert_eq!(got.thread_id, "t42");
    assert_eq!(got.tier, Tier::PastDue);
    assert_eq!(got.importance, 95);
    assert_eq!(got.sender, "alice@example.com");
    assert_eq!(got.one_line, "line 42");
    assert_eq!(got.deadline.as_deref(), Some(due.to_rfc3339().as_str()));

    // Unknown id, and another account's id, are both indistinguishable misses.
    assert!(store.event_by_id(acct, id + 1).unwrap().is_none());
    assert!(store.event_by_id(other, id).unwrap().is_none());
}

#[test]
fn latest_event_id_is_zero_until_something_happens() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();

    assert_eq!(
        store.latest_event_id(acct).unwrap(),
        0,
        "empty => the 0 cursor"
    );

    let a = store.append_event(&new_event(acct, 1)).unwrap().unwrap();
    let b = store.append_event(&new_event(acct, 2)).unwrap().unwrap();
    assert_eq!(store.latest_event_id(acct).unwrap(), b);
    assert!(b > a, "ids are monotonic");
    // A deduped append does not move the cursor.
    assert_eq!(store.append_event(&new_event(acct, 2)).unwrap(), None);
    assert_eq!(store.latest_event_id(acct).unwrap(), b);
    // Per-account, so a busy second account cannot skip this one's replay.
    assert_eq!(store.latest_event_id(other).unwrap(), 0);
}

#[test]
fn append_event_pokes_the_attached_notifier_only_on_a_real_insert() {
    let (store, acct) = store();

    // No notifier attached: appending must still work (a consumer that never
    // attaches simply polls the table instead).
    store.append_event(&new_event(acct, 1)).unwrap().unwrap();

    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    assert!(store.attach_event_notifier(tx).unwrap().is_none());

    let id = store.append_event(&new_event(acct, 2)).unwrap().unwrap();
    assert_eq!(
        rx.try_recv().unwrap(),
        id,
        "the new id is broadcast on insert"
    );

    // A deduped append broadcasts nothing — no phantom wake for a no-op.
    assert_eq!(store.append_event(&new_event(acct, 2)).unwrap(), None);
    assert!(rx.try_recv().is_err(), "no broadcast for a deduped append");
}

#[test]
fn append_event_survives_having_no_receivers() {
    // The broadcast payload is only a hint; the table is the source of truth.
    // A sender with every receiver dropped errors on send, and that must be
    // invisible to the caller.
    let (store, acct) = store();
    let (tx, rx) = tokio::sync::broadcast::channel::<i64>(8);
    drop(rx);
    store.attach_event_notifier(tx).unwrap();
    assert_eq!(store.append_event(&new_event(acct, 1)).unwrap(), Some(1));
}

// ---- REGISTERED PUSH DEVICES -----------------------------------------

const TOK_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";

const TOK_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2";

/// Registration is IDEMPOTENT: iOS re-registers on every launch, so a second
/// call must refresh the same row rather than fork a new one.
#[test]
fn upsert_device_is_idempotent_and_refreshes_liveness() {
    let (store, acct) = store();

    let first = store.upsert_device(acct, TOK_A, "ios", None).unwrap();
    assert_eq!(first.token, TOK_A);
    assert_eq!(first.platform, "ios");
    assert_eq!(first.account_id, acct);

    // Same token again: same row id, `created_at` preserved (first sight is a
    // fact), `last_registered_at` moved forward.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let again = store.upsert_device(acct, TOK_A, "ios", None).unwrap();
    assert_eq!(again.id, first.id, "a re-register must not fork a row");
    assert_eq!(again.created_at, first.created_at);
    assert!(again.last_registered_at >= first.last_registered_at);
    assert_eq!(store.list_devices(acct).unwrap().len(), 1);

    // A distinct token is a distinct device.
    store.upsert_device(acct, TOK_B, "ios", None).unwrap();
    let all = store.list_devices(acct).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, first.id, "listed oldest-first");
}

/// A token belonging to another account CANNOT be taken over by
/// re-registering it: the collision is an error and the original row is
/// untouched, or account B could repoint account A's phone at itself.
#[test]
fn a_cross_account_token_collision_is_refused_not_rebound() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();

    let a = store.upsert_device(acct, TOK_A, "ios", None).unwrap();
    assert!(store.list_devices(other).unwrap().is_empty());

    let err = store
        .upsert_device(other, TOK_A, "macos", None)
        .expect_err("another account must not be able to adopt this token");
    assert!(
        matches!(err, CoreError::InvalidInput(ref m) if !m.contains(TOK_A)),
        "expected InvalidInput that does not echo the token, got {err:?}"
    );

    // A's row is exactly as it was: same id, same account, same platform.
    let still = store.list_devices(acct).unwrap();
    assert_eq!(still.len(), 1);
    assert_eq!(still[0].id, a.id);
    assert_eq!(still[0].account_id, acct);
    assert_eq!(still[0].platform, "ios");
    assert_eq!(still[0].last_registered_at, a.last_registered_at);
    // And B gained nothing at all.
    assert!(store.list_devices(other).unwrap().is_empty());

    // The owner can still re-register it, of course.
    let refreshed = store.upsert_device(acct, TOK_A, "ios", None).unwrap();
    assert_eq!(refreshed.id, a.id);
}

/// Delete-by-token is the shape both the human door's DELETE and the pusher's
/// APNs-410 cleanup use: scoped, boolean, and idempotent.
#[test]
fn delete_device_by_token_is_scoped_and_idempotent() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();
    store.upsert_device(acct, TOK_A, "ios", None).unwrap();
    store.upsert_device(acct, TOK_B, "ios", None).unwrap();

    // Another account cannot delete this account's device.
    assert!(!store.delete_device_by_token(other, TOK_A).unwrap());
    assert_eq!(store.list_devices(acct).unwrap().len(), 2);

    assert!(store.delete_device_by_token(acct, TOK_A).unwrap());
    let left = store.list_devices(acct).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].token, TOK_B);

    // A second delete is a no-op, not an error.
    assert!(!store.delete_device_by_token(acct, TOK_A).unwrap());
    // An unknown token likewise.
    assert!(
        !store
            .delete_device_by_token(acct, "ffff0000ffff0000")
            .unwrap()
    );
}
