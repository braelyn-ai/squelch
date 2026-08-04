//! End-to-end `POST /v1/push` against a mock APNs on an ephemeral port, wired in
//! through `Config::apns_url_override` directly so these tests never mutate
//! process-global environment and can run in parallel. The env path itself is
//! covered by `tests/config_env.rs`, which needs a process to itself.

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

mod common;
use common::{BETA_TOPIC, TOPIC, config, spawn_relay};

/// A live token, a token APNs has retired, and one APNs calls malformed.
const LIVE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
const DEAD: &str = "deaddeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddeaddead";
const MALFORMED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2";

/// What the mock APNs saw for one request.
#[derive(Clone)]
struct Captured {
    path: String,
    headers: HeaderMap,
    body: Value,
}

#[derive(Clone, Default)]
struct Mock {
    seen: Arc<Mutex<Vec<Captured>>>,
}

/// Mock APNs: 410/400 for the two known-bad tokens, 200 otherwise. Requests are
/// recorded so the outgoing headers and payload can be asserted.
async fn apns_device(
    State(mock): State<Mock>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    mock.seen.lock().unwrap().push(Captured {
        path: format!("/3/device/{token}"),
        headers: headers.clone(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    });
    let (status, reason) = match token.as_str() {
        DEAD => (StatusCode::GONE, Some("Unregistered")),
        MALFORMED => (StatusCode::BAD_REQUEST, Some("BadDeviceToken")),
        _ => (StatusCode::OK, None),
    };
    let mut resp = match reason {
        Some(r) => (status, axum::Json(json!({ "reason": r }))).into_response(),
        None => status.into_response(),
    };
    resp.headers_mut()
        .insert("apns-id", format!("apns-id-for-{token}").parse().unwrap());
    resp
}

async fn spawn_mock() -> (String, Mock) {
    let mock = Mock::default();
    let app = Router::new()
        .route("/3/device/{token}", post(apns_device))
        .with_state(mock.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), mock)
}

/// Relay + mock, wired together.
async fn harness() -> (String, Mock) {
    let (apns, mock) = spawn_mock().await;
    let relay = spawn_relay(config(Some(apns), None)).await;
    (relay, mock)
}

