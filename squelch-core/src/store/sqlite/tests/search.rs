//! Keyword, semantic and hybrid recall tests.

use super::super::*;
use super::support::*;
use crate::types::{SealedKind, Tier};

#[test]
fn search_excludes_sealed_and_delete_rule_works() {
    let (store, acct) = store();

    triaged(acct, "g1", "t1")
        .subject("verification steps")
        .body("how to verify your account")
        .importance(60)
        .tier(Tier::Signal)
        .seed(&store);

    triaged(acct, "g2", "t2")
        .subject("verification code")
        .body("code 999")
        .importance(90)
        .sealed(SealedKind::Otp)
        .seed(&store);

    let hits = store.search(acct, "verification", 10, 0).unwrap();
    assert_eq!(hits.len(), 1, "sealed row must be excluded from search");
    assert_eq!(hits[0].thread_id, "t1");

    // delete_sender_rule
    let rid = store
        .set_sender_rule(acct, "*@x.com", "no", Disposition::Squelch)
        .unwrap();
    assert!(store.delete_sender_rule(acct, rid).unwrap());
    assert!(!store.delete_sender_rule(acct, rid).unwrap());
    assert!(store.list_sender_rules(acct).unwrap().is_empty());
}

// ---- SEMANTIC RECALL (v1) --------------------------------------------
//
// These exercise the vec0 index + gating with a deterministic, download-free
// `StubEmbedder`, so the SQL/gating/ranking are covered offline. The e2e test
// against the real fastembed model is feature-gated behind an env var
// (SQUELCH_EMBED_E2E) so CI never downloads weights.

use crate::embed::{Embedder, StubEmbedder};

use std::sync::Arc;

#[test]
fn sealed_message_is_never_embedded() {
    // The structural gate lives at the CALLER, so `messages_missing_vectors`
    // — the backfill's source — must NEVER return a sealed row. Both halves
    // are asserted: absent from the list, and its vec slot stays empty.
    let (store, acct) = store();

    // A normal message and a sealed OTP.
    let normal = triaged(acct, "g1", "t1")
        .importance(70)
        .tier(Tier::Signal)
        .seed(&store);

    let sealed = triaged(acct, "g2", "t2")
        .subject("Your verification code")
        .body("code 123456")
        .importance(90)
        .sealed(SealedKind::Otp)
        .seed(&store);

    // messages_missing_vectors returns the normal row, NEVER the sealed one.
    let missing = store.messages_missing_vectors(acct, 10).unwrap();
    assert!(missing.iter().any(|m| m.message_id == normal));
    assert!(
        !missing.iter().any(|m| m.message_id == sealed),
        "sealed message must be structurally absent from the backfill source"
    );

    // Simulate the backfill embedding only what it was handed: the sealed row
    // gets no vector.
    let embedder = StubEmbedder::new(VEC_DIMS);
    for m in &missing {
        embed_and_store(&store, &embedder, acct, m.message_id, &m.subject, &m.body);
    }
    assert_eq!(vec_count_for(&store, sealed), 0, "sealed row has no vector");
    assert_eq!(vec_count_for(&store, normal), 1, "normal row was embedded");
}

#[test]
fn sent_raw_body_is_stored_and_embeddable() {
    // A SENT message stores its full body (recall covers what the USER
    // wrote), and that body flows through the missing-vector backfill so it
    // becomes embeddable — even though sent mail is excluded from triage.
    let (store, acct) = store();

    // Sent mail ingests with a neutral normal-sensitivity triage row.
    let id = triaged(acct, "g-sent", "t-sent")
        .is_sent(true)
        .subject("re: the design doc")
        .body("I'll send you the revised design doc by Friday.")
        .seed(&store);

    // The raw body is stored verbatim.
    {
        let conn = store.lock().unwrap();
        let body: String = conn
            .query_row("SELECT body FROM messages WHERE id=?1", params![id], |r| r.get(0))
            .unwrap();
        assert!(body.contains("revised design doc by Friday"));
    }

    // And it is a backfill candidate (sent mail is embeddable for recall).
    let missing = store.messages_missing_vectors(acct, 10).unwrap();
    let row = missing
        .iter()
        .find(|m| m.message_id == id)
        .expect("sent message is a missing-vector candidate");
    assert!(row.body.contains("revised design doc"));

    let embedder = StubEmbedder::new(VEC_DIMS);
    embed_and_store(&store, &embedder, acct, id, &row.subject, &row.body);
    assert_eq!(vec_count_for(&store, id), 1);
}

