//! The sender directory (`senders`): what registers a sender, what never does,
//! and how the search field's `from:` menu ranks them.

use super::super::*;
use super::support::*;
use crate::types::SealedKind;
use chrono::TimeZone;

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, day, 12, 0, 0).unwrap()
}

#[test]
fn inbound_mail_registers_its_sender_and_counts_each_message_once() {
    let (store, acct) = store();
    for (gmail, day) in [("g1", 1), ("g2", 5), ("g3", 9)] {
        triaged(acct, gmail, "t")
            .from("dan@example.com")
            .from_name(Some("Dan Smith"))
            .received_at(at(day))
            .upsert(&store);
    }
    // A re-sighting of the same message (a re-fetch, a second history walk)
    // runs the whole upsert again. The count comes from `messages`, so it
    // cannot double; the newer sighting's name wins.
    triaged(acct, "g3", "t")
        .from("dan@example.com")
        .from_name(Some("Dan"))
        .received_at(at(9))
        .upsert(&store);

    let hits = store.search_senders(acct, "dan", 8).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].addr, "dan@example.com");
    assert_eq!(
        hits[0].msg_count, 3,
        "a re-sighting is not a fourth message"
    );
    assert_eq!(hits[0].display_name.as_deref(), Some("Dan"));
    assert_eq!(hits[0].last_received_at, at(9));
}

#[test]
fn a_nameless_sighting_never_blanks_a_stored_name() {
    let (store, acct) = store();
    triaged(acct, "g1", "t")
        .from("dan@example.com")
        .from_name(Some("Dan Smith"))
        .received_at(at(1))
        .upsert(&store);
    triaged(acct, "g2", "t")
        .from("dan@example.com")
        .from_name(None)
        .received_at(at(2))
        .upsert(&store);
    triaged(acct, "g3", "t")
        .from("dan@example.com")
        .from_name(Some(""))
        .received_at(at(3))
        .upsert(&store);
    let hits = store.search_senders(acct, "dan", 8).unwrap();
    assert_eq!(hits[0].display_name.as_deref(), Some("Dan Smith"));
    assert_eq!(hits[0].msg_count, 3);
}

#[test]
fn sent_and_spam_mail_never_register_a_sender() {
    let (store, acct) = store();
    triaged(acct, "g-sent", "t1")
        .from("me@example.com")
        .is_sent(true)
        .upsert(&store);
    triaged(acct, "g-spam", "t2")
        .from("winner@lottery.example")
        .is_spam(true)
        .upsert(&store);
    assert!(store.search_senders(acct, "me@", 8).unwrap().is_empty());
    assert!(store.search_senders(acct, "lottery", 8).unwrap().is_empty());

    // A spam sender who also sent one legitimate mail is offered on the
    // strength of that one, and counted for that one only.
    triaged(acct, "g-ok", "t3")
        .from("winner@lottery.example")
        .upsert(&store);
    let hits = store.search_senders(acct, "lottery", 8).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].msg_count, 1);
}

#[test]
fn a_sender_with_only_sealed_mail_is_not_offered() {
    let (store, acct) = store();
    triaged(acct, "g-otp", "t1")
        .from("no-reply@accounts.example")
        .subject("Your code")
        .body("123456")
        .sealed(SealedKind::Otp)
        .seed(&store);
    // Registered at ingest (sealed is decided later), refused at read time:
    // a `from:` search for this address would return nothing.
    assert!(
        store
            .search_senders(acct, "accounts", 8)
            .unwrap()
            .is_empty()
    );

    // One ordinary message behind the same address unlocks it.
    triaged(acct, "g-plain", "t2")
        .from("no-reply@accounts.example")
        .subject("Your monthly summary")
        .seed(&store);
    let hits = store.search_senders(acct, "accounts", 8).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].addr, "no-reply@accounts.example");
}

