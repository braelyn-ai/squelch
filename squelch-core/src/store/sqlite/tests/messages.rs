//! Ingest, thread view, attachment and sealed-read tests.

use super::super::*;
use super::support::*;
use crate::types::{SealedKind, Sensitivity, Tier};

#[test]
fn ingest_message_persists_attachments_and_thread_view_carries_them() {
    use crate::config::Stage1Config;
    let (store, acct) = store();

    let eml = "From: S <s@ex.com>\r\n\
               To: me@example.com\r\n\
               Subject: files\r\n\
               Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
               MIME-Version: 1.0\r\n\
               Content-Type: multipart/mixed; boundary=\"B\"\r\n\
               \r\n\
               --B\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
               --B\r\nContent-Type: application/pdf\r\n\
               Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
               Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n\
               --B--\r\n";
    let fetched = crate::sync::ingest::RawFetched {
        account_id: acct,
        gmail_msg_id: "g1".into(),
        gmail_thread_id: Some("t1".into()),
        raw: eml.as_bytes().to_vec(),
        internal_date: Some(Utc::now()),
        is_sent: false,
        account_addr: "me@example.com".into(),
    };
    let t = crate::sync::ingest::ingest(&fetched, &Stage1Config::default(), Utc::now(), |_| false);
    assert_eq!(t.attachments.len(), 1, "one pdf attachment extracted");
    let mid = store.ingest_message(&t).unwrap();

    // Thread view carries the attachment metadata (downloadable = true).
    let view = store.thread_view_with_html(acct, "t1").unwrap();
    assert_eq!(view.messages.len(), 1);
    let atts = &view.messages[0].attachments;
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0].filename, "doc.pdf");
    assert_eq!(atts[0].mime, "application/pdf");
    assert_eq!(atts[0].size, 5);
    assert!(atts[0].downloadable);

    // Bytes come back through attachment_bytes.
    let got = store.attachment_bytes(acct, atts[0].id).unwrap().expect("bytes");
    assert_eq!(got.0, "doc.pdf");
    assert_eq!(got.2.as_deref(), Some(&b"Hello"[..]));

    // Re-ingest is idempotent: still exactly one attachment row.
    let mid2 = store.ingest_message(&t).unwrap();
    assert_eq!(mid, mid2);
    let view2 = store.thread_view_with_html(acct, "t1").unwrap();
    assert_eq!(view2.messages[0].attachments.len(), 1, "re-ingest must not duplicate");
}

#[test]
fn double_attached_identical_file_cannot_kill_ingest() {
    // REMOTE INGEST DoS: two parts with the SAME filename and size violate
    // the UNIQUE key, which must collapse to one row rather than roll back
    // the whole message ingest.
    use crate::config::Stage1Config;
    let (store, acct) = store();

    let eml = "From: S <s@ex.com>\r\n\
               To: me@example.com\r\n\
               Subject: dup\r\n\
               Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
               MIME-Version: 1.0\r\n\
               Content-Type: multipart/mixed; boundary=\"B\"\r\n\
               \r\n\
               --B\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
               --B\r\nContent-Type: application/pdf\r\n\
               Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
               Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n\
               --B\r\nContent-Type: application/pdf\r\n\
               Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
               Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n\
               --B--\r\n";
    let fetched = crate::sync::ingest::RawFetched {
        account_id: acct,
        gmail_msg_id: "g-dup".into(),
        gmail_thread_id: Some("t-dup".into()),
        raw: eml.as_bytes().to_vec(),
        internal_date: Some(Utc::now()),
        is_sent: false,
        account_addr: "me@example.com".into(),
    };
    let t = crate::sync::ingest::ingest(&fetched, &Stage1Config::default(), Utc::now(), |_| false);
    assert_eq!(t.attachments.len(), 2, "both parts extracted");
    // The ingest MUST NOT error — identical duplicates collapse to one row.
    store.ingest_message(&t).expect("duplicate attachments must not fail ingest");
    let view = store.thread_view_with_html(acct, "t-dup").unwrap();
    assert_eq!(view.messages[0].attachments.len(), 1);
}

