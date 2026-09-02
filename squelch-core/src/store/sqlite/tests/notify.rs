//! The `notify_decisions` ledger: append-only, one row per (message, lane).

use super::super::*;
use super::support::*;
use chrono::Duration;

/// A ledger row for `message_id`, distinct per lane/decision so ordering and
/// content are visible in assertions.
fn decision(
    acct: AccountId,
    message_id: i64,
    lane: NotifyLane,
    decision: NotifyDecision,
) -> NewNotifyDecision {
    NewNotifyDecision {
        account_id: acct,
        message_id,
        lane,
        decision,
        notify_importance: Some(80),
        model_used: Some("claude-haiku-4-5".to_string()),
        latency_ms: Some(1_500),
    }
}

/// THE APPEND-ONLY RULE, which is the whole reason the table is worth having: a
/// lane's first answer about a message is a labeled example, and an answer that
/// can be rewritten is evidence of nothing. A Stage-2 pass behind Stage-1, or a
/// human re-triage a day later, must change nothing at all.
#[test]
fn a_second_decision_for_the_same_lane_is_ignored_and_the_first_row_is_untouched() {
    let (store, acct) = store();

    assert!(
        store
            .record_notify_decision(&decision(
                acct,
                1,
                NotifyLane::Deliberate,
                NotifyDecision::Sent
            ))
            .unwrap(),
        "the first record inserts"
    );
    let before = store
        .notify_decisions_since(acct, Utc::now() - Duration::hours(1), 100)
        .unwrap();
    assert_eq!(before.len(), 1);

    // Everything a caller could differ on: a different decision, a different
    // score, a different model, a different latency.
    let second = NewNotifyDecision {
        decision: NotifyDecision::DeclinedByModel,
        notify_importance: Some(10),
        model_used: Some("claude-opus-5".to_string()),
        latency_ms: Some(99_000),
        ..decision(acct, 1, NotifyLane::Deliberate, NotifyDecision::Sent)
    };
    assert!(
        !store.record_notify_decision(&second).unwrap(),
        "false == the duplicate was IGNORED, not applied"
    );

    let after = store
        .notify_decisions_since(acct, Utc::now() - Duration::hours(1), 100)
        .unwrap();
    assert_eq!(after.len(), 1, "still exactly one row for the lane");
    assert_eq!(
        after, before,
        "every column of the first row survives the ignored write"
    );
}

/// UNIQUE is on (message_id, LANE), not on message_id. Both lanes decide about
/// the same message independently and the cross-lane questions — rescued,
/// overturned, confirmed — are joins over the two rows, so collapsing them would
/// delete the only thing this table is asked.
#[test]
fn both_lanes_record_their_own_answer_about_one_message() {
    let (store, acct) = store();

    assert!(
        store
            .record_notify_decision(&decision(
                acct,
                7,
                NotifyLane::Fast,
                NotifyDecision::DeclinedByModel
            ))
            .unwrap()
    );
    // The RESCUE shape: the deliberate lane buzzes a message the fast lane
    // declined. Both facts stand; neither lane is wrong about the other.
    assert!(
        store
            .record_notify_decision(&NewNotifyDecision {
                latency_ms: None,
                ..decision(acct, 7, NotifyLane::Deliberate, NotifyDecision::Sent)
            })
            .unwrap()
    );

    let rows = store
        .notify_decisions_since(acct, Utc::now() - Duration::hours(1), 100)
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .map(|r| (r.lane, r.decision))
            .collect::<Vec<_>>(),
        vec![
            (NotifyLane::Fast, NotifyDecision::DeclinedByModel),
            (NotifyLane::Deliberate, NotifyDecision::Sent),
        ]
    );
    assert!(rows.iter().all(|r| r.message_id == 7));
    // Latency is a fast-lane column: the deliberate lane leaves it NULL rather
    // than reporting the triage pipeline's own age as a notification's.
    assert_eq!(rows[0].latency_ms, Some(1_500));
    assert_eq!(rows[1].latency_ms, None);
}

