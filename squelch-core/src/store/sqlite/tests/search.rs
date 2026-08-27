//! Keyword, semantic and hybrid recall tests.

use super::super::*;
use super::support::*;
use crate::store::{SearchFilter, parse_search_query};
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

// ---- MATCH-WINDOW SNIPPETS -------------------------------------------
//
// The hit's snippet is cut around the matched terms rather than being the
// stored head-of-message text, so a body-deep hit shows WHY it matched.

/// A long body whose only occurrence of the search term sits far past anything
/// a head-of-message snippet would ever include.
const DEEP_BODY: &str = "Thanks for subscribing to the weekly digest. \
    This week we cover garden tools, the spring planting calendar, a reader \
    letter about compost, and our usual roundup of local events. Scroll on for \
    the rest of it. Finally, a note on the pangolin conservation fundraiser we \
    are hosting next month at the community hall.";

#[test]
fn keyword_snippet_is_the_match_window_not_the_stored_head() {
    let (store, acct) = store();

    triaged(acct, "g1", "t1")
        .subject("weekly digest")
        .snippet("Thanks for subscribing to the weekly digest.")
        .body(DEEP_BODY)
        .seed(&store);

    let hits = store.search(acct, "pangolin", 10, 0).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].snippet.contains("pangolin"),
        "snippet must be the match window, got {:?}",
        hits[0].snippet
    );
    assert!(
        !hits[0].snippet.starts_with("Thanks for subscribing"),
        "the stored head-of-message snippet must not be what we return"
    );
    assert!(
        !hits[0].snippet.contains("<b>") && !hits[0].snippet.contains("["),
        "no highlight markers — the client paints the terms itself"
    );
}

#[test]
fn snippet_falls_back_to_the_stored_head_when_the_window_is_empty() {
    // A body-less message has no window to cut, so COALESCE/NULLIF keeps the
    // stored snippet rather than handing the client an empty string.
    let (store, acct) = store();

    triaged(acct, "g1", "t1")
        .subject("pangolin fundraiser")
        .snippet("stored head text")
        .body("")
        .seed(&store);

    let hits = store.search(acct, "pangolin", 10, 0).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].snippet, "stored head text");
}

// ---- QUERY OPERATORS -------------------------------------------------

fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    use chrono::{NaiveDate, TimeZone};
    Utc.from_utc_datetime(
        &NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    )
}

/// Three dated invoices from two senders, plus a sealed one and a sent one that
/// must never appear on the human door's search.
fn seed_operator_corpus(store: &SqliteStore, acct: AccountId) {
    triaged(acct, "g-jan", "t-jan")
        .from("jane@example.com")
        .from_name(Some("Jane Doe"))
        .subject("invoice january")
        .body("the january invoice is attached")
        .received_at(day(2026, 1, 15))
        .seed(store);

    triaged(acct, "g-feb", "t-feb")
        .from("jane@example.com")
        .from_name(Some("Jane Doe"))
        .subject("invoice february")
        .body("the february invoice is attached")
        .received_at(day(2026, 2, 15))
        .seed(store);

    triaged(acct, "g-bob", "t-bob")
        .from("bob@other.test")
        .from_name(Some("Bob"))
        .subject("invoice from bob")
        .body("bob's invoice is attached")
        .received_at(day(2026, 2, 20))
        .seed(store);

    triaged(acct, "g-seal", "t-seal")
        .from("jane@example.com")
        .subject("invoice verification code")
        .body("code 123456")
        .received_at(day(2026, 2, 16))
        .sealed(SealedKind::Otp)
        .seed(store);

    triaged(acct, "g-sent", "t-sent")
        .from("jane@example.com")
        .is_sent(true)
        .subject("re: invoice")
        .body("sending the invoice back")
        .received_at(day(2026, 2, 17))
        .seed(store);
}

