//! Integration tests for the assistant relay on the human door.
//!
//! The properties under test: the route is a 404 until BOTH gateway and key are
//! configured, the bearer layer wraps it, the upstream's status/content-type/
//! body cross back VERBATIM (the app's SSE parser depends on exact framing),
//! the daemon's credential — never the client's bearer — is what goes upstream,
//! and every request that spends money leaves an audit row.
//!
//! Two bodies are called "verbatim" in here and they are NOT the same property.
//! The RESPONSE is verbatim without exception. The REQUEST is verbatim except
//! for the model id, which the relay qualifies with its provider on the way to
//! a gateway, because only the daemon knows a gateway is downstream (see
//! `qualify_model_for_gateway`). Each half gets its own test so a future change
//! to one cannot pass itself off as the other.

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

/// A conversation body the tests post. Its model is BARE, which is what the app
/// actually sends: the same Settings choice drives BYOK straight to Anthropic,
/// where the provider prefix is the invalid spelling.
const REQUEST_BODY: &str = r#"{"model":"claude-sonnet-5","stream":true,"messages":[]}"#;

/// What the gateway must see instead, and the ONLY edit the relay is allowed to
/// make to a request body.
const QUALIFIED_MODEL: &str = "anthropic/claude-sonnet-5";

/// A body whose model already names a provider, spelled with whitespace and an
/// unusual key order so that a re-serialization would be visible in the bytes.
/// The relay has nothing to fix here, so these exact bytes must cross.
const PREQUALIFIED_BODY: &str =
    r#"{ "stream": true, "model": "anthropic/claude-sonnet-5", "messages": [] }"#;

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
                let stream =
                    tokio_stream::iter(frames.iter().map(|f| {
                        Ok::<_, std::convert::Infallible>(Bytes::from_static(f.as_bytes()))
                    }));
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
    relay_request_of(bearer, REQUEST_BODY)
}

/// [`relay_request`] over an arbitrary body, for the tests that care which
/// spelling of the model the app sent.
fn relay_request_of(bearer: bool, body: &'static str) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/client/assistant/messages")
        .header(header::CONTENT_TYPE, "application/json");
    if bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    b.body(Body::from(body)).unwrap()
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
    // client's bearer. What the body looked like is a separate property with
    // its own tests below; this one is about the response.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (headers, _body) = &seen[0];
    assert_eq!(headers.get("x-api-key").unwrap(), "sk-bf-test");
    // The gateway reads its virtual key ONLY from `x-bf-vk`; a mock on a
    // loopback port is still not Anthropic's endpoint, so the gate is open and
    // the same one the model qualifier rides.
    assert_eq!(headers.get("x-bf-vk").unwrap(), "sk-bf-test");
    assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    assert_eq!(headers.get(header::ACCEPT).unwrap(), "text/event-stream");
    assert!(headers.get(header::AUTHORIZATION).is_none());

    // Spend honesty: one audit row, no content details.
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].actor, "client-api");
    assert_eq!(audit[0].action, "assistant_relay");
    assert_eq!(audit[0].target, None);
    assert_eq!(audit[0].detail.as_deref(), Some("status:200"));
}

#[tokio::test]
async fn relay_names_the_provider_on_a_bare_model_and_changes_nothing_else() {
    // The bug this pins: the app sends a bare `claude-sonnet-5`, the gateway
    // resolves a provider before it looks at anything else, and the turn 400s
    // with "could not auto resolve a provider" before the virtual key, its
    // allow-list, or the budget are ever consulted. The app cannot send the
    // qualified id instead — the same setting drives BYOK straight to
    // Anthropic, where that spelling is the invalid one — so the daemon makes
    // the edit, behind the gate that already decides who gets a virtual key.
    const FRAMES: &[&str] = &["event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"];
    let (url, seen) = spawn_upstream(StatusCode::OK, "text/event-stream", &[], FRAMES).await;
    let (app, _store, _acct) = relay_app(url);

    let resp = app.oneshot(relay_request(true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let seen = seen.lock().unwrap();
    let (_headers, body) = &seen[0];
    let sent: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(sent["model"], QUALIFIED_MODEL);

    // ...and the model is the ONLY difference. The conversation is what makes
    // reading this body at all a thing worth bounding: the relay parses one
    // top-level field and must not touch, reorder, or drop anything else,
    // including keys it has never heard of.
    let mut posted: Value = serde_json::from_str(REQUEST_BODY).unwrap();
    posted["model"] = Value::String(QUALIFIED_MODEL.into());
    assert_eq!(sent, posted);
}

#[tokio::test]
async fn relay_forwards_an_already_qualified_body_byte_for_byte() {
    // The other half of the contract: with no edit to make, the relay does not
    // re-serialize at all, so a body it has no business rewriting reaches the
    // gateway as the app spelled it — whitespace, key order, and everything the
    // daemon does not model included. That is also what keeps the gateway's own
    // error about the app's own bytes truthful.
    const FRAMES: &[&str] = &["event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"];
    let (url, seen) = spawn_upstream(StatusCode::OK, "text/event-stream", &[], FRAMES).await;
    let (app, _store, _acct) = relay_app(url);

    let resp = app
        .oneshot(relay_request_of(true, PREQUALIFIED_BODY))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let seen = seen.lock().unwrap();
    let (_headers, body) = &seen[0];
    assert_eq!(body, PREQUALIFIED_BODY.as_bytes());
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
