//! End-to-end tests for the opens poller against a mock relay: an axum app on an
//! ephemeral loopback port, so the cursor rules are asserted on the requests that
//! actually left the daemon. The relay's own delete-on-collect behavior is
//! mirrored here, because the cursor is the ONLY thing standing between "an open
//! we wrote down" and "an open the relay threw away".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use squelch_core::store::{SqliteStore, Store};
use squelch_core::tracking::{CURSOR_KEY, OpensPoller};
use squelch_core::types::{NewMessage, Sensitivity, Tier};
use tokio::net::TcpListener;
use tokio::sync::watch;

// ---- mock relay -------------------------------------------------------------

#[derive(Clone, Default)]
struct Relay {
    /// Rows the relay is holding, oldest id first.
    rows: Arc<Mutex<Vec<Value>>>,
    /// Every `cursor` presented, in order.
    cursors: Arc<Mutex<Vec<i64>>>,
    /// Every `Authorization` header value, in order.
    auth: Arc<Mutex<Vec<Option<String>>>>,
    /// When set, answer 500 (relay outage).
    fail: Arc<AtomicBool>,
    /// When set, answer `307` pointing here — a relay trying to walk the poller
    /// (and its bearer) somewhere else.
    redirect_to: Arc<Mutex<Option<String>>>,
}

#[derive(serde::Deserialize)]
struct OpensQuery {
    #[serde(default)]
    cursor: i64,
}

async fn opens(
    State(relay): State<Relay>,
    Query(q): Query<OpensQuery>,
    headers: HeaderMap,
) -> Response {
    relay.cursors.lock().unwrap().push(q.cursor);
    relay.auth.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
    );

    if relay.fail.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Some(location) = relay.redirect_to.lock().unwrap().clone() {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(axum::http::header::LOCATION, location)],
        )
            .into_response();
    }

    // Presenting a cursor is what authorizes the delete, exactly as the real
    // relay behaves: rows at or below it are gone for good.
    let mut rows = relay.rows.lock().unwrap();
    rows.retain(|r| r["id"].as_i64().unwrap_or(0) > q.cursor);
    Json(json!({ "opens": rows.clone() })).into_response()
}

async fn spawn_relay() -> (String, Relay) {
    let relay = Relay::default();
    let app = Router::new()
        .route("/v1/opens", get(opens))
        .with_state(relay.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), relay)
}

fn row(id: i64, token: &str, opened_at: i64, user_agent: &str) -> Value {
    json!({ "id": id, "token": token, "opened_at": opened_at, "user_agent": user_agent })
}

// ---- daemon-side harness ----------------------------------------------------

/// A store with one sent message and one tracker pointing at it.
fn seeded() -> (Arc<SqliteStore>, i64, i64) {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let acct = store.ensure_account("me@example.com").unwrap();
    let message_id = store
        .upsert_message(&NewMessage {
            account_id: acct,
            gmail_msg_id: "sent-1".to_string(),
            thread_id: "thread-77".to_string(),
            from_addr: "me@example.com".to_string(),
            from_name: None,
            subject: "Re: Lunch?".to_string(),
            received_at: chrono::Utc::now(),
            snippet: String::new(),
            body: "noon works".to_string(),
            body_html: None,
            is_sent: true,
            is_spam: false,
            to_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
        })
        .unwrap();
    store
        .set_triage(
            message_id,
            acct,
            0,
            Tier::Noise,
            Sensitivity::Normal,
            None,
            "",
            "",
            None,
        )
        .unwrap();
    store
        .insert_send_tracker(acct, "tok-1", Some(message_id), 1_000)
        .unwrap();
    (store, acct, message_id)
}

fn cursor(store: &SqliteStore, acct: i64) -> Option<i64> {
    store
        .sync_state(acct, CURSOR_KEY)
        .unwrap()
        .map(|s| s.last_uid as i64)
}