#[test]
fn semantic_search_ranks_relevant_above_decoy_and_includes_sent() {
    // Plant a relevant SENT doc and an unrelated decoy; the query about what
    // the user said they'd send must rank the relevant doc first. Sent mail is
    // INCLUDED (recall wants it) — unlike keyword `search`, which excludes it.
    let embedder = Arc::new(StubEmbedder::new(VEC_DIMS));
    let (store, acct) = store_with_embedder(embedder.clone());

    // Relevant: the user promised to send an invoice.
    let rel = triaged(acct, "g-rel", "t-rel")
        .is_sent(true)
        .subject("invoice")
        .body("Hi Dana, I will send you the invoice for the consulting work tomorrow.")
        .seed(&store);

    // Decoy: completely unrelated received mail.
    let dec = triaged(acct, "g-dec", "t-dec")
        .subject("weekend hiking trip")
        .body("The mountain trail was gorgeous and the weather held up nicely.")
        .importance(20)
        .seed(&store);

    // Embed both through the missing-vector path (mirrors backfill).
    for m in store.messages_missing_vectors(acct, 10).unwrap() {
        embed_and_store(&store, &*embedder, acct, m.message_id, &m.subject, &m.body);
    }

    let hits = store
        .semantic_search(acct, "did I say I would send the invoice", 5)
        .unwrap();
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(hits[0].0, rel, "the relevant sent doc must rank first");
    // The decoy, if present, ranks strictly worse (larger distance).
    if let Some(d) = hits.iter().find(|(id, _)| *id == dec) {
        assert!(d.1 >= hits[0].1, "decoy must not beat the relevant doc");
    }
}

#[test]
fn semantic_search_excludes_sealed_even_if_a_vector_leaked() {
    // BELT-AND-SUSPENDERS: vectors are never written for sealed mail, but if a
    // vector somehow existed, semantic_search's re-join to triage must still
    // drop it. We force the pathological case by inserting a vector directly.
    let embedder = Arc::new(StubEmbedder::new(VEC_DIMS));
    let (store, acct) = store_with_embedder(embedder.clone());

    let sealed = triaged(acct, "g-seal", "t-seal")
        .subject("verification code")
        .body("your one time passcode is 999111")
        .importance(90)
        .sealed(SealedKind::Otp)
        .seed(&store);

    // Pathological: write a vector for the sealed row anyway (bypassing the gate).
    embed_and_store(&store, &*embedder, acct, sealed, "verification code",
        "your one time passcode is 999111");
    assert_eq!(vec_count_for(&store, sealed), 1, "vector was forced in");

    // semantic_search must STILL never return it (re-join drops sealed).
    let hits = store
        .semantic_search(acct, "verification code passcode", 5)
        .unwrap();
    assert!(
        !hits.iter().any(|(id, _)| *id == sealed),
        "sealed row must be excluded by the query-time re-join"
    );
}

#[test]
fn hybrid_search_fuses_keyword_and_vector_and_includes_sent() {
    // RRF hybrid: a sent doc that both keyword-matches and vector-matches the
    // query should surface. Confirms hybrid_search returns SearchHits and
    // includes sent mail (recall).
    const SUBJECT: &str = "contract";
    const BODY: &str = "I promised to send the signed contract to the vendor.";
    let embedder = Arc::new(StubEmbedder::new(VEC_DIMS));
    let (store, acct) = store_with_embedder(embedder.clone());

    let id = triaged(acct, "g-h", "t-h")
        .is_sent(true)
        .subject(SUBJECT)
        .body(BODY)
        .seed(&store);
    embed_and_store(&store, &*embedder, acct, id, SUBJECT, BODY);

    let hits = store.hybrid_search(acct, "signed contract vendor", 5).unwrap();
    assert!(
        hits.iter().any(|h| h.id == id),
        "hybrid search must surface the matching sent doc (recall includes sent mail)"
    );
}

