//! End-to-end tests for the APNs pusher against a mock relay: an axum app on an
//! ephemeral loopback port, so every assertion is made on the bytes that
//! actually left the daemon. `the_relay_never_sees_content` needs exactly that —
//! a struct-level assertion could be satisfied by a second, chattier code path.
//! The rest pin the cursor rules: at-least-once, and never a skip.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use squelch_core::push::{CURSOR_KEY, Pusher};
use squelch_core::store::{NewEvent, SqliteStore, Store};
use squelch_core::types::{EventKind, Tier};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

const DEV_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
const DEV_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2";

/// Content planted in every event so a leak onto the wire is greppable.
const SENDER: &str = "cfo@acme-example.com";
const ONE_LINE: &str = "wire transfer needs approval before 5pm";

// ---- mock relay -------------------------------------------------------------

#[derive(Clone, Default)]
struct Relay {
    /// Every request body, in order.
    seen: Arc<Mutex<Vec<Value>>>,
    /// Every `Authorization` header value, in order.
    auth: Arc<Mutex<Vec<Option<String>>>>,
    /// When set, answer 500 instead of pushing (relay outage).
    fail: Arc<AtomicBool>,
    /// When set, the relay is fine and APNs is not: HTTP 200 with every token
    /// reported `status: 0`.
    apns_down: Arc<AtomicBool>,
    /// Tokens APNs considers retired: reported back as `410 Unregistered`.
    dead: Arc<Mutex<Vec<String>>>,
    /// Tokens APNs is throttling: reported back as `429 TooManyRequests`.
    throttled: Arc<Mutex<Vec<String>>>,
    /// When set, answer `307` pointing at this absolute URL — a relay trying to
    /// walk the pusher (and its bearer) somewhere else.
    redirect_to: Arc<Mutex<Option<String>>>,
}

impl Relay {
    fn bodies(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

async fn push(State(relay): State<Relay>, headers: HeaderMap, body: Bytes) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    relay.seen.lock().unwrap().push(parsed.clone());
    relay.auth.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
    );

    if relay.fail.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // A hostile/misconfigured relay bouncing the token-bearing POST elsewhere.
    // 307 preserves method AND body, which is exactly what makes it dangerous.
    if let Some(location) = relay.redirect_to.lock().unwrap().clone() {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(axum::http::header::LOCATION, location)],
        )
            .into_response();
    }

    let dead = relay.dead.lock().unwrap().clone();
    let throttled = relay.throttled.lock().unwrap().clone();
    let apns_down = relay.apns_down.load(Ordering::SeqCst);
    let tokens = parsed["device_tokens"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let results: Vec<Value> = tokens
        .iter()
        .filter_map(|t| t.as_str())
        .map(|t| {
            if dead.iter().any(|d| d == t) {
                json!({ "token": t, "status": 410, "reason": "Unregistered" })
            } else if apns_down {
                json!({ "token": t, "status": 0, "reason": "unreachable" })
            } else if throttled.iter().any(|d| d == t) {
                json!({ "token": t, "status": 429, "reason": "TooManyRequests" })
            } else {
                json!({ "token": t, "status": 200, "apns_id": "mock" })
            }
        })
        .collect();
    Json(json!({ "results": results })).into_response()
}

async fn spawn_relay() -> (String, Relay) {
    let relay = Relay::default();
    let app = Router::new()
        .route("/v1/push", post(push))
        .with_state(relay.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), relay)
}

// ---- daemon-side harness ----------------------------------------------------

struct Harness {
    store: Arc<SqliteStore>,
    acct: i64,
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<squelch_core::Result<()>>,
}

impl Harness {
    /// Append one event exactly as the sync engine does — through the store, so
    /// the attached notifier fires and the pusher wakes.
    fn emit(&self, message_id: i64) -> i64 {
        self.store
            .append_event(&NewEvent {
                account_id: self.acct,
                message_id,
                thread_id: format!("thread{message_id}"),
                kind: EventKind::Urgent,
                tier: Tier::PastDue,
                importance: 95,
                sender: SENDER.to_string(),
                one_line: ONE_LINE.to_string(),
                deadline: Some("2026-08-01T17:00:00Z".to_string()),
            })
            .unwrap()
            .unwrap()
    }

