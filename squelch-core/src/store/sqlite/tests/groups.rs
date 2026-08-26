//! Send-group CRUD, the normalized recipient index, and the two-source history.

use super::super::*;
use super::support::*;
use crate::store::sqlite::groups::{GroupSendRecipient, GroupSendStatus, NewGroupMember};
use crate::types::{GroupMode, SealedKind};

fn t(days: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + chrono::Duration::days(days)
}

fn member(addr: &str) -> NewGroupMember {
    NewGroupMember {
        addr: addr.to_string(),
        display_name: None,
    }
}

/// The canonical fixture: an audience of three, addressed individually.
fn investors(store: &SqliteStore, acct: AccountId) -> i64 {
    store
        .create_send_group(
            acct,
            "Preseed Investors",
            GroupMode::Individual,
            "the seed round",
            &[
                member("ann@fund.com"),
                member("bo@fund.com"),
                member("cy@fund.com"),
            ],
        )
        .unwrap()
}

// ---- CRUD -----------------------------------------------------------------

#[test]
fn create_round_trips_with_members_and_mode() {
    let (store, acct) = store();
    let id = investors(&store, acct);

    let group = store.get_send_group(acct, id).unwrap().unwrap();
    assert_eq!(group.name, "Preseed Investors");
    assert_eq!(group.slug, "preseed investors");
    assert_eq!(group.mode, GroupMode::Individual);
    assert_eq!(group.note, "the seed round");
    assert_eq!(group.member_count, 3);
    assert_eq!(group.members.len(), 3);
    assert_eq!(group.last_sent_at, None);
}

/// The listing is a sidebar, not a contacts dump: counts travel, membership
/// does not.
#[test]
fn listing_carries_counts_but_not_membership() {
    let (store, acct) = store();
    investors(&store, acct);

    let listed = store.list_send_groups(acct).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].member_count, 3);
    assert!(
        listed[0].members.is_empty(),
        "the list read must not carry every member of every group"
    );
}

#[test]
fn a_second_group_by_the_same_name_is_a_user_error_not_a_500() {
    let (store, acct) = store();
    investors(&store, acct);

    // Different capitalization and spacing, same slug.
    let err = store
        .create_send_group(
            acct,
            "  preseed   INVESTORS ",
            GroupMode::To,
            "",
            &[member("dee@fund.com")],
        )
        .unwrap_err();
    assert!(
        matches!(err, CoreError::InvalidInput(ref m) if m.contains("already exists")),
        "expected a client-legible conflict, got {err:?}"
    );
}

#[test]
fn update_replaces_membership_wholesale() {
    let (store, acct) = store();
    let id = investors(&store, acct);

    store
        .update_send_group(
            acct,
            id,
            "Preseed Investors",
            GroupMode::Bcc,
            "",
            &[member("ann@fund.com"), member("zed@fund.com")],
        )
        .unwrap();

    let group = store.get_send_group(acct, id).unwrap().unwrap();
    assert_eq!(group.mode, GroupMode::Bcc);
    let addrs: Vec<&str> = group.members.iter().map(|m| m.addr.as_str()).collect();
    assert_eq!(addrs, vec!["ann@fund.com", "zed@fund.com"]);
    assert!(
        !addrs.contains(&"bo@fund.com"),
        "a member the editor dropped must not survive the write"
    );
}

/// Every read and write is account-scoped in SQL, not just at the call site: an
/// id from a request body must never reach another account's audience.
#[test]
fn another_accounts_group_is_invisible() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();
    let id = investors(&store, acct);

    assert!(store.get_send_group(other, id).unwrap().is_none());
    assert!(store.send_group_addrs(other, id).unwrap().is_empty());
    assert!(!store.delete_send_group(other, id).unwrap());
    assert!(
        !store
            .update_send_group(other, id, "Theirs", GroupMode::To, "", &[])
            .unwrap()
    );
    // Still intact for its owner.
    assert_eq!(
        store
            .get_send_group(acct, id)
            .unwrap()
            .unwrap()
            .member_count,
        3
    );
}

