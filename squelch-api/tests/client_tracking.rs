//! Read tracking over HTTP: the unauthenticated pixel route, the human door's
//! opens list, and the tracking-config toggle.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use squelch_api::router;
use squelch_core::store::{SqliteStore, Store};
use squelch_core::types::{Sensitivity, Tier};
use tower::ServiceExt;

mod common;
use common::{Harness, authed, authed_json, body_json, harness, msg};

/// A live token, in the shape the daemon actually mints: 32 url-safe base64
/// characters. The pixel route rejects anything outside that shape before it
/// reaches the store, so fixtures have to look real.
const KNOWN: &str = "Ab3xQz7-_KnownTokenFixture000001";

/// A stored sent message plus the tracker minted for it — the state a tracked
/// send leaves behind once its echo has landed.
fn tracked_send(store: &SqliteStore, acct: i64, token: &str) -> i64 {
    let id = store
        .upsert_message(&msg(acct, "sent-1", "thread-77", "Re: Lunch?", "noon works"))
        .unwrap();
    store
        .set_triage(id, acct, 0, Tier::Noise, Sensitivity::Normal, None, "", "", None)
        .unwrap();
    store.insert_send_tracker(acct, token, Some(id), 1_000).unwrap();
    id
}

/// A pixel fetch with no bearer and an optional User-Agent — what a recipient's
/// mail client actually sends.
fn pixel_request(token: &str, user_agent: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(format!("/t/{token}"));
    if let Some(ua) = user_agent {
        b = b.header(header::USER_AGENT, ua);
    }
    b.body(Body::empty()).unwrap()
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body().collect().await.unwrap().to_bytes().to_vec()
}

#[tokio::test]
async fn the_pixel_is_unauthenticated_and_records_a_known_token() {
    let Harness { app, store, acct } = harness(|store, acct| {
        tracked_send(store, acct, KNOWN);
    });

    let resp = app
        .clone()
        .oneshot(pixel_request(KNOWN, Some("Apple Mail/16.0")))
        .await
        .unwrap();
    // No bearer was presented and it still served.
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/gif");
    let cache = resp.headers()[header::CACHE_CONTROL].to_str().unwrap().to_string();
    assert!(cache.contains("no-store"), "an intermediary caching this eats every later open");
    let gif = body_bytes(resp).await;
    assert_eq!(gif.len(), 43);
    assert!(gif.starts_with(b"GIF89a"));
    assert_eq!(gif.last(), Some(&0x3B), "GIF trailer");

    let message_id = store.search(acct, "noon", 10, 0).unwrap()[0].id;
    let opens = store.message_opens(acct, message_id).unwrap();
    assert_eq!(opens.len(), 1);
    assert_eq!(opens[0].user_agent.as_deref(), Some("Apple Mail/16.0"));
    assert_eq!(opens[0].classification, "unknown");
}

/// THE INVARIANT: an unknown token is INDISTINGUISHABLE from a known one on the
/// wire — same status, same bytes, same headers — and leaves no row behind.
#[tokio::test]
async fn an_unknown_token_is_never_a_404_and_records_nothing() {
    let Harness { app, store, acct } = harness(|store, acct| {
        tracked_send(store, acct, KNOWN);
    });
    let message_id = store.search(acct, "noon", 10, 0).unwrap()[0].id;

    let known = app.clone().oneshot(pixel_request(KNOWN, None)).await.unwrap();
    let known_status = known.status();
    let known_headers = known.headers().clone();
    let known_body = body_bytes(known).await;

    for token in ["nope000000000000000000000000000", "", "%2e%2e%2f", "tok-known-DIFFERENT-000000000000", &"z".repeat(4096)] {
        let resp = app.clone().oneshot(pixel_request(token, None)).await.unwrap();
        assert_eq!(resp.status(), known_status, "status leaked for {token:?}");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], known_headers[header::CONTENT_TYPE]);
        assert_eq!(body_bytes(resp).await, known_body, "body leaked for {token:?}");
    }

    // Only the one real fetch was recorded.
    assert_eq!(store.message_opens(acct, message_id).unwrap().len(), 1);
}