async fn push(relay: &str, body: Value) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{relay}/v1/push"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let json = resp.json().await.unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn healthz_needs_no_auth() {
    // Configured WITH a bearer token: liveness must still answer.
    let relay = spawn_relay(config(None, Some("s3cret".into()))).await;
    let resp = reqwest::get(format!("{relay}/healthz")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn forwards_apns_headers_and_a_content_free_payload() {
    let (relay, mock) = harness().await;
    let (status, body) = push(
        &relay,
        json!({
            "device_tokens": [LIVE],
            "event_id": 4711,
            "collapse_id": "thread-99",
            "topic": BETA_TOPIC,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let seen = mock.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let c = &seen[0];
    assert_eq!(c.path, format!("/3/device/{LIVE}"));
    assert_eq!(c.headers["apns-topic"], BETA_TOPIC);
    assert_eq!(c.headers["apns-push-type"], "alert");
    assert_eq!(c.headers["apns-priority"], "10");
    assert_eq!(c.headers["apns-collapse-id"], "thread-99");

    // A real ES256 provider token under the `bearer` scheme.
    let auth = c.headers["authorization"].to_str().unwrap();
    let jwt = auth.strip_prefix("bearer ").expect("bearer scheme");
    assert_eq!(jwt.split('.').count(), 3);
    let header = jsonwebtoken::decode_header(jwt).unwrap();
    assert_eq!(header.alg, jsonwebtoken::Algorithm::ES256);
    assert_eq!(header.kid.as_deref(), Some("KEYID12345"));

    // The whole point of the relay: an opaque id and a generic alert.
    assert_eq!(c.body["aps"]["alert"]["title"], "squelch");
    assert_eq!(c.body["aps"]["alert"]["body"], "New mail surfaced");
    assert_eq!(c.body["aps"]["mutable-content"], 1);
    assert_eq!(c.body["aps"]["sound"], "default");
    assert_eq!(c.body["event_id"], 4711);
    assert_eq!(c.body.as_object().unwrap().len(), 2);

    assert_eq!(body["results"][0]["token"], LIVE);
    assert_eq!(body["results"][0]["status"], 200);
    assert_eq!(body["results"][0]["apns_id"], format!("apns-id-for-{LIVE}"));
    assert!(body["results"][0].get("reason").is_none());
}

#[tokio::test]
async fn omits_the_collapse_header_when_unset_and_defaults_the_topic() {
    let (relay, mock) = harness().await;
    let (status, _) = push(
        &relay,
        json!({ "device_tokens": [LIVE], "event_id": "abc" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let seen = mock.seen.lock().unwrap().clone();
    assert!(!seen[0].headers.contains_key("apns-collapse-id"));
    // First configured topic is the default.
    assert_eq!(seen[0].headers["apns-topic"], TOPIC);
    // String event ids survive verbatim.
    assert_eq!(seen[0].body["event_id"], "abc");
}

#[tokio::test]
async fn per_token_apns_status_passes_through_verbatim() {
    let (relay, mock) = harness().await;
    let (status, body) = push(
        &relay,
        json!({ "device_tokens": [LIVE, DEAD, MALFORMED], "event_id": 1 }),
    )
    .await;

    // One dead token never fails the batch: the daemon owns cleanup, not us.
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["token"], LIVE);
    assert_eq!(results[0]["status"], 200);
    assert_eq!(results[1]["token"], DEAD);
    assert_eq!(results[1]["status"], 410);
    assert_eq!(results[1]["reason"], "Unregistered");
    assert_eq!(results[2]["token"], MALFORMED);
    assert_eq!(results[2]["status"], 400);
    assert_eq!(results[2]["reason"], "BadDeviceToken");

    assert_eq!(mock.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn a_hundred_tokens_all_report() {
    let (relay, mock) = harness().await;
    let tokens: Vec<String> = (0..100).map(|i| format!("{LIVE:.62}{i:02x}")).collect();
    let (status, body) = push(&relay, json!({ "device_tokens": tokens, "event_id": 7 })).await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 100);
    // Results come back in request order despite the bounded fan-out.
    for (i, t) in tokens.iter().enumerate() {
        assert_eq!(results[i]["token"], *t);
        assert_eq!(results[i]["status"], 200);
    }
    assert_eq!(mock.seen.lock().unwrap().len(), 100);
}

#[tokio::test]
async fn unreachable_apns_is_a_per_token_zero_not_a_batch_failure() {
    // A port nothing is listening on.
    let dead_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = dead_listener.local_addr().unwrap();
    drop(dead_listener);

    let relay = spawn_relay(config(Some(format!("http://{addr}")), None)).await;
    let (status, body) = push(&relay, json!({ "device_tokens": [LIVE], "event_id": 1 })).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"][0]["status"], 0);
    assert_eq!(body["results"][0]["reason"], "unreachable");
}

#[tokio::test]
async fn malformed_requests_are_rejected_with_400() {
    let (relay, mock) = harness().await;
    let cases: Vec<(&str, Value)> = vec![
        (
            "empty token list",
            json!({ "device_tokens": [], "event_id": 1 }),
        ),
        (
            "non-hex token",
            json!({ "device_tokens": ["zzzzzzzzzzzzzzzz"], "event_id": 1 }),
        ),
        (
            "short token",
            json!({ "device_tokens": ["abcd"], "event_id": 1 }),
        ),
        (
            "too many tokens",
            json!({ "device_tokens": vec![LIVE; 101], "event_id": 1 }),
        ),
        (
            "unlisted topic",
            json!({ "device_tokens": [LIVE], "event_id": 1, "topic": "com.evil.app" }),
        ),
        (
            "oversized collapse_id",
            json!({ "device_tokens": [LIVE], "event_id": 1, "collapse_id": "c".repeat(65) }),
        ),
        (
            "object event_id",
            json!({ "device_tokens": [LIVE], "event_id": { "id": 1 } }),
        ),
        (
            "boolean event_id",
            json!({ "device_tokens": [LIVE], "event_id": true }),
        ),
        ("missing event_id", json!({ "device_tokens": [LIVE] })),
        (
            "unknown environment",
            json!({ "device_tokens": [LIVE], "event_id": 1, "environment": "staging" }),
        ),
        ("not an object", json!([1, 2, 3])),
        (
            // Unbounded, a 100x amplifier: the payload is cloned per token.
            "oversized event_id",
            json!({ "device_tokens": [LIVE], "event_id": "e".repeat(500_000) }),
        ),
    ];
    for (name, body) in cases {
        let (status, json) = push(&relay, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name} should be a 400");
        assert!(json["error"].is_string(), "{name} should carry an error");
    }
    // Nothing reached APNs.
    assert!(mock.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn bearer_auth_gates_the_push_route() {
    let (apns, mock) = spawn_mock().await;
    let relay = spawn_relay(config(Some(apns), Some("s3cret".into()))).await;
    let body = json!({ "device_tokens": [LIVE], "event_id": 1 });
    let client = reqwest::Client::new();
    let url = format!("{relay}/v1/push");

    for (name, header) in [
        ("no header", None),
        ("wrong token", Some("Bearer nope")),
        ("wrong scheme", Some("Basic s3cret")),
        ("empty token", Some("Bearer ")),
    ] {
        let mut req = client.post(&url).json(&body);
        if let Some(h) = header {
            req = req.header("authorization", h);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{name}");
    }
    assert!(mock.seen.lock().unwrap().is_empty());

    // The configured token, case-insensitive scheme, gets through.
    let resp = client
        .post(&url)
        .header("authorization", "bearer s3cret")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
}

/// The limiter lives INSIDE the auth layer: the deployment shares one bucket
/// behind the proxy, so junk that spent it would lock the real daemon out.
#[tokio::test]
async fn unauthenticated_floods_do_not_spend_the_authenticated_budget() {
    let (apns, mock) = spawn_mock().await;
    let token = "0123456789abcdef0123456789abcdef";
    let relay = spawn_relay(config(Some(apns), Some(token.into()))).await;
    let body = json!({ "device_tokens": [LIVE], "event_id": 1 });
    let client = reqwest::Client::new();
    let url = format!("{relay}/v1/push");

    let budget = squelch_relay::ratelimit::REQUESTS_PER_MINUTE as usize;
    for i in 0..budget + 1 {
        let resp = client.post(&url).json(&body).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "junk request {i}");
    }

    // The legitimate caller still has its full budget.
    let resp = client
        .post(&url)
        .header("authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rate_limit_returns_429_past_the_per_minute_budget() {
    let (relay, _mock) = harness().await;
    let body = json!({ "device_tokens": [LIVE], "event_id": 1 });
    let budget = squelch_relay::ratelimit::REQUESTS_PER_MINUTE as usize;
    // The bucket refills at budget/60 per second, so on a slow runner a token
    // can come back mid-loop: assert a 429 within twice the budget, not at an
    // exact index.
    let mut saw_429 = false;
    for i in 0..budget * 2 {
        let (status, _) = push(&relay, body.clone()).await;
        match status {
            StatusCode::OK => {}
            StatusCode::TOO_MANY_REQUESTS => {
                saw_429 = true;
                break;
            }
            other => panic!("request {i}: unexpected status {other}"),
        }
    }
    assert!(saw_429, "no 429 within twice the per-minute budget");

    // Liveness is outside the limiter.
    let resp = reqwest::get(format!("{relay}/healthz")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