#[test]
fn keyword_search_applies_from_and_date_filters() {
    let (store, acct) = store();
    seed_operator_corpus(&store, acct);

    let (text, filter) = parse_search_query("invoice from:jane");
    let hits = store.search_filtered(acct, &text, &filter, 10, 0).unwrap();
    let threads: Vec<&str> = hits.iter().map(|h| h.thread_id.as_str()).collect();
    assert_eq!(
        threads.len(),
        2,
        "jane's two received invoices: {threads:?}"
    );
    assert!(threads.contains(&"t-jan") && threads.contains(&"t-feb"));
    assert!(
        !threads.contains(&"t-seal"),
        "sealed stays absent with operators applied"
    );
    assert!(!threads.contains(&"t-sent"), "sent mail stays excluded");

    // Display-name match, not just the address.
    let (text, filter) = parse_search_query(r#"invoice from:"jane doe""#);
    assert_eq!(
        store
            .search_filtered(acct, &text, &filter, 10, 0)
            .unwrap()
            .len(),
        2
    );

    // after: is inclusive of midnight on the named day; before: is exclusive.
    let (text, filter) = parse_search_query("invoice after:2026-02-01");
    let hits = store.search_filtered(acct, &text, &filter, 10, 0).unwrap();
    let threads: Vec<&str> = hits.iter().map(|h| h.thread_id.as_str()).collect();
    assert_eq!(threads.len(), 2, "february only: {threads:?}");
    assert!(!threads.contains(&"t-jan"));

    let (text, filter) = parse_search_query("invoice before:2026-02-16");
    let hits = store.search_filtered(acct, &text, &filter, 10, 0).unwrap();
    let threads: Vec<&str> = hits.iter().map(|h| h.thread_id.as_str()).collect();
    assert_eq!(threads.len(), 2, "before the 16th: {threads:?}");
    assert!(!threads.contains(&"t-bob"));

    // Both bounds plus a sender: exactly the february invoice from Jane.
    let (text, filter) = parse_search_query("invoice from:jane after:2026-02-01 before:2026-03-01");
    let hits = store.search_filtered(acct, &text, &filter, 10, 0).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].thread_id, "t-feb");
}

#[test]
fn filter_only_listing_lists_newest_first_and_keeps_the_exclusions() {
    let (store, acct) = store();
    seed_operator_corpus(&store, acct);

    // Operators with no words left over: no FTS MATCH runs at all.
    let (text, filter) = parse_search_query("from:jane");
    assert!(text.is_empty(), "nothing to rank on");
    let hits = store.search_filtered(acct, &text, &filter, 10, 0).unwrap();
    let threads: Vec<&str> = hits.iter().map(|h| h.thread_id.as_str()).collect();
    assert_eq!(threads, vec!["t-feb", "t-jan"], "newest first");

    // The listing is a search path like any other: sealed and sent stay out.
    let (text, filter) = parse_search_query("after:2026-01-01");
    let hits = store.search_filtered(acct, &text, &filter, 10, 0).unwrap();
    let threads: Vec<&str> = hits.iter().map(|h| h.thread_id.as_str()).collect();
    assert_eq!(threads, vec!["t-bob", "t-feb", "t-jan"]);
    assert!(!threads.contains(&"t-seal") && !threads.contains(&"t-sent"));

    // And it paginates.
    let page = store.search_filtered(acct, "", &filter, 2, 0).unwrap();
    assert_eq!(page.len(), 2);
    let page2 = store.search_filtered(acct, "", &filter, 2, 2).unwrap();
    assert_eq!(page2.len(), 1);
    assert_eq!(page2[0].thread_id, "t-jan");
}

#[test]
fn from_filter_treats_like_wildcards_as_literal_text() {
    // `%` in the reader's value must match a literal percent sign, not every
    // sender — the ESCAPE clause plus value escaping, end to end.
    let (store, acct) = store();

    triaged(acct, "g1", "t1")
        .from("plain@example.com")
        .from_name(Some("Plain"))
        .subject("invoice")
        .body("body one")
        .seed(&store);

    triaged(acct, "g2", "t2")
        .from("odd%name@example.com")
        .from_name(Some("Odd"))
        .subject("invoice")
        .body("body two")
        .seed(&store);

    let (text, filter) = parse_search_query("invoice from:%");
    let hits = store.search_filtered(acct, &text, &filter, 10, 0).unwrap();
    assert_eq!(hits.len(), 1, "a literal % matches only the odd address");
    assert_eq!(hits[0].thread_id, "t2");

    // Same for `_`, which would otherwise match any single character.
    let (text, filter) = parse_search_query("invoice from:_");
    assert!(
        store
            .search_filtered(acct, &text, &filter, 10, 0)
            .unwrap()
            .is_empty(),
        "a literal underscore matches neither sender"
    );
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
            .query_row("SELECT body FROM messages WHERE id=?1", params![id], |r| {
                r.get(0)
            })
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
    embed_and_store(
        &store,
        &*embedder,
        acct,
        sealed,
        "verification code",
        "your one time passcode is 999111",
    );
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

    let hits = store
        .hybrid_search(acct, "signed contract vendor", &SearchFilter::default(), 5)
        .unwrap()
        .0;
    assert!(
        hits.iter().any(|h| h.id == id),
        "hybrid search must surface the matching sent doc (recall includes sent mail)"
    );
}

