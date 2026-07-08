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
        is_sent: false,
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