#[test]
fn attachment_bytes_guards_sealed_overcap_and_unknown() {
    let (store, acct) = store();

    // Normal parent with a stored attachment and an over-cap (NULL data) one.
    let mid = triaged(acct, "g1", "t1").importance(10).seed(&store);
    let a_ok = store
        .insert_attachment(acct, mid, "doc.pdf", "application/pdf", 5, Some(b"Hello"))
        .unwrap();
    let a_over = store
        .insert_attachment(acct, mid, "big.bin", "application/octet-stream", 11_000_000, None)
        .unwrap();

    // Normal parent, bytes present.
    let ok = store.attachment_bytes(acct, a_ok).unwrap().expect("row");
    assert_eq!(ok.2.as_deref(), Some(&b"Hello"[..]));

    // Over-cap: row resolves but data is None (endpoint -> 410).
    let over = store.attachment_bytes(acct, a_over).unwrap().expect("metadata row exists");
    assert!(over.2.is_none(), "over-cap attachment carries no bytes");

    // Unknown id -> None (endpoint -> 404).
    assert!(store.attachment_bytes(acct, 999_999).unwrap().is_none());

    // Sealed parent: attachment is stored, but attachment_bytes hides it
    // (returns None, indistinguishable from unknown -> 404).
    let sid = triaged(acct, "g2", "t2").sealed(SealedKind::Otp).seed(&store);
    let sealed_att = store
        .insert_attachment(acct, sid, "secret.pdf", "application/pdf", 6, Some(b"secret"))
        .unwrap();
    assert!(
        store.attachment_bytes(acct, sealed_att).unwrap().is_none(),
        "sealed parent must hide its attachment bytes"
    );
}

#[test]
fn field_reasons_roundtrip_through_ingest_and_attention_updates() {
    use crate::types::FieldReasons;
    let (store, acct) = store();

    // Build a normal inbound TriagedMessage carrying per-property reasons.
    let id = inbound_triaged(acct, "g1", "t1", "boss@work.com", Utc::now(), false)
        .importance(72)
        .tier(Tier::Signal)
        .reason("known contact")
        .field_reasons(FieldReasons {
            importance: Some("known contact -> signal importance 72".into()),
            deadline: None,
            tier: Some("known contact -> signal".into()),
        })
        .ingest(&store);

    // HUMAN DOOR: attention_updates carries the parsed field_reasons.
    let ups = store
        .attention_updates(acct, Utc::now() - chrono::Duration::days(1), None, None, None)
        .unwrap();
    let u = ups.iter().find(|u| u.update.id == id).expect("row present");
    let fr = u.update.field_reasons.as_ref().expect("field_reasons present");
    assert_eq!(fr.importance.as_deref(), Some("known contact -> signal importance 72"));
    assert_eq!(fr.tier.as_deref(), Some("known contact -> signal"));
    assert!(fr.deadline.is_none());
    // And it serializes into the /client/updates JSON as an object.
    let v = serde_json::to_value(&u.update).unwrap();
    assert_eq!(v["field_reasons"]["tier"], serde_json::json!("known contact -> signal"));

    // AGENT DOOR: ranked_updates (MCP) never carries field_reasons — the key
    // is absent from the serialized Update.
    let ranked = store
        .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), None)
        .unwrap();
    let r = ranked.iter().find(|u| u.id == id).expect("row present");
    assert!(r.field_reasons.is_none());
    let rv = serde_json::to_value(r).unwrap();
    assert!(rv.get("field_reasons").is_none(), "MCP payload must omit field_reasons: {rv}");
    // Same byte-absence discipline for the paperclip flag.
    assert!(r.has_attachments.is_none());
    assert!(
        rv.get("has_attachments").is_none(),
        "MCP payload must omit has_attachments: {rv}"
    );
}

#[test]
fn predating_triage_row_reads_back_as_none() {
    // A row written with no field_reasons (NULL column) reads back as None.
    let (store, acct) = store();
    let mid = triaged(acct, "g1", "t1")
        .importance(60)
        .tier(Tier::Signal)
        .one_line("x")
        .reason("y")
        .seed(&store);
    let ups = store
        .attention_updates(acct, Utc::now() - chrono::Duration::days(1), None, None, None)
        .unwrap();
    let u = ups.iter().find(|u| u.update.id == mid).unwrap();
    assert!(u.update.field_reasons.is_none());
}