    fn register(&self, token: &str) {
        self.store.upsert_device(self.acct, token, "ios").unwrap();
    }

    fn cursor(&self) -> Option<i64> {
        self.store
            .sync_state(self.acct, CURSOR_KEY)
            .unwrap()
            .map(|s| s.last_uid as i64)
    }

    fn devices(&self) -> Vec<String> {
        self.store
            .list_devices(self.acct)
            .unwrap()
            .into_iter()
            .map(|d| d.token)
            .collect()
    }

    /// Signal shutdown and wait for the task, failing if it does not stop
    /// promptly — a pusher that ignores the signal would hold `squelchd` open.
    async fn stop(self) -> Duration {
        let started = Instant::now();
        let _ = self.shutdown.send(true);
        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("the pusher must end on the shutdown signal")
            .expect("join")
            .expect("clean exit");
        started.elapsed()
    }
}

/// Wire up store + notifier + pusher exactly as `squelchd serve` does, then
/// spawn it. `seed` runs BEFORE the task starts, for the cold-start cases.
fn start(base_url: &str, seed: impl FnOnce(&SqliteStore, i64)) -> Harness {
    start_with_auth(base_url, None, seed)
}

/// [`start`] with a relay bearer configured, for the cases that care where that
/// bearer does — and does not — end up.
fn start_with_auth(
    base_url: &str,
    relay_token: Option<&str>,
    seed: impl FnOnce(&SqliteStore, i64),
) -> Harness {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let acct = store.ensure_account("me@example.com").unwrap();
    let (events, _) = broadcast::channel::<i64>(256);
    store.attach_event_notifier(events.clone()).unwrap();
    seed(&store, acct);

    let (shutdown, shutdown_rx) = watch::channel(false);
    let pusher = Pusher::for_test(store.clone() as Arc<dyn Store>, acct, base_url)
        .with_relay_auth(relay_token);
    let handle = tokio::spawn(pusher.run(events.subscribe(), shutdown_rx));
    Harness {
        store,
        acct,
        shutdown,
        handle,
    }
}

/// Poll `pred` until it holds or the deadline passes.
async fn wait_for(what: &str, timeout: Duration, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---- tests ------------------------------------------------------------------

/// The happy path: one POST per event, oldest first, cursor advancing to the
/// last accepted id.
#[tokio::test]
async fn a_push_advances_the_cursor() {
    let (base, relay) = spawn_relay().await;
    let h = start(&base, |_, _| {});
    h.register(DEV_A);

    // The first drain adopts the (empty) head as its cursor.
    wait_for("the initial cursor", Duration::from_secs(2), || {
        h.cursor() == Some(0)
    })
    .await;

    let first = h.emit(1);
    let last = h.emit(2);
    wait_for("both pushes", Duration::from_secs(5), || relay.count() == 2).await;
    wait_for("the cursor to catch up", Duration::from_secs(2), || {
        h.cursor() == Some(last)
    })
    .await;

    // One request per event, in order, each carrying its own id and thread.
    let bodies = relay.bodies();
    assert_eq!(bodies[0]["event_id"], json!(first));
    assert_eq!(bodies[1]["event_id"], json!(last));
    assert_eq!(bodies[0]["collapse_id"], json!("thread-thread1"));
    assert_eq!(bodies[1]["collapse_id"], json!("thread-thread2"));
    assert_eq!(bodies[0]["device_tokens"], json!([DEV_A]));

    h.stop().await;
}

/// THE INVARIANT. The pusher reads full event rows; the relay sees an id and a
/// collapse id and nothing else. Asserted on the wire.
#[tokio::test]
async fn the_relay_never_sees_content() {
    let (base, relay) = spawn_relay().await;
    let h = start(&base, |_, _| {});
    h.register(DEV_A);
    h.register(DEV_B);
    wait_for("the initial cursor", Duration::from_secs(2), || {
        h.cursor().is_some()
    })
    .await;

    h.emit(1);
    wait_for("the push", Duration::from_secs(5), || relay.count() == 1).await;

    let body = relay.bodies().remove(0);
    let obj = body.as_object().expect("a JSON object");

    // The shape is closed. A new key here is a privacy regression, not a feature.
    let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["collapse_id", "device_tokens", "event_id"]);

    // And no content survives anywhere in the encoded bytes.
    let raw = body.to_string();
    for leak in [
        SENDER,
        ONE_LINE,
        "past_due",
        "urgent",
        "95",
        "2026-08-01",
        "sender",
        "one_line",
        "tier",
        "importance",
        "deadline",
    ] {
        assert!(!raw.contains(leak), "content leaked to the relay: {leak}");
    }

    // Multi-device fan-out is one request, not one per device.
    assert_eq!(relay.count(), 1);
    assert_eq!(body["device_tokens"], json!([DEV_A, DEV_B]));

    h.stop().await;
}