#[test]
fn delete_takes_the_membership_with_it() {
    let (store, acct) = store();
    let id = investors(&store, acct);

    assert!(store.delete_send_group(acct, id).unwrap());
    assert!(store.get_send_group(acct, id).unwrap().is_none());
    assert!(
        store.send_group_addrs(acct, id).unwrap().is_empty(),
        "ON DELETE CASCADE should have taken the members"
    );
    // A second delete is a miss, not an error.
    assert!(!store.delete_send_group(acct, id).unwrap());
}

#[test]
fn autocomplete_prefers_prefix_matches_and_is_case_blind() {
    let (store, acct) = store();
    investors(&store, acct);
    store
        .create_send_group(
            acct,
            "Design Partners",
            GroupMode::To,
            "",
            &[member("d@x.com")],
        )
        .unwrap();
    store
        .create_send_group(
            acct,
            "Investors (angel)",
            GroupMode::To,
            "",
            &[member("a@x.com")],
        )
        .unwrap();

    let hits = store.search_send_groups(acct, "INVEST", 8).unwrap();
    let names: Vec<&str> = hits.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Investors (angel)", "Preseed Investors"],
        "the prefix match sorts above the substring match"
    );

    assert!(
        store.search_send_groups(acct, "", 8).unwrap().is_empty(),
        "an empty fragment must return nothing, not everything"
    );
}

#[test]
fn slug_lookup_is_how_the_send_path_resolves_a_group() {
    let (store, acct) = store();
    let id = investors(&store, acct);

    assert_eq!(
        store
            .send_group_by_slug(acct, "Preseed   Investors")
            .unwrap(),
        Some(id)
    );
    assert_eq!(store.send_group_by_slug(acct, "nobody").unwrap(), None);
}

// ---- the recipient index ---------------------------------------------------

/// The index is written from the FAITHFUL recipient set, so it can still say a
/// mail went to an address `contacts` deliberately refuses to learn.
#[test]
fn ingest_indexes_sent_recipients_including_self_and_robots() {
    let (store, acct) = store();
    triaged(acct, "g1", "t1")
        .is_sent(true)
        .to_addrs("Ann <ANN@fund.com>, noreply@x.com, me@example.com")
        .ingest(&store);

    let group = store
        .create_send_group(
            acct,
            "Odd Ones",
            GroupMode::To,
            "",
            &[member("noreply@x.com"), member("me@example.com")],
        )
        .unwrap();
    let history = store.group_history(acct, group, 50, 0).unwrap();
    assert_eq!(
        history.len(),
        1,
        "a robot address and the user's own address are still recipients"
    );
    assert_eq!(history[0].reached, 2);
}

/// Received mail has no recipients worth indexing, and an index that learned
/// them would read as an inbound-recipient table.
#[test]
fn received_mail_is_never_indexed() {
    let (store, acct) = store();
    triaged(acct, "g1", "t1")
        .is_sent(false)
        .to_addrs("ann@fund.com")
        .ingest(&store);

    let group = store
        .create_send_group(acct, "Ann", GroupMode::To, "", &[member("ann@fund.com")])
        .unwrap();
    assert!(store.group_history(acct, group, 50, 0).unwrap().is_empty());
}

// ---- derived history -------------------------------------------------------

/// THE POINT OF THE DERIVED HALF: a group created today, for people the user has
/// been emailing for a year, opens with that year in it.
#[test]
fn history_finds_mail_that_predates_the_group() {
    let (store, acct) = store();
    triaged(acct, "g1", "t1")
        .is_sent(true)
        .subject("Update #1")
        .received_at(t(0))
        .to_addrs("ann@fund.com, bo@fund.com")
        .ingest(&store);
    triaged(acct, "g2", "t2")
        .is_sent(true)
        .subject("Update #2")
        .received_at(t(30))
        .to_addrs("ann@fund.com, bo@fund.com, cy@fund.com")
        .ingest(&store);

    let id = investors(&store, acct);
    let history = store.group_history(acct, id, 50, 0).unwrap();

    assert_eq!(history.len(), 2);
    // Newest first.
    assert_eq!(history[0].subject, "Update #2");
    assert_eq!(history[0].reached, 3);
    assert_eq!(history[0].group_size, 3);
    assert_eq!(history[1].subject, "Update #1");
    assert_eq!(
        history[1].reached, 2,
        "a message that reached part of the group reports the part"
    );
    assert!(history.iter().all(|h| h.group_send_id.is_none()));
}

