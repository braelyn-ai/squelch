//! Integration tests for the human-door router.
//!
//! Covers: bearer auth (401 without / with bad token, 200 with good token),
//! search excludes sealed rows, reveal writes an audit row and returns the body
//! with `Cache-Control: no-store`, and pagination cursor round-trip.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use squelch_api::{ApiState, router};
use squelch_core::store::{SqliteStore, Store};
use squelch_core::types::{NewMessage, SealedKind, Sensitivity, Tier};
use tower::ServiceExt;

const TOKEN: &str = "test-secret-token";

fn msg(account_id: i64, gmail: &str, thread: &str, subject: &str, body: &str) -> NewMessage {
    NewMessage {
        account_id,
        gmail_msg_id: gmail.to_string(),
        thread_id: thread.to_string(),
        from_addr: "alice@example.com".to_string(),
        from_name: Some("Alice".to_string()),
        subject: subject.to_string(),
        received_at: chrono::Utc::now(),
        snippet: subject.to_string(),
        body: body.to_string(),
        body_html: None,
        is_sent: false,
        list_unsubscribe: None,
        list_unsub_one_click: false,
    }
}

/// Build state + router over an in-memory store seeded by `seed`.
fn app_with(seed: impl FnOnce(&SqliteStore, i64)) -> (axum::Router, Arc<SqliteStore>, i64) {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let acct = store.ensure_account("me@example.com").unwrap();
    seed(&store, acct);
    let state = ApiState::new(store.clone(), acct, TOKEN).unwrap();
    (router(state), store, acct)
}