#[test]
fn round_trips_a_message() {
    let (store, acct) = store();
    triaged(acct, "g1", "t1")
        .importance(80)
        .tier(Tier::Signal)
        .one_line("Lunch invite")
        .reason("known contact")
        .seed(&store);

    let updates = store
        .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), Some(1))
        .unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].sender, "alice@example.com");
    assert_eq!(updates[0].tier, Tier::Signal);

    let tv = store.thread_view(acct, "t1").unwrap();
    assert_eq!(tv.messages.len(), 1);
    assert_eq!(tv.subject, "Lunch?");
}

/// `thread_id_for_message` (the get_thread forgiveness fallback) resolves a
/// normal message id to its thread, returns None for an unknown id, and
/// returns None for a SEALED message id — so a sealed id is indistinguishable
/// from a nonexistent one and never leaks thread existence.
#[test]
fn thread_id_for_message_resolves_normal_and_hides_sealed() {
    let (store, acct) = store();

    let normal = triaged(acct, "g1", "t1")
        .importance(80)
        .tier(Tier::Signal)
        .seed(&store);
    let sealed = triaged(acct, "g2", "t2")
        .importance(90)
        .sealed(SealedKind::Otp)
        .seed(&store);

    assert_eq!(
        store.thread_id_for_message(acct, normal).unwrap().as_deref(),
        Some("t1")
    );
    assert_eq!(store.thread_id_for_message(acct, 999_999).unwrap(), None);
    assert_eq!(
        store.thread_id_for_message(acct, sealed).unwrap(),
        None,
        "sealed message id must not resolve (no thread-existence leak)"
    );
}

#[test]
fn sealed_rows_absent_from_updates_but_present_in_sealed_messages() {
    let (store, acct) = store();

    // A normal message.
    triaged(acct, "g1", "t1")
        .importance(80)
        .tier(Tier::Signal)
        .one_line("Lunch")
        .seed(&store);

    // A sealed OTP message in a different thread.
    triaged(acct, "g2", "t2")
        .subject("Your verification code")
        .from("noreply@bank.com")
        .importance(90)
        .sealed(SealedKind::Otp)
        .one_line("code")
        .reason("otp")
        .seed(&store);

    // ranked_updates must NOT include the sealed row.
    let updates = store
        .ranked_updates(acct, Utc::now() - chrono::Duration::days(1), None)
        .unwrap();
    assert_eq!(updates.len(), 1);
    assert!(updates.iter().all(|u| u.thread_id != "t2"));

    // thread_view on the sealed thread => NotFound.
    let err = store.thread_view(acct, "t2").unwrap_err();
    assert!(matches!(err, CoreError::NotFound));

    // Nonexistent thread also => NotFound (indistinguishable).
    let err2 = store.thread_view(acct, "does-not-exist").unwrap_err();
    assert!(matches!(err2, CoreError::NotFound));

    // The human-door html variant enforces the SAME guard: a sealed thread
    // (and a nonexistent one) are both NotFound, so html never leaks a
    // sealed thread either.
    assert!(matches!(
        store.thread_view_with_html(acct, "t2").unwrap_err(),
        CoreError::NotFound
    ));
    assert!(matches!(
        store
            .thread_view_with_html(acct, "does-not-exist")
            .unwrap_err(),
        CoreError::NotFound
    ));

    // sealed_messages (local-only) DOES surface it.
    let sealed = store.sealed_messages(acct).unwrap();
    assert_eq!(sealed.len(), 1);
    assert_eq!(sealed[0].thread_id, "t2");
    assert_eq!(sealed[0].sealed_kind.as_deref(), Some("otp"));
}

