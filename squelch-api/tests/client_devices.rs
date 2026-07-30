//! Integration tests for push-device registration on the human door:
//! `POST /client/devices` and `POST /client/devices/unregister`.
//!
//! The interesting properties are all about idempotence and about not leaking
//! capability material: iOS re-registers on every launch, so a second POST must
//! land on the same row, no response body may echo the token back, and the
//! token never appears in a URL path (which is why unregister is a POST with a
//! body rather than a DELETE with the token in the request line).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, Uri, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use squelch_api::{ApiState, router};
use squelch_core::store::{SqliteStore, Store};
use tower::ServiceExt;

const TOKEN: &str = "test-secret-token";
const DEV_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
const DEV_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2";

struct Harness {
    app: axum::Router,
    store: Arc<SqliteStore>,
    acct: i64,
}

fn harness() -> Harness {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let acct = store.ensure_account("me@example.com").unwrap();
    let state = ApiState::new(store.clone(), acct, TOKEN).unwrap();
    Harness {
        app: router(state),
        store,
        acct,
    }
}

fn post(uri: &str, body: Value, bearer: bool) -> Request<Body> {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

/// Unregister: the token rides in the BODY. If this ever needs a `{token}` path
/// segment again, that is a privacy regression, not a refactor.
fn unregister(token: &str, bearer: bool) -> Request<Body> {
    post(
        "/client/devices/unregister",
        json!({ "token": token }),
        bearer,
    )
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// The bearer layer covers the new routes exactly like every other `/client/*`
/// route — no route is ever mounted outside it.
#[tokio::test]
async fn registration_requires_the_bearer() {
    let h = harness();

    let resp = h
        .app
        .clone()
        .oneshot(post("/client/devices", json!({ "token": DEV_A }), false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = h
        .app
        .clone()
        .oneshot(unregister(DEV_A, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // And nothing was written on the way to the 401.
    assert!(h.store.list_devices(h.acct).unwrap().is_empty());
}

/// Register, read back through the store, unregister. The response describes the
/// row WITHOUT echoing the token.
#[tokio::test]
async fn register_then_unregister_round_trips() {
    let h = harness();

    let resp = h
        .app
        .clone()
        .oneshot(post("/client/devices", json!({ "token": DEV_A }), true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["platform"], "ios", "platform defaults to ios");
    assert!(v["id"].as_i64().is_some());
    assert!(v["created_at"].is_string());
    assert!(v["last_registered_at"].is_string());
    assert!(
        v.get("token").is_none(),
        "the response must not echo the device token back"
    );
    let raw = v.to_string();
    assert!(
        !raw.contains(DEV_A),
        "the token must not appear anywhere in the body"
    );

    let devices = h.store.list_devices(h.acct).unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].token, DEV_A);

    // An explicit platform is honoured and normalized.
    let resp = h
        .app
        .clone()
        .oneshot(post(
            "/client/devices",
            json!({ "token": DEV_B, "platform": "macOS" }),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["platform"], "macos");
    assert_eq!(h.store.list_devices(h.acct).unwrap().len(), 2);

    // Unregister is 204 and actually removes the row.
    let resp = h.app.clone().oneshot(unregister(DEV_A, true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let left = h.store.list_devices(h.acct).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].token, DEV_B);

    // Deleting again is still 204: the caller's intent is satisfied either way,
    // and a 404 would tell a caller which tokens this account has.
    let resp = h.app.oneshot(unregister(DEV_A, true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

/// THE TOKEN NEVER RIDES IN A URL. Unregister carries it in a body, and the old
/// `DELETE /client/devices/{token}` shape is gone — a path segment survives in
/// access logs, proxy logs and error reports, which is the very reason this
/// module refuses to echo a token in a RESPONSE body.
#[tokio::test]
async fn the_token_travels_in_the_body_not_the_path() {
    let h = harness();
    h.store.upsert_device(h.acct, DEV_A, "ios").unwrap();

    // The request line of the real call mentions no token at all.
    let req = unregister(DEV_A, true);
    let uri = req.uri().clone();
    assert_eq!(uri.path(), "/client/devices/unregister");
    assert!(!uri.to_string().contains(DEV_A));

    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(h.store.list_devices(h.acct).unwrap().is_empty());

    // And the retired shape is not routed: a token in the path is a 405/404,
    // never a working endpoint.
    h.store.upsert_device(h.acct, DEV_A, "ios").unwrap();
    let uri: Uri = format!("/client/devices/{DEV_A}").parse().unwrap();
    let req = Request::builder()
        .method(Method::DELETE)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::METHOD_NOT_ALLOWED,
        "DELETE with the token in the path must not be a route ({})",
        resp.status()
    );
    assert_eq!(
        h.store.list_devices(h.acct).unwrap().len(),
        1,
        "and it certainly must not have deleted anything"
    );
}

/// iOS hands the app its token on EVERY launch. Re-registering must refresh the
/// one row, never accumulate rows — otherwise a chatty app slowly turns one
/// device into a hundred push targets.
#[tokio::test]
async fn re_registration_is_idempotent() {
    let h = harness();

    let first = h
        .app
        .clone()
        .oneshot(post("/client/devices", json!({ "token": DEV_A }), true))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first = body_json(first).await;

    for _ in 0..3 {
        let resp = h
            .app
            .clone()
            .oneshot(post("/client/devices", json!({ "token": DEV_A }), true))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let again = body_json(resp).await;
        assert_eq!(again["id"], first["id"], "same device, same row");
        assert_eq!(
            again["created_at"], first["created_at"],
            "first sight is preserved"
        );
    }

    assert_eq!(h.store.list_devices(h.acct).unwrap().len(), 1);
}

/// Token validation mirrors the relay's own bounds, so a token the relay would
/// reject is refused at registration — where the client can actually see it —
/// rather than becoming a silent per-token push failure later.
#[tokio::test]
async fn a_token_the_relay_would_reject_is_refused_here() {
    let h = harness();

    for bad in [
        json!({ "token": "abc" }),           // too short
        json!({ "token": "a".repeat(201) }), // too long
        json!({ "token": "g".repeat(64) }),  // not hex
        json!({ "token": "" }),              // empty
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(post("/client/devices", bad.clone(), true))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 for {bad}"
        );
        // The rejection states the rule, never the offending value.
        let msg = body_json(resp).await["error"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(
            !msg.contains("ggg"),
            "the rejected token must not be reflected"
        );
    }

    // A bad platform is a 400 too, and writes nothing.
    let resp = h
        .app
        .clone()
        .oneshot(post(
            "/client/devices",
            json!({ "token": DEV_A, "platform": "ios; drop table devices" }),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A malformed token on unregister is a 400 as well, not a silent no-op.
    let resp = h
        .app
        .clone()
        .oneshot(unregister("nothex", true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    assert!(h.store.list_devices(h.acct).unwrap().is_empty());
}