/// Every column round-trips, including the three that are legitimately absent:
/// a suppressed row has no score, no model and no latency, and NULL must not
/// come back as a zero that reads like a verdict.
#[test]
fn a_row_round_trips_including_its_absent_columns() {
    let (store, acct) = store();
    store
        .record_notify_decision(&NewNotifyDecision {
            account_id: acct,
            message_id: 3,
            lane: NotifyLane::Fast,
            decision: NotifyDecision::Suppressed,
            notify_importance: None,
            model_used: None,
            latency_ms: None,
        })
        .unwrap();

    let rows = store
        .notify_decisions_since(acct, Utc::now() - Duration::hours(1), 100)
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.message_id, 3);
    assert_eq!(row.lane, NotifyLane::Fast);
    assert_eq!(row.decision, NotifyDecision::Suppressed);
    assert_eq!(row.notify_importance, None, "no model scored it");
    assert_eq!(row.model_used, None);
    assert_eq!(row.latency_ms, None);
    assert!(row.id > 0);

    // A model-scored row keeps its score and its model id verbatim — which is
    // what makes a decline a labeled example of a PARTICULAR model's judgement.
    store
        .record_notify_decision(&decision(
            acct,
            4,
            NotifyLane::Fast,
            NotifyDecision::DeclinedByModel,
        ))
        .unwrap();
    let rows = store
        .notify_decisions_since(acct, Utc::now() - Duration::hours(1), 100)
        .unwrap();
    assert_eq!(rows[1].notify_importance, Some(80));
    assert_eq!(rows[1].model_used.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(rows[1].latency_ms, Some(1_500));
}

/// THE ROLLOUT QUERY ITSELF (docs/NOTIFY.md §11.11 step 2 and §11.4's joins),
/// run as SQL over a fixture that exercises every shape it has to tell apart.
/// That number, not an argument, is what decides whether the threshold or the
/// model moves — so the query is worth a test of its own, and the fixture is
/// built to punish the two ways it can silently lie: a join that forgets
/// `unavailable` is rescuable would undercount rescues by the whole "the
/// gateway was down" class, and one that treats `suppressed` as a decline would
/// report a false-negative rate made entirely of mail the user asked to be rid
/// of.
#[test]
fn the_rollout_query_and_its_three_joins_come_out_right_on_a_fixture() {
    let (store, acct) = store();
    let record = |message_id, lane, decision| {
        assert!(
            store
                .record_notify_decision(&NewNotifyDecision {
                    account_id: acct,
                    message_id,
                    lane,
                    decision,
                    notify_importance: Some(70),
                    model_used: Some("claude-haiku-4-5".to_string()),
                    latency_ms: None,
                })
                .unwrap()
        );
    };
    use NotifyDecision::*;
    use NotifyLane::{Deliberate, Fast};
    // 1: RESCUED off a decline.       2: RESCUED off an outage.
    record(1, Fast, DeclinedByModel);
    record(1, Deliberate, Sent);
    record(2, Fast, Unavailable);
    record(2, Deliberate, Sent);
    // 3: OVERTURNED.                  4: CONFIRMED.
    record(3, Fast, Sent);
    record(3, Deliberate, DeclinedByModel);
    record(4, Fast, Sent);
    record(4, Deliberate, WouldSend);
    // 5: agreed silence, a true negative and none of the three.
    record(5, Fast, DeclinedByModel);
    record(5, Deliberate, DeclinedByModel);
    // 6: a squelched sender. NOT a decline on either lane, and so not an
    //    overturn when the deliberate lane says nothing either.
    record(6, Fast, Suppressed);
    record(6, Deliberate, Suppressed);
    // 7: nobody got to it in time. The drop §2a made visible.
    record(7, Fast, Unavailable);
    record(7, Deliberate, Expired);
    // 8: the fast lane alone, still in flight as far as the deliberate lane
    //    is concerned: no join may count a message with one row as anything.
    record(8, Fast, Sent);

    let conn = store.lock().unwrap();
    // §11.11 step 2, verbatim in shape: the whole distribution, one line per
    // (lane, decision).
    let mut stmt = conn
        .prepare("SELECT lane, decision, count(*) FROM notify_decisions GROUP BY 1,2 ORDER BY 1,2")
        .unwrap();
    let rows: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("deliberate".into(), "declined_by_model".into(), 2),
            ("deliberate".into(), "expired".into(), 1),
            ("deliberate".into(), "sent".into(), 2),
            ("deliberate".into(), "suppressed".into(), 1),
            ("deliberate".into(), "would_send".into(), 1),
            ("fast".into(), "declined_by_model".into(), 2),
            ("fast".into(), "sent".into(), 3),
            ("fast".into(), "suppressed".into(), 1),
            ("fast".into(), "unavailable".into(), 2),
        ]
    );

    /// One cross-lane join: the deliberate row says `slow`, the fast row for
    /// the same message says one of `fast`. Self-joined on `message_id`
    /// because §11.4 makes these facts joins rather than columns, which is what
    /// lets each lane record only what IT decided.
    fn join(conn: &Connection, slow: &str, fast: &[&str]) -> i64 {
        let list = fast
            .iter()
            .map(|d| format!("'{d}'"))
            .collect::<Vec<_>>()
            .join(",");
        conn.query_row(
            &format!(
                "SELECT count(*) FROM notify_decisions d
                   JOIN notify_decisions f
                     ON f.message_id = d.message_id AND f.lane = 'fast'
                  WHERE d.lane = 'deliberate' AND d.decision = ?1
                    AND f.decision IN ({list})"
            ),
            [slow],
            |r| r.get(0),
        )
        .unwrap()
    }

    // RESCUED counts both rescuable fast decisions. A query written against
    // `declined_by_model` alone reads 1 here and would have missed every
    // notification the gateway outage of 2026-08-19 cost.
    assert_eq!(
        join(&conn, "sent", &["declined_by_model", "unavailable"]),
        2
    );
    assert_eq!(
        join(&conn, "sent", &["declined_by_model"]),
        1,
        "and the two rescuable classes really are distinguishable"
    );
    // OVERTURNED and CONFIRMED, the two halves of "was the fast buzz right".
    assert_eq!(join(&conn, "declined_by_model", &["sent"]), 1);
    assert_eq!(join(&conn, "would_send", &["sent"]), 1);
    // AND SUPPRESSION IS IN NONE OF THEM. Message 6 is silent on both lanes and
    // must not appear as a decline the deliberate lane agreed with.
    assert_eq!(join(&conn, "declined_by_model", &["suppressed"]), 0);
}