#[test]
fn deadlines_exclude_sealed_source() {
    let (store, acct) = store();
    let mid = triaged(acct, "g1", "t1")
        .importance(50)
        .tier(Tier::Deadline)
        .sensitivity(Sensitivity::Sealed)
        .seed(&store);

    {
        let conn = store.lock().unwrap();
        conn.execute(
            "INSERT INTO deadlines(account_id, message_id, kind, due_at, past_due, source)
             VALUES(?1,?2,'bill',?3,0,'regex')",
            params![acct, mid, (Utc::now() + chrono::Duration::days(2)).to_rfc3339()],
        )
        .unwrap();
    }

    let ds = store.deadlines(acct, Some(30)).unwrap();
    assert!(ds.is_empty(), "sealed-source deadline must be hidden");
}

#[test]
fn sealed_body_reveal_audit_and_stats() {
    let (store, acct) = store();

    let s = triaged(acct, "g1", "t1")
        .body("secret 123456")
        .importance(90)
        .sealed(SealedKind::Otp)
        .seed(&store);

    let nid = triaged(acct, "g2", "t2")
        .importance(80)
        .tier(Tier::Signal)
        .seed(&store);

    // sealed_body returns only for the sealed message.
    let body = store.sealed_body(acct, s).unwrap();
    assert_eq!(body.body, "secret 123456");
    assert!(matches!(
        store.sealed_body(acct, nid).unwrap_err(),
        CoreError::NotFound
    ));

    // audit append + list
    let aid = store
        .append_audit(
            acct,
            &crate::store::NewAuditEntry {
                actor: "human".into(),
                action: "reveal_sealed".into(),
                target: Some(s.to_string()),
                detail: None,
            },
        )
        .unwrap();
    assert!(aid > 0);
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "reveal_sealed");

    // stats: 1 signal (t2), 1 sealed.
    let stats = store.stats(acct).unwrap();
    assert_eq!(stats.total, 1);
    assert_eq!(stats.tier_counts.get("signal").copied(), Some(1));
    assert_eq!(stats.sealed, 1);
}

#[test]
fn reingest_preserves_llm_classification_but_refreshes_heuristic_rows() {
    let (store, acct) = store();
    let since = Utc::now() - chrono::Duration::days(3650);

    // --- Row A: LLM-classified, then re-delivered. ---
    let a = triaged_row(acct, "g-a", "t-a", None, false, Sensitivity::Normal).ingest(&store);
    // Stage-1 refines it with a REAL model id + distinctive values.
    store
        .stage1_apply(&Stage1Applied {
            message_id: a,
            account_id: acct,
            importance: 88,
            tier: Tier::Signal,
            one_line: "LLM verdict".into(),
            reason: "stage-1 refined".into(),
            field_reasons: crate::types::FieldReasons::default(),
            stage1_model_used: "claude-haiku-4-5".into(),
            needs_stage2: false,
            deadline: None,
            category: Some("general".into()),
        })
        .unwrap();
    // Re-deliver the SAME message (heuristic seed carries importance 40).
    triaged_row(acct, "g-a", "t-a", None, false, Sensitivity::Normal).ingest(&store);
    let ups = store.ranked_updates(acct, since, None).unwrap();
    let ua = ups.iter().find(|u| u.id == a).expect("row A present");
    assert_eq!(ua.importance, 88, "paid LLM importance preserved on re-ingest");
    assert_eq!(ua.one_line, "LLM verdict", "paid LLM one_line preserved");
    assert_eq!(ua.tier, Tier::Signal, "paid LLM tier preserved");

    // --- Row B: still heuristic-only -> re-ingest refreshes the seed. ---
    let b = triaged_row(acct, "g-b", "t-b", None, false, Sensitivity::Normal).ingest(&store);
    triaged_row(acct, "g-b", "t-b", None, false, Sensitivity::Normal)
        .importance(71)
        .tier(Tier::Signal)
        .one_line("fresh seed")
        .ingest(&store);
    let ups = store.ranked_updates(acct, since, None).unwrap();
    let ub = ups.iter().find(|u| u.id == b).expect("row B present");
    assert_eq!(ub.importance, 71, "still-heuristic row adopts the new seed");
    assert_eq!(ub.one_line, "fresh seed");
}
