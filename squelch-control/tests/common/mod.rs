//! The Postgres a test runs against: one server, a schema of its own.
//!
//! Lifted from `store.rs`'s own test harness, and deliberately a COPY rather
//! than something exported from the crate: nothing outside `cfg(test)` should
//! grow a way to make throwaway schemas, and an integration test is a separate
//! crate that cannot see the unit tests' helpers anyway.
//!
//! ISOLATION IS A SCHEMA, NOT A TEST-ONLY CONSTRUCTOR. The URL handed to
//! [`squelch_control::ControlStore::connect`] is the production one with a
//! `search_path` appended, so every test here exercises the real connect path —
//! the pool, the advisory lock, the DDL, the migration — rather than a shortcut
//! that exists only under test.
//!
//! Each integration test binary compiles this file separately, so a helper one
//! binary does not call is dead code in that binary and nowhere else; the
//! `allow` below is that fact rather than a shrug.
#![allow(dead_code)]

use chrono::Utc;
use squelch_control::ControlStore;
use tokio_postgres::NoTls;

/// What to do about a missing [`TEST_URL_VAR`], as one sentence with the two
/// commands in it.
///
/// A LOUD PANIC RATHER THAN A SKIP, decided deliberately: a suite that skips
/// itself when the database is absent reports green while testing nothing, and
/// the day that matters is the day somebody's CI forgets the service container.
const NO_TEST_DATABASE: &str = "\
SQUELCH_TEST_PG_URL is not set, and these tests run against a real Postgres.

    docker run -d --name squelch-pg -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:16
    export SQUELCH_TEST_PG_URL=postgres://postgres:postgres@localhost:5432/postgres
";

/// The database every test in this crate connects to. One server, a schema per
/// test.
const TEST_URL_VAR: &str = "SQUELCH_TEST_PG_URL";

/// How long a leftover test schema is kept before the next run drops it. Long
/// enough that a slow test still owns its schema, short enough that a
/// developer's database does not fill up with them.
const SCHEMA_TTL_SECS: i64 = 3600;

/// A store of its own, on a schema of its own, and the URL that points at it.
///
/// THE URL IS RETURNED BECAUSE `ControlStore::client` IS CRATE-PRIVATE: a test
/// that has to read a column no method returns opens its own connection with
/// [`raw_client`], and it can only land on the same schema if it is given the
/// same URL.
pub async fn fresh_store() -> (ControlStore, String) {
    let url = fresh_schema().await;
    let store = ControlStore::connect(&url)
        .await
        .expect("connecting the test store");
    (store, url)
}

/// A fresh, empty schema, and the URL that points at it.
///
/// The name embeds the second it was made in, which is what lets the NEXT run
/// clean up after this one: there is no async `Drop`, so a test cannot reliably
/// drop its own schema, and a reaper that runs on the way in is the shape that
/// needs nothing to be remembered on the way out.
pub async fn fresh_schema() -> String {
    let base = std::env::var(TEST_URL_VAR)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| panic!("{NO_TEST_DATABASE}"));
    let client = raw_client(&base).await;
    reap_old_schemas(&client).await;

    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).expect("the system random source");
    let name = format!(
        "sqct_{}_{}",
        Utc::now().timestamp(),
        suffix
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    client
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .await
        .expect("creating the test schema");
    schema_url(&base, &name)
}

/// The base URL with a `search_path` pointing at one schema.
///
/// The `=` inside the option is PERCENT-ENCODED, because this is a query VALUE:
/// an unescaped one would end the `options` parameter and the rest would be read
/// as another key. The separator is `&` when the operator's URL already carries
/// a query string (`?sslmode=disable` is a common one), and `?` when it does
/// not.
pub fn schema_url(base: &str, schema: &str) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-csearch_path%3D{schema}")
}

/// A connection outside the store, for the harness and for the assertions that
/// read columns no method returns.
///
/// The connection task is spawned and forgotten: it ends when the client is
/// dropped, and a test process that exits with one still running has nothing to
/// lose.
pub async fn raw_client(url: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("connecting to {TEST_URL_VAR}: {e}\n\n{NO_TEST_DATABASE}"));
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Drop every `sqct_` schema older than [`SCHEMA_TTL_SECS`].
///
/// Only names this harness could have written are touched: the prefix, then a
/// timestamp that parses, then hex. A name that does not match that shape is
/// left alone however much it looks like ours, because the one thing this must
/// never do is drop a schema somebody's application lives in.
pub async fn reap_old_schemas(client: &tokio_postgres::Client) {
    let rows = client
        .query(
            "SELECT nspname FROM pg_namespace WHERE nspname LIKE 'sqct\\_%'",
            &[],
        )
        .await
        .expect("listing test schemas");
    let cutoff = Utc::now().timestamp() - SCHEMA_TTL_SECS;
    for row in &rows {
        let name: String = row.get(0);
        let Some((ts, hex)) = name
            .strip_prefix("sqct_")
            .and_then(|rest| rest.split_once('_'))
        else {
            continue;
        };
        let Ok(ts) = ts.parse::<i64>() else { continue };
        if ts >= cutoff || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        // Best effort: another run reaping the same schema at the same moment
        // wins the race and this one has nothing to do.
        let _ = client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {name} CASCADE"))
            .await;
    }
}
