//! Local-draft CRUD tests.

use super::super::*;
use super::support::*;
use crate::types::SealedKind;

/// A fixed base instant, so `updated_at` movement is asserted, not raced.
fn t(minutes: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + chrono::Duration::minutes(minutes)
}

#[test]
fn upsert_draft_edits_the_same_reply_key_in_place() {
    let (store, acct) = store();
    let first = store
        .upsert_draft(acct, Some(7), "alice@example.com", "Re: Lunch", "sure", t(0))
        .unwrap();
    assert_eq!(first.created_at, t(0));
    assert_eq!(first.updated_at, t(0));

    let second = store
        .upsert_draft(acct, Some(7), "alice@example.com", "Re: Lunch", "sure, 1pm", t(5))
        .unwrap();
    // Same composition: same id and created_at, only the body and clock move.
    assert_eq!(second.id, first.id);
    assert_eq!(second.created_at, t(0));
    assert_eq!(second.updated_at, t(5));
    assert_eq!(second.body, "sure, 1pm");

    let all = store.list_drafts(acct).unwrap();
    assert_eq!(all.len(), 1, "an edit must not add a row");
    assert_eq!(all[0], second);
}

#[test]
fn reply_and_new_message_drafts_coexist_and_each_upsert_in_place() {
    let (store, acct) = store();
    let reply = store
        .upsert_draft(acct, Some(7), "alice@example.com", "Re: Lunch", "sure", t(0))
        .unwrap();
    let fresh = store
        .upsert_draft(acct, None, "bob@example.com", "Hello", "hi", t(1))
        .unwrap();
    assert_ne!(fresh.id, reply.id);
    assert!(fresh.reply_to_message_id.is_none());
    assert_eq!(store.list_drafts(acct).unwrap().len(), 2);

    // The NULL key matches ITSELF (`IS`, not `=`), so the account keeps exactly
    // one new-message draft however many times it is saved.
    let fresh2 = store
        .upsert_draft(acct, None, "bob@example.com", "Hello", "hi again", t(2))
        .unwrap();
    assert_eq!(fresh2.id, fresh.id);
    assert_eq!(fresh2.created_at, t(1));
    assert_eq!(fresh2.updated_at, t(2));

    let all = store.list_drafts(acct).unwrap();
    assert_eq!(all.len(), 2);
    // A second reply target is its own row.
    store
        .upsert_draft(acct, Some(8), "carol@example.com", "Re: Docs", "ok", t(3))
        .unwrap();
    assert_eq!(store.list_drafts(acct).unwrap().len(), 3);

    // The one-per-key rule is enforced by the SCHEMA, not just by the upsert
    // path: a write that skips `upsert_draft` still cannot duplicate either key.
    let conn = store.lock().unwrap();
    for key in [None, Some(7)] {
        let e = conn.execute(
            "INSERT INTO drafts(account_id, reply_to_message_id, to_addr, subject, body,
                 created_at, updated_at)
             VALUES(?1,?2,'','','',?3,?3)",
            params![acct, key, t(4).to_rfc3339()],
        );
        let msg = e.expect_err("duplicate must be refused").to_string();
        assert!(msg.contains("UNIQUE"), "duplicate {key:?} key: {msg}");
    }
}

#[test]
fn delete_draft_is_account_scoped_and_reports_a_miss() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();
    let mine = store
        .upsert_draft(acct, Some(7), "alice@example.com", "Re: Lunch", "sure", t(0))
        .unwrap();

    // Unknown id => false (the handler turns this into 404).
    assert!(!store.delete_draft(acct, 9999).unwrap());
    // Another account's id is indistinguishable from unknown, and the row lives.
    assert!(!store.delete_draft(other, mine.id).unwrap());
    assert_eq!(store.list_drafts(acct).unwrap().len(), 1);

    assert!(store.delete_draft(acct, mine.id).unwrap());
    assert!(store.list_drafts(acct).unwrap().is_empty());
    // Deleting twice is a miss, not an error.
    assert!(!store.delete_draft(acct, mine.id).unwrap());
}

#[test]
fn list_drafts_orders_by_updated_at_desc_and_scopes_by_account() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();
    store
        .upsert_draft(acct, Some(1), "a@example.com", "one", "1", t(0))
        .unwrap();
    store
        .upsert_draft(acct, Some(2), "b@example.com", "two", "2", t(1))
        .unwrap();
    store
        .upsert_draft(acct, Some(3), "c@example.com", "three", "3", t(2))
        .unwrap();
    store
        .upsert_draft(other, Some(1), "z@example.com", "theirs", "z", t(9))
        .unwrap();

    let subjects: Vec<String> = store
        .list_drafts(acct)
        .unwrap()
        .into_iter()
        .map(|d| d.subject)
        .collect();
    assert_eq!(subjects, ["three", "two", "one"]);

    // Touching the oldest draft moves it to the front.
    store
        .upsert_draft(acct, Some(1), "a@example.com", "one", "1 again", t(3))
        .unwrap();
    let subjects: Vec<String> = store
        .list_drafts(acct)
        .unwrap()
        .into_iter()
        .map(|d| d.subject)
        .collect();
    assert_eq!(subjects, ["one", "three", "two"]);

    // The other account sees only its own.
    let theirs = store.list_drafts(other).unwrap();
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].subject, "theirs");
}

#[test]
fn list_drafts_hides_a_draft_whose_parent_went_sealed() {
    // The BELT, not the scrub: the draft row is inserted straight and the parent
    // is sealed by hand, so neither seal path runs. A draft keyed to sealed mail
    // must still never come back out of the list.
    let (store, acct) = store();
    let parent = triaged(acct, "g1", "t1").seed(&store);
    store
        .upsert_draft(acct, Some(parent), "alice@example.com", "Re: Lunch", "sure", t(0))
        .unwrap();
    store
        .upsert_draft(acct, None, "bob@example.com", "Hello", "hi", t(1))
        .unwrap();
    assert_eq!(store.list_drafts(acct).unwrap().len(), 2);

    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE triage SET sensitivity = 'sealed' WHERE message_id = ?1",
            params![parent],
        )
        .unwrap();

    let left = store.list_drafts(acct).unwrap();
    assert_eq!(left.len(), 1, "the sealed parent's draft is filtered out");
    assert!(
        left[0].reply_to_message_id.is_none(),
        "the NULL key compares against nothing and is never filtered"
    );
}

#[test]
fn reingest_that_seals_the_parent_deletes_its_draft() {
    // The other seal path: a re-ingest can turn a row that was normal when the
    // draft was saved into a sealed one, and it scrubs in the same transaction.
    let (store, acct) = store();
    let normal = triaged(acct, "g1", "t1");
    let parent = normal.ingest(&store);
    store
        .upsert_draft(acct, Some(parent), "alice@example.com", "Re: Lunch", "sure", t(0))
        .unwrap();

    let again = normal.clone().sealed(SealedKind::Otp).ingest(&store);
    assert_eq!(again, parent, "same Gmail id, same local row");
    assert!(
        store.list_drafts(acct).unwrap().is_empty(),
        "the re-ingest's seal took the draft with it"
    );
}

#[test]
fn account_email_reads_the_row_and_404s_unknown() {
    let (store, acct) = store();
    assert_eq!(store.account_email(acct).unwrap(), "me@example.com");
    assert!(matches!(
        store.account_email(9999).unwrap_err(),
        CoreError::NotFound
    ));
}
