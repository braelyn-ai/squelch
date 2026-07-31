//! Audit-log tests.

use super::super::*;
use super::support::*;

#[test]
fn list_audit_enriches_message_target_and_nulls_non_numeric() {
    let (store, acct) = store();

    // A stored message: from_name present => sender is the name; subject verbatim.
    let mid = triaged(acct, "g-audit", "t-audit")
        .from("news@sub.com")
        .from_name(Some("Newsletter Co"))
        .subject("Weekly digest")
        .upsert(&store);

    // Row 1: target is the message id -> enriched.
    store
        .append_audit(
            acct,
            &NewAuditEntry {
                actor: "client-api".into(),
                action: "unsubscribe".into(),
                target: Some(mid.to_string()),
                detail: Some("browser:news@sub.com".into()),
            },
        )
        .unwrap();
    // Row 2: non-numeric target (a rule pattern) -> nulls, no error.
    store
        .append_audit(
            acct,
            &NewAuditEntry {
                actor: "client-api".into(),
                action: "rule.create".into(),
                target: Some("*@spam.com".into()),
                detail: Some("42".into()),
            },
        )
        .unwrap();
    // Row 3: numeric target that is NOT a known message id -> nulls.
    store
        .append_audit(
            acct,
            &NewAuditEntry {
                actor: "client-api".into(),
                action: "archive".into(),
                target: Some("999999".into()),
                detail: Some("ok".into()),
            },
        )
        .unwrap();

    let log = store.list_audit(acct, 10).unwrap();
    assert_eq!(log.len(), 3);

    let unsub = log.iter().find(|a| a.action == "unsubscribe").unwrap();
    assert_eq!(unsub.target_sender.as_deref(), Some("Newsletter Co"));
    assert_eq!(unsub.target_subject.as_deref(), Some("Weekly digest"));

    let rule = log.iter().find(|a| a.action == "rule.create").unwrap();
    assert!(rule.target_sender.is_none(), "non-numeric target yields no enrichment");
    assert!(rule.target_subject.is_none());

    let arch = log.iter().find(|a| a.action == "archive").unwrap();
    assert!(arch.target_sender.is_none(), "unknown message id yields no enrichment");
    assert!(arch.target_subject.is_none());
}