/// The eval read: this account's rows, oldest first, capped, from a cutoff.
#[test]
fn the_since_read_is_ordered_capped_and_account_scoped() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();

    let before = Utc::now() - Duration::hours(1);
    for message_id in 1..=5 {
        store
            .record_notify_decision(&decision(
                acct,
                message_id,
                NotifyLane::Fast,
                NotifyDecision::Sent,
            ))
            .unwrap();
    }
    // Another account's ledger must never appear in this one's eval.
    store
        .record_notify_decision(&decision(other, 99, NotifyLane::Fast, NotifyDecision::Sent))
        .unwrap();

    // INSERT ORDER, oldest first. Not `created_at` order: several decisions
    // inside one clock tick are the normal case, and id is the only total order.
    let all = store.notify_decisions_since(acct, before, 100).unwrap();
    assert_eq!(
        all.iter().map(|r| r.message_id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    let ids: Vec<i64> = all.iter().map(|r| r.id).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids ascend: {ids:?}");

    // The limit truncates from the FRONT (the oldest), so a capped eval reads a
    // prefix of history rather than an arbitrary sample of it.
    let page = store.notify_decisions_since(acct, before, 2).unwrap();
    assert_eq!(
        page.iter().map(|r| r.message_id).collect::<Vec<_>>(),
        vec![1, 2]
    );

    // A cutoff in the future matches nothing; one in the past matches all.
    assert!(
        store
            .notify_decisions_since(acct, Utc::now() + Duration::hours(1), 100)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .notify_decisions_since(other, before, 100)
            .unwrap()
            .len(),
        1
    );
}

/// THE FAST LANE'S RE-ENTRY GATE. Per (message, lane) and per account, because
/// re-ingest is routine and `INSERT OR IGNORE` learns about a duplicate only
/// after the paid model call that produced it.
#[test]
fn the_exists_probe_is_scoped_to_one_message_one_lane_and_one_account() {
    let (store, acct) = store();
    let other = store.ensure_account("other@example.com").unwrap();

    assert!(
        !store
            .notify_decision_exists(acct, 1, NotifyLane::Fast)
            .unwrap(),
        "nothing decided yet"
    );
    store
        .record_notify_decision(&decision(acct, 1, NotifyLane::Fast, NotifyDecision::Sent))
        .unwrap();

    assert!(
        store
            .notify_decision_exists(acct, 1, NotifyLane::Fast)
            .unwrap()
    );
    // THE OTHER LANE HAS NOT ANSWERED. The two lanes decide independently and
    // the cross-lane facts in docs/NOTIFY.md §11.4 are joins over both rows, so
    // a fast row must never read as a deliberate one.
    assert!(
        !store
            .notify_decision_exists(acct, 1, NotifyLane::Deliberate)
            .unwrap()
    );
    assert!(
        !store
            .notify_decision_exists(acct, 2, NotifyLane::Fast)
            .unwrap()
    );
    assert!(
        !store
            .notify_decision_exists(other, 1, NotifyLane::Fast)
            .unwrap(),
        "message ids are per-account rows, and the probe says so"
    );
}
