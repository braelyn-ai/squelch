//! GET /client/mail-activity — the mailbox's own traffic, per day.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{authed, body_json, harness, msg, sent_msg};
use serde_json::Value;
use squelch_core::store::{SqliteStore, Store};
use squelch_core::types::{NewMessage, SealedKind, Sensitivity, Tier};
use tower::ServiceExt;

/// `hour` o'clock UTC, `days_ago` days back from today's UTC date.
fn at(days_ago: i64, hour: u32) -> chrono::DateTime<chrono::Utc> {
    (chrono::Utc::now() - chrono::Duration::days(days_ago))
        .date_naive()
        .and_hms_opt(hour, 0, 0)
        .unwrap()
        .and_utc()
}

fn day_key(days_ago: i64) -> String {
    at(days_ago, 0).format("%Y-%m-%d").to_string()
}

fn seed(store: &SqliteStore, acct: i64, m: NewMessage, tier: Tier, sensitivity: Sensitivity) {
    let id = store.upsert_message(&m).unwrap();
    let kind = (sensitivity == Sensitivity::Sealed).then_some(SealedKind::Otp);
    store
        .set_triage(id, acct, 50, tier, sensitivity, kind, "", "", None)
        .unwrap();
}

async fn get(app: axum::Router, uri: &str) -> Value {
    let resp = app.oneshot(authed("GET", uri)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

#[tokio::test]
async fn is_bearer_gated() {
    let app = harness(|_, _| {}).app;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/client/mail-activity")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn buckets_each_day_and_keeps_spam_and_sent_out_of_the_tiers() {
    let app = harness(|store, acct| {
        let yesterday = |gmail: &str, thread: &str| NewMessage {
            received_at: at(1, 9),
            ..msg(acct, gmail, thread, "hi", "body")
        };
        seed(
            store,
            acct,
            yesterday("g1", "t1"),
            Tier::Signal,
            Sensitivity::Normal,
        );
        seed(
            store,
            acct,
            yesterday("g2", "t2"),
            Tier::PastDue,
            Sensitivity::Normal,
        );
        seed(
            store,
            acct,
            yesterday("g3", "t3"),
            Tier::Noise,
            Sensitivity::Normal,
        );
        seed(
            store,
            acct,
            yesterday("g4", "t4"),
            Tier::Noise,
            Sensitivity::Sealed,
        );
        // A reply the user sent: out, and its neutral noise row is not noise.
        seed(
            store,
            acct,
            NewMessage {
                received_at: at(1, 10),
                ..sent_msg(acct, "g5", "t5", "re: hi", "alice@example.com")
            },
            Tier::Noise,
            Sensitivity::Normal,
        );
        // Spam: never mail that arrived, as far as this report is concerned.
        seed(
            store,
            acct,
            NewMessage {
                is_spam: true,
                ..yesterday("g6", "t6")
            },
            Tier::Noise,
            Sensitivity::Normal,
        );
        // Today: one noise.
        seed(
            store,
            acct,
            NewMessage {
                received_at: at(0, 12),
                ..msg(acct, "g7", "t7", "today", "body")
            },
            Tier::Noise,
            Sensitivity::Normal,
        );
    })
    .app;

    let body = get(app, "/client/mail-activity?days=7").await;
    assert_eq!(body["days"], 7);
    assert_eq!(body["since"], day_key(6));
    assert_eq!(body["until"], day_key(0));

    let rows = body["rows"].as_array().expect("rows");
    assert_eq!(
        rows.len(),
        2,
        "sparse: only the two days with mail\n{body:#}"
    );
    assert_eq!(
        rows[0],
        serde_json::json!({
            "day": day_key(1),
            "received": 4,
            "sent": 1,
            "sealed": 1,
            "past_due": 1,
            "deadline": 0,
            "signal": 1,
            "noise": 1,
        })
    );
    assert_eq!(rows[1]["day"], day_key(0));
    assert_eq!(rows[1]["received"], 1);
    assert_eq!(rows[1]["noise"], 1);
}

#[tokio::test]
async fn days_scopes_the_window_and_clamps() {
    let app = harness(|store, acct| {
        for (ago, gmail) in [(0, "g0"), (1, "g1"), (40, "g40")] {
            seed(
                store,
                acct,
                NewMessage {
                    received_at: at(ago, 12),
                    ..msg(acct, gmail, gmail, "s", "b")
                },
                Tier::Noise,
                Sensitivity::Normal,
            );
        }
    })
    .app;

    // Today only.
    let one = get(app.clone(), "/client/mail-activity?days=1").await;
    assert_eq!(one["days"], 1);
    assert_eq!(one["since"], day_key(0));
    assert_eq!(one["rows"].as_array().unwrap().len(), 1);

    // The default window reaches yesterday but not day 40.
    let default = get(app.clone(), "/client/mail-activity").await;
    assert_eq!(default["days"], 30);
    assert_eq!(default["rows"].as_array().unwrap().len(), 2);

    // Both edges clamp rather than 400.
    let zero = get(app.clone(), "/client/mail-activity?days=0").await;
    assert_eq!(zero["days"], 1);
    let huge = get(app, "/client/mail-activity?days=9999").await;
    assert_eq!(huge["days"], 365);
    assert_eq!(huge["rows"].as_array().unwrap().len(), 3);
}