#[test]
fn embedder_dims_mismatch_is_rejected_at_attach() {
    // The store asserts the embedder width matches the vec0 table at attach.
    let wrong = Arc::new(StubEmbedder::new(VEC_DIMS + 1));
    // `SqliteStore` is not `Debug`, so match on the Result rather than
    // `unwrap_err()` (which would require `Ok: Debug`).
    match SqliteStore::open_in_memory().unwrap().with_embedder(wrong) {
        Ok(_) => panic!("dims mismatch must be rejected at attach"),
        Err(e) => assert!(matches!(e, CoreError::InvalidInput(_))),
    }
}

#[test]
fn keyword_search_works_before_embedder_then_attaches_live() {
    // The serve-bind model: the store is already SHARED behind an Arc and
    // serving with NO embedder yet, so hybrid_search must work keyword-only,
    // semantic_search must fail gracefully, and an attach on &self must swap
    // the embedder in live with no restart.
    const SUBJECT: &str = "quarterly invoice";
    const BODY: &str = "The quarterly invoice from Acme is attached.";
    let (store, acct) = store();
    let store = Arc::new(store);

    let id = triaged(acct, "g-kw", "t-kw")
        .subject(SUBJECT)
        .body(BODY)
        .seed(&store);

    // 1) Keyword-only hybrid search returns the doc with no embedder attached.
    assert!(store.embedder().is_none(), "no embedder before background attach");
    let hits = store.hybrid_search(acct, "quarterly invoice", 5).unwrap();
    assert!(
        hits.iter().any(|h| h.id == id),
        "hybrid_search must return keyword hits before the embedder is ready"
    );

    // 2) Semantic search has nothing to embed against yet.
    assert!(store.semantic_search(acct, "quarterly invoice", 5).is_err());

    // 3) Background attach (post-Arc, &self) — the serve-bind mechanism.
    let embedder = Arc::new(StubEmbedder::new(VEC_DIMS));
    let prev = store.attach_embedder(embedder.clone()).unwrap();
    assert!(prev.is_none(), "no previous embedder");
    assert!(store.embedder().is_some(), "embedder attached live");

    // 4) Now embed the row and prove semantic recall works without any restart.
    embed_and_store(&store, &*embedder, acct, id, SUBJECT, BODY);
    let sem = store.semantic_search(acct, "quarterly invoice", 5).unwrap();
    assert!(
        sem.iter().any(|(hid, _)| *hid == id),
        "semantic_search must work once the embedder attaches — no rebind/restart"
    );
}

/// E2E against the REAL fastembed model. Gated behind SQUELCH_EMBED_E2E so CI
/// never downloads ONNX weights. Run with:
///   SQUELCH_EMBED_E2E=1 cargo test -p squelch-core embed_e2e
#[test]
fn embed_e2e_real_model_ranks_relevant_first() {
    if std::env::var("SQUELCH_EMBED_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping embed_e2e (set SQUELCH_EMBED_E2E=1 to run)");
        return;
    }
    use crate::config::EmbedConfig;
    use crate::embed::FastEmbedder;

    let embedder: Arc<dyn Embedder> =
        Arc::new(FastEmbedder::new(&EmbedConfig::default().settings()).unwrap());
    let (store, acct) = store_with_embedder(embedder.clone());

    let rel = triaged(acct, "g-rel", "t-rel")
        .is_sent(true)
        .subject("invoice")
        .body("I will send over the invoice for last month's work by end of day.")
        .seed(&store);

    let dec = triaged(acct, "g-dec", "t-dec")
        .subject("lunch")
        .body("Want to grab tacos on Thursday?")
        .seed(&store);

    for m in store.messages_missing_vectors(acct, 10).unwrap() {
        embed_and_store(&store, &*embedder, acct, m.message_id, &m.subject, &m.body);
    }

    let hits = store
        .semantic_search(acct, "when did I promise to send the invoice?", 5)
        .unwrap();
    assert_eq!(hits[0].0, rel, "real model must rank the invoice doc first");
    assert!(hits.iter().any(|(id, _)| *id == dec), "decoy present but lower");
}
