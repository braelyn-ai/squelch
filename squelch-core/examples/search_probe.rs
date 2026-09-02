//! MUTATION PROBE for the search legs: run a handful of queries against a COPY
//! of a real mailbox and print the top ten of each leg, so a ranking change can
//! be judged against mail somebody actually received rather than against a
//! fixture written by the person changing the ranking.
//!
//! Green tests prove nothing about a ranking (the recency PR's lesson). What
//! proves something is the same five queries before and after, side by side, on
//! a corpus with real term statistics — "wifi" occurring once in fifteen
//! hundred messages is the entire reason the motivating query works, and no
//! fixture reproduces that.
//!
//! ```sh
//! # NEVER against the live DB: copy it first, and open the copy.
//! sqlite3 "file:$SQUELCH_DB_PATH?mode=ro" ".backup '/tmp/corpus.db'"
//! DEVELOPER_DIR=/Library/Developer/CommandLineTools \
//!   cargo run -p squelch-core --example search_probe -- /tmp/corpus.db
//! ```
//!
//! IDS AND SUBJECTS ONLY. This prints a line per hit and never a body: the
//! whole point is to be safe to paste into a PR description.
//!
//! Opening the copy runs the normal `SqliteStore::open` path, migrations
//! included, so the `messages_fts` tokenizer rebuild happens here too and its
//! cost on a real mailbox is printed rather than guessed.

use std::sync::Arc;
use std::time::Instant;

use squelch_core::config::EmbedConfig;
use squelch_core::embed::FastEmbedder;
use squelch_core::store::{SearchFilter, SearchSort, SqliteStore, Store};

/// The queries the wave-1 work is judged on. The first is the motivating one:
/// the mail that answers it contains neither "conference" nor "password".
const QUERIES: &[&str] = &[
    "abstract conference wifi password",
    "conference wifi password",
    "wifi password",
    "stripe payout receipt",
    "abstract",
];

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: search_probe <path to a COPY of squelch.db>");
            std::process::exit(2);
        }
    };

    let t0 = Instant::now();
    let store = SqliteStore::open(&path).expect("open the copy");
    println!("open + migrate: {:?}", t0.elapsed());

    // Account 1 unless told otherwise: a personal mailbox has exactly one, and
    // asking for it by id keeps the probe from having to know an address (and
    // from `ensure_account`, which WRITES).
    let account_id: i64 = std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1);
    match store.account_email(account_id) {
        Ok(_) => println!("account_id: {account_id}"),
        Err(e) => {
            eprintln!("no account {account_id} in {path}: {e}");
            std::process::exit(2);
        }
    }

    // The embedder is OPTIONAL here: without the weights cached, the vector leg
    // is skipped rather than downloading 126 MB in the middle of a probe.
    let cfg = EmbedConfig::default();
    let cached = cfg
        .cache_dir
        .join("models--Xenova--bge-small-en-v1.5")
        .join("snapshots")
        .exists();
    let store = if cached {
        let t = Instant::now();
        let embedder = Arc::new(FastEmbedder::new(&cfg.settings()).expect("load the model"));
        println!("embedder loaded: {:?}", t.elapsed());
        store.with_embedder(embedder).expect("attach")
    } else {
        println!("NO MODEL CACHE at {} — vector legs skipped", cfg.cache_dir.display());
        store
    };

    for q in QUERIES {
        for sort in [SearchSort::Recent, SearchSort::BestMatch] {
            println!("\n=== keyword [{}] {q:?}", sort.as_str());
            let t = Instant::now();
            let hits = store
                .search_filtered(account_id, q, &SearchFilter::default(), sort, false, 10, 0)
                .expect("keyword search");
            print_hits(&hits, t);
        }
        if store.embedder().is_some() {
            for sort in [SearchSort::Recent, SearchSort::BestMatch] {
                println!("\n=== hybrid [{}] {q:?}", sort.as_str());
                let t = Instant::now();
                let (hits, _full) = store
                    .hybrid_search(account_id, q, &SearchFilter::default(), sort, false, 10)
                    .expect("hybrid search");
                print_hits(&hits, t);
            }
        }
    }
}

fn print_hits(hits: &[squelch_core::types::SearchHit], started: Instant) {
    println!("({} hits in {:?})", hits.len(), started.elapsed());
    for (i, h) in hits.iter().enumerate() {
        println!("{:>2}. #{:<5} {}", i + 1, h.id, h.subject);
    }
}
