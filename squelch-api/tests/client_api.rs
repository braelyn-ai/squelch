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
    // TASK 5: GET /client/stats surfaces a stage2 object with today's usage +
    // an estimated cost from the default per-MTok prices (1.0 in / 5.0 out).
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
    // cost = 1.0*(1e6/1e6) + 5.0*(0.2e6/1e6) = 1.0 + 1.0 = 2.0
    let cost = s2["est_cost_usd_today"].as_f64().unwrap();
    assert!((cost - 2.0).abs() < 1e-9, "expected 2.0, got {cost}");
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