/// APNs `410 Unregistered` comes back through the relay verbatim precisely so
/// THIS daemon deletes the row. The relay remembers nothing.
#[tokio::test]
async fn a_410_deletes_the_device() {
    let (base, relay) = spawn_relay().await;
    relay.dead.lock().unwrap().push(DEV_B.to_string());

    let h = start(&base, |_, _| {});
    h.register(DEV_A);
    h.register(DEV_B);
    wait_for("the initial cursor", Duration::from_secs(2), || {
        h.cursor().is_some()
    })
    .await;

    let id = h.emit(1);
    wait_for(
        "the dead device to be dropped",
        Duration::from_secs(5),
        || h.devices() == vec![DEV_A.to_string()],
    )
    .await;

    // The live device still got its push, and the event is not retried forever.
    wait_for("the cursor", Duration::from_secs(2), || {
        h.cursor() == Some(id)
    })
    .await;
    assert_eq!(relay.count(), 1);

    // A later event goes only to the survivor.
    h.emit(2);
    wait_for("the second push", Duration::from_secs(5), || {
        relay.count() == 2
    })
    .await;
    assert_eq!(relay.bodies()[1]["device_tokens"], json!([DEV_A]));

    h.stop().await;
}

/// A relay outage must DELAY pushes, never skip them: the cursor stays put and
/// the task retries after its backoff.
#[tokio::test]
async fn a_relay_failure_leaves_the_cursor_and_retries() {
    let (base, relay) = spawn_relay().await;
    relay.fail.store(true, Ordering::SeqCst);

    let h = start(&base, |_, _| {});
    h.register(DEV_A);
    wait_for("the initial cursor", Duration::from_secs(2), || {
        h.cursor() == Some(0)
    })
    .await;

    let id = h.emit(1);
    wait_for("the failed attempt", Duration::from_secs(5), || {
        relay.count() >= 1
    })
    .await;

    // The event was NOT consumed by a relay that refused it.
    assert_eq!(h.cursor(), Some(0), "a 500 must not advance the cursor");

    // Heal the relay; the backoff retry picks the very same event back up.
    relay.fail.store(false, Ordering::SeqCst);
    wait_for("the retry to land", Duration::from_secs(20), || {
        h.cursor() == Some(id)
    })
    .await;
    assert!(
        relay.count() >= 2,
        "the failed event was retried, not dropped"
    );
    assert_eq!(relay.bodies().last().unwrap()["event_id"], json!(id));

    h.stop().await;
}

/// THE OTHER HOP: the relay answers 200 while APNs behind it takes nothing, so
/// the event must be retried on the same terms as a relay outage — a 200 from
/// the relay is not evidence of delivery.
#[tokio::test]
async fn an_apns_failure_inside_a_200_leaves_the_cursor_and_retries() {
    let (base, relay) = spawn_relay().await;
    relay.apns_down.store(true, Ordering::SeqCst);

    let h = start(&base, |_, _| {});
    h.register(DEV_A);
    h.register(DEV_B);
    wait_for("the initial cursor", Duration::from_secs(2), || {
        h.cursor() == Some(0)
    })
    .await;

    let id = h.emit(1);
    wait_for("the failed attempt", Duration::from_secs(5), || {
        relay.count() >= 1
    })
    .await;

    // The relay said 200. Apple took nothing. The event is NOT consumed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        h.cursor(),
        Some(0),
        "a 200 carrying no delivered token must not advance the cursor"
    );
    // And the devices are still registered: status 0 is not 410.
    assert_eq!(h.devices().len(), 2);

    // Heal Apple; the backoff retry picks the very same event back up.
    relay.apns_down.store(false, Ordering::SeqCst);
    wait_for("the retry to land", Duration::from_secs(20), || {
        h.cursor() == Some(id)
    })
    .await;
    assert!(
        relay.count() >= 2,
        "the undelivered event was retried, not dropped"
    );
    assert_eq!(relay.bodies().last().unwrap()["event_id"], json!(id));

    h.stop().await;
}

