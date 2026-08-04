//! Read tracking end to end: `GET /t/{token}` answers strangers with a pixel,
//! `GET /v1/opens` answers only the daemon, and the cursor is an acknowledgement
//! that deletes what it covers.

use reqwest::{Response, StatusCode};
use serde_json::Value;

mod common;
use common::{config, spawn_relay};

const BEARER: &str = "0123456789abcdef0123456789abcdef";
/// Well-formed open tokens: URL-safe base64, 16..=64 characters.
const OPEN_A: &str = "aaaaAAAA1111-_bb";
const OPEN_B: &str = "bbbbBBBB2222-_cc";
const UA: &str = "Mozilla/5.0 (test agent)";

/// The transparent GIF89a the route always serves.
const GIF_LEN: usize = 42;

async fn fetch_pixel(relay: &str, token: &str) -> Response {
    reqwest::Client::new()
        .get(format!("{relay}/t/{token}"))
        .header("user-agent", UA)
        .send()
        .await
        .unwrap()
}

async fn drain(relay: &str, cursor: Option<i64>) -> Vec<Value> {
    let url = match cursor {
        Some(c) => format!("{relay}/v1/opens?cursor={c}"),
        None => format!("{relay}/v1/opens"),
    };
    let resp = reqwest::Client::new()
        .get(url)
        .header("authorization", format!("Bearer {BEARER}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    body["opens"].as_array().unwrap().clone()
}

/// A different status, body, or header for an unknown token would turn the
/// pixel into an oracle for which tokens exist.
#[tokio::test]
async fn the_pixel_is_byte_identical_for_garbage_and_for_a_real_token() {
    let relay = spawn_relay(config(None, Some(BEARER.into()))).await;

    let mut seen = Vec::new();
    for token in [
        OPEN_A,
        "junk",
        "!!!bogus!!!",
        &"z".repeat(300),
        "aaaaAAAA1111-_b$",
    ] {
        let resp = fetch_pixel(&relay, token).await;
        assert_eq!(resp.status(), StatusCode::OK, "token {token}");
        assert_eq!(resp.headers()["content-type"], "image/gif");
        let cache = resp.headers()["cache-control"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            cache.contains("no-store") && cache.contains("no-cache"),
            "{cache}"
        );
        assert_eq!(resp.headers()["pragma"], "no-cache");
        let body = resp.bytes().await.unwrap();
        assert_eq!(body.len(), GIF_LEN);
        assert_eq!(&body[..6], b"GIF89a");
        seen.push(body);
    }
    assert!(
        seen.windows(2).all(|w| w[0] == w[1]),
        "the pixel body differed between tokens"
    );

    // Only the well-formed token was buffered; the garbage went nowhere.
    let opens = drain(&relay, None).await;
    assert_eq!(opens.len(), 1);
    assert_eq!(opens[0]["token"], OPEN_A);
}

#[tokio::test]
async fn opens_round_trip_and_the_cursor_acks_by_deleting() {
    let relay = spawn_relay(config(None, Some(BEARER.into()))).await;
    for token in [OPEN_A, OPEN_A, OPEN_B] {
        assert_eq!(fetch_pixel(&relay, token).await.status(), StatusCode::OK);
    }

    let opens = drain(&relay, Some(0)).await;
    assert_eq!(opens.len(), 3);
    assert_eq!(opens[0]["token"], OPEN_A);
    assert_eq!(opens[1]["token"], OPEN_A);
    assert_eq!(opens[2]["token"], OPEN_B);
    assert_eq!(opens[0]["user_agent"], UA);
    // Recorded in wall-clock seconds, not milliseconds.
    let opened_at = opens[0]["opened_at"].as_i64().unwrap();
    assert!(opened_at > 1_700_000_000 && opened_at < 4_000_000_000);

    let ids: Vec<i64> = opens
        .iter()
        .map(|o| o["id"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert!(
        ids[0] < ids[1] && ids[1] < ids[2],
        "ids must ascend: {ids:?}"
    );

    // Acking the first two leaves exactly the third.
    let rest = drain(&relay, Some(ids[1])).await;
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0]["id"], ids[2]);
    // A stale cursor cannot resurrect acked rows.
    assert_eq!(drain(&relay, Some(0)).await.len(), 1);

    assert!(drain(&relay, Some(ids[2])).await.is_empty());
    assert!(drain(&relay, Some(0)).await.is_empty());
}

/// The drain is the daemon's door; the pixel is everyone's.
#[tokio::test]
async fn the_drain_needs_the_bearer_and_the_pixel_does_not() {
    let relay = spawn_relay(config(None, Some(BEARER.into()))).await;
    assert_eq!(fetch_pixel(&relay, OPEN_A).await.status(), StatusCode::OK);

    let client = reqwest::Client::new();
    let url = format!("{relay}/v1/opens");
    for (name, header) in [
        ("no header", None),
        ("wrong token", Some("Bearer nope")),
        (
            "wrong scheme",
            Some("Basic 0123456789abcdef0123456789abcdef"),
        ),
        ("empty token", Some("Bearer ")),
    ] {
        let mut req = client.get(&url);
        if let Some(h) = header {
            req = req.header("authorization", h);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{name}");
    }

    // Rejected drains never acked anything: the open is still buffered.
    assert_eq!(drain(&relay, None).await.len(), 1);
}

/// An anonymous relay serves the pixel but has no drain AT ALL. Serving
/// `/v1/push` open is a supported nuisance; serving `/v1/opens` open would hand
/// strangers live tracking tokens and let them delete the buffer on the way out.
#[tokio::test]
async fn an_anonymous_relay_buffers_opens_but_serves_no_drain() {
    let relay = spawn_relay(config(None, None)).await;

    assert_eq!(fetch_pixel(&relay, OPEN_A).await.status(), StatusCode::OK);

    let resp = reqwest::Client::new()
        .get(format!("{relay}/v1/opens"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "the route must not exist without a bearer, not merely reject"
    );
}
