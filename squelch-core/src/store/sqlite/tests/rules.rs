//! Sender-rule and unsubscribe-ledger tests.

use super::super::*;
use super::support::*;

#[test]
fn unsub_violation_bumps_only_after_grace_and_resets_on_rerequest() {
    let (store, acct) = store();
    let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    store
        .upsert_unsubscribe(acct, "news@x.com", "browser", None, t0)
        .unwrap();

    // Within the 72h grace => no violation.
    inbound_triaged(acct, "g1", "t1", "news@x.com", t0 + chrono::Duration::hours(1), false)
        .ingest(&store);
    assert_eq!(store.list_unsubscribes(acct).unwrap()[0].violation_count, 0);

    // Past the grace => first violation, last_violation_at stamped.
    let v1_at = t0 + chrono::Duration::hours(80);
    inbound_triaged(acct, "g2", "t2", "news@x.com", v1_at, false).ingest(&store);
    let rec = &store.list_unsubscribes(acct).unwrap()[0];
    assert_eq!(rec.violation_count, 1);
    assert_eq!(rec.last_violation_at, Some(v1_at));

    // Another past-grace message => second violation.
    inbound_triaged(acct, "g3", "t3", "news@x.com", t0 + chrono::Duration::hours(100), false)
        .ingest(&store);
    assert_eq!(store.list_unsubscribes(acct).unwrap()[0].violation_count, 2);

    // A FRESH request resets the ledger (clock restarts).
    let t_re = t0 + chrono::Duration::hours(200);
    store
        .upsert_unsubscribe(acct, "news@x.com", "browser", None, t_re)
        .unwrap();
    let rec = &store.list_unsubscribes(acct).unwrap()[0];
    assert_eq!(rec.violation_count, 0);
    assert!(rec.last_violation_at.is_none());
    assert!(rec.resolution.is_none());
}

#[test]
fn unsub_violation_ignores_resolved_and_sent_and_is_case_insensitive() {
    let (store, acct) = store();
    let t0 = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    store
        .upsert_unsubscribe(acct, "news@x.com", "browser", None, t0)
        .unwrap();

    // A SENT message past the grace never counts as a violation.
    inbound_triaged(acct, "gs", "ts", "news@x.com", t0 + chrono::Duration::hours(80), true)
        .ingest(&store);
    assert_eq!(store.list_unsubscribes(acct).unwrap()[0].violation_count, 0);

    // Mixed-case sender still matches the lowercased ledger key.
    inbound_triaged(acct, "g1", "t1", "News@X.com", t0 + chrono::Duration::hours(80), false)
        .ingest(&store);
    assert_eq!(store.list_unsubscribes(acct).unwrap()[0].violation_count, 1);

    // Once resolved, the detector is disarmed.
    assert!(store.set_unsubscribe_resolution(acct, "news@x.com", "blocked").unwrap());
    inbound_triaged(acct, "g2", "t2", "news@x.com", t0 + chrono::Duration::hours(100), false)
        .ingest(&store);
    assert_eq!(store.list_unsubscribes(acct).unwrap()[0].violation_count, 1);
}

#[test]
fn message_unsub_fields_reads_stored_headers_and_hides_sealed() {
    let (store, acct) = store();

    // Normal message carrying unsubscribe headers.
    let nid = triaged(acct, "g1", "t1")
        .from("News@Sub.com")
        .list_unsubscribe("<https://sub.com/u/1>", true)
        .importance(10)
        .seed(&store);
    let f = store.message_unsub_fields(acct, nid).unwrap().expect("present");
    assert_eq!(f.from_addr, "News@Sub.com");
    assert_eq!(f.list_unsubscribe.as_deref(), Some("<https://sub.com/u/1>"));
    assert!(f.list_unsub_one_click);

    // Sealed message => None (indistinguishable from unknown).
    let sid = triaged(acct, "g2", "t2")
        .list_unsubscribe("<https://sub.com/u/2>", false)
        .importance(90)
        .sealed(crate::types::SealedKind::Otp)
        .seed(&store);
    assert!(store.message_unsub_fields(acct, sid).unwrap().is_none());

    // Unknown id => None.
    assert!(store.message_unsub_fields(acct, 999_999).unwrap().is_none());
}

#[test]
fn sender_rules_round_trip() {
    let (store, acct) = store();
    let id = store
        .set_sender_rule(acct, "*@newsletter.com", "no marketing", Disposition::Squelch)
        .unwrap();
    assert!(id > 0);
    let rules = store.list_sender_rules(acct).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].disposition, Disposition::Squelch);
}

