//! PROVIDER SPAM: the structural-exclusion suite.
//!
//! `messages.is_spam` is worth exactly as much as the predicates that read it.
//! Every band, queue, count and search leg had to grow one, and the failure
//! mode of forgetting one is not a crash — it is spam quietly appearing in the
//! inbox, which is the state this feature exists to end.
//!
//! So the shape here is deliberately blunt: seed ONE spam row beside one
//! ordinary row, then walk every surface that lists mail and assert the spam id
//! is not in it. A new listing added later is not covered by these tests, which
//! is the honest limit of the approach — but a listing that DROPS its predicate
//! in a refactor is, and that is the likelier accident.

use super::super::*;
use super::support::*;
use crate::store::SpamScope;
use crate::types::{SealedKind, Tier};

/// One ordinary signal row and one spam row, in that order.
fn inbox_and_spam(store: &SqliteStore, acct: AccountId) -> (i64, i64) {
    let good = triaged(acct, "g-good", "t-good")
        .from("alice@example.com")
        .subject("Lunch tomorrow")
        .body("Are you free at noon? Let me know.")
        .tier(Tier::Signal)
        .importance(60)
        .seed(store);
    let spam = triaged(acct, "g-spam", "t-spam")
        .from("winner@lottery.example")
        .subject("Lunch tomorrow: you have won")
        .body("Are you free at noon? Claim your prize now.")
        .is_spam(true)
        .seed(store);
    (good, spam)
}

/// THE LOAD-BEARING TEST. Every listing that answers "what mail do I have",
/// asked at once, so a surface that forgot its predicate fails here rather than
/// in production.
///
/// The two fixtures share subject and body words on purpose: a search leg that
/// dropped the predicate would return both, not neither, so the assertions
/// cannot pass by the query simply missing.
#[test]
fn spam_is_absent_from_every_listing() {
    let (store, acct) = store();
    let (good, spam) = inbox_and_spam(&store, acct);
    let since = Utc::now() - chrono::Duration::days(1);

    let has = |ids: Vec<i64>, what: &str| {
        assert!(
            ids.contains(&good),
            "{what}: the ordinary row should be here"
        );
        assert!(!ids.contains(&spam), "{what}: LEAKED a spam row");
    };

    // The flat list and each sitrep band.
    for band in [
        None,
        Some(SitrepBand::Standing),
        Some(SitrepBand::New),
        Some(SitrepBand::Open),
    ] {
        let rows = store
            .attention_updates(acct, since, None, None, band, false, SpamScope::Exclude)
            .unwrap();
        // Standing/Open are legitimately empty for these fixtures; only the
        // spam half of the assertion applies to every band.
        assert!(
            !rows.iter().any(|u| u.update.id == spam),
            "band {band:?}: LEAKED a spam row"
        );
    }
    has(
        store
            .attention_updates(acct, since, None, None, None, false, SpamScope::Exclude)
            .unwrap()
            .iter()
            .map(|u| u.update.id)
            .collect(),
        "attention_updates",
    );

    // The agent door's ranked list.
    has(
        store
            .ranked_updates(acct, since, None)
            .unwrap()
            .iter()
            .map(|u| u.id)
            .collect(),
        "ranked_updates",
    );

    // Keyword search, on a term BOTH messages contain.
    has(
        store
            .search(acct, "lunch", 50, 0)
            .unwrap()
            .iter()
            .map(|h| h.id)
            .collect(),
        "search",
    );

    // The human door's filtered search and its no-query browse listing.
    let filter = SearchFilter::default();
    has(
        store
            .search_filtered(acct, "lunch", &filter, SearchSort::Recent, 50, 0)
            .unwrap()
            .iter()
            .map(|h| h.id)
            .collect(),
        "search_filtered",
    );

    // THE AGENT DOOR'S SEARCH, which is the one that matters most: /mcp reaches
    // the store through `hybrid_search`, and it is the only caller here whose
    // reader can be talked into things by what comes back. With no embedder
    // attached this degenerates to keyword-only, which is exactly the point —
    // it exercises `fts_recall` and `search_hit_by_id`, the two queries the
    // legs above do not touch.
    has(
        store
            .hybrid_search(acct, "lunch", &filter, SearchSort::Recent, 50)
            .unwrap()
            .0
            .iter()
            .map(|h| h.id)
            .collect(),
        "hybrid_search",
    );
}