#[tokio::test]
async fn the_google_image_proxy_is_recorded_as_proxied() {
    let Harness { app, store, acct } = harness(|store, acct| {
        tracked_send(store, acct, KNOWN);
    });

    let resp = app
        .oneshot(pixel_request(
            KNOWN,
            Some("Mozilla/5.0 (via ggpht.com GoogleImageProxy)"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let message_id = store.search(acct, "noon", 10, 0).unwrap()[0].id;
    let opens = store.message_opens(acct, message_id).unwrap();
    assert_eq!(opens[0].classification, "proxied");
}

#[tokio::test]
async fn opens_endpoint_is_bearer_gated_and_ordered_oldest_first() {
    let Harness { app, store, acct } = harness(|store, acct| {
        let id = tracked_send(store, acct, KNOWN);
        store.record_open(acct, KNOWN, 2_000, Some("Apple Mail/16.0"), "unknown").unwrap();
        store.record_open(acct, KNOWN, 1_000, None, "proxied").unwrap();
        assert!(id > 0);
    });
    let message_id = store.search(acct, "noon", 10, 0).unwrap()[0].id;
    let uri = format!("/client/messages/{message_id}/opens");

    let unauthed = Request::builder().uri(&uri).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(unauthed).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app.clone().oneshot(authed("GET", &uri)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let opens = json["opens"].as_array().unwrap();
    assert_eq!(opens.len(), 2);
    assert_eq!(opens[0]["opened_at"], 1_000);
    assert!(opens[0]["user_agent"].is_null());
    assert_eq!(opens[0]["classification"], "proxied");
    assert_eq!(opens[1]["opened_at"], 2_000);
    assert_eq!(opens[1]["user_agent"], "Apple Mail/16.0");

    // An untracked message is an empty list, not a 404.
    let resp = app
        .oneshot(authed("GET", "/client/messages/999999/opens"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["opens"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn tracking_config_defaults_off_and_reports_unconfigured() {
    let Harness { app, .. } = harness(|_, _| {});

    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/tracking-config"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["default_enabled"], false);
    // No base url on the default harness: the client should hide the toggle.
    assert_eq!(json["configured"], false);

    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/tracking-config",
            serde_json::json!({ "default_enabled": true }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["default_enabled"], true);

    // It persists across requests.
    let resp = app.oneshot(authed("GET", "/client/tracking-config")).await.unwrap();
    assert_eq!(body_json(resp).await["default_enabled"], true);
}

#[tokio::test]
async fn tracking_config_reports_configured_when_a_base_url_is_set() {
    let (state, store, acct) = common::state_with(|_, _| {});
    let app = router(state.with_tracking_base_url(Some("https://track.example.test/".to_string())));

    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/tracking-config"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["configured"], true);
    assert_eq!(json["default_enabled"], false, "configured is not enabled");

    // Turning the default off again is audited as a policy change.
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/tracking-config",
            serde_json::json!({ "default_enabled": false }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit[0].action, "tracking_policy");
    assert_eq!(audit[0].detail.as_deref(), Some("disabled"));
}

/// The pixel is the one unauthenticated route that touches the store, and its
/// URL is printed in every tracked email — so a token that cannot possibly be
/// ours is refused on shape, before the store mutex the whole daemon shares is
/// ever taken. The response is still the same image.
#[tokio::test]
async fn tokens_that_cannot_be_ours_never_reach_the_store() {
    let Harness { app, store, acct } = harness(|store, acct| {
        tracked_send(store, acct, KNOWN);
    });
    let message_id = store.search(acct, "noon", 10, 0).unwrap()[0].id;

    for token in [
        "short",         // under the minimum
        &"a".repeat(65),  // over the maximum
        "tok.with.dots.000000000000000", // not url-safe base64
        "tok+with+plus+0000000000000000",
    ] {
        let resp = app.clone().oneshot(pixel_request(token, None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "still an image for {token:?}");
        assert_eq!(body_bytes(resp).await.len(), 43);
    }

    assert!(store.message_opens(acct, message_id).unwrap().is_empty());
}

/// A base URL without a scheme would ride into the HTML as a RELATIVE url and
/// resolve against the recipient's mail client, so a typo must disable tracking
/// rather than point the pixel somewhere unintended.
#[tokio::test]
async fn a_base_url_without_a_scheme_leaves_tracking_unconfigured() {
    let (state, _store, _acct) = common::state_with(|_, _| {});
    let app = router(state.with_tracking_base_url(Some("track.example.test".to_string())));

    let resp = app.oneshot(authed("GET", "/client/tracking-config")).await.unwrap();
    assert_eq!(body_json(resp).await["configured"], false);
}