#[test]
fn update_sender_rule_edits_by_id_and_404s_unknown() {
    // Overwrites pattern/want/disposition by id; false for an unknown id.
    let (store, acct) = store();
    let id = store
        .set_sender_rule(acct, "*@old.com", "old want", Disposition::Squelch)
        .unwrap();

    let updated = store
        .update_sender_rule(acct, id, "*@new.com", "new want", Disposition::Surface)
        .unwrap();
    assert!(updated);
    let rules = store.list_sender_rules(acct).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].match_pattern, "*@new.com");
    assert_eq!(rules[0].want_text, "new want");
    assert_eq!(rules[0].disposition, Disposition::Surface);

    // Unknown id => false (handler turns this into 404).
    assert!(!store
        .update_sender_rule(acct, 9999, "*@x.com", "", Disposition::Squelch)
        .unwrap());
}

#[test]
fn set_sender_rule_audited_writes_both_rows() {
    let (store, acct) = store();
    let audit = NewAuditEntry {
        actor: "agent".into(),
        action: "rule.set".into(),
        target: Some("*@spam.com".into()),
        detail: Some("squelch: kill it".into()),
    };
    let id = store
        .set_sender_rule_audited(acct, "*@spam.com", "kill it", Disposition::Squelch, &audit)
        .unwrap();
    assert!(id > 0);

    let rules = store.list_sender_rules(acct).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].match_pattern, "*@spam.com");

    let log = store.list_audit(acct, 10).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].actor, "agent");
    assert_eq!(log[0].action, "rule.set");
    assert_eq!(log[0].target.as_deref(), Some("*@spam.com"));
}

#[test]
fn filtered_rules_reject_an_empty_want_text_on_every_write_path() {
    // A Filtered rule's want_text IS the rule ("filter everything except
    // <this>"). Empty, it degrades silently: `stage2_queue` maps it to None
    // and Stage-2 gets no instruction, while the UI still shows a rule. So
    // all three write paths reject it up front (`InvalidInput`).
    let (store, acct) = store();
    let audit = NewAuditEntry {
        actor: "agent".into(),
        action: "rule.set".into(),
        target: Some("*@vendor.com".into()),
        detail: None,
    };

    for want in ["", "   ", "\t\n"] {
        let e = store
            .set_sender_rule(acct, "*@vendor.com", want, Disposition::Filtered)
            .unwrap_err();
        assert!(matches!(e, CoreError::InvalidInput(_)), "set: {want:?}");
        let e = store
            .set_sender_rule_audited(acct, "*@vendor.com", want, Disposition::Filtered, &audit)
            .unwrap_err();
        assert!(matches!(e, CoreError::InvalidInput(_)), "set_audited: {want:?}");
    }
    // Nothing landed — no rule, and (fail-closed) no orphan audit row either.
    assert!(store.list_sender_rules(acct).unwrap().is_empty());
    assert!(store.list_audit(acct, 10).unwrap().is_empty());

    // The other dispositions don't require want_text; empty stays legal.
    let id = store
        .set_sender_rule(acct, "*@vendor.com", "", Disposition::Squelch)
        .unwrap();
    // ...but an UPDATE cannot smuggle the empty-want Filtered shape in.
    let e = store
        .update_sender_rule(acct, id, "*@vendor.com", " ", Disposition::Filtered)
        .unwrap_err();
    assert!(matches!(e, CoreError::InvalidInput(_)), "update path validates too");
    assert_eq!(store.list_sender_rules(acct).unwrap()[0].disposition, Disposition::Squelch);

    // With a real want_text, Filtered writes fine on both mutating paths.
    assert!(
        store
            .update_sender_rule(acct, id, "*@vendor.com", "only invoices", Disposition::Filtered)
            .unwrap()
    );
    store
        .set_sender_rule(acct, "*@other.com", "only receipts", Disposition::Filtered)
        .unwrap();
}

#[test]
fn set_sender_rule_audited_rolls_back_rule_when_audit_fails() {
    // FAIL-CLOSED: force the audit INSERT to error (drop the audit_log table)
    // and assert the rule write did NOT land — the whole tx rolled back.
    let (store, acct) = store();
    {
        let conn = store.lock().unwrap();
        conn.execute_batch("DROP TABLE audit_log").unwrap();
    }
    let audit = NewAuditEntry {
        actor: "agent".into(),
        action: "rule.set".into(),
        target: Some("*@spam.com".into()),
        detail: None,
    };
    let res =
        store.set_sender_rule_audited(acct, "*@spam.com", "kill it", Disposition::Squelch, &audit);
    assert!(res.is_err(), "audit failure must fail the whole call");
    // The rule write must have been rolled back.
    assert_eq!(store.list_sender_rules(acct).unwrap().len(), 0);
}
