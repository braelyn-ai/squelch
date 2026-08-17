//! Integration tests for the assistant relay on the human door.
//!
//! The properties under test: the route is a 404 until BOTH gateway and key are
//! configured, the bearer layer wraps it, the upstream's status/content-type/
//! body cross back VERBATIM (the app's SSE parser depends on exact framing),
//! the daemon's credential — never the client's bearer — is what goes upstream,
//! and every request that spends money leaves an audit row.

use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use serde_json::Value;
use squelch_api::{ApiState, AssistantRelay, router};
use squelch_core::config::ResolvedAssistant;
use squelch_core::store::Store;
use tower::ServiceExt;

mod common;
use common::{TOKEN, authed, body_json, harness, state_with};

/// A conversation body the tests post; content is arbitrary but must cross
/// upstream byte-for-byte.
const REQUEST_BODY: &str = r#"{"model":"claude-sonnet-5","stream":true,"messages":[]}"#;

/// What the mock gateway captured from one relayed request.
type Seen = Arc<Mutex<Vec<(HeaderMap, Bytes)>>>;

/// Spawn a mock gateway serving `/v1/messages`: records the request's headers +
/// body, then streams `frames` back one chunk at a time under `status`,
/// `content_type`, and any `extra_headers`. Returns the full messages URL and
/// the capture log.
async fn spawn_upstream(
    status: StatusCode,
    content_type: &'static str,
    extra_headers: &'static [(&'static str, &'static str)],
    frames: &'static [&'static str],
) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let capture = seen.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |headers: HeaderMap, body: Bytes| {
            let capture = capture.clone();
            async move {
                capture.lock().unwrap().push((headers, body));
                let stream = tokio_stream::iter(
                    frames
                        .iter()
                        .map(|f| Ok::<_, std::convert::Infallible>(Bytes::from_static(f.as_bytes()))),
                );
                let mut builder = Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, content_type);
                for (name, value) in extra_headers {
                    builder = builder.header(*name, *value);
                }
                builder.body(Body::from_stream(stream)).unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1/messages"), seen)
}

/// State + router with the relay pointed at `url`, keyed with a fixed test key.
fn relay_app(url: String) -> (axum::Router, Arc<squelch_core::store::SqliteStore>, i64) {
    let (state, store, acct) = state_with(|_, _| {});
    let state: ApiState = state.with_assistant(Some(AssistantRelay::new(ResolvedAssistant {
        api_key: "sk-bf-test".into(),
        url,
    })));
    (router(state), store, acct)
}

/// An authed POST of [`REQUEST_BODY`] to the relay route.
fn relay_request(bearer: bool) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/client/assistant/messages")
        .header(header::CONTENT_TYPE, "application/json");
    if bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    b.body(Body::from(REQUEST_BODY)).unwrap()
}

#[tokio::test]
async fn unconfigured_relay_is_404() {
    // The default harness has no relay attached — the self-host posture.
    let h = harness(|_, _| {});
    let resp = h.app.oneshot(relay_request(true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "assistant_relay_unavailable");
}

#[tokio::test]
async fn relay_route_needs_the_bearer() {
    // Configured relay, missing bearer: the auth layer answers before the
    // handler ever runs, so nothing reaches the (never-spawned) upstream.
    let (app, _store, _acct) = relay_app("http://127.0.0.1:9/v1/messages".into());
    let resp = app.oneshot(relay_request(false)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn relay_streams_upstream_bytes_verbatim_and_audits() {
    const FRAMES: &[&str] = &[
        "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
        "event: content_block_delta\ndata: {\"delta\":{\"text\":\"hi\"}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ];
    let (url, seen) = spawn_upstream(StatusCode::OK, "text/event-stream", &[], FRAMES).await;
    let (app, store, acct) = relay_app(url);

    let resp = app.oneshot(relay_request(true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, FRAMES.concat().as_bytes());

    // The upstream saw the daemon's credential and wire pins — and NOT the
    // client's bearer — with the conversation body forwarded byte-for-byte.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (headers, body) = &seen[0];
    assert_eq!(headers.get("x-api-key").unwrap(), "sk-bf-test");
    assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/json");
    assert_eq!(headers.get(header::ACCEPT).unwrap(), "text/event-stream");
    assert!(headers.get(header::AUTHORIZATION).is_none());
    assert_eq!(body, REQUEST_BODY.as_bytes());

    // Spend honesty: one audit row, no content details.
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].actor, "client-api");
    assert_eq!(audit[0].action, "assistant_relay");
    assert_eq!(audit[0].target, None);
    assert_eq!(audit[0].detail.as_deref(), Some("status:200"));
}

#[tokio::test]
async fn relay_mirrors_an_upstream_error_status_and_body() {
    const ERROR_BODY: &[&str] = &[r#"{"type":"error","error":{"type":"rate_limit_error"}}"#];
    let (url, _seen) = spawn_upstream(
        StatusCode::TOO_MANY_REQUESTS,
        "application/json",
        // retry-after is on the response whitelist; request-id stands in for
        // everything that is not.
        &[("retry-after", "13"), ("request-id", "req_secret")],
        ERROR_BODY,
    )
    .await;
    let (app, store, acct) = relay_app(url);

    let resp = app.oneshot(relay_request(true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    // The whitelist, both directions: the backoff hint crosses so the app can
    // honor it; anything the gateway says about itself does not.
    assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "13");
    assert!(resp.headers().get("request-id").is_none());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, ERROR_BODY[0].as_bytes());

    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].detail.as_deref(), Some("status:429"));
}

#[tokio::test]
async fn unreachable_gateway_is_502() {
    // Grab a port nothing listens on by binding and immediately dropping it.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (app, store, acct) = relay_app(format!("http://127.0.0.1:{port}/v1/messages"));

    let resp = app.oneshot(relay_request(true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let json = body_json(resp).await;
    assert_eq!(json["error"], "assistant_relay_unreachable");

    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].detail.as_deref(), Some("failed:transport"));
}

#[tokio::test]
async fn stats_reports_the_relay_capability() {
    // Without a relay: present and false, so the app reads an answer.
    let h = harness(|_, _| {});
    let resp = h.app.oneshot(authed("GET", "/client/stats")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: Value = body_json(resp).await;
    assert_eq!(json["assistant_relay"], false);

    // With one: true.
    let (app, _store, _acct) = relay_app("http://127.0.0.1:9/v1/messages".into());
    let resp = app.oneshot(authed("GET", "/client/stats")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: Value = body_json(resp).await;
    assert_eq!(json["assistant_relay"], true);
}
