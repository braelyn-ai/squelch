//! Integration tests for the human-door router: bearer auth, sealed exclusion
//! from search, the audited `no-store` reveal, actions against a mock Gmail, and
//! the wire shapes the desktop client decodes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use squelch_api::{ApiState, router};
use squelch_core::store::{SqliteStore, Store};
use squelch_core::types::{SealedKind, Sensitivity, Tier};
use tower::ServiceExt;

mod common;
use common::{Harness, TOKEN, authed, authed_json, body_json, harness, msg, sent_msg};

#[tokio::test]
async fn missing_token_is_401() {
    let Harness { app, .. } = harness(|_, _| {});
    let req = Request::builder()
        .uri("/client/stats")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_token_is_401() {
    let Harness { app, .. } = harness(|_, _| {});
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
    let Harness { app, .. } = harness(|_, _| {});
    let resp = app.oneshot(authed("GET", "/client/stats")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// A blank master token no longer stops the door coming up — pairing needs a
/// door to talk to — but it must not become a credential either. The door serves
/// and refuses everything until a device token exists.
#[tokio::test]
async fn an_empty_master_token_serves_and_refuses_everything() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    let acct = store.ensure_account("me@example.com").unwrap();

    for blank in ["", "   "] {
        let app = router(ApiState::new(store.clone(), acct, blank));
        for header_value in [None, Some("Bearer "), Some("Bearer    ")] {
            let mut req = Request::builder().uri("/client/stats");
            if let Some(h) = header_value {
                req = req.header(header::AUTHORIZATION, h);
            }
            let resp = app
                .clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{blank:?}");
        }
    }
}

#[tokio::test]
async fn search_excludes_sealed() {
    let Harness { app, .. } = harness(|store, acct| {
        // Normal message mentioning "verification"...
        let n = store
            .upsert_message(&msg(
                acct,
                "g1",
                "t1",
                "Your account verification steps",
                "hello",
            ))
            .unwrap();
        store
            .set_triage(
                n,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
        // ...and a sealed OTP mentioning it too.
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
    let Harness { app, store, acct } = harness(|store, acct| {
        let mid = store
            .upsert_message(&msg(acct, "g1", "t1", "shipped", "b"))
            .unwrap();
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
                    tracking_url: Some(
                        "https://www.ups.com/track?tracknum=1Z999AA10123456784".into(),
                    ),
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
    assert_eq!(
        json.as_array().unwrap().len(),
        2,
        "delivered included with flag"
    );
}

/// WIRE CONTRACT: the four carrier-poll fields ride out on `/client/shipments`
/// verbatim, so the client can show an ETA and the carrier's own words. A row
/// that has never been polled still carries all four KEYS, as nulls — a client
/// decoding them must never have to distinguish "absent" from "not yet polled".
#[tokio::test]
async fn shipments_carry_the_carrier_poll_fields() {
    use squelch_core::triage::{CarrierTrack, ShipmentInfo, ShipmentStatus};
    let t0 = chrono::Utc::now();
    let eta = t0 + chrono::Duration::hours(6);
    let polled = t0 + chrono::Duration::minutes(3);

    let Harness { app, .. } = harness(|store, acct| {
        let mid = store
            .upsert_message(&msg(acct, "g1", "t1", "shipped", "b"))
            .unwrap();
        let polled_id = store
            .upsert_shipment(
                acct,
                mid,
                &ShipmentInfo {
                    carrier: "ups".into(),
                    tracking_number: "1Z999AA10123456784".into(),
                    item_name: "Headphones".into(),
                    status: ShipmentStatus::Shipped,
                    tracking_url: None,
                },
                t0,
            )
            .unwrap();
        // A second row that no poll has ever touched, for the null case.
        store
            .upsert_shipment(
                acct,
                mid,
                &ShipmentInfo {
                    carrier: "usps".into(),
                    tracking_number: "9400111899223817428490".into(),
                    item_name: "Book".into(),
                    status: ShipmentStatus::Shipped,
                    tracking_url: None,
                },
                t0,
            )
            .unwrap();
        store
            .apply_carrier_track(
                acct,
                polled_id,
                &CarrierTrack {
                    status: Some(ShipmentStatus::OutForDelivery),
                    carrier_status_raw: "Out For Delivery".into(),
                    eta: Some(eta),
                    delivered_at: None,
                },
                polled,
            )
            .unwrap();
    });

    let resp = app
        .oneshot(authed("GET", "/client/shipments"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json.as_array().unwrap();
    assert_eq!(items.len(), 2);

    let polled_row = items
        .iter()
        .find(|i| i["tracking_number"] == "1Z999AA10123456784")
        .expect("the polled shipment");
    assert_eq!(polled_row["status"], "out_for_delivery");
    assert_eq!(polled_row["carrier_status_raw"], "Out For Delivery");
    // Timestamps go out as strings the client parses back to the same instant.
    let parse = |v: &Value| {
        v.as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap()
    };
    assert_eq!(parse(&polled_row["eta"]), eta);
    assert_eq!(parse(&polled_row["last_polled_at"]), polled);
    assert!(
        polled_row["delivered_at"].is_null(),
        "out for delivery is not delivered"
    );

    let unpolled = items
        .iter()
        .find(|i| i["tracking_number"] == "9400111899223817428490")
        .expect("the never-polled shipment");
    for field in [
        "carrier_status_raw",
        "eta",
        "delivered_at",
        "last_polled_at",
    ] {
        assert!(
            unpolled.get(field).is_some_and(Value::is_null),
            "{field} must be present and null before the first poll"
        );
    }
}

/// A configured kick reaches the poller's `Notify` and reports the carriers it
/// will hit. `notify_one` parks a permit when nobody is waiting yet, so awaiting
/// AFTER the request still completes — the timeout is what makes a missed poke
/// a failure rather than a hang.
#[tokio::test]
async fn shipment_poll_kicks_the_poller_and_lists_its_carriers() {
    let kick = Arc::new(tokio::sync::Notify::new());
    let (state, _store, _acct) = common::state_with(|_, _| {});
    // Deliberately unsorted + duplicated: the response must not depend on how
    // the caller assembled the list.
    let app = router(
        state.with_shipment_poll_kick(kick.clone(), vec!["ups".into(), "dhl".into(), "ups".into()]),
    );

    let resp = app
        .oneshot(authed("POST", "/client/shipments/poll"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["kicked"], true);
    assert_eq!(json["carriers"], serde_json::json!(["dhl", "ups"]));

    tokio::time::timeout(std::time::Duration::from_secs(1), kick.notified())
        .await
        .expect("the poller's kick must have been notified");
}

/// Carrier polling is BYOK, so no poller is the RESTING STATE, not an error: the
/// route serves 200 with `kicked: false` and an empty list, which is how the
/// client knows to hide the button. Still behind the bearer, like everything in
/// the `/client` tree.
#[tokio::test]
async fn shipment_poll_without_a_poller_is_a_200_that_kicked_nothing() {
    let Harness { app, .. } = harness(|_, _| {});

    let unauthed = Request::builder()
        .method("POST")
        .uri("/client/shipments/poll")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(unauthed).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .oneshot(authed("POST", "/client/shipments/poll"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["kicked"], false);
    assert_eq!(json["carriers"], serde_json::json!([]));
}

#[tokio::test]
async fn receipts_returns_rows_newest_first_and_is_bearer_gated() {
    use squelch_core::triage::ReceiptInfo;
    let Harness { app, .. } = harness(|store, acct| {
        let m1 = store
            .upsert_message(&msg(acct, "g1", "t1", "receipt a", "b"))
            .unwrap();
        let m2 = store
            .upsert_message(&msg(acct, "g2", "t2", "receipt b", "b"))
            .unwrap();
        // Older, then newer — expect newest-first ordering on read.
        store
            .upsert_receipt(
                acct,
                m1,
                "no-reply@baywheels.com",
                Some("Bay Wheels"),
                &ReceiptInfo {
                    amount: Some(3.49),
                    currency: Some("USD".into()),
                },
                chrono::Utc::now() - chrono::Duration::hours(2),
            )
            .unwrap();
        store
            .upsert_receipt(
                acct,
                m2,
                "orders@shop.com",
                None,
                &ReceiptInfo {
                    amount: None,
                    currency: None,
                },
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
    let resp = app
        .oneshot(authed("GET", "/client/receipts"))
        .await
        .unwrap();
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
    let Harness { app, store, acct } = harness(|store, acct| {
        let m = store
            .upsert_message(&msg(acct, "g1", "t1", "s", "b"))
            .unwrap();
        store
            .set_triage(
                m,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
        // An LLM-classified row, so there is something to reset.
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

    // The route is mounted (the 404 class of regression) and resets the window.
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
    let Harness { app, .. } = harness(|store, acct| {
        let m1 = store
            .upsert_message(&msg(acct, "g1", "t1", "statement", "b"))
            .unwrap();
        let m2 = store
            .upsert_message(&msg(acct, "g2", "t2", "alert", "b"))
            .unwrap();
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
    assert!(
        items[0].get("account_id").is_none(),
        "account_id off the wire"
    );
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
    let Harness { app, .. } = harness(|store, acct| {
        let m1 = store
            .upsert_message(&msg(acct, "g1", "t1", "invite", "b"))
            .unwrap();
        let m2 = store
            .upsert_message(&msg(acct, "g2", "t2", "cancel", "b"))
            .unwrap();
        let m3 = store
            .upsert_message(&msg(acct, "g3", "t3", "old", "b"))
            .unwrap();
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
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/calendar"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json.as_array().unwrap();
    assert_eq!(items.len(), 2, "30h-old row outside the default 24h window");
    assert_eq!(items[0]["kind"], "cancellation", "newest-received first");
    assert_eq!(
        items[0]["event_title"],
        Value::Null,
        "nullable fields serialize as null"
    );
    assert_eq!(items[0]["starts_at"], Value::Null);
    assert_eq!(items[0]["organizer"], Value::Null);
    assert_eq!(items[1]["kind"], "invite");
    assert_eq!(items[1]["event_title"], "Design review");
    assert_eq!(items[1]["organizer"], "Sam Doe");
    assert!(items[1]["starts_at"].is_string(), "iso8601 start");
    // Contract shape: exactly these keys (account_id deliberately absent).
    let keys: std::collections::BTreeSet<_> =
        items[0].as_object().unwrap().keys().cloned().collect();
    let expect: std::collections::BTreeSet<String> = [
        "id",
        "message_id",
        "thread_id",
        "kind",
        "event_title",
        "starts_at",
        "organizer",
        "received_at",
    ]
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

    // Out-of-range hours are CLAMPED, not rejected. The 1h-old row sits exactly
    // on the hours=1 boundary, so assert a subset rather than an exact count.
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
    assert_eq!(
        json.as_array().unwrap().len(),
        3,
        "clamped to 168h, rows within remain"
    );
}

/// A garbage `mode` value is a 400.
#[tokio::test]
async fn search_bad_mode_is_400() {
    let Harness { app, .. } = harness(|store, acct| {
        let n = store
            .upsert_message(&msg(acct, "g1", "t1", "hello world", "body"))
            .unwrap();
        store
            .set_triage(
                n,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
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
    let Harness { app, .. } = harness(|store, acct| {
        let n = store
            .upsert_message(&msg(acct, "g1", "t1", "quarterly report attached", "body"))
            .unwrap();
        store
            .set_triage(
                n,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
    });
    let resp = app
        .oneshot(authed("GET", "/client/search?q=quarterly&mode=semantic"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(
        json["match_kind"], "keyword",
        "no vectors => keyword fallback"
    );
    assert_eq!(json["items"].as_array().unwrap().len(), 1);
}

/// With an embedder attached, the default mode is hybrid, semantic/hybrid run,
/// and sealed mail is STILL excluded from every mode.
#[tokio::test]
async fn search_modes_with_embedder_and_sealed_excluded() {
    use squelch_core::embed::StubEmbedder;

    // 384-dim to match the vec0 table.
    let store = SqliteStore::open_in_memory()
        .unwrap()
        .with_embedder(Arc::new(StubEmbedder::new(384)))
        .unwrap();
    let acct = store.ensure_account("me@example.com").unwrap();
    let store = Arc::new(store);

    // Normal message + its vector.
    let n = store
        .upsert_message(&msg(
            acct,
            "g1",
            "t1",
            "acme invoice for services",
            "please pay",
        ))
        .unwrap();
    store
        .set_triage(
            n,
            acct,
            60,
            Tier::Signal,
            Sensitivity::Normal,
            None,
            "",
            "",
            None,
        )
        .unwrap();
    let v = store
        .embedder()
        .unwrap()
        .embed("acme invoice for services please pay")
        .unwrap();
    store.upsert_message_vector(acct, n, &v).unwrap();

    // Sealed OTP + (defensively) a leaked vector — must never surface.
    let s = store
        .upsert_message(&msg(acct, "g2", "t2", "acme verification code", "123456"))
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
    let sv = store
        .embedder()
        .unwrap()
        .embed("acme verification code 123456")
        .unwrap();
    store.upsert_message_vector(acct, s, &sv).unwrap();

    let state = ApiState::new(store.clone(), acct, TOKEN);
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
    assert!(
        items.iter().all(|i| i["thread_id"] != "t2"),
        "sealed never surfaces"
    );
    assert!(items.iter().any(|i| i["thread_id"] == "t1"));

    // Explicit semantic => runs semantic, sealed still absent.
    let resp = app
        .oneshot(authed("GET", "/client/search?q=acme&mode=semantic"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["match_kind"], "semantic");
    let items = json["items"].as_array().unwrap();
    assert!(
        items.iter().all(|i| i["thread_id"] != "t2"),
        "sealed never surfaces in semantic"
    );
}

// --- search: match-window snippets and query operators ----------------------

/// A body whose only occurrence of "pangolin" sits well past anything the
/// stored head-of-message snippet would ever cover.
const DEEP_BODY: &str = "Thanks for subscribing to the weekly digest. This week \
    we cover garden tools, the spring planting calendar, a reader letter about \
    compost, and our usual roundup of local events. Finally, a note on the \
    pangolin conservation fundraiser at the community hall.";

/// Midday UTC on the given day, so a `after:`/`before:` bound at midnight lands
/// unambiguously on one side of it.
fn at(y: i32, m: u32, d: u32) -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    chrono::Utc
        .with_ymd_and_hms(y, m, d, 12, 0, 0)
        .single()
        .unwrap()
}

#[tokio::test]
async fn search_snippet_is_the_body_match_window() {
    let Harness { app, .. } = harness(|store, acct| {
        let mut m = msg(acct, "g1", "t1", "weekly digest", DEEP_BODY);
        m.snippet = "Thanks for subscribing to the weekly digest.".into();
        let id = store.upsert_message(&m).unwrap();
        store
            .set_triage(
                id,
                acct,
                40,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
    });

    let resp = app
        .oneshot(authed("GET", "/client/search?q=pangolin"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let snippet = items[0]["snippet"].as_str().unwrap();
    assert!(
        snippet.contains("pangolin"),
        "a body-deep hit must show the matched term, got {snippet:?}"
    );
    assert!(
        !snippet.starts_with("Thanks for subscribing"),
        "not the stored intro paragraph"
    );
}

/// Two dated invoices from Jane, one from Bob, one sealed and one sent — the
/// corpus every operator test below filters over.
fn seed_operator_corpus(store: &SqliteStore, acct: i64) {
    // The shared fixture is from Alice; these four are from Jane, so `from:jane`
    // means something.
    let from_jane = |m: &mut squelch_core::types::NewMessage| {
        m.from_addr = "jane@example.com".into();
        m.from_name = Some("Jane Doe".into());
    };

    let mut jan = msg(acct, "g-jan", "t-jan", "invoice january", "january invoice");
    from_jane(&mut jan);
    jan.received_at = at(2026, 1, 15);
    let mut feb = msg(
        acct,
        "g-feb",
        "t-feb",
        "invoice february",
        "february invoice",
    );
    from_jane(&mut feb);
    feb.received_at = at(2026, 2, 15);

    let mut bob = msg(acct, "g-bob", "t-bob", "invoice from bob", "bob's invoice");
    bob.from_addr = "bob@other.test".into();
    bob.from_name = Some("Bob".into());
    bob.received_at = at(2026, 2, 20);

    for m in [&jan, &feb, &bob] {
        let id = store.upsert_message(m).unwrap();
        store
            .set_triage(
                id,
                acct,
                50,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
    }

    // Sealed: same sender, same word, must never appear on any leg.
    let mut sealed = msg(
        acct,
        "g-seal",
        "t-seal",
        "invoice code inside",
        "code 123456",
    );
    from_jane(&mut sealed);
    sealed.received_at = at(2026, 2, 16);
    let sid = store.upsert_message(&sealed).unwrap();
    store
        .set_triage(
            sid,
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

    // Sent: search is the human door's INBOX view, so this stays out too.
    let mut sent = msg(
        acct,
        "g-sent",
        "t-sent",
        "re: invoice",
        "sending the invoice back",
    );
    from_jane(&mut sent);
    sent.is_sent = true;
    sent.received_at = at(2026, 2, 17);
    store.upsert_message(&sent).unwrap();
}

#[tokio::test]
async fn search_operators_filter_by_sender_and_date() {
    let Harness { app, .. } = harness(seed_operator_corpus);

    let threads = |json: &Value| -> Vec<String> {
        json["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["thread_id"].as_str().unwrap().to_string())
            .collect()
    };

    // from: matches the address or the display name; the sealed row carrying the
    // same sender and the same word stays absent.
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/search?q=invoice%20from:jane"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["match_kind"], "keyword");
    let t = threads(&json);
    assert_eq!(t.len(), 2, "jane's two received invoices: {t:?}");
    assert!(!t.contains(&"t-seal".to_string()), "sealed stays absent");
    assert!(!t.contains(&"t-sent".to_string()), "sent stays excluded");

    // after: is inclusive at midnight UTC of the named day.
    let json = body_json(
        app.clone()
            .oneshot(authed("GET", "/client/search?q=invoice%20after:2026-02-01"))
            .await
            .unwrap(),
    )
    .await;
    let t = threads(&json);
    assert_eq!(t.len(), 2, "february onward: {t:?}");
    assert!(!t.contains(&"t-jan".to_string()));

    // before: is exclusive at midnight UTC of the named day.
    let json = body_json(
        app.clone()
            .oneshot(authed(
                "GET",
                "/client/search?q=invoice%20before:2026-02-16",
            ))
            .await
            .unwrap(),
    )
    .await;
    // Keyword hits come back in FTS rank order, so assert the SET, not the order.
    let t = threads(&json);
    assert_eq!(t.len(), 2, "january and february only: {t:?}");
    assert!(!t.contains(&"t-bob".to_string()), "bob is past the bound");

    // All three together narrow to exactly one message.
    let json = body_json(
        app.clone()
            .oneshot(authed(
                "GET",
                "/client/search?q=invoice%20from:jane%20after:2026-02-01%20before:2026-03-01",
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(threads(&json), vec!["t-feb".to_string()]);

    // An unparseable date is not an operator: the token stays in the search
    // text, and the request still succeeds.
    let resp = app
        .oneshot(authed("GET", "/client/search?q=invoice%20after:soon"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn search_with_only_operators_lists_and_keeps_exclusions() {
    let Harness { app, .. } = harness(seed_operator_corpus);

    // No words left after parsing => a plain newest-first listing, no FTS MATCH,
    // reported as the keyword kind.
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/search?q=from:jane"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["match_kind"], "keyword");
    let t: Vec<&str> = json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["thread_id"].as_str().unwrap())
        .collect();
    assert_eq!(t, vec!["t-feb", "t-jan"], "newest first");
    assert!(!t.contains(&"t-seal"), "sealed absent from the listing");
    assert!(!t.contains(&"t-sent"), "sent absent from the listing");

    // A date-only listing spans every non-sealed, non-sent row.
    let json = body_json(
        app.clone()
            .oneshot(authed("GET", "/client/search?q=after:2026-01-01"))
            .await
            .unwrap(),
    )
    .await;
    let t: Vec<&str> = json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["thread_id"].as_str().unwrap())
        .collect();
    assert_eq!(t, vec!["t-bob", "t-feb", "t-jan"]);

    // Operators that constrain nothing leave nothing to search: still a 400.
    let resp = app
        .oneshot(authed("GET", "/client/search?q=from:"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reveal_writes_audit_and_returns_body() {
    let Harness { app, store, acct } = harness(|store, acct| {
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

    let sealed_id = store.sealed_messages(acct).unwrap()[0].id;

    let resp = app
        .oneshot(authed(
            "POST",
            &format!("/client/sealed/{sealed_id}/reveal"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let json = body_json(resp).await;
    assert_eq!(json["body"], "your code is 987654");

    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "reveal_sealed");
    assert_eq!(
        audit[0].target.as_deref(),
        Some(sealed_id.to_string().as_str())
    );
}

#[tokio::test]
async fn sealed_list_has_no_bodies() {
    let Harness { app, .. } = harness(|store, acct| {
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
    assert!(
        items[0].get("body").is_none(),
        "no body field in sealed list"
    );
    assert_eq!(items[0]["kind"], "otp");
}

#[tokio::test]
async fn sent_listing_pages_the_outbox_and_hides_sealed_and_received_mail() {
    // The human door's only `is_sent = 1` listing: the user's own outbox, with
    // recipients and read receipts, newest first.
    let Harness { app, .. } = harness(|store, acct| {
        let seed = |gmail: &str, thread: &str, subject: &str, to: &str, sensitivity| {
            let id = store
                .upsert_message(&sent_msg(acct, gmail, thread, subject, to))
                .unwrap();
            store
                .set_triage(id, acct, 0, Tier::Noise, sensitivity, None, "", "", None)
                .unwrap();
            id
        };
        seed(
            "s1",
            "ts1",
            "first",
            "Alice <alice@friends.com>",
            Sensitivity::Normal,
        );
        seed(
            "s2",
            "ts2",
            "second",
            "bob@friends.com",
            Sensitivity::Normal,
        );
        // A sealed outbound copy and ordinary inbound mail: neither is listed.
        seed(
            "s3",
            "ts3",
            "sealed",
            "support@bank.com",
            Sensitivity::Sealed,
        );
        store
            .upsert_message(&msg(acct, "g-in", "t-in", "inbound", "hi"))
            .unwrap();
    });

    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/sent?limit=1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    // Newest first: each fixture stamps its own `now`, so "second" is later.
    assert_eq!(items[0]["subject"], "second");
    assert_eq!(items[0]["to"], "bob@friends.com");
    assert_eq!(items[0]["opens"], 0);
    assert!(items[0]["sent_at"].as_str().unwrap().contains('T'));
    assert!(items[0]["thread_id"].as_str().is_some());

    // The cursor pages to the older message, and no sealed or inbound row can
    // appear on any page.
    let cursor = json["next_cursor"]
        .as_str()
        .expect("next_cursor")
        .to_string();
    let resp2 = app
        .oneshot(authed(
            "GET",
            &format!("/client/sent?limit=1&cursor={cursor}"),
        ))
        .await
        .unwrap();
    let json2 = body_json(resp2).await;
    let items2 = json2["items"].as_array().unwrap();
    assert_eq!(items2.len(), 1);
    assert_eq!(items2[0]["subject"], "first");
    assert_eq!(items2[0]["to"], "Alice <alice@friends.com>");
}

#[tokio::test]
async fn sent_listing_needs_the_bearer() {
    let Harness { app, .. } = harness(|_, _| {});
    let req = Request::builder()
        .uri("/client/sent")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn pagination_cursor_round_trip() {
    let Harness { app, .. } = harness(|store, acct| {
        // 3 normal signal messages so limit=2 yields a next_cursor.
        for i in 0..3 {
            let g = format!("g{i}");
            let t = format!("t{i}");
            let m = store
                .upsert_message(&msg(acct, &g, &t, "update", "body"))
                .unwrap();
            store
                .set_triage(
                    m,
                    acct,
                    80,
                    Tier::Signal,
                    Sensitivity::Normal,
                    None,
                    "",
                    "",
                    None,
                )
                .unwrap();
        }
    });

    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/updates?limit=2"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["items"].as_array().unwrap().len(), 2);
    let cursor = json["next_cursor"]
        .as_str()
        .expect("next_cursor present")
        .to_string();

    // Second page via the cursor.
    let resp2 = app
        .oneshot(authed(
            "GET",
            &format!("/client/updates?limit=2&cursor={cursor}"),
        ))
        .await
        .unwrap();
    let json2 = body_json(resp2).await;
    assert_eq!(json2["items"].as_array().unwrap().len(), 1);
    assert!(json2.get("next_cursor").is_none() || json2["next_cursor"].is_null());
}

#[tokio::test]
async fn bad_cursor_is_400() {
    let Harness { app, .. } = harness(|_, _| {});
    let resp = app
        .oneshot(authed("GET", "/client/updates?cursor=@@notbase64@@"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn action_requires_confirm() {
    // The confirm gate fires before anything else.
    let Harness { app, .. } = harness(|_, _| {});
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
    // Confirmed, but no write credential configured => 403 with a hint.
    let Harness { app, .. } = harness(|store, acct| {
        let m = store
            .upsert_message(&msg(acct, "g1", "t1", "hi", "body"))
            .unwrap();
        store
            .set_triage(
                m,
                acct,
                80,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
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
    let Harness { app, store, acct } = harness(|_, _| {});
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
    assert!(
        !err.contains("483920"),
        "must NEVER echo the matched secret"
    );

    // A blocked send still audits.
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "send");
    assert_eq!(audit[0].actor, "client-api");
    assert_eq!(audit[0].detail.as_deref(), Some("blocked:guard"));
}

#[tokio::test]
async fn send_guard_override_passes_guard_then_403_no_creds() {
    // override_guard bypasses the guard, then the 403 gate stops it. Two audit
    // rows: the override note and the no-credential rejection.
    let Harness { app, store, acct } = harness(|_, _| {});
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
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.detail.as_deref() == Some("rejected:no_write_credential"))
    );
    assert!(audit.iter().any(|a| {
        a.detail
            .as_deref()
            .is_some_and(|d| d.starts_with("guard_override:"))
    }));
}

#[tokio::test]
async fn clean_send_passes_guard() {
    // A clean body clears the guard; the 403 for missing creds proves it did
    // not block.
    let Harness { app, .. } = harness(|_, _| {});
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

/// Serve `n` sequential requests, each answered `200 {}`, returning the raw
/// request bytes.
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

/// [`mock_gmail`]'s scripted sibling: serve one `(status, body)` per entry, in
/// order. The send path's calls differ in what they must return (ids, then a raw
/// message), and the echo's failure mode is a status, so both are scripted.
async fn mock_gmail_seq(
    responses: Vec<(u16, String)>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut reqs = Vec::new();
        for (status, body) in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let m = sock.read(&mut buf).await.unwrap();
            reqs.push(String::from_utf8_lossy(&buf[..m]).to_string());
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
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

/// The default harness plus a write credential pointed at a mock Gmail `base`.
fn app_with_writes(base: String, seed: impl FnOnce(&SqliteStore, i64)) -> Harness {
    let (state, store, acct) = common::state_with(seed);
    let state = state.with_write_test_harness(Arc::new(StubCreds), base);
    Harness {
        app: router(state),
        store,
        acct,
    }
}

#[tokio::test]
async fn archive_success_audits_ok_and_hits_gmail() {
    let (base, handle) = mock_gmail(1).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        let m = store
            .upsert_message(&msg(acct, "gmail-abc", "t1", "hi", "body"))
            .unwrap();
        store
            .set_triage(
                m,
                acct,
                80,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
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
    // A sealed message is invisible to actions: 404, and no Gmail call at all —
    // the write path can never touch sealed mail.
    let (base, handle) = mock_gmail(0).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
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
    handle.abort();
    // The attempted action is still audited.
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit[0].detail.as_deref(), Some("failed:target"));
}

#[tokio::test]
async fn reply_send_success_threads_and_audits_ok() {
    // Reply flow makes two Gmail calls: parent_headers GET, then send POST.
    let (base, handle) = mock_gmail(2).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        let m = store
            .upsert_message(&msg(
                acct,
                "gmail-parent",
                "thread-77",
                "Lunch?",
                "want lunch?",
            ))
            .unwrap();
        store
            .set_triage(
                m,
                acct,
                80,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
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
    assert!(
        reqs[0].starts_with("GET "),
        "first call reads parent headers"
    );
    assert!(reqs[0].contains("/messages/gmail-parent"));
    assert!(reqs[1].contains("/messages/send"));
    // Threaded onto the parent's Gmail thread.
    assert!(reqs[1].contains("\"threadId\":\"thread-77\""));

    // `mock_gmail` answers `{}`, so there is no sent id to echo back.
    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send" && a.detail.as_deref() == Some("ok"))
    );
}

/// The RFC822 Gmail hands back for the echoed reply, base64url-encoded the way
/// `format=raw` returns it.
fn sent_reply_raw_b64() -> String {
    use base64::Engine as _;
    let eml = "From: Me <me@example.com>\r\n\
               To: Alice <alice@example.com>\r\n\
               Subject: Re: Lunch?\r\n\
               Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
               Content-Type: text/plain; charset=\"UTF-8\"\r\n\
               \r\n\
               yes, noon works\r\n";
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(eml)
}

#[tokio::test]
async fn reply_send_echoes_the_sent_message_into_the_thread() {
    // Three Gmail calls: parent headers, the send, then the echo's raw read-back.
    let (base, handle) = mock_gmail_seq(vec![
        (200, "{}".to_string()),
        (
            200,
            "{\"id\":\"sent-1\",\"threadId\":\"thread-77\"}".to_string(),
        ),
        (
            200,
            format!(
                "{{\"id\":\"sent-1\",\"threadId\":\"thread-77\",\
                   \"internalDate\":\"1783591200000\",\"raw\":\"{}\"}}",
                sent_reply_raw_b64()
            ),
        ),
    ])
    .await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        let m = store
            .upsert_message(&msg(
                acct,
                "gmail-parent",
                "thread-77",
                "Lunch?",
                "want lunch?",
            ))
            .unwrap();
        store
            .set_triage(
                m,
                acct,
                80,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
    });
    let message_id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let resp = app
        .clone()
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
    let json = body_json(resp).await;
    assert_eq!(json["status"], "sent");
    assert_eq!(json["thread_id"], "thread-77");
    let echo_id = json["echo_message_id"].as_i64().expect("echoed local id");

    let reqs = handle.await.unwrap();
    assert_eq!(reqs.len(), 3);
    assert!(
        reqs[2].starts_with("GET "),
        "the echo reads the sent message back"
    );
    assert!(reqs[2].contains("/messages/sent-1?format=raw"));

    // The thread view carries the reply immediately, no Sent poll needed.
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/thread/thread-77"))
        .await
        .unwrap();
    let thread = body_json(resp).await;
    let msgs = thread["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "parent + echoed reply");
    assert!(
        msgs.iter().any(|m| m["id"].as_i64() == Some(echo_id)
            && m["content"].as_str().unwrap().contains("noon works")),
        "the echoed reply is in the thread"
    );

    // ...and creates no attention noise: sent mail never reaches /client/updates.
    let resp = app.oneshot(authed("GET", "/client/updates")).await.unwrap();
    let updates = body_json(resp).await;
    assert!(
        updates["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|u| u["id"].as_i64() != Some(echo_id)),
        "the echo is not an update"
    );

    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send" && a.detail.as_deref() == Some("ok"))
    );
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send.echo"
                && a.detail.as_deref() == Some(&*format!("ok:{echo_id}"))),
        "the echo audits its local id"
    );
}

#[tokio::test]
async fn send_with_no_echoed_id_skips_the_echo_and_still_succeeds() {
    // Cold send whose response carries no id: nothing to fetch back, so exactly
    // one Gmail call and a null echo id.
    let (base, handle) = mock_gmail_seq(vec![(200, "{}".to_string())]).await;
    let Harness { app, store, acct } = app_with_writes(base, |_, _| {});

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "subject": "Hi",
                "body": "see you Tuesday",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "sent");
    assert!(json["echo_message_id"].is_null());
    assert!(
        json["thread_id"].is_null(),
        "no parent and no echoed thread"
    );
    assert_eq!(handle.await.unwrap().len(), 1);

    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send.echo" && a.detail.as_deref() == Some("skipped:no_id"))
    );
}

#[tokio::test]
async fn echo_fetch_failure_never_fails_the_send() {
    // The mail is away; a 500 on the read-back costs the local echo, nothing else.
    let (base, handle) = mock_gmail_seq(vec![
        (
            200,
            "{\"id\":\"sent-1\",\"threadId\":\"thread-new\"}".to_string(),
        ),
        (500, "{\"error\":\"backendError\"}".to_string()),
    ])
    .await;
    let Harness { app, store, acct } = app_with_writes(base, |_, _| {});

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "subject": "Hi",
                "body": "see you Tuesday",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a failed echo is not a failed send"
    );
    let json = body_json(resp).await;
    assert!(json["echo_message_id"].is_null());
    // A cold send still reports the thread Gmail created, so the client can open it.
    assert_eq!(json["thread_id"], "thread-new");
    assert_eq!(handle.await.unwrap().len(), 2);

    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send" && a.detail.as_deref() == Some("ok"))
    );
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send.echo" && a.detail.as_deref() == Some("failed:fetch"))
    );
}

/// The RFC822 for a sent reply whose body trips seal detection (a quoted OTP),
/// base64url-encoded the way `format=raw` returns it. The SEND body is innocuous —
/// what seals is the copy Gmail hands back, not what the client typed.
fn sealed_reply_raw_b64() -> String {
    use base64::Engine as _;
    let eml = "From: Me <me@example.com>\r\n\
               To: Support <support@bank.com>\r\n\
               Subject: Re: Your verification code\r\n\
               Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
               Content-Type: text/plain; charset=\"UTF-8\"\r\n\
               \r\n\
               I never asked for this. Your one-time passcode is 483920.\r\n";
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(eml)
}

#[tokio::test]
async fn echo_of_a_sealed_sent_message_is_skipped_and_the_thread_still_opens() {
    // The echoed copy seals. Committing it would put a sealed row in the thread,
    // and the thread view 404s any thread holding one — the reply would take the
    // counterparty's mail down with it. So: no echo, and the thread still opens.
    let (base, handle) = mock_gmail_seq(vec![
        (200, "{}".to_string()),
        (
            200,
            "{\"id\":\"sent-1\",\"threadId\":\"thread-77\"}".to_string(),
        ),
        (
            200,
            format!(
                "{{\"id\":\"sent-1\",\"threadId\":\"thread-77\",\
                   \"internalDate\":\"1783591200000\",\"raw\":\"{}\"}}",
                sealed_reply_raw_b64()
            ),
        ),
    ])
    .await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        let m = store
            .upsert_message(&msg(
                acct,
                "gmail-parent",
                "thread-77",
                "Lunch?",
                "want lunch?",
            ))
            .unwrap();
        store
            .set_triage(
                m,
                acct,
                80,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
    });
    let message_id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "reply_to_message_id": message_id,
                "body": "please look into this",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a skipped echo is not a failed send"
    );
    let json = body_json(resp).await;
    assert_eq!(json["status"], "sent");
    assert!(
        json["echo_message_id"].is_null(),
        "the sealed copy is not echoed"
    );
    assert_eq!(handle.await.unwrap().len(), 3);

    // THE POINT: the thread the user was reading still opens, with the parent alone.
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/thread/thread-77"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "the thread must not 404");
    let thread = body_json(resp).await;
    assert_eq!(thread["messages"].as_array().map(Vec::len), Some(1));
    // And nothing sealed was committed at all.
    assert!(store.sealed_messages(acct).unwrap().is_empty());

    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send" && a.detail.as_deref() == Some("ok"))
    );
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send.echo" && a.detail.as_deref() == Some("skipped:sealed"))
    );
}

#[tokio::test]
async fn echo_of_an_empty_raw_read_never_ingests() {
    // A `format=raw` 200 with no `raw` field decodes to zero bytes, which would
    // ingest as a message with neither headers nor body. It is a failed fetch.
    let (base, handle) = mock_gmail_seq(vec![
        (
            200,
            "{\"id\":\"sent-1\",\"threadId\":\"thread-new\"}".to_string(),
        ),
        (
            200,
            "{\"id\":\"sent-1\",\"threadId\":\"thread-new\"}".to_string(),
        ),
    ])
    .await;
    let Harness { app, store, acct } = app_with_writes(base, |_, _| {});

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "subject": "Hi",
                "body": "see you Tuesday",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["echo_message_id"].is_null());
    assert_eq!(handle.await.unwrap().len(), 2);

    // Nothing was written: no local message, sent or otherwise.
    let stats = store
        .stats(acct, chrono::Utc::now() - chrono::Duration::days(30))
        .unwrap();
    assert_eq!(
        (stats.total, stats.sealed),
        (0, 0),
        "empty bytes must not become a message row"
    );
    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send.echo" && a.detail.as_deref() == Some("failed:fetch"))
    );
}

// --- read tracking on the send path -----------------------------------------

/// A public base URL for the pixel; nothing is ever fetched from it in tests.
const TRACK_BASE: &str = "https://track.example.test";

/// [`app_with_writes`] plus a configured `[tracking] base_url`.
fn app_with_tracking(
    base: String,
    track_base: Option<&str>,
    seed: impl FnOnce(&SqliteStore, i64),
) -> Harness {
    let (state, store, acct) = common::state_with(seed);
    let state = state
        .with_write_test_harness(Arc::new(StubCreds), base)
        .with_tracking_base_url(track_base.map(str::to_string));
    Harness {
        app: router(state),
        store,
        acct,
    }
}

/// The RFC822 a captured `messages/send` request carried, decoded out of its
/// `raw` field.
fn sent_mime(req: &str) -> String {
    use base64::Engine as _;
    let start = req.find("\"raw\":\"").expect("send body carries raw") + 7;
    let end = start + req[start..].find('"').expect("raw is terminated");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&req[start..end])
        .expect("raw is base64url");
    String::from_utf8(bytes).expect("mime is utf-8")
}

/// The token out of the one pixel URL in a built message.
fn pixel_token(mime: &str) -> String {
    let marker = format!("<img src=\"{TRACK_BASE}/t/");
    let start = mime.find(&marker).expect("the html part carries a pixel") + marker.len();
    let end = start + mime[start..].find('"').expect("the src is terminated");
    mime[start..end].to_string()
}

#[tokio::test]
async fn include_tracker_mints_a_pixel_the_store_can_resolve() {
    // Cold send: the send POST, then the echo's raw read-back.
    let (base, handle) = mock_gmail_seq(vec![
        (
            200,
            "{\"id\":\"sent-1\",\"threadId\":\"thread-new\"}".to_string(),
        ),
        (
            200,
            format!(
                "{{\"id\":\"sent-1\",\"threadId\":\"thread-new\",\
                   \"internalDate\":\"1783591200000\",\"raw\":\"{}\"}}",
                sent_reply_raw_b64()
            ),
        ),
    ])
    .await;
    let Harness { app, store, acct } = app_with_tracking(base, Some(TRACK_BASE), |_, _| {});

    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "subject": "Hi",
                "body": "see you Tuesday",
                "confirm": true,
                "include_tracker": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let echo_id = body_json(resp).await["echo_message_id"]
        .as_i64()
        .expect("echoed local id");

    let reqs = handle.await.unwrap();
    let mime = sent_mime(&reqs[0]);
    // The pixel rides the HTML alternative only; the plain part stays clean.
    assert!(mime.contains("multipart/alternative"));
    let token = pixel_token(&mime);
    assert_eq!(token.len(), 32, "32 url-safe chars");
    assert_eq!(
        mime.matches("<img").count(),
        1,
        "exactly one pixel, in the html part"
    );

    // The tracker is REAL: an open against its token records, and the echo
    // backfill makes it readable per message.
    assert!(
        store
            .record_open(acct, &token, 1_700, Some("Apple Mail/16.0"), "unknown")
            .unwrap(),
        "the minted token names a stored tracker"
    );
    let opens = store.message_opens(acct, echo_id).unwrap();
    assert_eq!(opens.len(), 1);
    assert_eq!(opens[0].opened_at, 1_700);

    let audit = store.list_audit(acct, 20).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send.tracker" && a.detail.as_deref() == Some("minted"))
    );
    // The audit trail must never carry the capability itself.
    assert!(
        !audit
            .iter()
            .any(|a| a.detail.as_deref().is_some_and(|d| d.contains(&token))),
        "the token is never audited"
    );
}

#[tokio::test]
async fn include_tracker_without_a_configured_base_url_sends_untracked() {
    let (base, handle) = mock_gmail_seq(vec![
        (
            200,
            "{\"id\":\"sent-1\",\"threadId\":\"thread-new\"}".to_string(),
        ),
        (200, "{}".to_string()),
    ])
    .await;
    let Harness { app, store, acct } = app_with_tracking(base, None, |_, _| {});

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "subject": "Hi",
                "body": "see you Tuesday",
                "confirm": true,
                "include_tracker": true
            }),
        ))
        .await
        .unwrap();
    // Unconfigured is not an error: the mail goes out, just unwatched.
    assert_eq!(resp.status(), StatusCode::OK);

    let reqs = handle.await.unwrap();
    let mime = sent_mime(&reqs[0]);
    assert!(!mime.contains("<img"), "no pixel without a base url");
    assert!(
        mime.contains("Content-Type: text/plain"),
        "and no html part was invented to carry one"
    );
    assert!(!mime.contains("multipart/alternative"));
    assert!(
        !store
            .list_audit(acct, 20)
            .unwrap()
            .iter()
            .any(|a| a.action == "send.tracker"),
        "nothing was minted, so nothing is audited"
    );
}

/// The tracker row is written BEFORE the send, so a token can never reach the
/// wire unrecorded. The cost of that ordering is an orphan whenever the send
/// fails — a row nothing will ever link to a message, still accepting opens for
/// the life of the database. The failure path has to take it back.
#[tokio::test]
async fn a_failed_send_leaves_no_tracker_behind() {
    let (base, handle) = mock_gmail_seq(vec![(500, "{\"error\":\"backend\"}".to_string())]).await;
    let Harness { app, store, acct } = app_with_tracking(base, Some(TRACK_BASE), |_, _| {});

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "subject": "Hi",
                "body": "see you Tuesday",
                "confirm": true,
                "include_tracker": true
            }),
        ))
        .await
        .unwrap();
    assert!(!resp.status().is_success());

    // The token that WAS minted, read back off the wire the recipient never saw.
    let reqs = handle.await.unwrap();
    let token = pixel_token(&sent_mime(&reqs[0]));
    assert!(
        !store
            .record_open(
                acct,
                &token,
                chrono::Utc::now().timestamp(),
                None,
                "unknown"
            )
            .unwrap(),
        "the tracker outlived its failed send"
    );

    let audit = store.list_audit(acct, 20).unwrap();
    let details = |action: &str| -> Vec<String> {
        audit
            .iter()
            .filter(|a| a.action == action)
            .filter_map(|a| a.detail.clone())
            .collect()
    };
    assert!(details("send.tracker").contains(&"minted".to_string()));
    assert!(details("send.tracker").contains(&"discarded".to_string()));
    assert!(details("send").contains(&"failed:gmail".to_string()));
}

#[tokio::test]
async fn a_send_that_does_not_ask_for_tracking_never_gets_a_pixel() {
    let (base, handle) = mock_gmail_seq(vec![
        (
            200,
            "{\"id\":\"sent-1\",\"threadId\":\"thread-new\"}".to_string(),
        ),
        (200, "{}".to_string()),
    ])
    .await;
    // Tracking IS configured; the send simply does not ask for it.
    let Harness { app, store, acct } = app_with_tracking(base, Some(TRACK_BASE), |_, _| {});

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "subject": "Hi",
                "body": "see you Tuesday",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let reqs = handle.await.unwrap();
    let mime = sent_mime(&reqs[0]);
    assert!(!mime.contains("<img"));
    assert!(!mime.contains(TRACK_BASE));
    assert!(!mime.contains("multipart/alternative"));
    assert!(
        !store
            .list_audit(acct, 20)
            .unwrap()
            .iter()
            .any(|a| a.action == "send.tracker")
    );
}

// --- reply-all: recipient preview + the Cc on the wire ----------------------

/// A `format=metadata` response whose headers are a group thread: Alice wrote
/// to the account, Bob and Carol, and Cc'd Dave. Display names carry commas and
/// a CRLF injection attempt, because that is what the derivation has to survive.
fn group_metadata_body() -> String {
    serde_json::json!({
        "id": "gmail-parent",
        "threadId": "thread-77",
        "payload": { "headers": [
            { "name": "Message-ID", "value": "<parent@x>" },
            { "name": "References", "value": "<root@x>" },
            { "name": "From", "value": "\"Doe, Alice\" <alice@example.com>" },
            // The account is listed in a DIFFERENT case than the accounts row,
            // and one display name tries to smuggle a header through a CRLF.
            { "name": "To", "value":
                "Me <ME@Example.com>, \"evil\r\nBcc: attacker@evil.test\" <bob@example.com>" },
            { "name": "Cc", "value": "Carol <carol@example.com>, Dave <dave@example.com>" },
        ]}
    })
    .to_string()
}

/// A parent whose Reply-To routes replies somewhere other than its From — a
/// mailing list, the common case Reply-To exists for at all.
fn reply_to_metadata_body() -> String {
    serde_json::json!({
        "id": "gmail-parent",
        "threadId": "thread-77",
        "payload": { "headers": [
            { "name": "Message-ID", "value": "<parent@x>" },
            { "name": "From", "value": "Alice <alice@example.com>" },
            { "name": "Reply-To", "value": "list@example.com" },
            { "name": "To", "value": "me@example.com" },
        ]}
    })
    .to_string()
}

#[tokio::test]
async fn a_plain_reply_previews_and_sends_the_same_reply_to_address() {
    // The bug this pins: the preview honored Reply-To while the send used the
    // stored From, so a user reviewed a list address and mailed an individual.
    // Both legs now derive, so they agree. One fetch each.
    let (pbase, phandle) = mock_gmail_seq(vec![(200, reply_to_metadata_body())]).await;
    let Harness { app, store, acct } = app_with_writes(pbase, |store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let preview = app
        .oneshot(authed(
            "GET",
            &format!("/client/messages/{id}/reply_recipients"),
        ))
        .await
        .unwrap();
    let json = body_json(preview).await;
    assert_eq!(json["to"], "list@example.com", "preview honors Reply-To");
    phandle.await.unwrap();

    let (sbase, shandle) = mock_gmail_seq(vec![
        (200, reply_to_metadata_body()),
        (200, "{}".to_string()),
    ])
    .await;
    let Harness { app, store, acct } = app_with_writes(sbase, |store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "reply_to_message_id": id,
                "body": "noon works",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mime = sent_mime(&shandle.await.unwrap()[1]);
    assert!(
        mime.contains("To: list@example.com\r\n"),
        "the send honors Reply-To too: {mime}"
    );
}

#[tokio::test]
async fn reply_recipients_preview_answers_sender_only_by_default() {
    let (base, handle) = mock_gmail_seq(vec![(200, group_metadata_body())]).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed(
            "GET",
            &format!("/client/messages/{id}/reply_recipients"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["to"], "alice@example.com");
    // No cc at all on a plain reply — the field is omitted, not empty.
    assert!(json.get("cc").is_none(), "{json}");

    let reqs = handle.await.unwrap();
    assert_eq!(
        reqs.len(),
        1,
        "the preview costs exactly one metadata fetch"
    );
    assert!(reqs[0].starts_with("GET "));
    assert!(reqs[0].contains("format=metadata"));
}

#[tokio::test]
async fn reply_recipients_preview_with_all_lists_the_room_minus_the_account() {
    let (base, handle) = mock_gmail_seq(vec![(200, group_metadata_body())]).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed(
            "GET",
            &format!("/client/messages/{id}/reply_recipients?all=true"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    handle.await.unwrap();
    assert_eq!(json["to"], "alice@example.com");
    // Bare addresses only, the account itself dropped, To before Cc.
    assert_eq!(
        json["cc"],
        "bob@example.com, carol@example.com, dave@example.com"
    );
    let wire = json.to_string();
    assert!(!wire.contains("evil.test"), "{wire}");
    assert!(!wire.contains("Doe"), "display names never survive: {wire}");
}

#[tokio::test]
async fn reply_recipients_preview_404s_a_sealed_message() {
    // Sealed and unknown are the same 404, and neither reaches Gmail.
    let (base, handle) = mock_gmail(0).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
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

    for uri in [
        format!("/client/messages/{sealed_id}/reply_recipients"),
        format!("/client/messages/{sealed_id}/reply_recipients?all=true"),
        "/client/messages/999999/reply_recipients".to_string(),
    ] {
        let resp = app.clone().oneshot(authed("GET", &uri)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    handle.abort();
}

#[tokio::test]
async fn reply_recipients_preview_needs_the_bearer_and_a_write_credential() {
    // No write credential: a 403 the client can show, not a 500.
    let Harness { app, store, acct } = harness(|store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;
    let resp = app
        .clone()
        .oneshot(authed(
            "GET",
            &format!("/client/messages/{id}/reply_recipients"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // And the route is behind the bearer like everything else in the tree.
    let req = Request::builder()
        .uri(format!("/client/messages/{id}/reply_recipients"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reply_recipients_preview_surfaces_an_upstream_failure_as_502() {
    let (base, handle) = mock_gmail_seq(vec![(500, "{\"error\":\"backend\"}".to_string())]).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;
    let resp = app
        .oneshot(authed(
            "GET",
            &format!("/client/messages/{id}/reply_recipients"),
        ))
        .await
        .unwrap();
    handle.await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn reply_all_send_addresses_the_room_off_one_metadata_fetch() {
    let (base, handle) =
        mock_gmail_seq(vec![(200, group_metadata_body()), (200, "{}".to_string())]).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "reply_to_message_id": id,
                "body": "noon works",
                "reply_all": true,
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let reqs = handle.await.unwrap();
    // TWO calls, not three: the headers fetch that threads the reply is the same
    // one that derived its audience.
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].starts_with("GET ") && reqs[0].contains("format=metadata"));
    let mime = sent_mime(&reqs[1]);
    assert!(mime.contains("To: alice@example.com\r\n"), "{mime}");
    assert!(
        mime.contains("Cc: bob@example.com, carol@example.com, dave@example.com\r\n"),
        "{mime}"
    );
    // Threading still comes off the same fetch.
    assert!(mime.contains("In-Reply-To: <parent@x>\r\n"));
    assert!(mime.contains("References: <root@x> <parent@x>\r\n"));
    // Nothing the crafted display name carried reached the headers.
    let headers = mime.split("\r\n\r\n").next().unwrap();
    assert!(!headers.contains("Bcc"), "{headers}");
    assert!(!headers.contains("evil.test"), "{headers}");
    assert!(
        !headers.to_lowercase().contains("me@example.com"),
        "the account never copies itself: {headers}"
    );

    // The ledger records a reply-all AS one, with its recipient count (to +
    // cc = 1 + 3), so a wide send is never filed as a plain "ok".
    assert!(
        store
            .list_audit(acct, 20)
            .unwrap()
            .iter()
            .any(|a| a.action == "send" && a.detail.as_deref() == Some("ok:reply_all:4"))
    );
}

#[tokio::test]
async fn an_ordinary_reply_still_carries_no_cc() {
    let (base, handle) =
        mock_gmail_seq(vec![(200, group_metadata_body()), (200, "{}".to_string())]).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "reply_to_message_id": id,
                "body": "noon works",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let reqs = handle.await.unwrap();
    let mime = sent_mime(&reqs[1]);
    assert!(!mime.contains("Cc:"), "{mime}");
    // A plain reply now DERIVES too (so the preview route and the send agree),
    // but with no Reply-To in the fixture the derived To is just the From
    // address — same result, one recipient, no Cc.
    assert!(mime.contains("To: alice@example.com\r\n"), "{mime}");
}

#[tokio::test]
async fn an_explicit_to_wins_but_keeps_the_derived_copies() {
    let (base, handle) =
        mock_gmail_seq(vec![(200, group_metadata_body()), (200, "{}".to_string())]).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "reply_to_message_id": id,
                "to": "bob@example.com",
                "body": "noon works",
                "reply_all": true,
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mime = sent_mime(&handle.await.unwrap()[1]);
    assert!(mime.contains("To: bob@example.com\r\n"), "{mime}");
    // Bob was promoted out of the Cc rather than being written twice.
    assert!(
        mime.contains("Cc: carol@example.com, dave@example.com\r\n"),
        "{mime}"
    );
}

#[tokio::test]
async fn reply_all_without_a_parent_is_a_400() {
    let (base, handle) = mock_gmail(0).await;
    let Harness { app, store, acct } = app_with_writes(base, |_, _| {});
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "subject": "Hi",
                "body": "hello",
                "reply_all": true,
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    handle.abort();
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit[0].action, "send");
    assert_eq!(
        audit[0].detail.as_deref(),
        Some("rejected:reply_all_no_parent")
    );
}

/// Reply-all's whole point is the audience. If the headers it comes from cannot
/// be read, quietly sending to the sender alone would deliver to fewer people
/// than the user reviewed — so the send fails instead. (A plain reply keeps its
/// best-effort threading: see `reply_send_survives_a_headers_failure`.)
#[tokio::test]
async fn reply_all_refuses_to_send_when_the_parent_headers_cannot_be_read() {
    let (base, handle) = mock_gmail_seq(vec![(500, "{\"error\":\"backend\"}".to_string())]).await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "reply_to_message_id": id,
                "body": "noon works",
                "reply_all": true,
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    // The mock was scripted for ONE request: nothing was sent.
    let reqs = handle.await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].starts_with("GET "));

    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send" && a.detail.as_deref() == Some("failed:gmail"))
    );
    assert!(
        !audit
            .iter()
            .any(|a| a.action == "send" && a.detail.as_deref() == Some("ok"))
    );
}

/// The other half of the rule above: a PLAIN reply whose headers fetch fails
/// still goes out, unthreaded, to the sender the store already knows.
#[tokio::test]
async fn reply_send_survives_a_headers_failure() {
    let (base, handle) = mock_gmail_seq(vec![
        (500, "{\"error\":\"backend\"}".to_string()),
        (200, "{}".to_string()),
    ])
    .await;
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
        seed_one_signal(store, acct, "gmail-parent", "thread-77", "Lunch?");
    });
    let id = store.search(acct, "lunch", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "reply_to_message_id": id,
                "body": "noon works",
                "confirm": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mime = sent_mime(&handle.await.unwrap()[1]);
    assert!(mime.contains("To: alice@example.com\r\n"), "{mime}");
    assert!(!mime.contains("In-Reply-To:"), "{mime}");
    assert!(!mime.contains("Cc:"), "{mime}");
}

// --- sitrep: seen-ledger + bands + resolution over HTTP ---------------------

use squelch_core::types::AttentionStatus;

/// Seed one signal message and return its local id via search.
fn seed_one_signal(store: &SqliteStore, acct: i64, gmail: &str, thread: &str, subj: &str) -> i64 {
    let m = store
        .upsert_message(&msg(acct, gmail, thread, subj, "body"))
        .unwrap();
    store
        .set_triage(
            m,
            acct,
            80,
            Tier::Signal,
            Sensitivity::Normal,
            None,
            "",
            "",
            None,
        )
        .unwrap();
    m
}

#[tokio::test]
async fn updates_stamp_once_and_carry_prestamp_surfaced_at() {
    let Harness { app, store, acct } = harness(|store, acct| {
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

    // Stamped as a side effect of the fetch.
    let after = store
        .attention_updates(
            acct,
            chrono::Utc::now() - chrono::Duration::days(1),
            None,
            None,
            None,
        )
        .unwrap();
    let first_stamp = after[0].surfaced_at.expect("surfaced_at now set");
    assert_eq!(after[0].status, AttentionStatus::Open);

    // Second fetch: surfaced_at is now present and unchanged (stamp-once).
    let resp2 = app.oneshot(authed("GET", "/client/updates")).await.unwrap();
    let json2 = body_json(resp2).await;
    assert!(!json2["items"][0]["surfaced_at"].is_null());
    let after2 = store
        .attention_updates(
            acct,
            chrono::Utc::now() - chrono::Duration::days(1),
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        after2[0].surfaced_at,
        Some(first_stamp),
        "stamp did not move"
    );
}

#[tokio::test]
async fn peek_returns_the_same_rows_without_stamping_the_ledger() {
    let Harness { app, store, acct } = harness(|store, acct| {
        seed_one_signal(store, acct, "g1", "t1", "hi");
        seed_one_signal(store, acct, "g2", "t2", "there");
    });
    let window = || chrono::Utc::now() - chrono::Duration::days(1);

    // peek=true: the agent reads on the user's behalf. Same page...
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/updates?peek=true"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["items"].as_array().unwrap().len(), 2);

    // ...and the ledger is untouched: nothing was actually shown to anyone.
    let after_peek = store
        .attention_updates(acct, window(), None, None, None)
        .unwrap();
    assert_eq!(after_peek.len(), 2);
    for u in &after_peek {
        assert!(u.surfaced_at.is_none(), "peek must not stamp surfaced_at");
        assert_eq!(
            u.status,
            AttentionStatus::New,
            "peek must not promote status"
        );
    }

    // Control: the same request without peek stamps, so the skip is the param
    // and not a broken fixture.
    let resp2 = app.oneshot(authed("GET", "/client/updates")).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let after_normal = store
        .attention_updates(acct, window(), None, None, None)
        .unwrap();
    for u in &after_normal {
        assert!(u.surfaced_at.is_some(), "normal fetch stamps surfaced_at");
        assert_eq!(u.status, AttentionStatus::Open);
    }
}

#[tokio::test]
async fn updates_carry_field_reasons_object() {
    use squelch_core::types::FieldReasons;
    let Harness { app, .. } = harness(|store, acct| {
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
    assert_eq!(
        fr["importance"],
        Value::String("known contact -> signal importance 80".into())
    );
    assert_eq!(fr["tier"], Value::String("known contact -> signal".into()));
    assert!(
        fr.get("deadline").is_none(),
        "absent deadline reason must be omitted, not null"
    );
}

#[tokio::test]
async fn updates_without_reasons_omit_the_field_reasons_key() {
    // A row with no recorded reasons omits the key entirely — the desktop
    // treats absent the same as null.
    let Harness { app, .. } = harness(|store, acct| {
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
    let Harness { app, .. } = harness(|store, acct| {
        // A past_due bill (standing) plus a plain signal.
        let bill = store
            .upsert_message(&msg(acct, "g1", "t1", "PG&E past due", "pay"))
            .unwrap();
        store
            .set_triage(
                bill,
                acct,
                95,
                Tier::PastDue,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
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
    let Harness { app, .. } = harness(|_, _| {});
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
    let Harness { app, store, acct } = harness(|store, acct| {
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
        .attention_updates(
            acct,
            chrono::Utc::now() - chrono::Duration::days(1),
            None,
            Some(AttentionStatus::Done),
            None,
        )
        .unwrap();
    assert_eq!(done.len(), 1);
    assert!(done[0].resolved_at.is_some());

    // The dismiss is audited.
    let audit = store.list_audit(acct, 10).unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == "set_status" && a.detail.as_deref() == Some("done"))
    );

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
    let Harness { app, .. } = harness(|_, _| {});
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
    let Harness { app, store, acct } = harness(|store, acct| {
        let s = store
            .upsert_message(&msg(acct, "g1", "t1", "code", "123456"))
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
    let Harness { app, store, acct } = app_with_writes(base, |store, acct| {
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
        .attention_updates(
            acct,
            chrono::Utc::now() - chrono::Duration::days(1),
            None,
            Some(AttentionStatus::Done),
            None,
        )
        .unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].update.id, message_id);
    assert!(done[0].resolved_at.is_some());
}

#[tokio::test]
async fn update_rule_edits_in_place_and_404s_bogus() {
    let Harness { app, .. } = harness(|_, _| {});

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

/// A mock provider serving the same `status`/`body` to every request, returning
/// its URL. Serves repeatedly, not once: a single request can bill more than one
/// classifier call (create, then edit) and a spent one-shot would look like a
/// transport failure rather than a verdict.
async fn spawn_classifier_mock(status: u16, body: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16384];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    format!("http://{addr}")
}

/// A state whose rule inference points at `url`. The classifier is the only thing
/// this state can talk to.
fn harness_pointing_inference_at(url: String, seed: impl FnOnce(&SqliteStore, i64)) -> Harness {
    use squelch_core::config::Stage2Provider;
    use squelch_core::triage::rule_infer::RuleInferClient;

    let (state, store, acct) = common::state_with(seed);
    let state = state.with_rule_inference(Some(RuleInferClient::new(
        reqwest::Client::new(),
        url,
        "sk-test",
        Stage2Provider::Anthropic,
        "claude-haiku-4-5",
    )));
    Harness {
        app: router(state),
        store,
        acct,
    }
}

/// A one-shot mock of the Anthropic Messages API answering with `verdict`, plus
/// the state decorated to point rule inference at it. Usage on the mocked
/// response is 50 in / 5 out, which is what the ledger assertions expect.
async fn app_with_rule_inference(
    verdict: &'static str,
    seed: impl FnOnce(&SqliteStore, i64),
) -> Harness {
    let body = format!(
        r#"{{"content":[{{"type":"text","text":"{{\"disposition\":\"{verdict}\"}}"}}],"stop_reason":"end_turn","usage":{{"input_tokens":50,"output_tokens":5}}}}"#
    );
    let url = spawn_classifier_mock(200, body).await;
    harness_pointing_inference_at(url, seed)
}

/// The same wiring against a classifier that FAILS permanently (a 400).
async fn app_with_failing_rule_inference(seed: impl FnOnce(&SqliteStore, i64)) -> Harness {
    let body =
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"nope"}}"#.to_string();
    let url = spawn_classifier_mock(400, body).await;
    harness_pointing_inference_at(url, seed)
}

#[tokio::test]
async fn omitted_disposition_is_inferred_and_returned() {
    // No LLM configured on the default harness, so inference resolves to the
    // safe answer: filtered, stored and echoed back so the client can say so.
    let Harness { app, store, acct } = harness(|_, _| {});

    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "billing@acme.com",
                "want": "i only care about the invoices"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    assert_eq!(
        created["disposition"], "filtered",
        "the resolved disposition rides the response"
    );
    let rule_id = created["rule_id"].as_i64().unwrap();

    let rules = store.list_sender_rules(acct).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].id, rule_id);
    assert_eq!(
        rules[0].disposition,
        squelch_core::types::Disposition::Filtered
    );
    assert_eq!(rules[0].want_text, "i only care about the invoices");
}

#[tokio::test]
async fn literal_auto_disposition_infers_like_an_absent_one() {
    let Harness { app, store, acct } = harness(|_, _| {});

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "news@acme.com",
                "want": "the weekly digest is noise",
                "disposition": "auto"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["disposition"], "filtered");
    assert_eq!(
        store.list_sender_rules(acct).unwrap()[0].disposition,
        squelch_core::types::Disposition::Filtered
    );
}

#[tokio::test]
async fn only_an_absent_key_or_exact_auto_asks_for_inference() {
    // INFERENCE IS OPT-IN, EXACTLY. A present-but-unparseable disposition is a
    // client bug, and folding it into "infer it then" hands a model a decision
    // the client thought it had made. Empty string, wrong case, and garbage all
    // 400 for the same reason "SURFACE" does.
    let Harness { app, store, acct } = harness(|_, _| {});

    for bad in ["", "   ", "AUTO", "Auto", "SURFACE", " squelch", "block"] {
        let resp = app
            .clone()
            .oneshot(authed_json(
                "POST",
                "/client/rules",
                serde_json::json!({
                    "match_pattern": "x@acme.com",
                    "want": "never show me anything from them",
                    "disposition": bad
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "disposition {bad:?} must be a 400, not a silent inference"
        );
    }
    assert!(
        store.list_sender_rules(acct).unwrap().is_empty(),
        "nothing stored"
    );

    // The exact lowercase literal is the one spelling that infers.
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "x@acme.com",
                "want": "never show me anything from them",
                "disposition": "auto"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["disposition"], "filtered");
}

#[tokio::test]
async fn a_configured_classifier_that_fails_still_saves_the_rule() {
    // A CONFIGURED but broken classifier is the interesting case: "no LLM at all"
    // short-circuits before the network. The save must still land, filtered.
    let Harness { app, store, acct } = app_with_failing_rule_inference(|_, _| {}).await;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "billing@acme.com",
                "want": "never show me anything from them"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "inference never fails a save"
    );
    assert_eq!(body_json(resp).await["disposition"], "filtered");
    assert_eq!(
        store.list_sender_rules(acct).unwrap()[0].disposition,
        squelch_core::types::Disposition::Filtered
    );
    // A call that produced no verdict produced no billable usage either.
    assert!(
        !store
            .list_usage_by_category(acct, 30)
            .unwrap()
            .iter()
            .any(|(c, _)| c == "rule_infer"),
        "a failed call writes no ledger row"
    );
}

#[tokio::test]
async fn rule_inference_spend_lands_in_its_own_ledger_category() {
    // UNMETERED SPEND IS THE BUG: this is a model call on an interactive path, so
    // it has to show up in the ledger like every other one. Create and update
    // both bill it.
    let Harness { app, store, acct } = app_with_rule_inference("filtered", |_, _| {}).await;

    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "news@acme.com",
                "want": "the weekly digest is noise"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let rule_id = body_json(resp).await["rule_id"].as_i64().unwrap();

    let row = |store: &SqliteStore| {
        store
            .list_usage_by_category(acct, 30)
            .unwrap()
            .into_iter()
            .find(|(c, _)| c == "rule_infer")
            .map(|(_, days)| days)
    };
    let days = row(&store).expect("the create call is billed under rule_infer");
    assert_eq!(days.len(), 1, "one day of spend");
    assert_eq!(days[0].calls, 1);
    assert_eq!(days[0].input_tokens, 50);
    assert_eq!(days[0].output_tokens, 5);

    // The edit re-infers, so it bills a second call onto the same line.
    let resp = app
        .oneshot(authed_json(
            "PUT",
            &format!("/client/rules/{rule_id}"),
            serde_json::json!({
                "match_pattern": "news@acme.com",
                "want": "actually just the billing ones"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let days = row(&store).expect("the update path bills too");
    assert_eq!(
        days[0].calls, 2,
        "create and update each bill their own call"
    );
    assert_eq!(days[0].input_tokens, 100);
    assert_eq!(days[0].output_tokens, 10);

    // And it stays its OWN category: never folded into stage1/stage2 spend.
    let cats: Vec<String> = store
        .list_usage_by_category(acct, 30)
        .unwrap()
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    assert_eq!(cats, vec!["rule_infer".to_string()]);
}

#[tokio::test]
async fn explicit_disposition_is_honored_verbatim_and_echoed() {
    // Inference never runs when the client names a disposition: "surface" with a
    // conditional-sounding want stays surface.
    let Harness { app, store, acct } = harness(|_, _| {});

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "boss@acme.com",
                "want": "only the ones about the launch",
                "disposition": "surface"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["disposition"], "surface");
    assert_eq!(
        store.list_sender_rules(acct).unwrap()[0].disposition,
        squelch_core::types::Disposition::Surface
    );
}

#[tokio::test]
async fn omitted_disposition_with_empty_want_is_400() {
    // There is nothing to infer FROM, and a rule that says nothing is not a rule.
    let Harness { app, store, acct } = harness(|_, _| {});

    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({ "match_pattern": "x@acme.com", "want": "   " }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        store.list_sender_rules(acct).unwrap().is_empty(),
        "nothing stored"
    );

    // Same on the edit path.
    let resp = app
        .oneshot(authed_json(
            "PUT",
            "/client/rules/1",
            serde_json::json!({ "match_pattern": "x@acme.com", "want": "" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn update_reinfers_from_the_submitted_want_when_disposition_is_omitted() {
    let Harness { app, store, acct } = harness(|_, _| {});

    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "a@acme.com",
                "want": "always show me these",
                "disposition": "surface"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let rule_id = body_json(resp).await["rule_id"].as_i64().unwrap();

    // The edit omits the disposition: it is re-read from the new sentence, and
    // with no LLM wired in that is filtered.
    let resp = app
        .oneshot(authed_json(
            "PUT",
            &format!("/client/rules/{rule_id}"),
            serde_json::json!({
                "match_pattern": "a@acme.com",
                "want": "actually just the billing ones"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let updated = body_json(resp).await;
    assert_eq!(updated["rule_id"].as_i64().unwrap(), rule_id);
    assert_eq!(updated["disposition"], "filtered");
    assert_eq!(
        store.list_sender_rules(acct).unwrap()[0].disposition,
        squelch_core::types::Disposition::Filtered
    );
}

#[tokio::test]
async fn an_inferred_squelch_never_resolves_the_senders_open_mail() {
    // SIDE-EFFECT GUARD: the model says squelch, so the RULE is squelch — but a
    // misclassification must never mass-resolve a sender's mail, so the resolve
    // side effect stays behind an EXPLICIT squelch. Asking to sweep does not buy
    // past that: sweep is permission, not authority.
    let seeded = std::cell::RefCell::new(Vec::<i64>::new());
    let Harness { app, store, acct } = app_with_rule_inference("squelch", |store, acct| {
        seeded.borrow_mut().push(seed_unsub_msg(
            store,
            acct,
            "g1",
            "spam@evil.com",
            None,
            false,
        ));
    })
    .await;
    let spam_id = seeded.borrow()[0];

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "spam@evil.com",
                "want": "never show me anything from them",
                "source_message_id": spam_id,
                "sweep": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(resp).await["disposition"],
        "squelch",
        "the rule IS squelch"
    );
    assert_eq!(
        store.list_sender_rules(acct).unwrap()[0].disposition,
        squelch_core::types::Disposition::Squelch
    );

    let done = store
        .attention_updates(
            acct,
            chrono::Utc::now() - chrono::Duration::days(1),
            None,
            Some(AttentionStatus::Done),
            None,
        )
        .unwrap();
    assert!(
        !done.iter().any(|u| u.update.id == spam_id),
        "an INFERRED squelch resolves nothing, sweep or no sweep"
    );
}

#[tokio::test]
async fn a_literal_auto_squelch_is_inferred_and_never_resolves_open_mail() {
    // The literal "auto" asks for inference; it is NOT the client making the
    // decision, so the verdict it produces is exactly as inferred as an absent
    // key's. Nothing else distinguishes the two spellings — the no-LLM tests
    // can't, both paths land on filtered — so this is the one test standing
    // between "auto" and the explicit-squelch mass-resolve gate.
    let seeded = std::cell::RefCell::new(Vec::<i64>::new());
    let Harness { app, store, acct } = app_with_rule_inference("squelch", |store, acct| {
        seeded.borrow_mut().push(seed_unsub_msg(
            store,
            acct,
            "g1",
            "spam@evil.com",
            None,
            false,
        ));
    })
    .await;
    let spam_id = seeded.borrow()[0];

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "spam@evil.com",
                "want": "never show me anything from them",
                "disposition": "auto",
                "source_message_id": spam_id,
                "sweep": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(resp).await["disposition"],
        "squelch",
        "the rule IS squelch"
    );
    assert_eq!(
        store.list_sender_rules(acct).unwrap()[0].disposition,
        squelch_core::types::Disposition::Squelch
    );

    let done = store
        .attention_updates(
            acct,
            chrono::Utc::now() - chrono::Duration::days(1),
            None,
            Some(AttentionStatus::Done),
            None,
        )
        .unwrap();
    assert!(
        !done.iter().any(|u| u.update.id == spam_id),
        "an \"auto\" squelch is an inferred squelch: it resolves nothing"
    );
}

#[tokio::test]
async fn rule_mutations_write_audit_rows() {
    let Harness { app, store, acct } = harness(|_, _| {});

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
    assert!(audit.iter().all(|a| a.actor == "client-api"));
    let create = audit.iter().find(|a| a.action == "rule.create").unwrap();
    assert_eq!(create.target.as_deref(), Some("*@old.com"));
    // The detail carries the id AND the resolved disposition: an inferred
    // verdict is nowhere in the request body, so the log is the only record.
    assert_eq!(
        create.detail.as_deref(),
        Some(format!("{rule_id} disposition=squelch").as_str())
    );
    let update = audit.iter().find(|a| a.action == "rule.update").unwrap();
    assert_eq!(update.target.as_deref(), Some("*@new.com"));
    assert_eq!(
        update.detail.as_deref(),
        Some(format!("{rule_id} disposition=surface").as_str())
    );
    let delete = audit.iter().find(|a| a.action == "rule.delete").unwrap();
    assert_eq!(delete.target.as_deref(), Some(rule_id.to_string().as_str()));
}

#[tokio::test]
async fn failed_rule_mutations_write_no_audit_row() {
    // A 404 (unknown id) on PUT/DELETE changed nothing, so it writes no row.
    let Harness { app, store, acct } = harness(|_, _| {});

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
    // Cost comes from the default Stage-2 per-MTok prices (3.0 in / 15.0 out),
    // with cache writes at 1.25x and reads at 0.1x of the input price.
    let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let Harness { app, .. } = harness(move |store, acct| {
        // 2 calls: 1_000_000 in, 200_000 out, 200_000 cache-write, 1_000_000
        // cache-read.
        store
            .stage2_bump_usage(acct, &day, 600_000, 100_000, 200_000, 1_000_000)
            .unwrap();
        store
            .stage2_bump_usage(acct, &day, 400_000, 100_000, 0, 0)
            .unwrap();
    });

    let resp = app.oneshot(authed("GET", "/client/stats")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let s2 = &json["stage2"];
    assert_eq!(s2["calls_today"], 2);
    assert_eq!(s2["input_tokens_today"], 1_000_000);
    assert_eq!(s2["output_tokens_today"], 200_000);
    assert_eq!(s2["cache_creation_tokens_today"], 200_000);
    assert_eq!(s2["cache_read_tokens_today"], 1_000_000);
    // cost = 3.0*(1e6/1e6) + 15.0*(0.2e6/1e6)
    //      + 3.0*1.25*(0.2e6/1e6) + 3.0*0.1*(1e6/1e6) = 3.0 + 3.0 + 0.75 + 0.3
    let cost = s2["est_cost_usd_today"].as_f64().unwrap();
    assert!((cost - 7.05).abs() < 1e-9, "expected 7.05, got {cost}");
}

#[tokio::test]
async fn stats_carry_gmail_inbox_unread_only_once_the_sync_loop_has_fetched_it() {
    // Never fetched (old DB, or every fetch so far failed): the key is ABSENT.
    // A zeroed object would read as "your inbox is clear", which is a claim this
    // daemon cannot make without having asked Gmail.
    let Harness { app, .. } = harness(|_, _| {});
    let resp = app.oneshot(authed("GET", "/client/stats")).await.unwrap();
    let json = body_json(resp).await;
    assert!(
        json.get("inbox_unread").is_none(),
        "unfetched counts must be absent, not zero"
    );

    // Once the sync loop has stored a pair, it rides along with the stats.
    let Harness { app, store, acct } = harness(|store, acct| {
        store.set_inbox_unread(acct, 214, 190).unwrap();
    });
    let stamped = store.inbox_unread(acct).unwrap().unwrap().fetched_at;
    let resp = app.oneshot(authed("GET", "/client/stats")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["inbox_unread"]["messages"], 214);
    assert_eq!(json["inbox_unread"]["threads"], 190);
    // The counts keep being served while a fetch is failing, so the stamp is
    // what separates a live pair from one frozen since the scope was revoked.
    assert_eq!(
        json["inbox_unread"]["fetched_at"],
        Value::String(stamped.to_rfc3339()),
        "the stored stamp, verbatim RFC3339"
    );
    // Still the same body it always was.
    assert!(json["stage2"].is_object());

    // A real zero IS reported: nothing unread is a different answer from not
    // knowing, and the client renders them differently.
    let Harness { app, .. } = harness(|store, acct| {
        store.set_inbox_unread(acct, 0, 0).unwrap();
    });
    let resp = app.oneshot(authed("GET", "/client/stats")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["inbox_unread"]["messages"], 0);
    assert_eq!(json["inbox_unread"]["threads"], 0);
}

#[tokio::test]
async fn usage_returns_rows_totals_and_is_bearer_gated() {
    // Newest-first daily rows, totals costed at the default 3.0 in / 15.0 out
    // (cache writes at 1.25x, reads at 0.1x of the input price).
    let Harness { app, .. } = harness(|store, acct| {
        store
            .stage2_bump_usage(acct, "2026-07-08", 400_000, 100_000, 0, 0)
            .unwrap();
        store
            .stage2_bump_usage(acct, "2026-07-09", 600_000, 100_000, 400_000, 2_000_000)
            .unwrap();
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
    assert_eq!(rows[0]["cache_creation_tokens"], 400_000);
    assert_eq!(rows[0]["cache_read_tokens"], 2_000_000);
    assert_eq!(rows[1]["day"], "2026-07-08");

    let totals = &json["totals"];
    assert_eq!(totals["calls"], 2);
    assert_eq!(totals["input_tokens"], 1_000_000);
    assert_eq!(totals["output_tokens"], 200_000);
    assert_eq!(totals["cache_creation_tokens"], 400_000);
    assert_eq!(totals["cache_read_tokens"], 2_000_000);
    // cost = 3.0*(1e6/1e6) + 15.0*(0.2e6/1e6)
    //      + 3.0*1.25*(0.4e6/1e6) + 3.0*0.1*(2e6/1e6) = 3.0 + 3.0 + 1.5 + 0.6
    let cost = totals["est_cost_usd"].as_f64().unwrap();
    assert!((cost - 8.1).abs() < 1e-9, "expected 8.1, got {cost}");

    // Stage-1 and Stage-2 appear as separate usage categories.
    assert_eq!(json["model"], "claude-sonnet-5");
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
    let Harness { app, .. } = harness(|store, acct| {
        let bill = store
            .upsert_message(&msg(acct, "g1", "t1", "bill due", "pay"))
            .unwrap();
        store
            .set_triage(
                bill,
                acct,
                95,
                Tier::Deadline,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
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
    let Harness { app, .. } = harness(|store, acct| {
        // One HTML message and one plain-text message in the same thread.
        let mut html_msg = msg(acct, "g-html", "t-html", "Newsletter", "flattened text");
        html_msg.body_html = Some("<p>Hello <strong>world</strong></p>".to_string());
        let h = store.upsert_message(&html_msg).unwrap();
        store
            .set_triage(
                h,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();

        let plain = msg(acct, "g-plain", "t-html", "Newsletter", "just text");
        let p = store.upsert_message(&plain).unwrap();
        store
            .set_triage(
                p,
                acct,
                55,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
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
    // Per-message triage rides along for in-thread attention highlighting.
    assert_eq!(msgs[0]["tier"], "signal");
    assert_eq!(msgs[0]["attention_open"], true, "unresolved row reads open");
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
        .set_triage(
            id,
            acct,
            10,
            Tier::Noise,
            Sensitivity::Normal,
            None,
            "",
            "",
            None,
        )
        .unwrap();
    id
}

#[tokio::test]
async fn unsubscribe_returns_first_http_url_and_records_and_audits() {
    // The server returns the first http(s) URL for the client to open; it makes
    // no outbound request. A mailto ahead of the URL is skipped.
    let Harness { app, store, acct } = harness(|store, acct| {
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
        .oneshot(authed_json(
            "POST",
            "/client/unsubscribe",
            serde_json::json!({ "message_id": mid }),
        ))
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
    assert!(audit.iter().all(|a| {
        a.detail
            .as_deref()
            .map(|d| !d.contains("sub.com/u"))
            .unwrap_or(true)
    }));
}

#[tokio::test]
async fn unsubscribe_browser_returns_url_and_does_not_fetch() {
    let Harness { app, store, acct } = harness(|store, acct| {
        seed_unsub_msg(
            store,
            acct,
            "g1",
            "news@sub.com",
            Some("<https://sub.com/u/9>"),
            false,
        );
    });
    let mid = store.search(acct, "Newsletter", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/unsubscribe",
            serde_json::json!({ "message_id": mid }),
        ))
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
async fn unsubscribe_resolves_the_source_email_to_done() {
    // Unsubscribing IS the disposition of that email: the triage row must not
    // stay open demanding a second act.
    let Harness { app, store, acct } = harness(|store, acct| {
        seed_unsub_msg(
            store,
            acct,
            "g1",
            "news@sub.com",
            Some("<https://sub.com/u/9>"),
            false,
        );
    });
    let mid = store.search(acct, "Newsletter", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/unsubscribe",
            serde_json::json!({ "message_id": mid }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let done = store
        .attention_updates(
            acct,
            chrono::Utc::now() - chrono::Duration::days(1),
            None,
            Some(AttentionStatus::Done),
            None,
        )
        .unwrap();
    assert!(
        done.iter().any(|u| u.update.id == mid),
        "unsubscribed email is done"
    );
}

#[tokio::test]
async fn squelch_rule_with_source_message_resolves_it_surface_does_not() {
    // Blocking a sender from an email marks that email done; a surface rule is
    // tuning, not a verdict, and must leave its source open.
    // Ids captured from the seeder, not inferred from a search ordering. They
    // used to be taken as max/min, which had them BACKWARDS — spam is seeded
    // first and therefore holds the LOWER id. Nothing caught it while the
    // handler resolved whatever message id it was handed; resolving by sender
    // reads the from_addr, so the mix-up became visible immediately.
    let seeded = std::cell::RefCell::new(Vec::<i64>::new());
    let Harness { app, store, acct } = harness(|store, acct| {
        seeded.borrow_mut().push(seed_unsub_msg(
            store,
            acct,
            "g1",
            "spam@evil.com",
            None,
            false,
        ));
        seeded.borrow_mut().push(seed_unsub_msg(
            store,
            acct,
            "g2",
            "friend@nice.com",
            None,
            false,
        ));
    });
    let (spam_id, nice_id) = (seeded.borrow()[0], seeded.borrow()[1]);

    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "spam@evil.com",
                "want": "",
                "disposition": "squelch",
                "source_message_id": spam_id,
                "sweep": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "friend@nice.com",
                "want": "always surface",
                "disposition": "surface",
                "source_message_id": nice_id,
                "sweep": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let done = store
        .attention_updates(
            acct,
            chrono::Utc::now() - chrono::Duration::days(1),
            None,
            Some(AttentionStatus::Done),
            None,
        )
        .unwrap();
    assert!(
        done.iter().any(|u| u.update.id == spam_id),
        "squelch-rule source is done"
    );
    assert!(
        !done.iter().any(|u| u.update.id == nice_id),
        "surface-rule source stays open"
    );
}

#[tokio::test]
async fn an_explicit_squelch_without_sweep_resolves_nothing() {
    // THE UNDO CASE. Deleting a squelch rule and undoing the delete recreates it
    // with the same explicit disposition, and that recreate must not re-sweep an
    // inbox the user has since worked through. Same for the rule editor's save.
    // `sweep` is the client saying "and clear what they already sent"; without it
    // the rule is written and nothing else happens.
    let seeded = std::cell::RefCell::new(Vec::<i64>::new());
    let Harness { app, store, acct } = harness(|store, acct| {
        seeded.borrow_mut().push(seed_unsub_msg(
            store,
            acct,
            "g1",
            "spam@evil.com",
            None,
            false,
        ));
    });
    let spam_id = seeded.borrow()[0];

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "spam@evil.com",
                "want": "never show me anything from them",
                "disposition": "squelch",
                "source_message_id": spam_id
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        store.list_sender_rules(acct).unwrap()[0].disposition,
        squelch_core::types::Disposition::Squelch,
        "the rule is still a squelch"
    );

    let done = store
        .attention_updates(
            acct,
            chrono::Utc::now() - chrono::Duration::days(1),
            None,
            Some(AttentionStatus::Done),
            None,
        )
        .unwrap();
    assert!(
        !done.iter().any(|u| u.update.id == spam_id),
        "no sweep was asked for, so the sender's open mail stays open"
    );
}

/// Seed one triaged message from `from` on its OWN thread. Distinct threads
/// matter here: the bands collapse a thread to one representative row, so
/// same-thread seeds would hide exactly the sibling a sweep is supposed to clear.
fn seed_from_on_thread(
    store: &SqliteStore,
    acct: i64,
    gmail: &str,
    thread: &str,
    from: &str,
) -> i64 {
    let mut m = msg(acct, gmail, thread, "Newsletter", "body");
    m.from_addr = from.to_string();
    let id = store.upsert_message(&m).unwrap();
    store
        .set_triage(
            id,
            acct,
            10,
            Tier::Noise,
            Sensitivity::Normal,
            None,
            "",
            "",
            None,
        )
        .unwrap();
    id
}

#[tokio::test]
async fn sweep_resolves_by_sender_and_a_wildcard_resolves_only_the_source() {
    // WITH sweep, both resolve paths run: a plain address clears every open row
    // from that sender, and a wildcard clears ONLY the message the click came
    // from, because "*@evil.com" is a domain verdict and mass-resolving a whole
    // domain is a bigger claim than the click made.
    let seeded = std::cell::RefCell::new(Vec::<i64>::new());
    let Harness { app, store, acct } = harness(|store, acct| {
        let mut s = seeded.borrow_mut();
        s.push(seed_from_on_thread(
            store,
            acct,
            "g1",
            "t1",
            "spam@evil.com",
        ));
        s.push(seed_from_on_thread(
            store,
            acct,
            "g2",
            "t2",
            "spam@evil.com",
        ));
        s.push(seed_from_on_thread(
            store,
            acct,
            "g3",
            "t3",
            "other@junk.com",
        ));
        s.push(seed_from_on_thread(
            store,
            acct,
            "g4",
            "t4",
            "someone@junk.com",
        ));
    });
    let (spam_a, spam_b, junk_src, junk_other) = {
        let s = seeded.borrow();
        (s[0], s[1], s[2], s[3])
    };

    // Non-wildcard + sweep => resolve_sender_done: BOTH of that sender's rows.
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "spam@evil.com",
                "want": "never show me anything from them",
                "disposition": "squelch",
                "sweep": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Wildcard + sweep => resolve_done on the source message only.
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/rules",
            serde_json::json!({
                "match_pattern": "*@junk.com",
                "want": "never show me anything from them",
                "disposition": "squelch",
                "source_message_id": junk_src,
                "sweep": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let done = store
        .attention_updates(
            acct,
            chrono::Utc::now() - chrono::Duration::days(1),
            None,
            Some(AttentionStatus::Done),
            None,
        )
        .unwrap();
    let is_done = |id: i64| done.iter().any(|u| u.update.id == id);
    assert!(
        is_done(spam_a) && is_done(spam_b),
        "every row from the swept sender is done"
    );
    assert!(is_done(junk_src), "the wildcard's source message is done");
    assert!(!is_done(junk_other), "the rest of the domain is untouched");
}

#[tokio::test]
async fn unsubscribe_mailto_only_is_422() {
    // A mailto-only List-Unsubscribe has no http(s) link => 422; the server never
    // sends anything.
    let Harness { app, store, acct } = harness(|store, acct| {
        seed_unsub_msg(
            store,
            acct,
            "g1",
            "news@sub.com",
            Some("<mailto:unsub@sub.com?subject=Bye>"),
            false,
        );
    });
    let mid = store.search(acct, "Newsletter", 10, 0).unwrap()[0].id;

    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/unsubscribe",
            serde_json::json!({ "message_id": mid }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        store.list_unsubscribes(acct).unwrap().len(),
        0,
        "no ledger row on 422"
    );
}

#[tokio::test]
async fn unsubscribe_no_info_is_422() {
    let Harness { app, store, acct } = harness(|store, acct| {
        seed_unsub_msg(store, acct, "g1", "news@sub.com", None, false);
    });
    let mid = store.search(acct, "Newsletter", 10, 0).unwrap()[0].id;
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/unsubscribe",
            serde_json::json!({ "message_id": mid }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn unsubscribe_unknown_and_sealed_are_404() {
    let Harness { app, store, acct } = harness(|store, acct| {
        // A sealed message that (defensively) carries an unsub header.
        let s = store
            .upsert_message(&{
                let mut m = msg(acct, "g-otp", "t-otp", "verification code", "123456");
                m.list_unsubscribe = Some("<https://sub.com/u/1>".into());
                m
            })
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

    // Sealed => 404 (indistinguishable from unknown).
    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/unsubscribe",
            serde_json::json!({ "message_id": sealed_id }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Unknown id => 404.
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/unsubscribe",
            serde_json::json!({ "message_id": 999999 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_unsubscribes_newest_first_and_bearer_gated() {
    let Harness { app, .. } = harness(|store, acct| {
        store
            .upsert_unsubscribe(
                acct,
                "old@x.com",
                "browser",
                None,
                chrono::Utc::now() - chrono::Duration::hours(3),
            )
            .unwrap();
        store
            .upsert_unsubscribe(acct, "new@x.com", "one_click", None, chrono::Utc::now())
            .unwrap();
    });

    // Bearer-gated.
    let unauth = Request::builder()
        .uri("/client/unsubscribes")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(unauth).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let resp = app
        .oneshot(authed("GET", "/client/unsubscribes"))
        .await
        .unwrap();
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
    let Harness { app, store, acct } = harness(|store, acct| {
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
    assert_eq!(
        store.list_unsubscribes(acct).unwrap()[0]
            .resolution
            .as_deref(),
        Some("blocked")
    );
    let audit = store.list_audit(acct, 10).unwrap();
    assert!(audit.iter().any(
        |a| a.action == "unsub_resolution" && a.detail.as_deref() == Some("news@x.com:blocked")
    ));

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
    let Harness { app, .. } = harness(|store, acct| {
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
    let Harness { app, .. } = harness(|_, _| {});
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
    let Harness { app, store, acct } = harness(|_, _| {});
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
    let Harness { app, store, acct } = harness(|_, _| {});
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/triage-config",
            serde_json::json!({ "stage1_global_daily_cap": 0 }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        store
            .stage2_cap_overrides(acct)
            .unwrap()
            .stage1_global_daily_cap,
        None
    );
}

#[tokio::test]
async fn triage_config_get_computes_trailing_averages() {
    // Seed 4 recent inbound messages + a usage ledger day, and confirm the
    // trailing-14d averages and per-call token means.
    let Harness { app, .. } = harness(|store, acct| {
        for i in 0..4 {
            let m = msg(acct, &format!("g{i}"), &format!("t{i}"), "hi", "body");
            store.upsert_message(&m).unwrap();
        }
        // One day with 2 calls, 1000 in / 200 out tokens.
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        store.stage2_bump_usage(acct, &day, 600, 120, 0, 0).unwrap();
        store.stage2_bump_usage(acct, &day, 400, 80, 0, 0).unwrap();
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
    let Harness { app, store, acct } = harness(|_, _| {});
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
    let Harness { app, store, acct } = harness(|_, _| {});

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
    assert_eq!(
        store.stage2_cap_overrides(acct).unwrap(),
        Default::default()
    );
}

// --- attachments ------------------------------------------------------------

/// Collect a response body into raw bytes.
async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
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
    let Harness { app, store, acct } = harness(|store, acct| {
        // Thread t1: one message with a stored pdf + an over-cap (NULL data) part.
        let m1 = store
            .upsert_message(&msg(acct, "g1", "t1", "s1", "b1"))
            .unwrap();
        store
            .set_triage(
                m1,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
        store
            .insert_attachment(acct, m1, "doc.pdf", "application/pdf", 5, Some(b"Hello"))
            .unwrap();
        store
            .insert_attachment(
                acct,
                m1,
                "big.bin",
                "application/octet-stream",
                11_000_000,
                None,
            )
            .unwrap();
        // Thread t2: a message with NO attachments -> [] on the wire.
        let m2 = store
            .upsert_message(&msg(acct, "g2", "t2", "s2", "b2"))
            .unwrap();
        store
            .set_triage(
                m2,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
    });
    let _ = (&store, acct);

    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/thread/t1"))
        .await
        .unwrap();
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

    let resp = app
        .oneshot(authed("GET", "/client/thread/t2"))
        .await
        .unwrap();
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
    let Harness { app, .. } = harness(move |store, acct| {
        let m = store
            .upsert_message(&msg(acct, "g1", "t1", "s", "b"))
            .unwrap();
        store
            .set_triage(
                m,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
        let mut v = ids_seed.lock().unwrap();
        v.push((
            store
                .insert_attachment(acct, m, "doc.pdf", "application/pdf", 3, Some(b"pdf"))
                .unwrap(),
            "pdf",
        ));
        v.push((
            store
                .insert_attachment(acct, m, "pic.png", "image/png", 3, Some(b"png"))
                .unwrap(),
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
                .insert_attachment(
                    acct,
                    m,
                    "shout.svg",
                    "IMAGE/SVG+XML; charset=x",
                    3,
                    Some(b"svg"),
                )
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
        assert_eq!(
            header_str(&resp, header::CACHE_CONTROL),
            "private, max-age=3600"
        );
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
    let Harness { app, .. } = harness(move |store, acct| {
        let m = store
            .upsert_message(&msg(acct, "g1", "t1", "s", "b"))
            .unwrap();
        store
            .set_triage(
                m,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
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
    let Harness { app, .. } = harness(move |store, acct| {
        let m = store
            .upsert_message(&msg(acct, "g1", "t1", "s", "b"))
            .unwrap();
        store
            .set_triage(
                m,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();
        // Metadata only (data == None): over the ingest cap.
        *id_seed.lock().unwrap() = store
            .insert_attachment(
                acct,
                m,
                "big.bin",
                "application/octet-stream",
                11_000_000,
                None,
            )
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
    let Harness { app, .. } = harness(move |store, acct| {
        let m = store
            .upsert_message(&msg(acct, "g1", "t1", "code", "secret"))
            .unwrap();
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
    let Harness { app, .. } = harness(|_, _| {});
    let req = Request::builder()
        .uri("/client/attachments/1")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "byte endpoint is behind auth"
    );
}

#[tokio::test]
async fn triage_feedback_round_trip_records_and_audits() {
    let mut mid = 0;
    let Harness { app, store, acct } = harness(|store, acct| {
        let m = store
            .upsert_message(&msg(acct, "g1", "t1", "s", "b"))
            .unwrap();
        store
            .set_triage(
                m,
                acct,
                60,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
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
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "route must be mounted and accept the correction"
    );
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

// --- local drafts: human-door CRUD + send cleanup ---------------------------

/// Seed one ordinary (non-sealed) message and return its local id.
fn seed_draft_parent(store: &SqliteStore, acct: i64) -> i64 {
    let m = store
        .upsert_message(&msg(
            acct,
            "gmail-parent",
            "t-draft",
            "Lunch?",
            "want lunch?",
        ))
        .unwrap();
    store
        .set_triage(
            m,
            acct,
            80,
            Tier::Signal,
            Sensitivity::Normal,
            None,
            "",
            "",
            None,
        )
        .unwrap();
    m
}

/// PUT one draft and hand back its wire view.
async fn put_draft(app: &axum::Router, body: Value) -> Value {
    let resp = app
        .clone()
        .oneshot(authed_json("PUT", "/client/drafts", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

/// The current draft list as a JSON array.
async fn list_drafts(app: &axum::Router) -> Value {
    let resp = app
        .clone()
        .oneshot(authed("GET", "/client/drafts"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

#[tokio::test]
async fn put_draft_creates_then_edits_the_same_key_in_place() {
    let mut parent = 0;
    let Harness { app, .. } = harness(|store, acct| parent = seed_draft_parent(store, acct));

    let first = put_draft(
        &app,
        serde_json::json!({
            "reply_to_message_id": parent,
            "to": "alice@example.com",
            "subject": "Re: Lunch?",
            "body": "sure"
        }),
    )
    .await;
    assert_eq!(first["reply_to_message_id"], parent);
    assert_eq!(first["to"], "alice@example.com");
    assert_eq!(first["body"], "sure");

    // Same key => the same composition, edited: one row, same id.
    let second = put_draft(
        &app,
        serde_json::json!({
            "reply_to_message_id": parent,
            "to": "alice@example.com",
            "subject": "Re: Lunch?",
            "body": "sure, 1pm"
        }),
    )
    .await;
    assert_eq!(
        second["id"], first["id"],
        "an edit must not mint a new draft"
    );
    assert_eq!(second["body"], "sure, 1pm");
    assert_eq!(
        second["created_at"], first["created_at"],
        "created_at survives an edit"
    );

    let all = list_drafts(&app).await;
    assert_eq!(all.as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn put_draft_defaults_missing_text_fields_and_keys_new_mail_on_null() {
    let Harness { app, .. } = harness(|_, _| {});
    let draft = put_draft(&app, serde_json::json!({ "to": "bob@example.com" })).await;
    assert!(
        draft["reply_to_message_id"].is_null(),
        "no parent => the new-message slot"
    );
    assert_eq!(
        draft["subject"], "",
        "a missing text field stores empty, not 400"
    );
    assert_eq!(draft["body"], "");
}

#[tokio::test]
async fn put_draft_on_sealed_or_unknown_parent_is_404_and_stores_nothing() {
    let Harness { app, store, acct } = harness(|store, acct| {
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

    // Sealed and unknown parents are the SAME 404: a draft can never be keyed to
    // sealed mail, and the endpoint is no existence oracle.
    for parent in [sealed_id, 999_999] {
        let resp = app
            .clone()
            .oneshot(authed_json(
                "PUT",
                "/client/drafts",
                serde_json::json!({ "reply_to_message_id": parent, "body": "hi" }),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "parent {parent} must 404"
        );
    }
    assert!(
        store.list_drafts(acct).unwrap().is_empty(),
        "a 404 stores no draft"
    );
    assert_eq!(list_drafts(&app).await.as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn drafts_list_carries_the_to_field_name_on_the_wire() {
    let Harness { app, .. } = harness(|_, _| {});
    put_draft(
        &app,
        serde_json::json!({ "to": "bob@example.com", "subject": "Hello", "body": "hi" }),
    )
    .await;

    let all = list_drafts(&app).await;
    assert_eq!(all.as_array().map(Vec::len), Some(1));
    assert_eq!(
        all[0]["to"], "bob@example.com",
        "the column is to_addr; the wire says `to`"
    );
    assert!(
        all[0].get("to_addr").is_none(),
        "no store column name leaks"
    );
    assert!(
        all[0].get("account_id").is_none(),
        "no account id on the wire"
    );
    assert_eq!(all[0]["subject"], "Hello");
    assert_eq!(all[0]["body"], "hi");
    // RFC3339 timestamps, same as every other wire view.
    assert!(all[0]["created_at"].as_str().unwrap().contains('T'));
    assert!(all[0]["updated_at"].as_str().unwrap().contains('T'));
}

#[tokio::test]
async fn delete_draft_then_deleting_it_again_is_404() {
    let Harness { app, .. } = harness(|_, _| {});
    let draft = put_draft(
        &app,
        serde_json::json!({ "to": "bob@example.com", "body": "hi" }),
    )
    .await;
    let id = draft["id"].as_i64().unwrap();

    let resp = app
        .clone()
        .oneshot(authed("DELETE", &format!("/client/drafts/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "deleted");
    assert_eq!(json["id"], id);
    assert_eq!(list_drafts(&app).await.as_array().map(Vec::len), Some(0));

    let resp = app
        .oneshot(authed("DELETE", &format!("/client/drafts/{id}")))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a second delete has nothing to remove"
    );
}

#[tokio::test]
async fn draft_reads_and_writes_are_no_store() {
    // A draft is unsent thinking: like the revealed sealed body, it must not be
    // retained by anything on the path to the client.
    let Harness { app, .. } = harness(|_, _| {});
    let resp = app
        .clone()
        .oneshot(authed_json(
            "PUT",
            "/client/drafts",
            serde_json::json!({ "to": "bob@example.com", "body": "hi" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );

    let resp = app.oneshot(authed("GET", "/client/drafts")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
}

#[tokio::test]
async fn draft_crud_writes_no_audit_rows() {
    // The audit log is the ledger of reveals and Gmail writes. A draft never
    // leaves the machine, so it must leave no trace there.
    let Harness { app, store, acct } = harness(|_, _| {});
    let draft = put_draft(
        &app,
        serde_json::json!({ "to": "bob@example.com", "body": "hi" }),
    )
    .await;
    let id = draft["id"].as_i64().unwrap();
    put_draft(
        &app,
        serde_json::json!({ "to": "bob@example.com", "body": "hi again" }),
    )
    .await;
    let _ = list_drafts(&app).await;
    app.oneshot(authed("DELETE", &format!("/client/drafts/{id}")))
        .await
        .unwrap();

    assert!(
        store.list_audit(acct, 10).unwrap().is_empty(),
        "draft CRUD is not audited"
    );
}

#[tokio::test]
async fn successful_send_discards_the_draft_it_consumed() {
    // Cold send: one Gmail call (no parent headers to fetch).
    let (base, handle) = mock_gmail(1).await;
    let Harness { app, store, acct } = app_with_writes(base, |_, _| {});

    let draft = put_draft(
        &app,
        serde_json::json!({ "to": "alice@example.com", "subject": "Hi", "body": "see you Tuesday" }),
    )
    .await;
    let draft_id = draft["id"].as_i64().unwrap();

    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "subject": "Hi",
                "body": "see you Tuesday",
                "confirm": true,
                "draft_id": draft_id
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let reqs = handle.await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].contains("/messages/send"));

    assert_eq!(
        list_drafts(&app).await.as_array().map(Vec::len),
        Some(0),
        "the send consumed the draft"
    );
    // The send itself is still audited; the draft cleanup is not. `mock_gmail`
    // answers `{}`, so the echo has no id to fetch back and audits the skip.
    let audit = store.list_audit(acct, 10).unwrap();
    assert_eq!(audit.len(), 2);
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send" && a.detail.as_deref() == Some("ok"))
    );
    assert!(
        audit
            .iter()
            .any(|a| a.action == "send.echo" && a.detail.as_deref() == Some("skipped:no_id"))
    );
}

#[tokio::test]
async fn send_with_an_unknown_draft_id_still_succeeds() {
    // A stale draft id is bookkeeping, not a send failure.
    let (base, handle) = mock_gmail(1).await;
    let Harness { app, .. } = app_with_writes(base, |_, _| {});
    let resp = app
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "body": "see you Tuesday",
                "confirm": true,
                "draft_id": 4242
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(handle.await.unwrap().len(), 1);
}

#[tokio::test]
async fn guard_blocked_send_leaves_the_draft_intact() {
    let Harness { app, .. } = harness(|_, _| {});
    let draft = put_draft(
        &app,
        serde_json::json!({
            "to": "alice@example.com",
            "body": "your verification code is 483920"
        }),
    )
    .await;
    let draft_id = draft["id"].as_i64().unwrap();

    let resp = app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/actions/send",
            serde_json::json!({
                "to": "alice@example.com",
                "body": "your verification code is 483920",
                "confirm": true,
                "draft_id": draft_id
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // A blocked send is recoverable: the composition is exactly where it was.
    let all = list_drafts(&app).await;
    assert_eq!(all.as_array().map(Vec::len), Some(1));
    assert_eq!(all[0]["id"], draft_id);
    assert_eq!(all[0]["body"], "your verification code is 483920");
}

#[tokio::test]
async fn draft_routes_require_bearer_auth() {
    let Harness { app, .. } = harness(|_, _| {});
    let unauthed = [
        Request::builder()
            .uri("/client/drafts")
            .body(Body::empty())
            .unwrap(),
        common::json_request(
            "PUT",
            "/client/drafts",
            &serde_json::json!({ "to": "a@example.com" }),
            false,
        ),
        Request::builder()
            .method("DELETE")
            .uri("/client/drafts/1")
            .body(Body::empty())
            .unwrap(),
    ];
    for req in unauthed {
        let uri = req.uri().clone();
        let method = req.method().clone();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must be behind auth"
        );
    }
}

#[tokio::test]
async fn thread_view_marks_sent_to_senders_known_and_strangers_not() {
    use squelch_core::store::ContactEntry;
    let Harness { app, .. } = harness(|store, acct| {
        // Both authenticated at ingest, so the split under test is purely the
        // contact half.
        let mut known = msg(acct, "g1", "t1", "lunch?", "from a known contact");
        known.auth_pass = Some(true);
        store.upsert_message(&known).unwrap();
        let mut stranger = msg(acct, "g2", "t1", "lunch?", "from a stranger");
        stranger.from_addr = "promo@nowhere.io".into();
        stranger.from_name = None;
        stranger.auth_pass = Some(true);
        store.upsert_message(&stranger).unwrap();
        // Seeded through the Sent-derived contacts table — the same signal
        // triage's known-contact floor reads. Cased differently from the
        // message's `from_addr` on purpose: the column is NOCASE.
        store
            .merge_harvested_contacts(
                acct,
                &[ContactEntry {
                    addr: "Alice@Example.com".into(),
                    display_name: None,
                    sent_count: 1,
                    last_sent_at: None,
                }],
            )
            .unwrap();
    });

    let resp = app
        .oneshot(authed("GET", "/client/thread/t1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let msgs = json["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    let known = |addr: &str| -> Value {
        msgs.iter()
            .find(|m| m["from_addr"] == addr)
            .unwrap_or_else(|| panic!("{addr} missing from the thread"))["sender_known"]
            .clone()
    };
    assert_eq!(known("alice@example.com"), Value::Bool(true));
    assert_eq!(known("promo@nowhere.io"), Value::Bool(false));
    // The flatten must not have disturbed the fields the reader already decodes.
    assert!(msgs[0]["id"].is_i64());
    assert!(msgs[0].get("html").is_some());
    assert!(msgs[0]["attachments"].is_array());
}

#[tokio::test]
async fn thread_view_sender_known_needs_a_sent_message_not_merely_contact_row() {
    use squelch_core::store::ContactEntry;
    let Harness { app, .. } = harness(|store, acct| {
        store
            .upsert_message(&msg(acct, "g1", "t1", "hi", "body"))
            .unwrap();
        // sent_count 0: harvested but never actually written to. `is_known_contact`
        // requires sent_count > 0, and the read-time bit must not soften that.
        store
            .merge_harvested_contacts(
                acct,
                &[ContactEntry {
                    addr: "alice@example.com".into(),
                    display_name: None,
                    sent_count: 0,
                    last_sent_at: None,
                }],
            )
            .unwrap();
    });

    let resp = app
        .oneshot(authed("GET", "/client/thread/t1"))
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["messages"][0]["sender_known"], Value::Bool(false));
}

#[tokio::test]
async fn thread_view_sender_known_needs_the_mail_to_pass_email_authentication() {
    use squelch_core::store::ContactEntry;
    // Three copies of the same known contact's address, differing only in the
    // stored email-authentication verdict. A From header is free to write, so
    // only the authenticated one may claim the reader's pixel bypass — and the
    // NULL row (pre-existing mail, never re-synced) must read as "no".
    let Harness { app, .. } = harness(|store, acct| {
        for (label, auth_pass) in [
            ("authenticated", Some(true)),
            ("failed-auth", Some(false)),
            ("never-evaluated", None),
        ] {
            let mut m = msg(acct, label, "t1", "hi", label);
            m.auth_pass = auth_pass;
            store.upsert_message(&m).unwrap();
        }
        store
            .merge_harvested_contacts(
                acct,
                &[ContactEntry {
                    addr: "alice@example.com".into(),
                    display_name: None,
                    sent_count: 3,
                    last_sent_at: None,
                }],
            )
            .unwrap();
    });

    let resp = app
        .oneshot(authed("GET", "/client/thread/t1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let msgs = json["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    let known = |label: &str| -> Value {
        msgs.iter()
            .find(|m| m["content"] == label)
            .unwrap_or_else(|| panic!("{label} missing from the thread"))["sender_known"]
            .clone()
    };
    assert_eq!(known("authenticated"), Value::Bool(true));
    assert_eq!(
        known("failed-auth"),
        Value::Bool(false),
        "a known contact still needs an authentication pass"
    );
    assert_eq!(
        known("never-evaluated"),
        Value::Bool(false),
        "NULL auth_pass grants no bypass"
    );
    // WIRE CONTRACT: sender_known stays a plain bool, and the verdict that
    // gates it is not itself exposed.
    assert!(msgs[0]["sender_known"].is_boolean());
    assert!(msgs[0].get("auth_pass").is_none());
}