/// Mail to someone who is not in the group is not the group's history.
#[test]
fn history_ignores_mail_to_non_members() {
    let (store, acct) = store();
    triaged(acct, "g1", "t1")
        .is_sent(true)
        .to_addrs("stranger@elsewhere.com")
        .ingest(&store);

    let id = investors(&store, acct);
    assert!(store.group_history(acct, id, 50, 0).unwrap().is_empty());
}

/// A group history is a sent-mail listing, so it inherits the sealed guard —
/// including the THREAD-level one, where the user's own reply in a thread sealed
/// by a sibling commits as 'normal'.
#[test]
fn history_excludes_sealed_mail_and_sealed_threads() {
    let (store, acct) = store();
    triaged(acct, "sealed", "t1")
        .is_sent(true)
        .sealed(SealedKind::Otp)
        .to_addrs("ann@fund.com")
        .ingest(&store);
    // A clean sent row whose THREAD holds a sealed sighting.
    triaged(acct, "sibling-sealed", "t2")
        .sealed(SealedKind::Otp)
        .ingest(&store);
    triaged(acct, "mine", "t2")
        .is_sent(true)
        .to_addrs("ann@fund.com")
        .ingest(&store);

    let id = investors(&store, acct);
    assert!(
        store.group_history(acct, id, 50, 0).unwrap().is_empty(),
        "neither a sealed row nor a row in a sealed thread may list"
    );
}

#[test]
fn history_pages_over_the_merged_set() {
    let (store, acct) = store();
    for i in 0..5 {
        triaged(acct, &format!("g{i}"), &format!("t{i}"))
            .is_sent(true)
            .subject(&format!("Update #{i}"))
            .received_at(t(i))
            .to_addrs("ann@fund.com")
            .ingest(&store);
    }
    let id = investors(&store, acct);

    let first = store.group_history(acct, id, 2, 0).unwrap();
    let second = store.group_history(acct, id, 2, 2).unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 2);
    assert_eq!(first[0].subject, "Update #4");
    assert_eq!(second[0].subject, "Update #2");
    assert!(
        store.group_history(acct, id, 2, 4).unwrap().len() == 1,
        "the tail page holds the remainder"
    );
}

// ---- recorded history ------------------------------------------------------

/// A recorded send ALSO matches the derived query. It must appear ONCE.
#[test]
fn a_recorded_send_is_not_also_counted_as_derived() {
    let (store, acct) = store();
    let msg = triaged(acct, "g1", "t1")
        .is_sent(true)
        .subject("Update #3")
        .received_at(t(10))
        .to_addrs("ann@fund.com, bo@fund.com, cy@fund.com")
        .ingest(&store);
    let id = investors(&store, acct);

    store
        .record_group_send(
            acct,
            id,
            "Update #3",
            GroupMode::To,
            t(10),
            &[
                GroupSendRecipient {
                    addr: "ann@fund.com".into(),
                    message_id: Some(msg),
                    status: GroupSendStatus::Sent,
                    error: None,
                },
                GroupSendRecipient {
                    addr: "bo@fund.com".into(),
                    message_id: Some(msg),
                    status: GroupSendStatus::Sent,
                    error: None,
                },
                GroupSendRecipient {
                    addr: "cy@fund.com".into(),
                    message_id: Some(msg),
                    status: GroupSendStatus::Sent,
                    error: None,
                },
            ],
        )
        .unwrap();

    let history = store.group_history(acct, id, 50, 0).unwrap();
    assert_eq!(
        history.len(),
        1,
        "the recorded row claims the message, so the derived query must skip it"
    );
    assert!(history[0].group_send_id.is_some());
    assert_eq!(history[0].reached, 3);
    assert_eq!(history[0].failed, 0);
    assert_eq!(history[0].message_id, Some(msg));
    assert_eq!(history[0].subject, "Update #3");
}