#[test]
fn prefix_matches_outrank_substring_matches_then_volume_decides() {
    let (store, acct) = store();
    // "ann" is inside joanne@ three times over, and the start of ann@ once.
    for gmail in ["g1", "g2", "g3"] {
        triaged(acct, gmail, "t")
            .from("joanne@example.com")
            .upsert(&store);
    }
    triaged(acct, "g4", "t")
        .from("ann@example.com")
        .upsert(&store);
    let hits = store.search_senders(acct, "ann", 8).unwrap();
    assert_eq!(
        hits.iter().map(|h| h.addr.as_str()).collect::<Vec<_>>(),
        vec!["ann@example.com", "joanne@example.com"]
    );
    // Within one tier, volume wins: both are substring matches for "example".
    let hits = store.search_senders(acct, "example", 8).unwrap();
    assert_eq!(hits[0].addr, "joanne@example.com");
    assert_eq!(hits[0].msg_count, 3);
    // The display name is a match surface too, on both tiers.
    triaged(acct, "g5", "t")
        .from("x@corp.example")
        .from_name(Some("Annette Corp"))
        .upsert(&store);
    let hits = store.search_senders(acct, "annette", 8).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].addr, "x@corp.example");
}

#[test]
fn like_metacharacters_in_the_fragment_are_literal() {
    let (store, acct) = store();
    triaged(acct, "g1", "t")
        .from("dan@example.com")
        .upsert(&store);
    assert!(store.search_senders(acct, "%", 8).unwrap().is_empty());
    assert!(store.search_senders(acct, "_", 8).unwrap().is_empty());
    assert!(store.search_senders(acct, "d_n", 8).unwrap().is_empty());
}

#[test]
fn an_empty_fragment_lists_the_senders_with_the_most_mail() {
    let (store, acct) = store();
    for gmail in ["g1", "g2", "g3"] {
        triaged(acct, gmail, "t")
            .from("busy@example.com")
            .upsert(&store);
    }
    triaged(acct, "g4", "t")
        .from("quiet@example.com")
        .upsert(&store);
    for q in ["", "   "] {
        let hits = store.search_senders(acct, q, 8).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.addr.as_str()).collect::<Vec<_>>(),
            vec!["busy@example.com", "quiet@example.com"],
            "{q:?}"
        );
    }
    assert_eq!(store.search_senders(acct, "", 1).unwrap().len(), 1);
}

#[test]
fn the_directory_is_scoped_to_the_account() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();
    triaged(acct, "g1", "t")
        .from("dan@example.com")
        .upsert(&store);
    triaged(other, "g2", "t")
        .from("dan@example.com")
        .upsert(&store);
    triaged(other, "g3", "t")
        .from("dan@example.com")
        .upsert(&store);
    let mine = store.search_senders(acct, "dan", 8).unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].msg_count, 1, "the other account's mail is not mine");
    assert_eq!(
        store.search_senders(other, "dan", 8).unwrap()[0].msg_count,
        2
    );
}

#[test]
fn rescuing_a_message_from_spam_registers_its_sender() {
    let (store, acct) = store();
    // Seeded WITH a triage row: `clear_spam` refuses a row that has none to
    // check for sealing, exactly as the not-spam route does.
    let id = triaged(acct, "g-spam", "t")
        .from("newsletter@shop.example")
        .from_name(Some("The Shop"))
        .is_spam(true)
        .seed(&store);
    assert!(store.search_senders(acct, "shop", 8).unwrap().is_empty());
    assert!(store.clear_spam(acct, id).unwrap());
    let hits = store.search_senders(acct, "shop", 8).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].addr, "newsletter@shop.example");
    assert_eq!(hits[0].display_name.as_deref(), Some("The Shop"));
    assert_eq!(hits[0].msg_count, 1);
}

#[test]
fn the_limit_is_honoured() {
    let (store, acct) = store();
    for i in 0..12 {
        triaged(acct, &format!("g{i}"), "t")
            .from(&format!("sender{i}@example.com"))
            .upsert(&store);
    }
    assert_eq!(store.search_senders(acct, "sender", 5).unwrap().len(), 5);
    assert_eq!(store.search_senders(acct, "sender", 50).unwrap().len(), 12);
}
