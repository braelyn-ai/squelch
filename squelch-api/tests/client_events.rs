//! Integration tests for the notification door: `GET /client/events` (SSE) and
//! `GET /client/events/{id}`. The stream is INFINITE by design, so nothing here
//! may collect a whole body — every read pulls frames with a timeout and
//! asserts on what arrived, or on what deliberately did not.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use squelch_api::router;
use squelch_core::store::{NewEvent, SqliteStore, Store};
use squelch_core::types::{EventKind, Tier};
use tokio::sync::{broadcast, watch};
use tower::ServiceExt;

mod common;
use common::{authed, state_with};

/// How long a read waits before the test calls the stream hung.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Total budget for a shutdown to actually close a body. Longer than
/// [`READ_TIMEOUT`] on purpose: this one is only ever spent in full when the
/// test is about to FAIL, so it buys tolerance for a loaded CI runner at no
/// cost to the passing path, and the failure it reports is unambiguous.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long "nothing should arrive" waits before believing it.
const QUIET: Duration = Duration::from_millis(300);

struct Harness {
    app: axum::Router,
    store: Arc<SqliteStore>,
    acct: i64,
    /// Held so the daemon-side shutdown signal stays alive for the connection.
    shutdown: watch::Sender<bool>,
}

impl Harness {
    /// Append one event and return its id, through the real store so the
    /// attached notifier fires exactly as it does in the daemon.
    fn emit(&self, message_id: i64) -> i64 {
        self.store
            .append_event(&event_for(self.acct, message_id))
            .unwrap()
            .unwrap()
    }
}

fn event_for(account_id: i64, message_id: i64) -> NewEvent {
    NewEvent {
        account_id,
        message_id,
        thread_id: format!("t{message_id}"),
        kind: EventKind::Surfaced,
        tier: Tier::Signal,
        importance: 70,
        sender: "alice@example.com".to_string(),
        one_line: format!("line {message_id}"),
        deadline: None,
        sealed_kind: None,
    }
}

/// The production wiring: one broadcast sender attached to BOTH the store and
/// the state, plus the shutdown watch squelchd hands the human door.
fn harness(seed: impl FnOnce(&SqliteStore, i64)) -> Harness {
    let (state, store, acct) = state_with(seed);
    let (event_tx, _) = broadcast::channel::<i64>(256);
    store.attach_event_notifier(event_tx.clone()).unwrap();
    let (shutdown, shutdown_rx) = watch::channel(false);
    let state = state
        .with_event_notifier(event_tx)
        .with_shutdown(shutdown_rx);
    Harness {
        app: router(state),
        store,
        acct,
        shutdown,
    }
}

/// One parsed SSE frame.
#[derive(Debug)]
struct Frame {
    id: Option<String>,
    data: String,
}

impl Frame {
    fn json(&self) -> Value {
        serde_json::from_str(&self.data).expect("data field must be JSON")
    }
}

/// Incremental SSE reader: one frame at a time with a timeout, since `to_bytes`
/// would hang forever on a live feed.
struct Reader {
    body: Body,
    buf: String,
    ended: bool,
}

impl Reader {
    fn new(resp: axum::response::Response) -> Self {
        Self {
            body: resp.into_body(),
            buf: String::new(),
            ended: false,
        }
    }

    /// Take a complete frame out of the buffer, skipping keep-alive comments.
    fn take_buffered(&mut self) -> Option<Frame> {
        while let Some(end) = self.buf.find("\n\n") {
            let raw = self.buf[..end].to_string();
            self.buf.drain(..end + 2);
            let mut frame = Frame {
                id: None,
                data: String::new(),
            };
            for line in raw.lines() {
                if let Some(v) = line.strip_prefix("id:") {
                    frame.id = Some(v.trim_start().to_string());
                } else if let Some(v) = line.strip_prefix("data:") {
                    frame.data.push_str(v.trim_start());
                }
            }
            // A comment-only chunk is the keep-alive ping, not an event.
            if frame.id.is_some() || !frame.data.is_empty() {
                return Some(frame);
            }
        }
        None
    }

    /// Next frame within `wait`. `None` means ended OR nothing arrived in time;
    /// `ended` tells the two apart.
    async fn next_within(&mut self, wait: Duration) -> Option<Frame> {
        loop {
            if let Some(frame) = self.take_buffered() {
                return Some(frame);
            }
            if self.ended {
                return None;
            }
            match tokio::time::timeout(wait, self.body.frame()).await {
                Err(_) => return None,
                Ok(None) => {
                    self.ended = true;
                    return None;
                }
                Ok(Some(chunk)) => {
                    let chunk = chunk.expect("body error");
                    if let Some(data) = chunk.data_ref() {
                        self.buf.push_str(std::str::from_utf8(data).unwrap());
                    }
                }
            }
        }
    }