/// The header's noise count is the DOOR to the noise page, so it must count
/// what that page lists and nothing else. Spam and sent mail both land
/// tier=noise without ever being triaged; neither belongs in the number.
#[test]
fn stats_count_spam_on_its_own_and_keep_it_out_of_the_tiers() {
    let (store, acct) = store();
    triaged(acct, "g-noise", "t-noise")
        .tier(Tier::Noise)
        .seed(&store);
    triaged(acct, "g-sent", "t-sent")
        .is_sent(true)
        .tier(Tier::Noise)
        .seed(&store);
    triaged(acct, "g-spam", "t-spam")
        .is_spam(true)
        .tier(Tier::Noise)
        .seed(&store);

    let stats = store
        .stats(acct, Utc::now() - chrono::Duration::days(1))
        .unwrap();
    assert_eq!(
        stats.tier_counts.get("noise").copied().unwrap_or(0),
        1,
        "only the real noise row counts toward noise"
    );
    assert_eq!(stats.spam, 1, "spam is counted on its own");
}

/// Spam never enters an LLM queue. The ingest path stamps 'n/a' markers, which
/// is the first defense; this pins the second — the queue predicates themselves.
#[test]
fn spam_never_queues_for_a_model() {
    let (store, acct) = store();
    let spam = triaged(acct, "g-spam", "t-spam")
        .is_spam(true)
        .body("Please review the attached invoice and wire payment today.")
        .confident(false)
        .ingest(&store);

    let queued = store.stage1_queue(acct, 50).unwrap();
    assert!(
        !queued.iter().any(|r| r.message_id == spam),
        "spam must never reach Stage-1"
    );
    assert_eq!(
        stage_markers(&store, spam).0.as_deref(),
        Some("n/a"),
        "ingest stamps spam as never-to-be-modelled"
    );
}

/// A reminder on a spam row would be unreachable forever — no listing shows it,
/// so it could never be seen or cancelled, and `reminded_at` is the one standing
/// -band arm that carries no tier test. The store refuses to stamp one.
#[test]
fn spam_refuses_a_reminder() {
    let (store, acct) = store();
    let spam = triaged(acct, "g-spam", "t-spam").is_spam(true).seed(&store);
    let stamped = store
        .set_reminder(acct, spam, Utc::now() + chrono::Duration::hours(1))
        .unwrap();
    assert!(!stamped, "a spam row takes no reminder");
}

/// The spam page: the ONE listing that asks for the other side of the verdict.
#[test]
fn the_spam_page_lists_spam_and_only_spam() {
    let (store, acct) = store();
    let (good, spam) = inbox_and_spam(&store, acct);
    let rows = store
        .attention_updates(
            acct,
            Utc::now() - chrono::Duration::days(1),
            None,
            None,
            None,
            false,
            SpamScope::Only,
        )
        .unwrap();
    let ids: Vec<i64> = rows.iter().map(|u| u.update.id).collect();
    assert_eq!(ids, vec![spam], "the page is exactly the spam");
    assert!(!ids.contains(&good));
}

/// Sealed outranks spam. A login code Gmail misfiled is still a login code, and
/// the spam page is a page someone reads.
#[test]
fn a_sealed_row_never_reaches_the_spam_page() {
    let (store, acct) = store();
    let sealed = triaged(acct, "g-otp", "t-otp")
        .is_spam(true)
        .sealed(SealedKind::Otp)
        .seed(&store);
    let rows = store
        .attention_updates(
            acct,
            Utc::now() - chrono::Duration::days(1),
            None,
            None,
            None,
            false,
            SpamScope::Only,
        )
        .unwrap();
    assert!(!rows.iter().any(|u| u.update.id == sealed));
}

/// THE AGENT DOOR GETS NO SPAM AT ALL, not spam it is told to distrust — the
/// same shape sealed mail gets. A thread of nothing but spam is `NotFound`
/// through `/mcp`, and a MIXED thread hands over only the half that was
/// delivered, so a spoof landing in a real conversation cannot reach a reader
/// that might act on it.
#[test]
fn the_agent_door_thread_view_drops_spam() {
    let (store, acct) = store();

    // All spam: absent, exactly like a sealed thread.
    triaged(acct, "g-spam", "t-junk")
        .is_spam(true)
        .body("wire the deposit today")
        .seed(&store);
    assert!(
        store.thread_view(acct, "t-junk").is_err(),
        "a thread of nothing but spam must 404, not come back empty"
    );

    // Mixed: only the delivered message crosses.
    let real = triaged(acct, "g-real", "t-mixed")
        .from("dana@northwind.example")
        .body("here are the redlines")
        .seed(&store);
    triaged(acct, "g-spoof", "t-mixed")
        .from("dana@northwind-example.co")
        .is_spam(true)
        .body("wire the deposit to the updated account below")
        .seed(&store);

    let view = store.thread_view(acct, "t-mixed").unwrap();
    let ids: Vec<i64> = view.messages.iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![real], "the spoof must not reach the agent");
}

