//! Integration tests for push-device registration on the human door.
//!
//! The properties under test are idempotence (iOS re-registers on every launch,
//! so a second POST lands on the same row) and not leaking capability material:
//! no response echoes the token, and no token appears in a URL path.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use squelch_core::store::Store;
use tower::ServiceExt;

mod common;
use common::{authed, body_json, harness, json_request};

const DEV_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
const DEV_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2";

fn post(uri: &str, body: Value, bearer: bool) -> Request<Body> {
    json_request("POST", uri, &body, bearer)
}

/// Unregister: the token rides in the BODY. Moving it to a `{token}` path
/// segment is a privacy regression, not a refactor.
fn unregister(token: &str, bearer: bool) -> Request<Body> {
    post(
        "/client/devices/unregister",
        json!({ "token": token }),
        bearer,
    )
}

/// The bearer layer covers these routes like every other `/client/*` route —
/// nothing is ever mounted outside it.
#[tokio::test]
async fn registration_requires_the_bearer() {
    let h = harness(|_, _| {});

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
    let h = harness(|_, _| {});

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

/// THE TOKEN NEVER RIDES IN A URL: a path segment survives in access logs,
/// proxy logs and error reports. `DELETE /client/devices/{token}` must not be a
/// route.
#[tokio::test]
async fn the_token_travels_in_the_body_not_the_path() {
    let h = harness(|_, _| {});
    h.store.upsert_device(h.acct, DEV_A, "ios").unwrap();

    // The request line of the real call mentions no token at all.
    let req = unregister(DEV_A, true);
    let uri = req.uri().clone();
    assert_eq!(uri.path(), "/client/devices/unregister");
    assert!(!uri.to_string().contains(DEV_A));

    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(h.store.list_devices(h.acct).unwrap().is_empty());

    // A token in the path is a 405/404, never a working endpoint.
    h.store.upsert_device(h.acct, DEV_A, "ios").unwrap();
    let req = authed("DELETE", &format!("/client/devices/{DEV_A}"));
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

/// iOS hands the app its token on EVERY launch, so re-registering must refresh
/// the one row — otherwise one device slowly becomes a hundred push targets.
#[tokio::test]
async fn re_registration_is_idempotent() {
    let h = harness(|_, _| {});

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

/// Token validation mirrors the relay's bounds, so a token the relay would
/// reject fails at registration instead of silently at push time.
#[tokio::test]
async fn a_token_the_relay_would_reject_is_refused_here() {
    let h = harness(|_, _| {});

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