    /// Next frame, or panic. The normal read.
    async fn next(&mut self) -> Frame {
        self.next_within(READ_TIMEOUT)
            .await
            .unwrap_or_else(|| panic!("expected an SSE frame (stream ended: {})", self.ended))
    }

    /// Read until the body ENDS, up to `wait` in total, returning whatever
    /// arrived first. Panics if the stream is still open when the budget runs
    /// out, which is the one failure worth telling apart: a HUNG connection.
    ///
    /// Shutdown is a race the test must not care about. The signal and the
    /// replay drain are two arms of one `select!`, so a frame already queued
    /// when the flag flips is allowed out ahead of the close, and a busy runner
    /// can put the whole thing after an arbitrary scheduling delay. Neither is
    /// a bug. Asserting on a SINGLE read conflates all three of "a frame came
    /// first", "the scheduler was slow" and "the connection hung", because
    /// [`Reader::next_within`] answers `None` to the first two as well.
    async fn drain_until_end(&mut self, wait: Duration) -> Vec<Frame> {
        let deadline = Instant::now() + wait;
        let mut frames = Vec::new();
        while !self.ended {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "stream still open after {wait:?}, not ended"
            );
            if let Some(frame) = self.next_within(left).await {
                frames.push(frame);
            } else {
                assert!(self.ended, "stream still open after {wait:?}, not ended");
            }
        }
        frames
    }

    /// Assert nothing arrives for `QUIET`.
    async fn expect_quiet(&mut self) {
        if let Some(frame) = self.next_within(QUIET).await {
            panic!("expected silence, got frame {frame:?}");
        }
        assert!(!self.ended, "stream must stay open while quiet");
    }
}

/// Open the SSE stream and assert the response envelope.
async fn open(h: &Harness, uri: &str) -> Reader {
    let resp = h.app.clone().oneshot(authed("GET", uri)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.starts_with("text/event-stream"), "content-type was {ct}");
    Reader::new(resp)
}

// --- auth --------------------------------------------------------------------

#[tokio::test]
async fn events_stream_requires_bearer() {
    let h = harness(|_, _| {});
    let req = Request::builder()
        .uri("/client/events?after=0")
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the feed is behind auth"
    );

    let req = Request::builder()
        .uri("/client/events?after=0")
        .header(header::AUTHORIZATION, "Bearer nope")
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a wrong token is no token"
    );
}