fn authed(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn missing_token_is_401() {
    let (app, _s, _a) = app_with(|_, _| {});
    let req = Request::builder()
        .uri("/client/stats")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_token_is_401() {
    let (app, _s, _a) = app_with(|_, _| {});
    let req = Request::builder()
        .uri("/client/stats")
        .header(header::AUTHORIZATION, "Bearer nope")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn good_token_is_200() {
    let (app, _s, _a) = app_with(|_, _| {});
    let resp = app.oneshot(authed("GET", "/client/stats")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn state_refuses_empty_token() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let acct = store.ensure_account("me@example.com").unwrap();
    assert!(ApiState::new(store.clone(), acct, "").is_err());
    assert!(ApiState::new(store, acct, "   ").is_err());
}

#[tokio::test]
async fn search_excludes_sealed() {
    let (app, _s, _a) = app_with(|store, acct| {
        // Normal message mentioning "verification".
        let n = store
            .upsert_message(&msg(acct, "g1", "t1", "Your account verification steps", "hello"))
            .unwrap();
        store
            .set_triage(n, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        // Sealed OTP also mentioning "verification".
        let s = store
            .upsert_message(&msg(acct, "g2", "t2", "verification code inside", "123456"))
            .unwrap();
        store
            .set_triage(
                s,
                acct,
                90,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "",
                "",
                None,
            )
            .unwrap();
    });

    let resp = app
        .oneshot(authed("GET", "/client/search?q=verification"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "sealed hit must be excluded from search");
    assert_eq!(items[0]["thread_id"], "t1");
    // No embedder attached => default mode resolves to keyword.
    assert_eq!(json["match_kind"], "keyword");
}

#[tokio::test]
async fn shipments_returns_en_route_by_default_and_delivered_with_flag() {
    use squelch_core::triage::{ShipmentInfo, ShipmentStatus};
    let (app, store, acct) = app_with(|store, acct| {
        let mid = store.upsert_message(&msg(acct, "g1", "t1", "shipped", "b")).unwrap();
        // One en-route (shipped) and one delivered.
        store
            .upsert_shipment(
                acct,
                mid,
                &ShipmentInfo {
                    carrier: "ups".into(),
                    tracking_number: "1Z999AA10123456784".into(),
                    item_name: "Headphones".into(),
                    status: ShipmentStatus::Shipped,
                    tracking_url: Some("https://www.ups.com/track?tracknum=1Z999AA10123456784".into()),
                },
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .upsert_shipment(
                acct,
                mid,
                &ShipmentInfo {
                    carrier: "usps".into(),
                    tracking_number: "9400111899223817428490".into(),
                    item_name: "Book".into(),
                    status: ShipmentStatus::Delivered,
                    tracking_url: None,
                },
                chrono::Utc::now(),
            )
            .unwrap();
    });
    let _ = (&store, acct);

    // Default: en-route only (the shipped one, not the delivered one).
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/shipments"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json.as_array().unwrap();
    assert_eq!(items.len(), 1, "delivered excluded by default");
    assert_eq!(items[0]["tracking_number"], "1Z999AA10123456784");
    assert_eq!(items[0]["status"], "shipped");

    // include_delivered=true: both.
    let resp = app
        .oneshot(authed("GET", "/client/shipments?include_delivered=true"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 2, "delivered included with flag");
}

#[tokio::test]
async fn receipts_returns_rows_newest_first_and_is_bearer_gated() {
    use squelch_core::triage::ReceiptInfo;
    let (app, _s, _a) = app_with(|store, acct| {
        let m1 = store.upsert_message(&msg(acct, "g1", "t1", "receipt a", "b")).unwrap();
        let m2 = store.upsert_message(&msg(acct, "g2", "t2", "receipt b", "b")).unwrap();
        // Older, then newer — expect newest-first ordering on read.
        store
            .upsert_receipt(
                acct,
                m1,
                "no-reply@baywheels.com",
                Some("Bay Wheels"),
                &ReceiptInfo { amount: Some(3.49), currency: Some("USD".into()) },
                chrono::Utc::now() - chrono::Duration::hours(2),
            )
            .unwrap();
        store
            .upsert_receipt(
                acct,
                m2,
                "orders@shop.com",
                None,
                &ReceiptInfo { amount: None, currency: None },
                chrono::Utc::now(),
            )
            .unwrap();
    });

    // Bearer-gated: no token => 401.
    let unauth = Request::builder()
        .uri("/client/receipts")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(unauth).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Authed: rows, newest-first, with clean sender + null amount preserved.
    let resp = app.oneshot(authed("GET", "/client/receipts")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["from_addr"], "orders@shop.com", "newest first");
    assert_eq!(items[0]["amount"], Value::Null, "null amount preserved");
    assert_eq!(items[1]["from_addr"], "no-reply@baywheels.com");
    assert_eq!(items[1]["from_name"], "Bay Wheels");
    assert_eq!(items[1]["amount"], 3.49);
    assert_eq!(items[1]["currency"], "USD");
}

#[tokio::test]
async fn retriage_route_exists_resets_and_audits() {
    let (app, store, acct) = app_with(|store, acct| {
        let m = store.upsert_message(&msg(acct, "g1", "t1", "s", "b")).unwrap();
        store
            .set_triage(m, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        // Simulate an LLM-classified row (via the public apply path) so there is
        // something to reset.
        store
            .stage1_apply(&squelch_core::store::Stage1Applied {
                message_id: m,
                account_id: acct,
                importance: 50,
                tier: squelch_core::types::Tier::Signal,
                one_line: "x".into(),
                reason: "x".into(),
                field_reasons: Default::default(),
                stage1_model_used: "claude-x".into(),
                needs_stage2: false,
                deadline: None,
                category: Some("general".into()),
            })
            .unwrap();
    });

    // Route exists (the 404 class of regression) + resets the window.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/retriage",
            serde_json::json!({ "days": 7 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["reset"], 1);

    // Audited.
    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit.iter().any(|e| e.action == "retriage"),
        "retriage must land an audit row"
    );
}

#[tokio::test]
async fn banking_returns_rows_newest_first_and_is_bearer_gated() {
    use squelch_core::store::BankingApplied;
    let (app, _s, _a) = app_with(|store, acct| {
        let m1 = store.upsert_message(&msg(acct, "g1", "t1", "statement", "b")).unwrap();
        let m2 = store.upsert_message(&msg(acct, "g2", "t2", "alert", "b")).unwrap();
        // Older statement, then newer alert -> expect newest-first ordering.
        store
            .banking_apply(&BankingApplied {
                message_id: m1,
                account_id: acct,
                kind: "statement".into(),
                institution: Some("Chase".into()),
                amount: Some(1234.56),
                currency: Some("USD".into()),
                account_hint: Some("…1234".into()),
                received_at: chrono::Utc::now() - chrono::Duration::hours(2),
                extractor_model_used: "claude-haiku-4-5".into(),
                auto_resolve: true,
            })
            .unwrap();
        store
            .banking_apply(&BankingApplied {
                message_id: m2,
                account_id: acct,
                kind: "transaction_alert".into(),
                institution: None,
                amount: Some(42.10),
                currency: Some("USD".into()),
                account_hint: None,
                received_at: chrono::Utc::now(),
                extractor_model_used: "claude-haiku-4-5".into(),
                auto_resolve: true,
            })
            .unwrap();
    });

    // Bearer-gated: no token => 401.
    let unauth = Request::builder()
        .uri("/client/banking")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(unauth).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Authed: newest-first, exact wire shape (no account_id), nulls preserved.
    let resp = app.oneshot(authed("GET", "/client/banking")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json.as_array().unwrap();
    assert_eq!(items.len(), 2);
    // Newest first: the transaction alert.
    assert_eq!(items[0]["kind"], "transaction_alert");
    assert_eq!(items[0]["institution"], Value::Null);
    assert_eq!(items[0]["account_hint"], Value::Null);
    assert_eq!(items[0]["amount"], 42.10);
    assert!(items[0].get("account_id").is_none(), "account_id off the wire");
    // Then the statement.
    assert_eq!(items[1]["kind"], "statement");
    assert_eq!(items[1]["institution"], "Chase");
    assert_eq!(items[1]["amount"], 1234.56);
    assert_eq!(items[1]["account_hint"], "…1234");
    assert_eq!(items[1]["currency"], "USD");
}

#[tokio::test]
async fn calendar_returns_windowed_rows_newest_first_and_is_bearer_gated() {
    use squelch_core::triage::{CalendarInfo, CalendarKind};
    let (app, _s, _a) = app_with(|store, acct| {
        let m1 = store.upsert_message(&msg(acct, "g1", "t1", "invite", "b")).unwrap();
        let m2 = store.upsert_message(&msg(acct, "g2", "t2", "cancel", "b")).unwrap();
        let m3 = store.upsert_message(&msg(acct, "g3", "t3", "old", "b")).unwrap();
        // 2h old invite, 1h old cancellation, 30h old update (outside the
        // default 24h received_at window).
        store
            .upsert_calendar_update(
                acct,
                m1,
                &CalendarInfo {
                    kind: CalendarKind::Invite,
                    event_title: Some("Design review".into()),
                    starts_at: Some(chrono::Utc::now() + chrono::Duration::days(2)),
                    organizer: Some("Sam Doe".into()),
                },
                chrono::Utc::now() - chrono::Duration::hours(2),
            )
            .unwrap();
        store
            .upsert_calendar_update(
                acct,
                m2,
                &CalendarInfo {
                    kind: CalendarKind::Cancellation,
                    event_title: None,
                    starts_at: None,
                    organizer: None,
                },
                chrono::Utc::now() - chrono::Duration::hours(1),
            )
            .unwrap();
        store
            .upsert_calendar_update(
                acct,
                m3,
                &CalendarInfo {
                    kind: CalendarKind::Update,
                    event_title: Some("Old thing".into()),
                    starts_at: None,
                    organizer: None,
                },
                chrono::Utc::now() - chrono::Duration::hours(30),
            )
            .unwrap();
    });

    // Bearer-gated like every /client route: no token => 401.
    let unauth = Request::builder()
        .uri("/client/calendar")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(unauth).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Default 24h window: two rows, newest-RECEIVED first, exact wire shape.
    let resp = app.clone().oneshot(authed("GET", "/client/calendar")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json.as_array().unwrap();
    assert_eq!(items.len(), 2, "30h-old row outside the default 24h window");
    assert_eq!(items[0]["kind"], "cancellation", "newest-received first");
    assert_eq!(items[0]["event_title"], Value::Null, "nullable fields serialize as null");
    assert_eq!(items[0]["starts_at"], Value::Null);
    assert_eq!(items[0]["organizer"], Value::Null);
    assert_eq!(items[1]["kind"], "invite");
    assert_eq!(items[1]["event_title"], "Design review");
    assert_eq!(items[1]["organizer"], "Sam Doe");
    assert!(items[1]["starts_at"].is_string(), "iso8601 start");
    // Contract shape: exactly these keys (account_id deliberately absent).
    let keys: std::collections::BTreeSet<_> =
        items[0].as_object().unwrap().keys().cloned().collect();
    let expect: std::collections::BTreeSet<String> =
        ["id", "message_id", "thread_id", "kind", "event_title", "starts_at", "organizer",
         "received_at"]
            .into_iter()
            .map(String::from)
            .collect();
    assert_eq!(keys, expect, "wire contract keys");
    assert!(items[0]["id"].is_i64());
    assert!(items[0]["message_id"].is_i64());
    assert!(items[0]["received_at"].is_string());

    // Wider window picks up the 30h row.
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/calendar?hours=48"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 3);

    // Out-of-range hours are CLAMPED, not rejected: hours=0 -> 1 (only the
    // 1h-old row misses even that? no — 1h-old is exactly at the boundary; use
    // presence of a 200 + subset semantics instead of exact count).
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/calendar?hours=0"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "clamped, not 400");
    let json = body_json(resp).await;
    assert!(json.as_array().unwrap().len() <= 1, "hours=0 clamps to 1h");

    // Absurdly large hours clamp to a week (still 200, still all three rows).
    let resp = app
        .oneshot(authed("GET", "/client/calendar?hours=999999"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().unwrap().len(), 3, "clamped to 168h, rows within remain");
}

/// A garbage `mode` value is a 400.
#[tokio::test]
async fn search_bad_mode_is_400() {
    let (app, _s, _a) = app_with(|store, acct| {
        let n = store
            .upsert_message(&msg(acct, "g1", "t1", "hello world", "body"))
            .unwrap();
        store
            .set_triage(n, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
    });
    let resp = app
        .oneshot(authed("GET", "/client/search?q=hello&mode=bogus"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Explicit `mode=semantic` with NO vector index degrades to keyword and reports
/// the kind actually run — never erroring the caller.
#[tokio::test]
async fn search_semantic_without_vectors_falls_back_to_keyword() {
    let (app, _s, _a) = app_with(|store, acct| {
        let n = store
            .upsert_message(&msg(acct, "g1", "t1", "quarterly report attached", "body"))
            .unwrap();
        store
            .set_triage(n, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
    });
    let resp = app
        .oneshot(authed("GET", "/client/search?q=quarterly&mode=semantic"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["match_kind"], "keyword", "no vectors => keyword fallback");
    assert_eq!(json["items"].as_array().unwrap().len(), 1);
}

/// With an embedder attached, the default mode is hybrid, semantic/hybrid run,
/// and sealed mail is STILL excluded from every mode.
#[tokio::test]
async fn search_modes_with_embedder_and_sealed_excluded() {
    use squelch_core::embed::StubEmbedder;

    // Attach a deterministic stub embedder (384-dim to match the vec0 table)
    // before wrapping the store in an Arc.
    let store = SqliteStore::open_in_memory()
        .unwrap()
        .with_embedder(Arc::new(StubEmbedder::new(384)))
        .unwrap();
    let acct = store.ensure_account("me@example.com").unwrap();
    let store = Arc::new(store);

    // Normal message + its vector.
    let n = store
        .upsert_message(&msg(acct, "g1", "t1", "acme invoice for services", "please pay"))
        .unwrap();
    store
        .set_triage(n, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
        .unwrap();
    let v = store.embedder().unwrap().embed("acme invoice for services please pay").unwrap();
    store.upsert_message_vector(acct, n, &v).unwrap();

    // Sealed OTP + (defensively) a leaked vector — must never surface.
    let s = store
        .upsert_message(&msg(acct, "g2", "t2", "acme verification code", "123456"))
        .unwrap();
    store
        .set_triage(s, acct, 90, Tier::Noise, Sensitivity::Sealed, Some(SealedKind::Otp), "", "", None)
        .unwrap();
    let sv = store.embedder().unwrap().embed("acme verification code 123456").unwrap();
    store.upsert_message_vector(acct, s, &sv).unwrap();

    let state = ApiState::new(store.clone(), acct, TOKEN).unwrap();
    let app = router(state);

    // Default (no mode) => hybrid; sealed absent.
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/search?q=acme"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["match_kind"], "hybrid");
    let items = json["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i["thread_id"] != "t2"), "sealed never surfaces");
    assert!(items.iter().any(|i| i["thread_id"] == "t1"));

    // Explicit semantic => runs semantic, sealed still absent.
    let resp = app
        .oneshot(authed("GET", "/client/search?q=acme&mode=semantic"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["match_kind"], "semantic");
    let items = json["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i["thread_id"] != "t2"), "sealed never surfaces in semantic");
}

#[tokio::test]
async fn reveal_writes_audit_and_returns_body() {
    let (app, store, acct) = app_with(|store, acct| {
        let s = store
            .upsert_message(&msg(acct, "g1", "t1", "code", "your code is 987654"))
            .unwrap();
        store
            .set_triage(
                s,
                acct,
                90,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "",
                "",
                None,
            )
            .unwrap();
    });

    // Find the sealed message id.
    let sealed_id = store.sealed_messages(acct).unwrap()[0].id;

    let resp = app
        .oneshot(authed("POST", &format!("/client/sealed/{sealed_id}/reveal")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let json = body_json(resp).await;
    assert_eq!(json["body"], "your code is 987654");

    // Audit row was written.
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "reveal_sealed");
    assert_eq!(audit[0].target.as_deref(), Some(sealed_id.to_string().as_str()));
}

#[tokio::test]
async fn sealed_list_has_no_bodies() {
    let (app, _s, _a) = app_with(|store, acct| {
        let s = store
            .upsert_message(&msg(acct, "g1", "t1", "code", "secret body 111"))
            .unwrap();
        store
            .set_triage(
                s,
                acct,
                90,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "",
                "",
                None,
            )
            .unwrap();
    });

    let resp = app.oneshot(authed("GET", "/client/sealed")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].get("body").is_none(), "no body field in sealed list");
    assert_eq!(items[0]["kind"], "otp");
}

#[tokio::test]
async fn pagination_cursor_round_trip() {
    let (app, _s, _a) = app_with(|store, acct| {
        // 3 normal signal messages so limit=2 yields a next_cursor.
        for i in 0..3 {
            let g = format!("g{i}");
            let t = format!("t{i}");
            let m = store
                .upsert_message(&msg(acct, &g, &t, "update", "body"))
                .unwrap();
            store
                .set_triage(m, acct, 80, Tier::Signal, Sensitivity::Normal, None, "", "", None)
                .unwrap();
        }
    });

    // First page.
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/updates?limit=2"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["items"].as_array().unwrap().len(), 2);
    let cursor = json["next_cursor"].as_str().expect("next_cursor present").to_string();

    // Second page via the cursor.
    let resp2 = app
        .oneshot(authed("GET", &format!("/client/updates?limit=2&cursor={cursor}")))
        .await
        .unwrap();
    let json2 = body_json(resp2).await;
    assert_eq!(json2["items"].as_array().unwrap().len(), 1);
    assert!(json2.get("next_cursor").is_none() || json2["next_cursor"].is_null());
}

#[tokio::test]
async fn bad_cursor_is_400() {
    let (app, _s, _a) = app_with(|_, _| {});
    let resp = app
        .oneshot(authed("GET", "/client/updates?cursor=@@notbase64@@"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Build an authed POST with a JSON body.
fn authed_json(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn action_requires_confirm() {
    // confirm gate fires before anything else: missing confirm => 400.
    let (app, _s, _a) = app_with(|_, _| {});
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/archive",
            serde_json::json!({ "message_id": 1 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert!(
        json["error"].as_str().unwrap().contains("confirm"),
        "400 must explain the confirm contract"
    );
}

#[tokio::test]
async fn action_without_write_credential_is_403() {
    // confirm present, but no write credential configured => 403 with a hint.
    let (app, _s, _a) = app_with(|store, acct| {
        let m = store
            .upsert_message(&msg(acct, "g1", "t1", "hi", "body"))
            .unwrap();
        store
            .set_triage(m, acct, 80, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
    });
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/archive",
            serde_json::json!({ "message_id": 1, "confirm": true }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json = body_json(resp).await;
    assert!(json["error"].as_str().unwrap().contains("--write"));
}

#[tokio::test]
async fn send_outbound_guard_blocks_and_audits() {
    let (app, store, acct) = app_with(|_, _| {});
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "body": "your verification code is 483920",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let json = body_json(resp).await;
    let err = json["error"].as_str().unwrap();
    assert!(err.contains("otp_code"), "422 lists redacted match kinds");
    assert!(!err.contains("483920"), "must NEVER echo the matched secret");

    // A blocked send still audits.
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "send");
    assert_eq!(audit[0].actor, "client-api");
    assert_eq!(audit[0].detail.as_deref(), Some("blocked:guard"));
}

#[tokio::test]
async fn send_guard_override_passes_guard_then_403_no_creds() {
    // With override_guard the guard is bypassed; without a write credential the
    // action then hits the 403 gate. Two audit rows: the override note + the
    // no-credential rejection.
    let (app, store, acct) = app_with(|_, _| {});
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "body": "your verification code is 483920",
                "confirm": true,
                "override_guard": true
            }),
        ))
        .await
        .unwrap();
    // Guard passed; no write creds => 403.
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let audit = store.list_audit(acct, 10).unwrap();
    // newest first: rejection then override note.
    assert!(audit.iter().any(|a| a.detail.as_deref() == Some("rejected:no_write_credential")));
    assert!(
        audit
            .iter()
            .any(|a| a.detail.as_deref().is_some_and(|d| d.starts_with("guard_override:")))
    );
}

#[tokio::test]
async fn clean_send_passes_guard() {
    // A clean body must clear the guard (then hit 403 for no creds, proving the
    // guard did not block).
    let (app, _s, _a) = app_with(|_, _| {});
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "body": "Hi Alice, sounds great, see you Tuesday. Bob",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "clean body clears the guard; only the missing write credential blocks it"
    );
}

// --- end-to-end action success (through the handler, mock Gmail) ------------

use async_trait::async_trait;
use squelch_core::credentials::{CredentialStore, OAuthToken};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct StubCreds;
#[async_trait]
impl CredentialStore for StubCreds {
    async fn token(&self, _a: i64) -> squelch_core::Result<OAuthToken> {
        Ok(OAuthToken {
            access_token: "WRITE-TOKEN".into(),
            refresh_token: None,
            expires_at: None,
        })
    }
}

/// Serve `n` sequential HTTP requests, each answered `200 {}`. Returns the
/// captured raw request bytes. Runs on a background task.
async fn mock_gmail(n: usize) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut reqs = Vec::new();
        for _ in 0..n {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let m = sock.read(&mut buf).await.unwrap();
            reqs.push(String::from_utf8_lossy(&buf[..m]).to_string());
            let body = "{}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        }
        reqs
    });
    (format!("http://{addr}"), handle)
}

fn app_with_writes(
    base: String,
    seed: impl FnOnce(&SqliteStore, i64),
) -> (axum::Router, Arc<SqliteStore>, i64) {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let acct = store.ensure_account("me@example.com").unwrap();
    seed(&store, acct);
    let state = ApiState::new(store.clone(), acct, TOKEN)
        .unwrap()
        .with_write_test_harness(Arc::new(StubCreds), base);
    (router(state), store, acct)
}

#[tokio::test]
async fn archive_success_audits_ok_and_hits_gmail() {
    let (base, handle) = mock_gmail(1).await;
    let (app, store, acct) = app_with_writes(base, |store, acct| {
        let m = store
            .upsert_message(&msg(acct, "gmail-abc", "t1", "hi", "body"))
            .unwrap();
        store
            .set_triage(m, acct, 80, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
    });
    // Grab the local message id via search (non-sealed).
    let message_id = store.search(acct, "hi", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/archive",
            serde_json::json!({ "message_id": message_id, "confirm": true }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let reqs = handle.await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].contains("/messages/gmail-abc/modify"));
    assert!(reqs[0].contains("\"removeLabelIds\":[\"INBOX\"]"));

    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit[0].action, "archive");
    assert_eq!(audit[0].actor, "client-api");
    assert_eq!(audit[0].detail.as_deref(), Some("ok"));
}

#[tokio::test]
async fn action_on_sealed_message_is_404() {
    // A sealed message must be invisible to actions: archive => 404 (and no
    // Gmail call is made). Proves the write path can never touch sealed mail.
    let (base, handle) = mock_gmail(0).await;
    let (app, store, acct) = app_with_writes(base, |store, acct| {
        let s = store
            .upsert_message(&msg(acct, "gmail-sealed", "t9", "code", "123456"))
            .unwrap();
        store
            .set_triage(
                s,
                acct,
                90,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "",
                "",
                None,
            )
            .unwrap();
    });
    let sealed_id = store.sealed_messages(acct).unwrap()[0].id;
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/archive",
            serde_json::json!({ "message_id": sealed_id, "confirm": true }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // No Gmail request should have been issued.
    handle.abort();
    // The attempted action is still audited as a target failure.
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit[0].detail.as_deref(), Some("failed:target"));
}

#[tokio::test]
async fn reply_send_success_threads_and_audits_ok() {
    // Reply flow makes two Gmail calls: parent_headers GET, then send POST.
    let (base, handle) = mock_gmail(2).await;
    let (app, store, acct) = app_with_writes(base, |store, acct| {
        let m = store
            .upsert_message(&msg(acct, "gmail-parent", "thread-77", "Lunch?", "want lunch?"))
            .unwrap();
        store
            .set_triage(m, acct, 80, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
    });
    let message_id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "reply_to_message_id": message_id,
                "body": "yes, noon works",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let reqs = handle.await.unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].starts_with("GET "), "first call reads parent headers");
    assert!(reqs[0].contains("/messages/gmail-parent"));
    assert!(reqs[1].contains("/messages/send"));
    // Threaded onto the parent's Gmail thread.
    assert!(reqs[1].contains("\"threadId\":\"thread-77\""));

    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit[0].action, "send");
    assert_eq!(audit[0].detail.as_deref(), Some("ok"));
}

// --- sitrep: seen-ledger + bands + resolution over HTTP ---------------------

use squelch_core::types::AttentionStatus;

/// Seed one signal message and return its local id via search.
fn seed_one_signal(store: &SqliteStore, acct: i64, gmail: &str, thread: &str, subj: &str) -> i64 {
    let m = store
        .upsert_message(&msg(acct, gmail, thread, subj, "body"))
        .unwrap();
    store
        .set_triage(m, acct, 80, Tier::Signal, Sensitivity::Normal, None, "", "", None)
        .unwrap();
    m
}

#[tokio::test]
async fn updates_stamp_once_and_carry_prestamp_surfaced_at() {
    let (app, store, acct) = app_with(|store, acct| {
        seed_one_signal(store, acct, "g1", "t1", "hi");
    });

    // First fetch: pre-stamp surfaced_at is null (this row was never surfaced).
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/updates"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert!(
        items[0]["surfaced_at"].is_null(),
        "response carries PRE-stamp value (null on first surface)"
    );
    assert_eq!(items[0]["status"], "new", "pre-stamp status is new");

    // The ledger was stamped as a side effect.
    let after = store
        .attention_updates(acct, chrono::Utc::now() - chrono::Duration::days(1), None, None, None)
        .unwrap();
    let first_stamp = after[0].surfaced_at.expect("surfaced_at now set");
    assert_eq!(after[0].status, AttentionStatus::Open);

    // Second fetch: surfaced_at is now present and unchanged (stamp-once).
    let resp2 = app
        .oneshot(authed("GET", "/client/updates"))
        .await
        .unwrap();
    let json2 = body_json(resp2).await;
    assert!(!json2["items"][0]["surfaced_at"].is_null());
    let after2 = store
        .attention_updates(acct, chrono::Utc::now() - chrono::Duration::days(1), None, None, None)
        .unwrap();
    assert_eq!(after2[0].surfaced_at, Some(first_stamp), "stamp did not move");
}

#[tokio::test]
async fn updates_carry_field_reasons_object() {
    use squelch_core::types::FieldReasons;
    let (app, _s, _a) = app_with(|store, acct| {
        let id = seed_one_signal(store, acct, "g1", "t1", "hi");
        store
            .set_field_reasons(
                id,
                acct,
                &FieldReasons {
                    importance: Some("known contact -> signal importance 80".into()),
                    deadline: None,
                    tier: Some("known contact -> signal".into()),
                },
            )
            .unwrap();
    });

    let resp = app.oneshot(authed("GET", "/client/updates")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let item = &json["items"][0];
    // WIRE CONTRACT: field_reasons is an object with per-property string values;
    // only the properties that carry a reason appear (deadline is absent here).
    let fr = &item["field_reasons"];
    assert!(fr.is_object(), "field_reasons must be an object: {item}");
    assert_eq!(fr["importance"], Value::String("known contact -> signal importance 80".into()));
    assert_eq!(fr["tier"], Value::String("known contact -> signal".into()));
    assert!(fr.get("deadline").is_none(), "absent deadline reason must be omitted, not null");
}

#[tokio::test]
async fn updates_without_reasons_omit_the_field_reasons_key() {
    // A row with no recorded reasons (predates the feature / Stage-1 wrote none)
    // omits the key entirely — the desktop treats absent the same as null.
    let (app, _s, _a) = app_with(|store, acct| {
        seed_one_signal(store, acct, "g1", "t1", "hi");
    });
    let resp = app.oneshot(authed("GET", "/client/updates")).await.unwrap();
    let json = body_json(resp).await;
    assert!(
        json["items"][0].get("field_reasons").is_none(),
        "no reasons recorded -> no field_reasons key: {}",
        json["items"][0]
    );
}

#[tokio::test]
async fn band_query_filters_server_side() {
    let (app, _s, _a) = app_with(|store, acct| {
        // A past_due bill (standing) + a plain signal.
        let bill = store
            .upsert_message(&msg(acct, "g1", "t1", "PG&E past due", "pay"))
            .unwrap();
        store
            .set_triage(bill, acct, 95, Tier::PastDue, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        seed_one_signal(store, acct, "g2", "t2", "hello");
    });

    let resp = app
        .oneshot(authed("GET", "/client/updates?band=standing"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "standing = past_due/deadline only");
    assert_eq!(items[0]["thread_id"], "t1");
    assert_eq!(items[0]["tier"], "past_due");
}

#[tokio::test]
async fn bad_band_and_status_are_400() {
    let (app, _s, _a) = app_with(|_, _| {});
    let r1 = app
        .clone()
        .oneshot(authed("GET", "/client/updates?band=bogus"))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::BAD_REQUEST);
    let r2 = app
        .oneshot(authed("GET", "/client/updates?status=bogus"))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn dismiss_and_reopen_endpoint() {
    let (app, store, acct) = app_with(|store, acct| {
        seed_one_signal(store, acct, "g1", "t1", "hi");
    });
    let id = store.search(acct, "hi", 10, 0).unwrap()[0].id;

    // Dismiss -> done.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            &format!("/client/updates/{id}/status"),
            serde_json::json!({ "status": "done" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let done = store
        .attention_updates(acct, chrono::Utc::now() - chrono::Duration::days(1), None, Some(AttentionStatus::Done), None)
        .unwrap();
    assert_eq!(done.len(), 1);
    assert!(done[0].resolved_at.is_some());

    // The dismiss is audited.
    let audit = store.list_audit(acct, 10).unwrap();
    assert!(audit.iter().any(|a| a.action == "set_status" && a.detail.as_deref() == Some("done")));

    // Reopen -> open.
    let resp2 = app
        .oneshot(authed_json(
            "POST",
            &format!("/client/updates/{id}/status"),
            serde_json::json!({ "status": "open" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
}

#[tokio::test]
async fn dismiss_unknown_message_is_404() {
    let (app, _s, _a) = app_with(|_, _| {});
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/updates/999/status",
            serde_json::json!({ "status": "done" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dismiss_sealed_message_is_404() {
    // A sealed row must be invisible to the status endpoint.
    let (app, store, acct) = app_with(|store, acct| {
        let s = store
            .upsert_message(&msg(acct, "g1", "t1", "code", "123456"))
            .unwrap();
        store
            .set_triage(
                s, acct, 90, Tier::Noise, Sensitivity::Sealed, Some(SealedKind::Otp), "", "", None,
            )
            .unwrap();
    });
    let sealed_id = store.sealed_messages(acct).unwrap()[0].id;
    let resp = app
        .oneshot(authed_json(
            "POST",
            &format!("/client/updates/{sealed_id}/status"),
            serde_json::json!({ "status": "done" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn archive_success_resolves_target_to_done() {
    let (base, handle) = mock_gmail(1).await;
    let (app, store, acct) = app_with_writes(base, |store, acct| {
        seed_one_signal(store, acct, "gmail-abc", "t1", "hi");
    });
    let message_id = store.search(acct, "hi", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/archive",
            serde_json::json!({ "message_id": message_id, "confirm": true }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = handle.await.unwrap();

    // RESOLUTION: the target row is now done + resolved_at set.
    let done = store
        .attention_updates(acct, chrono::Utc::now() - chrono::Duration::days(1), None, Some(AttentionStatus::Done), None)
        .unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].update.id, message_id);
    assert!(done[0].resolved_at.is_some());
}

#[tokio::test]
async fn update_rule_edits_in_place_and_404s_bogus() {
    // TASK 6: create -> PUT -> GET shows updated -> 404 on a bogus id.
    let (app, _s, _a) = app_with(|_, _| {});

    // Create a rule.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "*@old.com",
                "want": "old want",
                "disposition": "squelch"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let rule_id = created["rule_id"].as_i64().unwrap();

    // PUT updates it in place.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "PUT",
            &format!("/client/rules/{rule_id}"),
            serde_json::json!({
                "match_pattern": "*@new.com",
                "want": "new want",
                "disposition": "surface"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET reflects the update (same id, new fields).
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/rules"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let rules = json.as_array().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["id"].as_i64().unwrap(), rule_id);
    assert_eq!(rules[0]["match_pattern"], "*@new.com");
    assert_eq!(rules[0]["want_text"], "new want");
    assert_eq!(rules[0]["disposition"], "surface");

    // PUT a bogus id => 404.
    let resp = app
        .oneshot(authed_json(
            "PUT",
            "/client/rules/999999",
            serde_json::json!({
                "match_pattern": "*@x.com",
                "want": "",
                "disposition": "squelch"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rule_mutations_write_audit_rows() {
    // Each of POST/PUT/DELETE /client/rules writes a best-effort audit row
    // (actor="client-api"), so the human review UI can see rule changes.
    let (app, store, acct) = app_with(|_, _| {});

    // POST => rule.create, target = match_pattern.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "*@old.com",
                "want": "old want",
                "disposition": "squelch"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let rule_id = body_json(resp).await["rule_id"].as_i64().unwrap();

    // PUT => rule.update.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "PUT",
            &format!("/client/rules/{rule_id}"),
            serde_json::json!({
                "match_pattern": "*@new.com",
                "want": "new want",
                "disposition": "surface"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // DELETE => rule.delete, target = rule id.
    let resp = app
        .clone()
        .oneshot(authed("DELETE", &format!("/client/rules/{rule_id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let audit = store.list_audit(acct, 20).unwrap();
    // Newest-first. All three rows are actor="client-api".
    assert!(audit.iter().all(|a| a.actor == "client-api"));
    let create = audit.iter().find(|a| a.action == "rule.create").unwrap();
    assert_eq!(create.target.as_deref(), Some("*@old.com"));
    assert_eq!(create.detail.as_deref(), Some(rule_id.to_string().as_str()));
    let update = audit.iter().find(|a| a.action == "rule.update").unwrap();
    assert_eq!(update.target.as_deref(), Some("*@new.com"));
    let delete = audit.iter().find(|a| a.action == "rule.delete").unwrap();
    assert_eq!(delete.target.as_deref(), Some(rule_id.to_string().as_str()));
}

#[tokio::test]
async fn failed_rule_mutations_write_no_audit_row() {
    // A 404 (unknown id) on PUT/DELETE changed nothing, so it writes no row.
    let (app, store, acct) = app_with(|_, _| {});

    let resp = app
        .clone()
        .oneshot(authed_json(
            "PUT",
            "/client/rules/999999",
            serde_json::json!({
                "match_pattern": "*@x.com",
                "want": "",
                "disposition": "squelch"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .oneshot(authed("DELETE", "/client/rules/999999"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    assert_eq!(store.list_audit(acct, 20).unwrap().len(), 0);
}

#[tokio::test]
async fn stats_expose_stage2_usage_and_cost() {
    // GET /client/stats surfaces a stage2 object with today's usage + an
    // estimated cost from the default Stage-2 (claude-sonnet-5) per-MTok prices
    // (3.0 in / 15.0 out).
    let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let (app, _s, _a) = app_with(move |store, acct| {
        // 2 calls: 1_000_000 input tokens, 200_000 output tokens today.
        store.stage2_bump_usage(acct, &day, 600_000, 100_000).unwrap();
        store.stage2_bump_usage(acct, &day, 400_000, 100_000).unwrap();
    });

    let resp = app.oneshot(authed("GET", "/client/stats")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let s2 = &json["stage2"];
    assert_eq!(s2["calls_today"], 2);
    assert_eq!(s2["input_tokens_today"], 1_000_000);
    assert_eq!(s2["output_tokens_today"], 200_000);
    // cost = 3.0*(1e6/1e6) + 15.0*(0.2e6/1e6) = 3.0 + 3.0 = 6.0
    let cost = s2["est_cost_usd_today"].as_f64().unwrap();
    assert!((cost - 6.0).abs() < 1e-9, "expected 6.0, got {cost}");
}

#[tokio::test]
async fn usage_returns_rows_totals_and_is_bearer_gated() {
    // GET /client/usage: newest-first daily rows, aggregate totals with est cost
    // from the default Stage-2 (claude-sonnet-5) per-MTok prices (3.0 in / 15.0
    // out), and the model label.
    let (app, _s, _a) = app_with(|store, acct| {
        store.stage2_bump_usage(acct, "2026-07-08", 400_000, 100_000).unwrap();
        store.stage2_bump_usage(acct, "2026-07-09", 600_000, 100_000).unwrap();
    });

    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/usage"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;

    let rows = json["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // Newest-first.
    assert_eq!(rows[0]["day"], "2026-07-09");
    assert_eq!(rows[0]["input_tokens"], 600_000);
    assert_eq!(rows[1]["day"], "2026-07-08");

    let totals = &json["totals"];
    assert_eq!(totals["calls"], 2);
    assert_eq!(totals["input_tokens"], 1_000_000);
    assert_eq!(totals["output_tokens"], 200_000);
    // cost = 3.0*(1e6/1e6) + 15.0*(0.2e6/1e6) = 6.0
    let cost = totals["est_cost_usd"].as_f64().unwrap();
    assert!((cost - 6.0).abs() < 1e-9, "expected 6.0, got {cost}");

    // Default Stage-2 model label present.
    assert_eq!(json["model"], "claude-sonnet-5");
    // Stage-1 and Stage-2 appear as separate usage categories.
    assert_eq!(json["categories"]["stage2"]["model"], "claude-sonnet-5");
    assert_eq!(json["categories"]["stage1"]["model"], "claude-haiku-4-5");

    // Bearer-gated: no token => 401.
    let req = Request::builder()
        .uri("/client/usage")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn stats_expose_bands_and_last_surfaced_at() {
    let (app, _s, _a) = app_with(|store, acct| {
        let bill = store
            .upsert_message(&msg(acct, "g1", "t1", "bill due", "pay"))
            .unwrap();
        store
            .set_triage(bill, acct, 95, Tier::Deadline, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        seed_one_signal(store, acct, "g2", "t2", "hello");
    });

    // Before any surface: bands.new = 2, last_surfaced_at null.
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/stats"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["bands"]["standing"], 1);
    assert_eq!(json["bands"]["new"], 2);
    assert!(json["last_surfaced_at"].is_null());

    // Surface via /client/updates, then last_surfaced_at is set and new drops.
    let _ = app
        .clone()
        .oneshot(authed("GET", "/client/updates"))
        .await
        .unwrap();
    let resp2 = app.oneshot(authed("GET", "/client/stats")).await.unwrap();
    let json2 = body_json(resp2).await;
    assert_eq!(json2["bands"]["new"], 0);
    assert_eq!(json2["bands"]["open"], 2);
    assert!(!json2["last_surfaced_at"].is_null());
}

// --- GET /client/thread/{id} carries per-message sanitized html -------------

#[tokio::test]
async fn thread_response_carries_html_field() {
    let (app, _s, _a) = app_with(|store, acct| {
        // One HTML message and one plain-text message in the same thread.
        let mut html_msg = msg(acct, "g-html", "t-html", "Newsletter", "flattened text");
        html_msg.body_html = Some("<p>Hello <strong>world</strong></p>".to_string());
        let h = store.upsert_message(&html_msg).unwrap();
        store
            .set_triage(h, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();

        let plain = msg(acct, "g-plain", "t-html", "Newsletter", "just text");
        let p = store.upsert_message(&plain).unwrap();
        store
            .set_triage(p, acct, 55, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
    });

    let resp = app
        .oneshot(authed("GET", "/client/thread/t-html"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let msgs = json["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    // The HTML message carries sanitized html; the plain one carries null.
    assert_eq!(msgs[0]["html"], "<p>Hello <strong>world</strong></p>");
    assert_eq!(msgs[1]["html"], Value::Null);
    // Text content is always present (client fallback).
    assert_eq!(msgs[0]["content"], "flattened text");
}

// --- UNSUBSCRIBE endpoints ---------------------------------------------------

/// Seed a message carrying explicit List-Unsubscribe fields; return its id. The
/// one-click flag is still stored (ingest keeps it) but no longer affects
/// selection — the handler only extracts the first http(s) URL.
fn seed_unsub_msg(
    store: &SqliteStore,
    acct: i64,
    gmail: &str,
    from: &str,
    header: Option<&str>,
    one_click: bool,
) -> i64 {
    let mut m = msg(acct, gmail, "t-unsub", "Newsletter", "body");
    m.from_addr = from.to_string();
    m.list_unsubscribe = header.map(|h| h.to_string());
    m.list_unsub_one_click = one_click;
    let id = store.upsert_message(&m).unwrap();
    store
        .set_triage(id, acct, 10, Tier::Noise, Sensitivity::Normal, None, "", "", None)
        .unwrap();
    id
}

#[tokio::test]
async fn unsubscribe_returns_first_http_url_and_records_and_audits() {
    // The server returns the first http(s) URL for the client to open; it makes
    // no outbound request. A mailto ahead of the URL is skipped.
    let (app, store, acct) = app_with(|store, acct| {
        seed_unsub_msg(
            store,
            acct,
            "g1",
            "News@Sub.com",
            Some("<mailto:u@sub.com>, <https://sub.com/u/1?x=2>"),
            true,
        );
    });
    let mid = store.search(acct, "Newsletter", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json("POST", "/client/unsubscribe", serde_json::json!({ "message_id": mid })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["method"], "browser");
    assert_eq!(json["sender"], "news@sub.com", "sender is lowercased");
    assert_eq!(json["url"], "https://sub.com/u/1?x=2");

    // A ledger row exists for the lowercased sender, method 'browser'.
    let recs = store.list_unsubscribes(acct).unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].sender, "news@sub.com");
    assert_eq!(recs[0].method, "browser");
    assert_eq!(recs[0].violation_count, 0);

    // Audited (method + sender), never the URL.
    let audit = store.list_audit(acct, 10).unwrap();
    let row = audit.iter().find(|a| a.action == "unsubscribe").unwrap();
    assert_eq!(row.detail.as_deref(), Some("browser:news@sub.com"));
    assert!(audit.iter().all(|a| a.detail.as_deref().map(|d| !d.contains("sub.com/u")).unwrap_or(true)));
}

#[tokio::test]
async fn unsubscribe_browser_returns_url_and_does_not_fetch() {
    let (app, store, acct) = app_with(|store, acct| {
        seed_unsub_msg(store, acct, "g1", "news@sub.com", Some("<https://sub.com/u/9>"), false);
    });
    let mid = store.search(acct, "Newsletter", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json("POST", "/client/unsubscribe", serde_json::json!({ "message_id": mid })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["method"], "browser");
    assert_eq!(json["sender"], "news@sub.com");
    assert_eq!(json["url"], "https://sub.com/u/9");
    assert_eq!(store.list_unsubscribes(acct).unwrap()[0].method, "browser");
}

#[tokio::test]
async fn unsubscribe_mailto_only_is_422() {
    // A mailto-only List-Unsubscribe has no http(s) link => 422; the server never
    // sends anything.
    let (app, store, acct) = app_with(|store, acct| {
        seed_unsub_msg(store, acct, "g1", "news@sub.com", Some("<mailto:unsub@sub.com?subject=Bye>"), false);
    });
    let mid = store.search(acct, "Newsletter", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json("POST", "/client/unsubscribe", serde_json::json!({ "message_id": mid })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(store.list_unsubscribes(acct).unwrap().len(), 0, "no ledger row on 422");
}

#[tokio::test]
async fn unsubscribe_no_info_is_422() {
    let (app, store, acct) = app_with(|store, acct| {
        seed_unsub_msg(store, acct, "g1", "news@sub.com", None, false);
    });
    let mid = store.search(acct, "Newsletter", 10, 0).unwrap()[0].id;
    let resp = app
        .oneshot(authed_json("POST", "/client/unsubscribe", serde_json::json!({ "message_id": mid })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn unsubscribe_unknown_and_sealed_are_404() {
    let (app, store, acct) = app_with(|store, acct| {
        // A sealed message that (defensively) carries an unsub header.
        let s = store
            .upsert_message(&{
                let mut m = msg(acct, "g-otp", "t-otp", "verification code", "123456");
                m.list_unsubscribe = Some("<https://sub.com/u/1>".into());
                m
            })
            .unwrap();
        store
            .set_triage(s, acct, 90, Tier::Noise, Sensitivity::Sealed, Some(SealedKind::Otp), "", "", None)
            .unwrap();
    });
    let sealed_id = store.sealed_messages(acct).unwrap()[0].id;

    // Sealed => 404 (indistinguishable from unknown).
    let resp = app
        .clone()
        .oneshot(authed_json("POST", "/client/unsubscribe", serde_json::json!({ "message_id": sealed_id })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Unknown id => 404.
    let resp = app
        .oneshot(authed_json("POST", "/client/unsubscribe", serde_json::json!({ "message_id": 999999 })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_unsubscribes_newest_first_and_bearer_gated() {
    let (app, _s, _a) = app_with(|store, acct| {
        store
            .upsert_unsubscribe(acct, "old@x.com", "browser", None, chrono::Utc::now() - chrono::Duration::hours(3))
            .unwrap();
        store
            .upsert_unsubscribe(acct, "new@x.com", "one_click", None, chrono::Utc::now())
            .unwrap();
    });

    // Bearer-gated.
    let unauth = Request::builder().uri("/client/unsubscribes").body(Body::empty()).unwrap();
    assert_eq!(app.clone().oneshot(unauth).await.unwrap().status(), StatusCode::UNAUTHORIZED);

    let resp = app.oneshot(authed("GET", "/client/unsubscribes")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["sender"], "new@x.com", "newest requested_at first");
    assert_eq!(items[0]["method"], "one_click");
    assert_eq!(items[0]["violation_count"], 0);
    assert_eq!(items[0]["last_violation_at"], Value::Null);
    assert_eq!(items[0]["resolution"], Value::Null);
    assert_eq!(items[1]["sender"], "old@x.com");
}

#[tokio::test]
async fn unsubscribe_resolution_sets_blocked_and_404s_unknown_and_400s_bad_value() {
    let (app, store, acct) = app_with(|store, acct| {
        store
            .upsert_unsubscribe(acct, "news@x.com", "browser", None, chrono::Utc::now())
            .unwrap();
    });

    // blocked => 200, echoes sender + resolution.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/unsubscribes/resolution",
            serde_json::json!({ "sender": "News@X.com", "resolution": "blocked" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["sender"], "news@x.com");
    assert_eq!(json["resolution"], "blocked");
    assert_eq!(store.list_unsubscribes(acct).unwrap()[0].resolution.as_deref(), Some("blocked"));
    let audit = store.list_audit(acct, 10).unwrap();
    assert!(audit.iter().any(|a| a.action == "unsub_resolution" && a.detail.as_deref() == Some("news@x.com:blocked")));

    // Unknown sender => 404.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/unsubscribes/resolution",
            serde_json::json!({ "sender": "nobody@x.com", "resolution": "dismissed" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Bad resolution value => 400.
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/unsubscribes/resolution",
            serde_json::json!({ "sender": "news@x.com", "resolution": "nuke" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn thread_sealed_is_not_found_even_with_html() {
    let (app, _s, _a) = app_with(|store, acct| {
        let mut sealed = msg(acct, "g-otp", "t-sealed", "verification code", "123456");
        sealed.body_html = Some("<p>code 123456</p>".to_string());
        let s = store.upsert_message(&sealed).unwrap();
        store
            .set_triage(
                s,
                acct,
                90,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "",
                "",
                None,
            )
            .unwrap();
    });

    let resp = app
        .oneshot(authed("GET", "/client/thread/t-sealed"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// --- /client/triage-config --------------------------------------------------

#[tokio::test]
async fn triage_config_get_default_shape() {
    // With no override rows and a default state, GET reports the built-in default
    // caps, all sources "default", the price fields, and null tokens/call (the
    // usage ledger is empty).
    let (app, _s, _a) = app_with(|_, _| {});
    let resp = app
        .oneshot(authed("GET", "/client/triage-config"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;

    assert_eq!(json["thread_daily_cap"], 3);
    assert_eq!(json["sender_daily_cap"], 5);
    assert_eq!(json["global_daily_cap"], 200);
    assert_eq!(json["sources"]["thread_daily_cap"], "default");
    assert_eq!(json["sources"]["sender_daily_cap"], "default");
    assert_eq!(json["sources"]["global_daily_cap"], "default");
    // Empty ledger => tokens/call are null; calls/inbound averages are 0.
    assert!(json["avg_tokens_in_per_call"].is_null());
    assert!(json["avg_tokens_out_per_call"].is_null());
    assert_eq!(json["avg_stage2_calls_per_day"], 0.0);
    assert_eq!(json["avg_inbound_per_day"], 0.0);
    // Prices are present (default Stage2Config values on ApiState::new).
    assert!(json["price_in_per_mtok"].is_number());
    assert!(json["price_out_per_mtok"].is_number());

    // Stage-2 escalation model label.
    assert_eq!(json["stage2_model"], "claude-sonnet-5");

    // Stage-1 block: default small model, GLOBAL-only cap (default 1000),
    // "default" source, null tokens/call (empty ledger), and prices.
    let s1 = &json["stage1"];
    assert_eq!(s1["model"], "claude-haiku-4-5");
    assert_eq!(s1["global_daily_cap"], 1000);
    assert_eq!(s1["source"], "default");
    assert_eq!(s1["avg_calls_per_day"], 0.0);
    assert!(s1["avg_tokens_in_per_call"].is_null());
    assert!(s1["avg_tokens_out_per_call"].is_null());
    assert!(s1["price_in_per_mtok"].is_number());
    assert!(s1["price_out_per_mtok"].is_number());
}

#[tokio::test]
async fn triage_config_post_persists_stage1_global_cap_override() {
    let (app, store, acct) = app_with(|_, _| {});
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/triage-config",
            serde_json::json!({ "stage1_global_daily_cap": 750 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["stage1"]["global_daily_cap"], 750);
    assert_eq!(json["stage1"]["source"], "override");

    // Persisted where the Stage-1 pass re-reads it each cycle.
    let o = store.stage2_cap_overrides(acct).unwrap();
    assert_eq!(o.stage1_global_daily_cap, Some(750));
}

#[tokio::test]
async fn triage_config_post_rejects_out_of_range_stage1_cap() {
    let (app, store, acct) = app_with(|_, _| {});
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/triage-config",
            serde_json::json!({ "stage1_global_daily_cap": 0 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(store.stage2_cap_overrides(acct).unwrap().stage1_global_daily_cap, None);
}

#[tokio::test]
async fn triage_config_get_computes_trailing_averages() {
    // Seed 4 recent inbound messages + a usage ledger day, and confirm the
    // trailing-14d averages and per-call token means.
    let (app, _s, _a) = app_with(|store, acct| {
        for i in 0..4 {
            let m = msg(acct, &format!("g{i}"), &format!("t{i}"), "hi", "body");
            store.upsert_message(&m).unwrap();
        }
        // One day with 2 calls, 1000 in / 200 out tokens.
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        store.stage2_bump_usage(acct, &day, 600, 120).unwrap();
        store.stage2_bump_usage(acct, &day, 400, 80).unwrap();
    });

    let resp = app
        .oneshot(authed("GET", "/client/triage-config"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    // 4 inbound / 14 days.
    assert!((json["avg_inbound_per_day"].as_f64().unwrap() - 4.0 / 14.0).abs() < 1e-9);
    // 2 calls / 14 days.
    assert!((json["avg_stage2_calls_per_day"].as_f64().unwrap() - 2.0 / 14.0).abs() < 1e-9);
    // 1000 in / 2 calls = 500; 200 out / 2 calls = 100.
    assert!((json["avg_tokens_in_per_call"].as_f64().unwrap() - 500.0).abs() < 1e-9);
    assert!((json["avg_tokens_out_per_call"].as_f64().unwrap() - 100.0).abs() < 1e-9);
}

#[tokio::test]
async fn triage_config_post_persists_override_and_reports_it() {
    let (app, store, acct) = app_with(|_, _| {});
    // Set thread + global (omit sender — subset allowed).
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/triage-config",
            serde_json::json!({ "thread_daily_cap": 5, "global_daily_cap": 300 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;

    // Response reflects the fresh effective values + "override" sources for the
    // two set caps; the untouched sender cap stays default.
    assert_eq!(json["thread_daily_cap"], 5);
    assert_eq!(json["global_daily_cap"], 300);
    assert_eq!(json["sender_daily_cap"], 5);
    assert_eq!(json["sources"]["thread_daily_cap"], "override");
    assert_eq!(json["sources"]["global_daily_cap"], "override");
    assert_eq!(json["sources"]["sender_daily_cap"], "default");

    // Persisted to the store (what the Stage-2 pass re-reads each cycle).
    let o = store.stage2_cap_overrides(acct).unwrap();
    assert_eq!(o.thread_daily_cap, Some(5));
    assert_eq!(o.global_daily_cap, Some(300));
    assert_eq!(o.sender_daily_cap, None);
}

#[tokio::test]
async fn triage_config_post_rejects_out_of_range_and_non_integer() {
    let (app, store, acct) = app_with(|_, _| {});

    // Zero is below the min.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/triage-config",
            serde_json::json!({ "thread_daily_cap": 0 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Above the max.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/triage-config",
            serde_json::json!({ "global_daily_cap": 100001 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A non-integer (float) is rejected at deserialization.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/triage-config",
            serde_json::json!({ "thread_daily_cap": 5.5 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Nothing was persisted by any rejected request.
    assert_eq!(store.stage2_cap_overrides(acct).unwrap(), Default::default());
}

// --- attachments ------------------------------------------------------------

/// Collect a response body into raw bytes.
async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body().collect().await.unwrap().to_bytes().to_vec()
}

fn header_str(resp: &axum::response::Response, name: header::HeaderName) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn thread_carries_attachment_metadata_and_empty_when_none() {
    let (app, store, acct) = app_with(|store, acct| {
        // Thread t1: one message with a stored pdf + an over-cap (NULL data) part.
        let m1 = store.upsert_message(&msg(acct, "g1", "t1", "s1", "b1")).unwrap();
        store
            .set_triage(m1, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        store
            .insert_attachment(acct, m1, "doc.pdf", "application/pdf", 5, Some(b"Hello"))
            .unwrap();
        store
            .insert_attachment(acct, m1, "big.bin", "application/octet-stream", 11_000_000, None)
            .unwrap();
        // Thread t2: a message with NO attachments -> [] on the wire.
        let m2 = store.upsert_message(&msg(acct, "g2", "t2", "s2", "b2")).unwrap();
        store
            .set_triage(m2, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
    });
    let _ = (&store, acct);

    let resp = app.clone().oneshot(authed("GET", "/client/thread/t1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let atts = &json["messages"][0]["attachments"];
    assert_eq!(atts.as_array().unwrap().len(), 2);
    // Ordered by id: pdf first (downloadable), big second (not downloadable).
    assert_eq!(atts[0]["filename"], "doc.pdf");
    assert_eq!(atts[0]["mime"], "application/pdf");
    assert_eq!(atts[0]["size"], 5);
    assert_eq!(atts[0]["downloadable"], true);
    assert_eq!(atts[1]["filename"], "big.bin");
    assert_eq!(atts[1]["downloadable"], false);

    let resp = app.oneshot(authed("GET", "/client/thread/t2")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(
        json["messages"][0]["attachments"],
        serde_json::json!([]),
        "attachments key is always present, [] when none"
    );
}

#[tokio::test]
async fn attachment_bytes_headers_apply_render_safety_whitelist() {
    // Seed one attachment of each interesting mime; ids come back in insert order.
    let ids = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(i64, &'static str)>::new()));
    let ids_seed = ids.clone();
    let (app, _store, _acct) = app_with(move |store, acct| {
        let m = store.upsert_message(&msg(acct, "g1", "t1", "s", "b")).unwrap();
        store
            .set_triage(m, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        let mut v = ids_seed.lock().unwrap();
        v.push((
            store.insert_attachment(acct, m, "doc.pdf", "application/pdf", 3, Some(b"pdf")).unwrap(),
            "pdf",
        ));
        v.push((
            store.insert_attachment(acct, m, "pic.png", "image/png", 3, Some(b"png")).unwrap(),
            "png",
        ));
        v.push((
            store
                .insert_attachment(acct, m, "vec.svg", "image/svg+xml", 3, Some(b"svg"))
                .unwrap(),
            "svg",
        ));
        v.push((
            store
                .insert_attachment(acct, m, "page.html", "text/html", 3, Some(b"htm"))
                .unwrap(),
            "html",
        ));
        // Case/parameter tricks + the xml family: all must force octet-stream.
        v.push((
            store
                .insert_attachment(acct, m, "shout.svg", "IMAGE/SVG+XML; charset=x", 3, Some(b"svg"))
                .unwrap(),
            "svg-shout",
        ));
        v.push((
            store
                .insert_attachment(acct, m, "bare.svg", "image/svg", 3, Some(b"svg"))
                .unwrap(),
            "svg-bare",
        ));
        v.push((
            store
                .insert_attachment(acct, m, "x.xhtml", "application/xhtml+xml", 3, Some(b"xht"))
                .unwrap(),
            "xhtml",
        ));
        v.push((
            store
                .insert_attachment(acct, m, "x.xml", "text/xml", 3, Some(b"xml"))
                .unwrap(),
            "txml",
        ));
        v.push((
            store
                .insert_attachment(acct, m, "y.xml", "application/xml", 3, Some(b"xml"))
                .unwrap(),
            "axml",
        ));
    });

    let ids = ids.lock().unwrap().clone();
    for (id, kind) in ids {
        let resp = app
            .clone()
            .oneshot(authed("GET", &format!("/client/attachments/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{kind} should 200");
        let ct = header_str(&resp, header::CONTENT_TYPE);
        match kind {
            "pdf" => assert_eq!(ct, "application/pdf"),
            "png" => assert_eq!(ct, "image/png"),
            // Scriptable types must NEVER be renderable from our origin.
            "svg" => assert_eq!(ct, "application/octet-stream", "svg must be octet-stream"),
            "html" => assert_eq!(ct, "application/octet-stream", "html must be octet-stream"),
            "svg-shout" | "svg-bare" | "xhtml" | "txml" | "axml" => assert_eq!(
                ct, "application/octet-stream",
                "{kind}: scriptable/xml-family mime must be octet-stream"
            ),
            _ => unreachable!(),
        }
        // Common security headers on every served attachment.
        assert_eq!(header_str(&resp, header::X_CONTENT_TYPE_OPTIONS), "nosniff");
        assert_eq!(header_str(&resp, header::CACHE_CONTROL), "private, max-age=3600");
        assert!(
            header_str(&resp, header::CONTENT_DISPOSITION).starts_with("attachment;"),
            "must be an attachment disposition"
        );
        let bytes = body_bytes(resp).await;
        assert_eq!(bytes.len(), 3, "{kind} bytes served");
    }
}

#[tokio::test]
async fn attachment_filename_is_sanitized_in_disposition() {
    let id = std::sync::Arc::new(std::sync::Mutex::new(0i64));
    let id_seed = id.clone();
    let (app, _store, _acct) = app_with(move |store, acct| {
        let m = store.upsert_message(&msg(acct, "g1", "t1", "s", "b")).unwrap();
        store
            .set_triage(m, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        *id_seed.lock().unwrap() = store
            .insert_attachment(
                acct,
                m,
                "../../evil\"name.pdf",
                "application/pdf",
                3,
                Some(b"pdf"),
            )
            .unwrap();
    });
    let id = *id.lock().unwrap();
    let resp = app
        .oneshot(authed("GET", &format!("/client/attachments/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let disp = header_str(&resp, header::CONTENT_DISPOSITION);
    // Path separators and quotes stripped; the header stays well-formed.
    assert!(!disp.contains('/'), "no path separators: {disp}");
    assert!(!disp.contains('\\'), "no backslashes: {disp}");
    assert_eq!(disp, "attachment; filename=\"....evilname.pdf\"");
}

#[tokio::test]
async fn attachment_over_cap_is_410() {
    let id = std::sync::Arc::new(std::sync::Mutex::new(0i64));
    let id_seed = id.clone();
    let (app, _store, _acct) = app_with(move |store, acct| {
        let m = store.upsert_message(&msg(acct, "g1", "t1", "s", "b")).unwrap();
        store
            .set_triage(m, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        // Metadata only (data == None): over the ingest cap.
        *id_seed.lock().unwrap() = store
            .insert_attachment(acct, m, "big.bin", "application/octet-stream", 11_000_000, None)
            .unwrap();
    });
    let id = *id.lock().unwrap();
    let resp = app
        .oneshot(authed("GET", &format!("/client/attachments/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::GONE);
}

#[tokio::test]
async fn attachment_on_sealed_parent_is_404() {
    let id = std::sync::Arc::new(std::sync::Mutex::new(0i64));
    let id_seed = id.clone();
    let (app, _store, _acct) = app_with(move |store, acct| {
        let m = store.upsert_message(&msg(acct, "g1", "t1", "code", "secret")).unwrap();
        store
            .set_triage(
                m,
                acct,
                0,
                Tier::Noise,
                Sensitivity::Sealed,
                Some(SealedKind::Otp),
                "",
                "",
                None,
            )
            .unwrap();
        *id_seed.lock().unwrap() = store
            .insert_attachment(acct, m, "secret.pdf", "application/pdf", 6, Some(b"secret"))
            .unwrap();
    });
    let id = *id.lock().unwrap();
    let resp = app
        .clone()
        .oneshot(authed("GET", &format!("/client/attachments/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "sealed parent -> 404");

    // An unknown id is likewise 404 (indistinguishable from the sealed case).
    let resp = app
        .oneshot(authed("GET", "/client/attachments/999999"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn attachment_requires_bearer_auth() {
    let (app, _store, _acct) = app_with(|_, _| {});
    let req = Request::builder()
        .uri("/client/attachments/1")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "byte endpoint is behind auth");
}

#[tokio::test]
async fn triage_feedback_round_trip_records_and_audits() {
    let mut mid = 0;
    let (app, store, acct) = app_with(|store, acct| {
        let m = store.upsert_message(&msg(acct, "g1", "t1", "s", "b")).unwrap();
        store
            .set_triage(m, acct, 60, Tier::Signal, Sensitivity::Normal, None, "", "", None)
            .unwrap();
        mid = m;
    });

    // Correct the tier; the response carries the recorded row.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/triage-feedback",
            serde_json::json!({
                "message_id": mid,
                "dimension": "tier",
                "to_value": "noise",
                "note": "newsletter, not signal"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "route must be mounted and accept the correction");
    let json = body_json(resp).await;
    assert_eq!(json["dimension"], "tier");
    assert_eq!(json["from_value"], "signal");
    assert_eq!(json["to_value"], "noise");

    // The correction is the dataset: GET returns it.
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/triage-feedback"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json.as_array().map(Vec::len), Some(1));
    assert_eq!(json[0]["message_id"], mid);

    // Audited.
    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit.iter().any(|e| e.action == "triage_correction"),
        "correction must land an audit row"
    );

    // Invalid axis / invalid value are 400, not recorded.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/triage-feedback",
            serde_json::json!({ "message_id": mid, "dimension": "vibes", "to_value": "noise" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/triage-feedback",
            serde_json::json!({ "message_id": mid, "dimension": "tier", "to_value": "spam" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