// ---- "not spam" ---------------------------------------------------------

/// The rescue: the flag clears, the row requeues for a real verdict with the
/// force stamp its age needs, and the attention lifecycle goes back to `new` so
/// it lands in the New band rather than reading as already-seen.
#[test]
fn clearing_spam_returns_the_row_to_the_inbox_and_requeues_it() {
    let (store, acct) = store();
    let spam = triaged(acct, "g-spam", "t-spam")
        .is_spam(true)
        .received_at(Utc::now() - chrono::Duration::days(20))
        .seed(&store);
    // The spam page listed it, which stamped the seen-ledger.
    store.mark_surfaced(acct, &[spam]).unwrap();

    assert!(store.clear_spam(acct, spam).unwrap());

    let rows = store
        .attention_updates(
            acct,
            Utc::now() - chrono::Duration::days(30),
            None,
            None,
            None,
            false,
            SpamScope::Exclude,
        )
        .unwrap();
    assert!(
        rows.iter().any(|u| u.update.id == spam),
        "the rescued row is ordinary mail now"
    );
    let row = rows.iter().find(|u| u.update.id == spam).unwrap();
    assert_eq!(
        row.status,
        AttentionStatus::New,
        "it reads as newly arrived"
    );
    assert!(
        row.surfaced_at.is_none(),
        "the spam page's stamp is cleared"
    );

    let (stage1, retriage_at) = stage_markers(&store, spam);
    assert!(
        stage1.is_none(),
        "the 'n/a' marker is gone, so Stage-1 will look at it"
    );
    assert!(
        retriage_at.is_some(),
        "and the force stamp keeps its age from stale-skipping it"
    );

    // And it is off the spam page.
    let spam_rows = store
        .attention_updates(
            acct,
            Utc::now() - chrono::Duration::days(30),
            None,
            None,
            None,
            false,
            SpamScope::Only,
        )
        .unwrap();
    assert!(spam_rows.is_empty());
}

/// Idempotent-ish by construction: a second call changes nothing and says so,
/// which is what lets the handler treat `false` as "already in the state you
/// asked for" rather than an error.
#[test]
fn clearing_spam_twice_reports_the_second_as_no_change() {
    let (store, acct) = store();
    let spam = triaged(acct, "g-spam", "t-spam").is_spam(true).seed(&store);
    assert!(store.clear_spam(acct, spam).unwrap());
    assert!(!store.clear_spam(acct, spam).unwrap());
}

/// Sealed rows are refused here as everywhere: unsealing by hand is not a thing
/// this path gets to do.
#[test]
fn clearing_spam_refuses_a_sealed_row() {
    let (store, acct) = store();
    let sealed = triaged(acct, "g-otp", "t-otp")
        .is_spam(true)
        .sealed(SealedKind::Otp)
        .seed(&store);
    assert!(!store.clear_spam(acct, sealed).unwrap());
}

/// A message seen under a visible label can never be hidden by a later spam
/// sighting. The walk order is the mechanism; this is the backstop that holds
/// regardless of ingest order.
#[test]
fn the_upsert_keeps_a_message_visible_once_it_has_been_seen_outside_spam() {
    let (store, acct) = store();
    let inbox = triaged(acct, "g-both", "t-both").is_spam(false);
    let spam = triaged(acct, "g-both", "t-both").is_spam(true);

    // Spam sighting second.
    let id = inbox.upsert(&store);
    spam.upsert(&store);
    assert!(!is_spam_flag(&store, id), "a spam re-ingest cannot hide it");

    // And spam sighting FIRST, which must also end visible.
    let (store2, acct2) = super::support::store();
    let spam2 = triaged(acct2, "g-both", "t-both").is_spam(true);
    let inbox2 = triaged(acct2, "g-both", "t-both").is_spam(false);
    let id2 = spam2.upsert(&store2);
    inbox2.upsert(&store2);
    assert!(!is_spam_flag(&store2, id2), "the visible copy wins");
}

/// The two LLM-queue markers off a triage row: `(stage1_model_used, retriage_at)`.
fn stage_markers(store: &SqliteStore, message_id: i64) -> (Option<String>, Option<String>) {
    let conn = store.lock().unwrap();
    conn.query_row(
        "SELECT stage1_model_used, retriage_at FROM triage WHERE message_id = ?1",
        [message_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn is_spam_flag(store: &SqliteStore, message_id: i64) -> bool {
    let conn = store.lock().unwrap();
    conn.query_row(
        "SELECT is_spam FROM messages WHERE id = ?1",
        [message_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        != 0
}