/// Spawn the poller, wait for `want` to hold, then stop it. Fails the test if
/// the poller does not reach the state within a few seconds or does not stop on
/// the shutdown signal.
async fn run_until(poller: OpensPoller, want: impl Fn() -> bool) {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(async move { poller.run(shutdown_rx).await });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !want() {
        assert!(
            std::time::Instant::now() < deadline,
            "the poller never reached the expected state"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = shutdown_tx.send(true);
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the poller must end on the shutdown signal")
        .expect("join")
        .expect("clean exit");
}

// ---- tests ------------------------------------------------------------------

#[tokio::test]
async fn opens_are_collected_classified_and_notified_once_per_message() {
    let (base, relay) = spawn_relay().await;
    let (store, acct, message_id) = seeded();
    *relay.rows.lock().unwrap() = vec![
        row(1, "tok-1", 1_100, "Apple Mail/16.0"),
        row(
            2,
            "tok-1",
            1_200,
            "Mozilla/5.0 (via ggpht.com GoogleImageProxy)",
        ),
        // A token this daemon never minted: collected, recorded nowhere.
        row(3, "tok-stranger", 1_300, "curl/8"),
    ];

    let poller = OpensPoller::for_test(store.clone() as Arc<dyn Store>, acct, &base)
        .with_relay_auth(Some("relay-secret"));
    let s = store.clone();
    run_until(poller, move || cursor(&s, acct) == Some(3)).await;

    let opens = store.message_opens(acct, message_id).unwrap();
    assert_eq!(opens.len(), 2, "the stranger's token recorded nothing");
    assert_eq!(opens[0].classification, "unknown");
    assert_eq!(opens[1].classification, "proxied");

    // ONE event for the message, however many times it was opened.
    let events = store.events_after(acct, 0, 50).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind.as_str(), "opened");
    assert_eq!(events[0].message_id, message_id);
    assert_eq!(events[0].thread_id, "thread-77");
    assert_eq!(events[0].one_line, "Re: Lunch?");

    // The bearer went to the relay on every request, and the first poll started
    // at zero rather than skipping the backlog.
    let auth = relay.auth.lock().unwrap().clone();
    assert!(
        auth.iter()
            .all(|a| a.as_deref() == Some("Bearer relay-secret"))
    );
    assert_eq!(relay.cursors.lock().unwrap()[0], 0);
}

/// The cursor is presented once past the collected rows, so the relay deletes
/// exactly what was written down — and a second pass collects nothing twice.
#[tokio::test]
async fn a_collected_open_is_never_collected_twice() {
    let (base, relay) = spawn_relay().await;
    let (store, acct, message_id) = seeded();
    *relay.rows.lock().unwrap() = vec![row(7, "tok-1", 1_100, "Apple Mail/16.0")];

    let poller = OpensPoller::for_test(store.clone() as Arc<dyn Store>, acct, &base);
    let s = store.clone();
    run_until(poller, move || cursor(&s, acct) == Some(7)).await;
    assert_eq!(store.message_opens(acct, message_id).unwrap().len(), 1);

    // A fresh poller over the same store re-presents cursor 7 and gets nothing.
    let poller = OpensPoller::for_test(store.clone() as Arc<dyn Store>, acct, &base);
    let relay_seen = relay.cursors.clone();
    let before = relay_seen.lock().unwrap().len();
    run_until(poller, move || relay_seen.lock().unwrap().len() > before).await;
    assert_eq!(
        store.message_opens(acct, message_id).unwrap().len(),
        1,
        "the same relay row must not be recorded twice"
    );
    assert!(relay.cursors.lock().unwrap()[1..].iter().all(|c| *c == 7));
}

/// A relay that is down moves nothing: no rows, no cursor, and the poller keeps
/// running so a later pass collects what is still being held.
#[tokio::test]
async fn a_failing_relay_leaves_the_cursor_alone() {
    let (base, relay) = spawn_relay().await;
    let (store, acct, message_id) = seeded();
    *relay.rows.lock().unwrap() = vec![row(1, "tok-1", 1_100, "Apple Mail/16.0")];
    relay.fail.store(true, Ordering::SeqCst);

    let poller = OpensPoller::for_test(store.clone() as Arc<dyn Store>, acct, &base);
    let seen = relay.cursors.clone();
    run_until(poller, move || !seen.lock().unwrap().is_empty()).await;

    assert_eq!(cursor(&store, acct), None, "a failed poll writes no cursor");
    assert!(store.message_opens(acct, message_id).unwrap().is_empty());
    // Nothing was deleted relay-side either: the rows are still there to collect.
    assert_eq!(relay.rows.lock().unwrap().len(), 1);
}

/// THE INVARIANT: a `307` must not walk the relay bearer to another host. With
/// redirects off, a 3xx is just a non-2xx — no second request, cursor unmoved.
#[tokio::test]
async fn a_redirect_is_refused_and_the_bearer_never_follows() {
    let (evil_base, evil) = spawn_relay().await;
    let (base, relay) = spawn_relay().await;
    *relay.redirect_to.lock().unwrap() = Some(format!("{evil_base}/v1/opens"));
    *relay.rows.lock().unwrap() = vec![row(1, "tok-1", 1_100, "Apple Mail/16.0")];

    let (store, acct, message_id) = seeded();
    let poller = OpensPoller::for_test(store.clone() as Arc<dyn Store>, acct, &base)
        .with_relay_auth(Some("relay-secret"));
    let seen = relay.cursors.clone();
    run_until(poller, move || !seen.lock().unwrap().is_empty()).await;

    assert!(
        evil.auth.lock().unwrap().is_empty(),
        "the bearer followed a redirect off the configured relay"
    );
    assert_eq!(cursor(&store, acct), None);
    assert!(store.message_opens(acct, message_id).unwrap().is_empty());
}

/// An open whose tracker never got a message id is still recorded; there is just
/// no event to raise, because nothing names the message yet.
#[tokio::test]
async fn an_unlinked_tracker_records_the_open_without_an_event() {
    let (base, relay) = spawn_relay().await;
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let acct = store.ensure_account("me@example.com").unwrap();
    store
        .insert_send_tracker(acct, "tok-unlinked", None, 1_000)
        .unwrap();
    *relay.rows.lock().unwrap() = vec![row(1, "tok-unlinked", 1_100, "Apple Mail/16.0")];

    let poller = OpensPoller::for_test(store.clone() as Arc<dyn Store>, acct, &base);
    let s = store.clone();
    run_until(poller, move || cursor(&s, acct) == Some(1)).await;

    assert!(store.events_after(acct, 0, 10).unwrap().is_empty());
    // The row is there; it becomes readable the moment a message id is linked.
    let message_id = store
        .upsert_message(&NewMessage {
            account_id: acct,
            gmail_msg_id: "sent-late".to_string(),
            thread_id: "thread-9".to_string(),
            from_addr: "me@example.com".to_string(),
            from_name: None,
            subject: "Later".to_string(),
            received_at: chrono::Utc::now(),
            snippet: String::new(),
            body: String::new(),
            body_html: None,
            is_sent: true,
            is_spam: false,
            to_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
        })
        .unwrap();
    assert!(
        store
            .set_send_tracker_message(acct, "tok-unlinked", message_id)
            .unwrap()
    );
    assert_eq!(store.message_opens(acct, message_id).unwrap().len(), 1);
}
