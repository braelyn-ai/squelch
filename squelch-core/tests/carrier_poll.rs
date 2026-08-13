//! End-to-end tests for the shipment poller against a mock carrier: an axum app
//! on an ephemeral loopback port speaking DHL's wire shape, so the pacing rules
//! are asserted on the requests that actually left the daemon.
//!
//! The mock counts and TIMES every hit, because the guarantees under test are
//! all about calls that must NOT happen: a cooling-down carrier must go quiet, a
//! retired shipment must stop being asked about, a delivered one must never be
//! asked about at all, and two calls to one carrier must be spaced. A poller
//! that got the row states right while hammering the API would pass a
//! store-only test and lose the operator their API key.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use squelch_core::carriers::poller::ShipmentPoller;
use squelch_core::carriers::{CarrierClient, CarrierRegistry, TrackError, dhl::DhlClient};
use squelch_core::store::{SqliteStore, Store};
use squelch_core::triage::{CarrierTrack, ShipmentInfo, ShipmentStatus};
use squelch_core::types::{NewMessage, Shipment};
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};

// ---- mock carrier -----------------------------------------------------------

/// What the mock answers next. One mode for the whole server: every test drives
/// exactly one failure class.
#[derive(Clone)]
enum Mode {
    /// A normal track: DHL `statusCode` plus its human description.
    Track(&'static str, &'static str),
    NotFound,
    /// 429, optionally with a `Retry-After` in seconds.
    RateLimited(Option<u64>),
    /// ONE POISON NUMBER: that number gets a 200 whose body will not parse (a
    /// `TrackError::Transient`, deterministically, forever), every other number
    /// gets a normal track. This is the real shape of the failure — a carrier
    /// that is up and answering, about a row it will never answer usefully.
    PoisonOne(&'static str),
}

#[derive(Clone)]
struct Carrier {
    /// When each request arrived, in order. The spacing assertions read this.
    hits: Arc<Mutex<Vec<Instant>>>,
    /// Every tracking number asked about, in order.
    asked: Arc<Mutex<Vec<String>>>,
    mode: Arc<Mutex<Mode>>,
}

impl Carrier {
    fn new(mode: Mode) -> Self {
        Self {
            hits: Arc::new(Mutex::new(Vec::new())),
            asked: Arc::new(Mutex::new(Vec::new())),
            mode: Arc::new(Mutex::new(mode)),
        }
    }

    fn hits(&self) -> usize {
        self.hits.lock().unwrap().len()
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

async fn track(
    State(carrier): State<Carrier>,
    _headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    carrier.hits.lock().unwrap().push(Instant::now());
    carrier
        .asked
        .lock()
        .unwrap()
        .push(query.get("trackingNumber").cloned().unwrap_or_default());

    let asked = query.get("trackingNumber").cloned().unwrap_or_default();
    let mode = carrier.mode.lock().unwrap().clone();
    match mode {
        Mode::Track(code, description) => Json(json!({
            "shipments": [{ "status": { "statusCode": code, "description": description } }]
        }))
        .into_response(),
        Mode::PoisonOne(number) if asked == number => {
            (StatusCode::OK, "{ this is not the response you ordered").into_response()
        }
        Mode::PoisonOne(_) => Json(json!({
            "shipments": [{ "status": { "statusCode": "transit", "description": "In transit" } }]
        }))
        .into_response(),
        Mode::NotFound => StatusCode::NOT_FOUND.into_response(),
        Mode::RateLimited(retry_after) => {
            let mut headers = HeaderMap::new();
            if let Some(secs) = retry_after {
                headers.insert("retry-after", secs.to_string().parse().unwrap());
            }
            (StatusCode::TOO_MANY_REQUESTS, headers).into_response()
        }
    }
}

async fn spawn_carrier(mode: Mode) -> (String, Carrier) {
    let carrier = Carrier::new(mode);
    let app = Router::new()
        .route("/track/shipments", get(track))
        .with_state(carrier.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), carrier)
}

/// The real [`DhlClient`] with a shrunken `min_interval`, so the spacing test
/// asserts the poller's pacing in milliseconds instead of DHL's five seconds.
/// Everything else — the request, the parsing, the error classes — is the
/// production client.
struct Spaced {
    inner: DhlClient,
    interval: Duration,
}

#[async_trait]
impl CarrierClient for Spaced {
    fn carrier(&self) -> &'static str {
        self.inner.carrier()
    }