/// THE BEARER STOPS AT THE CONFIGURED RELAY. A hostile relay (or a host with a
/// sloppy rewrite rule) answering `307` must not walk the token-bearing POST
/// elsewhere; refusing redirects makes that an ordinary non-2xx.
#[tokio::test]
async fn a_relay_redirect_is_refused_and_never_carries_the_bearer() {
    let (elsewhere_base, elsewhere) = spawn_relay().await;
    let (base, relay) = spawn_relay().await;
    *relay.redirect_to.lock().unwrap() = Some(format!("{elsewhere_base}/v1/push"));

    let h = start_with_auth(&base, Some("relay-secret"), |_, _| {});
    h.register(DEV_A);
    wait_for("the initial cursor", Duration::from_secs(2), || {
        h.cursor() == Some(0)
    })
    .await;

    let id = h.emit(1);
    wait_for("the redirected attempt", Duration::from_secs(5), || {
        relay.count() >= 1
    })
    .await;
    // Give a follower time to follow before believing this one did not.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        elsewhere.count(),
        0,
        "the pusher followed the relay's redirect"
    );
    assert!(
        elsewhere.auth.lock().unwrap().is_empty(),
        "the relay bearer reached a host the operator never configured"
    );
    // It DID reach the configured relay — this is the same request, simply not
    // followed onward.
    assert_eq!(
        relay.auth.lock().unwrap()[0].as_deref(),
        Some("Bearer relay-secret")
    );
    assert_eq!(
        h.cursor(),
        Some(0),
        "a 3xx is a non-2xx: the cursor must not advance"
    );

    // And it lands in the ordinary backoff: stop redirecting, the retry lands.
    *relay.redirect_to.lock().unwrap() = None;
    wait_for("the retry to land", Duration::from_secs(20), || {
        h.cursor() == Some(id)
    })
    .await;
    assert_eq!(elsewhere.count(), 0);

    h.stop().await;
}

/// A PARTIAL delivery is a delivery. One throttled device must not make the
/// pusher re-notify every healthy one — the cursor advances and the 429 is a
/// log line, not a retry.
#[tokio::test]
async fn a_partial_delivery_still_advances_the_cursor() {
    let (base, relay) = spawn_relay().await;
    relay.throttled.lock().unwrap().push(DEV_B.to_string());

    let h = start(&base, |_, _| {});
    h.register(DEV_A);
    h.register(DEV_B);
    wait_for("the initial cursor", Duration::from_secs(2), || {
        h.cursor() == Some(0)
    })
    .await;

    let id = h.emit(1);
    wait_for("the push", Duration::from_secs(5), || relay.count() == 1).await;
    wait_for("the cursor", Duration::from_secs(2), || {
        h.cursor() == Some(id)
    })
    .await;

    // Give it room to retry before believing it did not, and confirm the
    // throttled device is still registered (429 is not 410).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(relay.count(), 1, "a landed event must not be re-pushed");
    assert_eq!(h.devices().len(), 2);

    h.stop().await;
}

/// With nothing registered there is nobody to deliver to, so an unpushed event
/// is history rather than backlog: the cursor advances and no socket is ever
/// opened toward the relay.
#[tokio::test]
async fn no_devices_advances_the_cursor_without_a_request() {
    let (base, relay) = spawn_relay().await;
    let h = start(&base, |_, _| {});
    wait_for("the initial cursor", Duration::from_secs(2), || {
        h.cursor() == Some(0)
    })
    .await;

    h.emit(1);
    let last = h.emit(2);
    wait_for("the cursor to skip past", Duration::from_secs(5), || {
        h.cursor() == Some(last)
    })
    .await;
    assert_eq!(relay.count(), 0, "the relay must never have been called");

    // Registering now does not resurrect the skipped events, by design.
    h.register(DEV_A);
    let fresh = h.emit(3);
    wait_for("the first real push", Duration::from_secs(5), || {
        relay.count() == 1
    })
    .await;
    assert_eq!(relay.bodies()[0]["event_id"], json!(fresh));

    h.stop().await;
}