#[test]
fn hybrid_snippet_is_the_match_window_for_keyword_hits_only() {
    // A hit that came off the FTS list gets its window; a vector-only hit has no
    // matched term to cut around, so it keeps the stored head-of-message text.
    let embedder = Arc::new(StubEmbedder::new(VEC_DIMS));
    let (store, acct) = store_with_embedder(embedder.clone());

    let keyword_hit = triaged(acct, "g-kw", "t-kw")
        .subject("weekly digest")
        .snippet("Thanks for subscribing to the weekly digest.")
        .body(DEEP_BODY)
        .seed(&store);

    let vector_only = triaged(acct, "g-vec", "t-vec")
        .subject("unrelated")
        .snippet("stored head for the vector hit")
        .body("nothing in here matches the query terms at all")
        .seed(&store);
    embed_and_store(
        &store,
        &*embedder,
        acct,
        vector_only,
        "unrelated",
        "nothing in here matches the query terms at all",
    );

    let hits = store
        .hybrid_search(acct, "pangolin", &SearchFilter::default(), 10)
        .unwrap()
        .0;

    let kw = hits.iter().find(|h| h.id == keyword_hit).expect("fts hit");
    assert!(
        kw.snippet.contains("pangolin"),
        "keyword-side hit shows its match window, got {:?}",
        kw.snippet
    );
    if let Some(v) = hits.iter().find(|h| h.id == vector_only) {
        assert_eq!(
            v.snippet, "stored head for the vector hit",
            "vector-only hit keeps the stored snippet"
        );
    }
}

#[test]
fn hybrid_and_semantic_apply_the_filter_post_hoc() {
    let embedder = Arc::new(StubEmbedder::new(VEC_DIMS));
    let (store, acct) = store_with_embedder(embedder.clone());

    let jane = triaged(acct, "g-jane", "t-jane")
        .from("jane@example.com")
        .subject("invoice")
        .body("the invoice for the consulting work")
        .received_at(day(2026, 2, 10))
        .seed(&store);

    let bob = triaged(acct, "g-bob", "t-bob")
        .from("bob@other.test")
        .subject("invoice")
        .body("the invoice for the consulting work")
        .received_at(day(2026, 2, 11))
        .seed(&store);

    for m in store.messages_missing_vectors(acct, 10).unwrap() {
        embed_and_store(&store, &*embedder, acct, m.message_id, &m.subject, &m.body);
    }

    let (text, filter) = parse_search_query("invoice from:jane");
    let hits = store.hybrid_search(acct, &text, &filter, 10).unwrap().0;
    assert!(hits.iter().any(|h| h.id == jane));
    assert!(
        !hits.iter().any(|h| h.id == bob),
        "the sender filter drops bob from the fused window"
    );

    let (text, filter) = parse_search_query("invoice before:2026-02-11");
    let hits = store
        .semantic_search_hits(acct, &text, &filter, 10)
        .unwrap()
        .0;
    assert!(hits.iter().any(|h| h.id == jane));
    assert!(
        !hits.iter().any(|h| h.id == bob),
        "before: is exclusive, so bob's later message is out"
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
    assert!(
        store.embedder().is_none(),
        "no embedder before background attach"
    );
    let hits = store
        .hybrid_search(acct, "quarterly invoice", &SearchFilter::default(), 5)
        .unwrap()
        .0;
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
    assert!(
        hits.iter().any(|(id, _)| *id == dec),
        "decoy present but lower"
    );
}

/// E2E against the REAL fastembed model: `max_tokens` must reach the tokenizer.
/// Nothing offline can prove that — a stub embedder has no tokenizer, and the
/// config tests only prove the number travels as far as `EmbedSettings`. Same
/// SQUELCH_EMBED_E2E gate, so CI skips it.
#[test]
fn embed_e2e_max_tokens_reaches_the_tokenizer() {
    if std::env::var("SQUELCH_EMBED_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping embed_e2e (set SQUELCH_EMBED_E2E=1 to run)");
        return;
    }
    use crate::config::EmbedConfig;
    use crate::embed::FastEmbedder;

    // ~2000 characters, past both cuts. Varied sentences on purpose: a repeated
    // string tokenizes to one repeated token, and both windows would then read
    // the same thing and agree for the wrong reason.
    let mut text = String::new();
    let mut day = 0;
    while text.len() < 2000 {
        text.push_str(&format!(
            "On day {day} we shipped invoice {day} to the Denver warehouse and confirmed delivery. "
        ));
        day += 1;
    }

    let embed_at = |max_tokens: usize| {
        let cfg = EmbedConfig {
            max_tokens,
            ..EmbedConfig::default()
        };
        FastEmbedder::new(&cfg.settings())
            .unwrap()
            .embed(&text)
            .unwrap()
    };
    let short = embed_at(256);
    let long = embed_at(512);

    let dot: f32 = short.iter().zip(long.iter()).map(|(a, b)| a * b).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let cos = dot / (norm(&short) * norm(&long));
    assert!(
        cos < 0.999,
        "256 and 512 tokens of the same 2000-char text produced the same vector (cos {cos}); max_tokens never reached the tokenizer"
    );
}