/// A half-delivered fan-out is a state the history has to be able to state
/// plainly: eleven got it, one did not, and which one is knowable.
#[test]
fn a_partly_failed_fan_out_reports_both_halves() {
    let (store, acct) = store();
    let ann_copy = triaged(acct, "g1", "t1")
        .is_sent(true)
        .subject("Update #4")
        .received_at(t(20))
        .to_addrs("ann@fund.com")
        .ingest(&store);
    let id = investors(&store, acct);

    store
        .record_group_send(
            acct,
            id,
            "Update #4",
            GroupMode::Individual,
            t(20),
            &[
                GroupSendRecipient {
                    addr: "ann@fund.com".into(),
                    message_id: Some(ann_copy),
                    status: GroupSendStatus::Sent,
                    error: None,
                },
                GroupSendRecipient {
                    addr: "bo@fund.com".into(),
                    message_id: None,
                    status: GroupSendStatus::Failed,
                    error: Some("gmail rejected the recipient".into()),
                },
                GroupSendRecipient {
                    addr: "cy@fund.com".into(),
                    message_id: None,
                    status: GroupSendStatus::Failed,
                    error: Some("gmail rejected the recipient".into()),
                },
            ],
        )
        .unwrap();

    let history = store.group_history(acct, id, 50, 0).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].reached, 1);
    assert_eq!(history[0].failed, 2);
    assert_eq!(history[0].group_size, 3);
    assert_eq!(history[0].mode, GroupMode::Individual);
}

/// The snapshot is why the denominator is stored rather than counted: "3 of 3"
/// about last quarter must keep meaning what it meant when the group grows.
#[test]
fn a_recorded_denominator_survives_the_group_changing() {
    let (store, acct) = store();
    let id = investors(&store, acct);
    store
        .record_group_send(
            acct,
            id,
            "Update #5",
            GroupMode::Bcc,
            t(5),
            &[
                GroupSendRecipient {
                    addr: "ann@fund.com".into(),
                    message_id: None,
                    status: GroupSendStatus::Sent,
                    error: None,
                },
                GroupSendRecipient {
                    addr: "bo@fund.com".into(),
                    message_id: None,
                    status: GroupSendStatus::Sent,
                    error: None,
                },
                GroupSendRecipient {
                    addr: "cy@fund.com".into(),
                    message_id: None,
                    status: GroupSendStatus::Sent,
                    error: None,
                },
            ],
        )
        .unwrap();

    store
        .update_send_group(
            acct,
            id,
            "Preseed Investors",
            GroupMode::Individual,
            "",
            &[
                member("ann@fund.com"),
                member("bo@fund.com"),
                member("cy@fund.com"),
                member("dee@fund.com"),
                member("eve@fund.com"),
            ],
        )
        .unwrap();

    let history = store.group_history(acct, id, 50, 0).unwrap();
    assert_eq!(history[0].group_size, 3, "the snapshot, not today's five");
    assert_eq!(history[0].reached, 3);
}

/// Deleting a group is a statement about who gets addressed next, not a licence
/// to erase what was already sent.
#[test]
fn recorded_sends_outlive_the_group() {
    let (store, acct) = store();
    let id = investors(&store, acct);
    store
        .record_group_send(
            acct,
            id,
            "Update #6",
            GroupMode::To,
            t(1),
            &[GroupSendRecipient {
                addr: "ann@fund.com".into(),
                message_id: None,
                status: GroupSendStatus::Sent,
                error: None,
            }],
        )
        .unwrap();

    store.delete_send_group(acct, id).unwrap();

    let conn = store.lock().unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM group_sends WHERE account_id = ?1 AND group_id = ?2",
            params![acct, id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1, "the record of the send is not the group");
}

#[test]
fn last_sent_at_reports_the_most_recent_recorded_send() {
    let (store, acct) = store();
    let id = investors(&store, acct);
    for day in [1, 40, 12] {
        store
            .record_group_send(
                acct,
                id,
                "Update",
                GroupMode::To,
                t(day),
                &[GroupSendRecipient {
                    addr: "ann@fund.com".into(),
                    message_id: None,
                    status: GroupSendStatus::Sent,
                    error: None,
                }],
            )
            .unwrap();
    }
    let group = store.get_send_group(acct, id).unwrap().unwrap();
    assert_eq!(group.last_sent_at, Some(t(40)));
}