    fn min_interval(&self) -> Duration {
        self.interval
    }

    async fn track(&self, tracking_number: &str) -> Result<CarrierTrack, TrackError> {
        self.inner.track(tracking_number).await
    }
}

/// A registry holding one DHL client aimed at the mock, spaced as asked.
fn registry(base: &str, interval: Duration) -> CarrierRegistry {
    let client = Spaced {
        inner: DhlClient::for_test(base, reqwest::Client::new()),
        interval,
    };
    let mut registry = CarrierRegistry::new();
    registry.insert(
        "dhl".to_string(),
        Arc::new(client) as Arc<dyn CarrierClient>,
    );
    registry
}

// ---- daemon-side harness ----------------------------------------------------

const NUMBER: &str = "00340434292135100186";
const OTHER: &str = "00340434292135100187";
const THIRD: &str = "00340434292135100188";

/// A store with one triaged message every shipment hangs off.
fn seeded() -> (Arc<SqliteStore>, i64, i64) {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let acct = store.ensure_account("me@example.com").unwrap();
    let message_id = store
        .upsert_message(&NewMessage {
            account_id: acct,
            gmail_msg_id: "ship-1".to_string(),
            thread_id: "thread-77".to_string(),
            from_addr: "shipping@example.com".to_string(),
            from_name: None,
            subject: "Your package is on its way".to_string(),
            received_at: chrono::Utc::now(),
            snippet: String::new(),
            body: String::new(),
            body_html: None,
            is_sent: false,
            to_addrs: None,
            list_unsubscribe: None,
            list_unsub_one_click: false,
            auth_pass: None,
        })
        .unwrap();
    (store, acct, message_id)
}

fn land(
    store: &SqliteStore,
    acct: i64,
    message_id: i64,
    number: &str,
    status: ShipmentStatus,
) -> i64 {
    store
        .upsert_shipment(
            acct,
            message_id,
            &ShipmentInfo {
                carrier: "dhl".to_string(),
                tracking_number: number.to_string(),
                item_name: "Socks".to_string(),
                status,
                tracking_url: None,
            },
            chrono::Utc::now(),
        )
        .unwrap()
}

fn row(store: &SqliteStore, acct: i64, number: &str) -> Shipment {
    // u32::MAX = no ambiguous-row suppression: these tests are about polling,
    // and several of them deliberately drive `poll_failures` up.
    store
        .list_shipments(acct, true, u32::MAX)
        .unwrap()
        .into_iter()
        .find(|s| s.tracking_number == number)
        .expect("the shipment row")
}

/// Poll `want` until it holds, failing the test if it never does.
async fn wait_for(what: &str, want: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !want() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A running poller plus the switch that stops it.
struct Running {
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<Result<(), squelch_core::CoreError>>,
    /// The kick handle, taken before the poller moved into its task.
    kick: Arc<Notify>,
}

fn start(poller: ShipmentPoller) -> Running {
    let (shutdown, shutdown_rx) = watch::channel(false);
    let kick = poller.kick_handle();
    let handle = tokio::spawn(async move { poller.run(shutdown_rx).await });
    Running {
        shutdown,
        handle,
        kick,
    }
}

impl Running {
    async fn stop(self) {
        let _ = self.shutdown.send(true);
        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("the poller must end on the shutdown signal")
            .expect("join")
            .expect("clean exit");
    }
}

// ---- tests ------------------------------------------------------------------

/// The happy path: a due row is polled, the carrier's answer replaces the
/// email-inferred status, and the row's click target survives.
#[tokio::test]
async fn a_due_shipment_is_polled_and_its_row_advances() {
    let (base, carrier) = spawn_carrier(Mode::Track("transit", "Out for delivery")).await;
    let (store, acct, message_id) = seeded();
    land(&store, acct, message_id, NUMBER, ShipmentStatus::Shipped);

    let poller = ShipmentPoller::for_test(
        store.clone() as Arc<dyn Store>,
        acct,
        registry(&base, Duration::ZERO),
    );
    let running = start(poller);
    let s = store.clone();
    wait_for("the row to advance", move || {
        row(&s, acct, NUMBER).status == "out_for_delivery"
    })
    .await;
    running.stop().await;

    let advanced = row(&store, acct, NUMBER);
    assert_eq!(
        advanced.carrier_status_raw.as_deref(),
        Some("transit Out for delivery"),
        "the carrier's own string is recorded verbatim"
    );
    assert!(advanced.last_polled_at.is_some(), "the attempt is stamped");
    assert_eq!(
        advanced.thread_id.as_deref(),
        Some("thread-77"),
        "a poll must not touch last_message_id: the row stays clickable"
    );
    assert_eq!(carrier.asked()[0], NUMBER);
}

/// A number the carrier does not know counts against the shipment, and once the
/// cap is reached the row leaves the pollable set for good — no matter how long
/// the poller keeps running.
#[tokio::test]
async fn a_not_found_row_retires_at_its_failure_cap() {
    let (base, carrier) = spawn_carrier(Mode::NotFound).await;
    let (store, acct, message_id) = seeded();
    land(&store, acct, message_id, NUMBER, ShipmentStatus::Shipped);

    let poller = ShipmentPoller::for_test(
        store.clone() as Arc<dyn Store>,
        acct,
        registry(&base, Duration::ZERO),
    )
    .with_max_failures(2)
    .with_tick_interval(Duration::from_millis(10));
    let running = start(poller);

    let s = store.clone();
    wait_for("the row to retire", move || {
        s.list_pollable_shipments(acct, chrono::Utc::now() - chrono::Duration::days(365), 2)
            .unwrap()
            .is_empty()
    })
    .await;
    // Several more ticks' worth of chances to poll a retired row.
    tokio::time::sleep(Duration::from_millis(200)).await;
    running.stop().await;

    assert_eq!(
        carrier.hits(),
        2,
        "a retired shipment must never be asked about again"
    );
    // The row is still there and still not delivered; it is just out of the
    // poll queue.
    assert_eq!(row(&store, acct, NUMBER).status, "shipped");
}

/// A 429 is a CARRIER verdict: the carrier goes quiet, and the row it happened
/// to be holding keeps its old (absent) stamp so it is first in line when the
/// cooldown lifts.
#[tokio::test]
async fn a_rate_limited_carrier_goes_quiet_and_the_row_is_not_stamped() {
    let (base, carrier) = spawn_carrier(Mode::RateLimited(Some(1))).await;
    let (store, acct, message_id) = seeded();
    land(&store, acct, message_id, NUMBER, ShipmentStatus::Shipped);
    land(&store, acct, message_id, OTHER, ShipmentStatus::Shipped);

    let poller = ShipmentPoller::for_test(
        store.clone() as Arc<dyn Store>,
        acct,
        registry(&base, Duration::ZERO),
    )
    .with_tick_interval(Duration::from_millis(10));
    let running = start(poller);

    let hits = carrier.hits.clone();
    wait_for("the first 429", move || !hits.lock().unwrap().is_empty()).await;
    // ~20 ticks. The carrier said "1 second"; the floor is 15 minutes, so not
    // one of them may reach the server.
    tokio::time::sleep(Duration::from_millis(200)).await;
    running.stop().await;

    assert_eq!(
        carrier.hits(),
        1,
        "the whole carrier cools down, including the rest of its queue"
    );
    for number in [NUMBER, OTHER] {
        assert_eq!(
            row(&store, acct, number).last_polled_at,
            None,
            "a skipped row must not be stamped as polled"
        );
    }
}

/// DEFECT: one poison number used to starve its carrier's whole queue. A
/// transient answer wrote NOTHING to the row — no stamp, no counter — and cooled
/// the WHOLE carrier down, ending the pass. Because the queue sorts never-polled
/// first, the unstamped row was still the head of it next pass: the poller spent
/// every pass, forever, on the one number that could not work, and the parcels
/// behind it were never called at all.
///
/// The poison row is landed FIRST here precisely so it holds that head position.
#[tokio::test]
async fn a_transient_row_does_not_deny_service_to_the_rest_of_the_queue() {
    let (base, carrier) = spawn_carrier(Mode::PoisonOne(NUMBER)).await;
    let (store, acct, message_id) = seeded();
    for number in [NUMBER, OTHER, THIRD] {
        land(&store, acct, message_id, number, ShipmentStatus::Shipped);
    }

    let poller = ShipmentPoller::for_test(
        store.clone() as Arc<dyn Store>,
        acct,
        registry(&base, Duration::ZERO),
    )
    .with_tick_interval(Duration::from_millis(10));
    let running = start(poller);

    let s = store.clone();
    wait_for("the rows queued behind the poison one", move || {
        [OTHER, THIRD]
            .iter()
            .all(|n| row(&s, acct, n).last_polled_at.is_some())
    })
    .await;
    running.stop().await;

    let poisoned = row(&store, acct, NUMBER);
    assert!(
        poisoned.last_polled_at.is_some(),
        "the attempt reached the carrier: stamp it, or this row owns the queue head forever"
    );
    assert_eq!(
        poisoned.poll_failures, 0,
        "a transient failure is the carrier's problem, not the parcel's"
    );
    assert!(
        carrier.asked().iter().any(|n| n == OTHER),
        "the queue behind the poison row was never served: {:?}",
        carrier.asked()
    );
}

/// Delivered is terminal. The store's own filter is what enforces it, and this
/// pins that the poller does not route around it.
#[tokio::test]
async fn a_delivered_shipment_is_never_polled() {
    let (base, carrier) = spawn_carrier(Mode::Track("transit", "In transit")).await;
    let (store, acct, message_id) = seeded();
    land(&store, acct, message_id, OTHER, ShipmentStatus::Delivered);
    land(&store, acct, message_id, NUMBER, ShipmentStatus::Shipped);

    let poller = ShipmentPoller::for_test(
        store.clone() as Arc<dyn Store>,
        acct,
        registry(&base, Duration::ZERO),
    )
    .with_tick_interval(Duration::from_millis(10));
    let running = start(poller);

    let s = store.clone();
    wait_for("the live row to be polled", move || {
        row(&s, acct, NUMBER).last_polled_at.is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    running.stop().await;

    assert!(
        !carrier.asked().iter().any(|n| n == OTHER),
        "a delivered parcel was polled: {:?}",
        carrier.asked()
    );
    assert_eq!(row(&store, acct, OTHER).last_polled_at, None);
}

/// The kick is what a future `POST /client/shipments/poll` pulls: a pass starts
/// now, without waiting out a tick that is minutes away.
#[tokio::test]
async fn a_kick_forces_an_immediate_pass() {
    let (base, carrier) = spawn_carrier(Mode::Track("transit", "In transit")).await;
    let (store, acct, message_id) = seeded();
    land(&store, acct, message_id, NUMBER, ShipmentStatus::Shipped);

    // A tick this long means the second poll can only come from the kick.
    let poller = ShipmentPoller::for_test(
        store.clone() as Arc<dyn Store>,
        acct,
        registry(&base, Duration::ZERO),
    )
    .with_tick_interval(Duration::from_secs(600));
    let running = start(poller);

    let hits = carrier.hits.clone();
    wait_for("the startup pass", move || !hits.lock().unwrap().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(carrier.hits(), 1, "the tick alone would not poll again");

    running.kick.notify_one();
    let hits = carrier.hits.clone();
    wait_for("the kicked pass", move || hits.lock().unwrap().len() >= 2).await;
    running.stop().await;
}

/// Within one carrier, calls are sequential and spaced by its own
/// `min_interval` — the floor that keeps a burst of newly-detected parcels from
/// spending a rate limit in one second.
#[tokio::test]
async fn calls_to_one_carrier_are_spaced_by_its_min_interval() {
    const SPACING: Duration = Duration::from_millis(200);

    let (base, carrier) = spawn_carrier(Mode::Track("transit", "In transit")).await;
    let (store, acct, message_id) = seeded();
    for number in [NUMBER, OTHER, THIRD] {
        land(&store, acct, message_id, number, ShipmentStatus::Shipped);
    }

    let poller = ShipmentPoller::for_test(
        store.clone() as Arc<dyn Store>,
        acct,
        registry(&base, SPACING),
    )
    .with_tick_interval(Duration::from_secs(600));
    let running = start(poller);

    let hits = carrier.hits.clone();
    wait_for("all three polls", move || hits.lock().unwrap().len() >= 3).await;
    running.stop().await;

    let hits = carrier.hits.lock().unwrap().clone();
    assert_eq!(hits.len(), 3, "each due row is polled exactly once");
    for pair in hits.windows(2) {
        let gap = pair[1].duration_since(pair[0]);
        // Loose on purpose: the assertion is that a floor exists, not that the
        // timer is exact. A missing floor shows up as a gap near zero.
        assert!(
            gap >= SPACING.mul_f64(0.8),
            "two calls to one carrier were {gap:?} apart, under the {SPACING:?} floor"
        );
    }
}