#[tokio::test]
async fn event_by_id_requires_bearer() {
    let h = harness(|_, _| {});
    let id = h.emit(1);
    let req = Request::builder()
        .uri(format!("/client/events/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the NSE's by-id fetch is behind auth too"
    );
}

// --- GET /client/events/{id} -------------------------------------------------

#[tokio::test]
async fn event_by_id_round_trips() {
    let h = harness(|_, _| {});
    let id = h.emit(7);

    let resp = h
        .app
        .clone()
        .oneshot(authed("GET", &format!("/client/events/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], id);
    assert_eq!(json["message_id"], 7);
    assert_eq!(json["thread_id"], "t7");
    assert_eq!(json["kind"], "surfaced", "EventKind serializes snake_case");
    assert_eq!(json["tier"], "signal");
    assert_eq!(json["importance"], 70);
    assert_eq!(json["sender"], "alice@example.com");
    assert_eq!(json["one_line"], "line 7");
    assert_eq!(json["deadline"], Value::Null);
    assert!(json["created_at"].is_string());
}

#[tokio::test]
async fn unknown_event_id_is_404() {
    let h = harness(|_, _| {});
    let resp = h
        .app
        .clone()
        .oneshot(authed("GET", "/client/events/999999"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn another_accounts_event_is_404() {
    let h = harness(|_, _| {});
    let other = h.store.ensure_account("other@example.com").unwrap();
    let theirs = h
        .store
        .append_event(&event_for(other, 42))
        .unwrap()
        .unwrap();

    // Indistinguishable from a missing id: an event id is a guessable integer.
    let resp = h
        .app
        .clone()
        .oneshot(authed("GET", &format!("/client/events/{theirs}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // And it never leaks into this account's replay either.
    let mine = h.emit(1);
    let mut r = open(&h, "/client/events?after=0").await;
    assert_eq!(r.next().await.json()["id"], mine);
    r.expect_quiet().await;
}

// --- replay ------------------------------------------------------------------

#[tokio::test]
async fn replays_everything_past_the_cursor_in_order() {
    let h = harness(|_, _| {});
    let ids: Vec<i64> = (1..=3).map(|m| h.emit(m)).collect();

    let mut r = open(&h, &format!("/client/events?after={}", ids[0])).await;

    for want in &ids[1..] {
        let frame = r.next().await;
        assert_eq!(
            frame.id.as_deref(),
            Some(want.to_string().as_str()),
            "the SSE id field IS the cursor the client sends back"
        );
        assert_eq!(frame.json()["id"], *want);
    }
    // The row at the cursor itself is NOT re-sent, and nothing else follows.
    r.expect_quiet().await;
}

#[tokio::test]
async fn replay_spans_multiple_read_batches() {
    // 250 > the 100-row replay batch: the drain loop must page, not truncate.
    let h = harness(|_, _| {});
    let ids: Vec<i64> = (1..=250).map(|m| h.emit(m)).collect();

    let mut r = open(&h, "/client/events?after=0").await;
    let mut got = Vec::new();
    for _ in 0..ids.len() {
        got.push(r.next().await.json()["id"].as_i64().unwrap());
    }
    assert_eq!(got, ids, "every row, oldest first, no gap at a batch seam");
    r.expect_quiet().await;
}

#[tokio::test]
async fn no_cursor_means_live_only_no_backlog() {
    // A fresh install must not be handed the history as a notification storm.
    let h = harness(|_, _| {});
    for m in 1..=5 {
        h.emit(m);
    }

    let mut r = open(&h, "/client/events").await;
    r.expect_quiet().await;

    // But it is live: the next real event arrives, and only that one.
    let fresh = h.emit(6);
    let frame = r.next().await;
    assert_eq!(frame.id.as_deref(), Some(fresh.to_string().as_str()));
    assert_eq!(frame.json()["message_id"], 6);
    r.expect_quiet().await;
}

// --- live --------------------------------------------------------------------

#[tokio::test]
async fn replay_then_live_on_one_connection() {
    let h = harness(|_, _| {});
    let seeded: Vec<i64> = (1..=2).map(|m| h.emit(m)).collect();

    let mut r = open(&h, "/client/events?after=0").await;
    for want in &seeded {
        assert_eq!(r.next().await.json()["id"], *want);
    }
    r.expect_quiet().await;

    // Appended AFTER the stream was already open and drained.
    let live = h.emit(3);
    let frame = r.next().await;
    assert_eq!(frame.id.as_deref(), Some(live.to_string().as_str()));
    assert_eq!(frame.json()["one_line"], "line 3");
}

#[tokio::test]
async fn live_works_off_a_store_attached_notifier_alone() {
    // Half-wired: the notifier is on the store but never reached the state, so
    // the handler falls back to the store's.
    let (state, store, acct) = state_with(|_, _| {});
    let (event_tx, _) = broadcast::channel::<i64>(16);
    store.attach_event_notifier(event_tx).unwrap();
    let (shutdown, _shutdown_rx) = watch::channel(false);
    let h = Harness {
        app: router(state),
        store,
        acct,
        shutdown,
    };

    let mut r = open(&h, "/client/events").await;
    r.expect_quiet().await;
    let live = h.emit(1);
    assert_eq!(r.next().await.json()["id"], live);
}

#[tokio::test]
async fn no_notifier_at_all_still_replays_and_holds_the_connection() {
    // Nothing in the process can append events, so parking is correct — the
    // stream must NOT report end-of-stream the moment it opens.
    let (state, store, acct) = state_with(|_, _| {});
    let (shutdown, _rx) = watch::channel(false);
    let h = Harness {
        app: router(state),
        store,
        acct,
        shutdown,
    };
    // No notifier anywhere: this append can only be seen by the replay drain.
    let seeded = h.emit(1);

    let mut r = open(&h, "/client/events?after=0").await;
    assert_eq!(r.next().await.json()["id"], seeded);
    r.expect_quiet().await;
}

// --- shutdown ----------------------------------------------------------------

#[tokio::test]
async fn stream_ends_when_the_daemon_shuts_down() {
    // Without this, graceful shutdown waits on an infinite body forever and
    // squelchd can never exit with a client connected.
    let h = harness(|_, _| {});
    let seeded = h.emit(1);

    let mut r = open(&h, "/client/events?after=0").await;
    assert_eq!(r.next().await.json()["id"], seeded);

    h.shutdown.send(true).unwrap();

    // The body must actually END, not just go quiet: `drain_until_end` panics
    // rather than returning if it is still open when the budget runs out.
    assert!(
        r.drain_until_end(SHUTDOWN_TIMEOUT).await.is_empty(),
        "no further frames"
    );
}

#[tokio::test]
async fn a_stream_opened_after_shutdown_ends_immediately() {
    let h = harness(|_, _| {});
    h.emit(1);
    h.shutdown.send(true).unwrap();

    // `wait_for` checks the CURRENT value first, so an already-flipped signal
    // closes the stream instead of being missed until the next change. Whether
    // the seeded event escapes ahead of the close is the `select!` arms racing
    // and is not what this asserts: already-shutting-down means END, not a hung
    // connection, and `drain_until_end` panics on the hang.
    let mut r = open(&h, "/client/events?after=0").await;
    r.drain_until_end(SHUTDOWN_TIMEOUT).await;
}