/// A first-ever start joins at the HEAD of the log. A phone that registers today
/// must not be handed last week's mail as a notification storm — the same rule a
/// cursorless SSE client gets.
#[tokio::test]
async fn a_cold_start_joins_at_the_head() {
    let (base, relay) = spawn_relay().await;
    let h = start(&base, |store, acct| {
        for message_id in 1..=3 {
            store
                .append_event(&NewEvent {
                    account_id: acct,
                    message_id,
                    thread_id: format!("old{message_id}"),
                    kind: EventKind::Surfaced,
                    tier: Tier::Signal,
                    importance: 70,
                    sender: SENDER.to_string(),
                    one_line: ONE_LINE.to_string(),
                    deadline: None,
                })
                .unwrap()
                .unwrap();
        }
        store.upsert_device(acct, DEV_A, "ios").unwrap();
    });

    wait_for("the head cursor", Duration::from_secs(2), || {
        h.cursor() == Some(3)
    })
    .await;
    // Give it room to misbehave before believing it did not.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(relay.count(), 0, "history must not be replayed as pushes");

    // What happens NEXT is delivered normally.
    let fresh = h.emit(4);
    wait_for("the new event", Duration::from_secs(5), || {
        relay.count() == 1
    })
    .await;
    assert_eq!(relay.bodies()[0]["event_id"], json!(fresh));

    h.stop().await;
}

/// The task must end on the daemon's shutdown watch even while parked on its
/// idle interval — otherwise graceful shutdown waits out a full minute.
#[tokio::test]
async fn shutdown_ends_the_task_promptly() {
    let (base, _relay) = spawn_relay().await;
    let h = start(&base, |_, _| {});
    wait_for("the first drain", Duration::from_secs(2), || {
        h.cursor().is_some()
    })
    .await;

    // Parked on the 60s idle tick at this point.
    let elapsed = h.stop().await;
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown took {elapsed:?}; the pusher is not watching the signal"
    );
}

/// The relay bearer is presented when configured, and the wake channel closing
/// (the store, and with it the process, going away) ends the task.
#[tokio::test]
async fn the_relay_bearer_is_presented_when_configured() {
    let (base, relay) = spawn_relay().await;
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let acct = store.ensure_account("me@example.com").unwrap();
    let (events, _) = broadcast::channel::<i64>(256);
    store.attach_event_notifier(events.clone()).unwrap();
    store.upsert_device(acct, DEV_A, "ios").unwrap();

    let (shutdown, shutdown_rx) = watch::channel(false);
    let pusher = Pusher::for_test(store.clone() as Arc<dyn Store>, acct, &base)
        .with_relay_auth(Some("relay-secret"));
    let handle = tokio::spawn(pusher.run(events.subscribe(), shutdown_rx));

    // Let the cold start settle on the (empty) head first: an event appended
    // before the first drain would legitimately be joined past, not pushed.
    wait_for("the initial cursor", Duration::from_secs(2), || {
        store.sync_state(acct, CURSOR_KEY).unwrap().is_some()
    })
    .await;

    store
        .append_event(&NewEvent {
            account_id: acct,
            message_id: 1,
            thread_id: "t1".to_string(),
            kind: EventKind::Urgent,
            tier: Tier::PastDue,
            importance: 95,
            sender: SENDER.to_string(),
            one_line: ONE_LINE.to_string(),
            deadline: None,
        })
        .unwrap()
        .unwrap();

    wait_for("the push", Duration::from_secs(5), || relay.count() == 1).await;
    assert_eq!(
        relay.auth.lock().unwrap()[0].as_deref(),
        Some("Bearer relay-secret")
    );

    let _ = shutdown.send(true);
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("prompt shutdown")
        .expect("join")
        .expect("clean exit");
}
