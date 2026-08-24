//! Integration tests for invite sharing on the human door.
//!
//! The properties under test are the ones a unit test of either half cannot
//! see, because they are properties of the SEAM between the control plane, the
//! user's Gmail, and the person pressing the button:
//!
//! - the routes report `can_share: false` rather than failing when this daemon
//!   has no control plane, so the app never renders a button that could only
//!   refuse,
//! - the code that reaches Gmail is the one the control plane minted, in a mail
//!   addressed to the friend and to nobody else,
//! - THE RECIPIENT NEVER CROSSES THE MINT WIRE. The request to the control
//!   plane carries a bearer and nothing else, which is asserted on the captured
//!   bytes rather than trusted,
//! - one friend failing does not take the others with it, and a quota refusal
//!   still hands back a row per name,
//! - and nothing is minted until every refusal that costs nothing has run.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use serde_json::{Value, json};
use squelch_api::{ApiState, router};
use squelch_core::credentials::CredentialStore;
use squelch_core::credentials::OAuthToken;
use squelch_core::store::Store;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;

mod common;
use common::{Harness, authed, authed_json, body_json, msg};

/// The code the mock control plane mints. Shaped like a real one, because the
/// mail copy is asserted on it.
const CODE: &str = "ABCD-EFGH-JKMN-PQRS";
const SIGNUP: &str = "https://signup.passband.test";

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

/// What the mock control plane captured: the headers and body of every mint.
type Minted = Arc<Mutex<Vec<(HeaderMap, String)>>>;

/// A mock control plane. `answers` is one response per mint, in order, so a
/// test can script "two succeed, then the quota is gone".
async fn spawn_control(answers: Vec<(StatusCode, Value)>) -> (String, Minted) {
    let seen: Minted = Arc::default();
    let capture = seen.clone();
    let answers = Arc::new(Mutex::new(answers.into_iter()));
    let app = axum::Router::new().route(
        "/tenant/invite",
        axum::routing::post(move |headers: HeaderMap, body: String| {
            let capture = capture.clone();
            let answers = answers.clone();
            async move {
                capture.lock().unwrap().push((headers, body));
                let next = answers.lock().unwrap().next();
                match next {
                    Some((status, body)) => (status, axum::Json(body)),
                    None => (
                        StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(json!({"error": "quota_exhausted"})),
                    ),
                }
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), seen)
}

/// One successful mint answer.
fn minted(remaining: i64) -> (StatusCode, Value) {
    (
        StatusCode::OK,
        json!({
            "code": CODE,
            "signup_url": SIGNUP,
            "expires_at": (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339(),
            "remaining": remaining,
        }),
    )
}

/// Serve `n` sequential Gmail sends, each answered `200`, returning the raw
/// request bytes so the mail itself can be asserted on.
async fn mock_gmail(n: usize) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut reqs = Vec::new();
        for _ in 0..n {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 32768];
            let m = sock.read(&mut buf).await.unwrap();
            reqs.push(String::from_utf8_lossy(&buf[..m]).to_string());
            let body = r#"{"id":"sent-1","threadId":"t-1"}"#;
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

/// The full harness: a daemon that can mint (pointed at `control`) and send
/// (pointed at a mock Gmail).
fn sharing_app(control: Option<String>, gmail: String) -> Harness {
    let (state, store, acct) = common::state_with(|_, _| {});
    let state: ApiState = state
        .with_write_test_harness(Arc::new(StubCreds), gmail)
        .with_sharing(
            control.and_then(|url| squelch_api::Sharing::new(url, "pbs_share_token".into())),
        );
    Harness {
        app: router(state),
        store,
        acct,
    }
}

/// A daemon with no control plane says so, and says it calmly: the app reads
/// one boolean and never offers a button.
#[tokio::test]
async fn a_daemon_with_no_control_plane_cannot_share() {
    let (gmail, _g) = mock_gmail(0).await;
    let h = sharing_app(None, gmail);

    let resp = h
        .app
        .clone()
        .oneshot(authed("GET", "/client/invites"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["can_share"], false);
    assert_eq!(body["reason"], "no_control_plane");
    // No mail to preview, because there is no mail.
    assert!(body["preview"].is_null(), "{body}");

    // And the POST refuses rather than half-trying.
    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/invites",
            json!({ "recipients": ["friend@example.com"] }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A daemon that can MINT but cannot SEND is not a daemon that can share, and
/// the two refusals are told apart because only one of them names a command
/// that fixes it.
///
/// On hosted this cannot happen (signup asks for all three Gmail scopes in one
/// consent), which is exactly why it is worth a test: the case nobody meets is
/// the case nobody notices breaking.
#[tokio::test]
async fn a_daemon_that_cannot_send_mail_cannot_share_either() {
    let (control, _log) = spawn_control(vec![]).await;
    // The full harness minus the write credential: sharing configured, no way
    // to send.
    let (state, store, acct) = common::state_with(|_, _| {});
    let state: ApiState =
        state.with_sharing(squelch_api::Sharing::new(control, "pbs_share_token".into()));
    let h = Harness {
        app: router(state),
        store,
        acct,
    };

    let body = body_json(
        h.app
            .clone()
            .oneshot(authed("GET", "/client/invites"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["can_share"], false, "{body}");
    assert_eq!(body["reason"], "no_write_credential");

    // And the POST refuses on the same fact, before minting anything.
    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/invites",
            json!({ "recipients": ["friend@example.com"] }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// The whole seam, end to end: the code the control plane minted is the code in
/// the mail, the mail is addressed to the friend, and the mint wire carried the
/// bearer and NOT the friend.
#[tokio::test]
async fn the_minted_code_reaches_the_friend_and_the_friend_never_reaches_the_control_plane() {
    let (control, minted_log) = spawn_control(vec![minted(9)]).await;
    let (gmail, sends) = mock_gmail(1).await;
    let h = sharing_app(Some(control), gmail);

    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/invites",
            json!({ "recipients": ["friend@example.com"], "note": "thought of you" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["results"][0]["email"], "friend@example.com");
    assert_eq!(body["results"][0]["sent"], true);
    assert_eq!(body["remaining"], 9);

    // THE MAIL: addressed to the friend, carrying the minted code and the
    // user's own line.
    let sent = sends.await.unwrap();
    let raw = &sent[0];
    assert!(raw.contains("/messages/send"), "{raw}");
    // The MIME is base64url in the JSON body, so the assertion is on the
    // decoded message rather than on the wire bytes.
    let decoded = decode_sent_raw(raw);
    assert!(decoded.contains("To: friend@example.com"), "{decoded}");
    assert!(
        decoded.contains("Subject: I invited you to Passband"),
        "{decoded}"
    );
    assert!(decoded.contains(CODE), "{decoded}");
    assert!(decoded.contains("thought of you"), "{decoded}");
    // A cold send: it joins no conversation.
    assert!(!decoded.contains("In-Reply-To:"), "{decoded}");

    // THE MINT WIRE: a bearer, and no recipient anywhere in it.
    let log = minted_log.lock().unwrap();
    assert_eq!(log.len(), 1);
    let (headers, mint_body) = &log[0];
    assert_eq!(
        headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer pbs_share_token"
    );
    assert!(
        !mint_body.contains("friend@example.com"),
        "the recipient must never cross this wire: {mint_body}"
    );
    assert!(
        !mint_body.contains("thought of you"),
        "and neither must the note: {mint_body}"
    );
}

/// One friend's failure is one friend's failure. The others still go, and the
/// response says which was which.
#[tokio::test]
async fn one_failure_does_not_take_the_others_with_it() {
    // Two mints succeed; the third answers 503.
    let (control, _log) = spawn_control(vec![
        minted(8),
        minted(7),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "unavailable"}),
        ),
    ])
    .await;
    let (gmail, sends) = mock_gmail(2).await;
    let h = sharing_app(Some(control), gmail);

    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/invites",
            json!({ "recipients": ["a@example.com", "b@example.com", "c@example.com"] }),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["results"][0]["sent"], true);
    assert_eq!(body["results"][1]["sent"], true);
    assert_eq!(body["results"][2]["sent"], false);
    assert!(body["results"][2]["error"].as_str().is_some());
    assert_eq!(sends.await.unwrap().len(), 2, "two mails, not three");
}

/// A quota refusal ends the press, and EVERY remaining name still gets a row:
/// three results for a press of three, or the missing names read as sent.
#[tokio::test]
async fn a_quota_refusal_still_answers_for_every_name() {
    // One mint, then the mock's default: quota exhausted.
    let (control, log) = spawn_control(vec![minted(0)]).await;
    let (gmail, sends) = mock_gmail(1).await;
    let h = sharing_app(Some(control), gmail);

    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/invites",
            json!({ "recipients": ["a@example.com", "b@example.com", "c@example.com"] }),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "a row per name: {body}");
    assert_eq!(results[0]["sent"], true);
    assert_eq!(results[1]["sent"], false);
    assert_eq!(results[2]["sent"], false);
    // AND THE PRESS STOPPED: the third name was never asked about, because the
    // answer could only have been the same.
    assert_eq!(
        log.lock().unwrap().len(),
        2,
        "one mint per attempt, and it stopped"
    );
    assert_eq!(sends.await.unwrap().len(), 1);
}

/// Everything that can refuse for free refuses BEFORE a code is minted: a code
/// nobody receives is spent quota the user cannot get back.
#[tokio::test]
async fn nothing_is_minted_for_a_request_that_could_never_send() {
    let (control, log) = spawn_control(vec![minted(9)]).await;
    let (gmail, _g) = mock_gmail(0).await;
    let h = sharing_app(Some(control), gmail);

    for body in [
        json!({ "recipients": [] }),
        json!({ "recipients": ["   "] }),
        json!({ "recipients": ["not-an-address"] }),
        json!({ "recipients": ["a@x.test", "b@x.test", "c@x.test", "d@x.test", "e@x.test", "f@x.test"] }),
        // A note with a secret in it: the same outbound guard every other send
        // passes, on a send that is no less real for being an invite.
        json!({ "recipients": ["a@x.test"], "note": "my key is sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" }),
    ] {
        let resp = h
            .app
            .clone()
            .oneshot(authed_json("POST", "/client/invites", body.clone()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    assert!(
        log.lock().unwrap().is_empty(),
        "a refused press mints nothing"
    );
}

/// The same name twice is one invite. A pasted list with a repeat in it would
/// otherwise mail somebody twice and spend two of their friend's codes.
#[tokio::test]
async fn a_repeated_name_is_one_invite() {
    let (control, log) = spawn_control(vec![minted(9), minted(8)]).await;
    let (gmail, sends) = mock_gmail(1).await;
    let h = sharing_app(Some(control), gmail);

    let resp = h
        .app
        .clone()
        .oneshot(authed_json(
            "POST",
            "/client/invites",
            json!({ "recipients": ["Friend@Example.com", "friend@example.com", "  "] }),
        ))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["results"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(log.lock().unwrap().len(), 1, "one code, not two");
    assert_eq!(sends.await.unwrap().len(), 1);
}

/// The preview is the real copy, rendered by the daemon, so the app cannot show
/// one thing and send another.
#[tokio::test]
async fn the_preview_is_the_mail() {
    let (control, _log) = spawn_control(vec![]).await;
    let (gmail, _g) = mock_gmail(0).await;
    let h = sharing_app(Some(control), gmail);

    let resp = h
        .app
        .clone()
        .oneshot(authed("GET", "/client/invites"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["can_share"], true);
    let preview = body["preview"].as_str().expect("a preview came back");
    assert!(preview.contains("Passband"), "{preview}");
    assert!(preview.contains("XXXX-XXXX-XXXX-XXXX"), "{preview}");
    assert!(preview.contains("one use"), "{preview}");
    // A fresh in-memory store has no mail, so there is no honest number and the
    // copy makes no numeric claim.
    assert!(!preview.contains('%'), "{preview}");
}

/// The open ledger is written by the CLIENT saying so, not by serving a thread:
/// the app prefetches threads nobody looked at, and opens warmed ones from its
/// own cache without asking this daemon anything.
#[tokio::test]
async fn opening_is_a_thing_the_client_says_not_a_thing_the_get_infers() {
    let (gmail, _g) = mock_gmail(0).await;
    let h = sharing_app(None, gmail);
    let m = h
        .store
        .upsert_message(&msg(h.acct, "g1", "t1", "hi", "body"))
        .unwrap();
    h.store
        .set_triage(
            m,
            h.acct,
            80,
            squelch_core::types::Tier::Signal,
            squelch_core::types::Sensitivity::Normal,
            None,
            "one line",
            "reason",
            None,
        )
        .unwrap();

    // Reading the thread stamps NOTHING. This is the prefetch's path.
    let resp = h
        .app
        .clone()
        .oneshot(authed("GET", "/client/thread/t1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rate = h
        .store
        .share_open_rate(h.acct, chrono::Utc::now() - chrono::Duration::days(1))
        .unwrap();
    assert_eq!(rate.received, 1);
    assert_eq!(rate.opened, 0, "serving a body is not evidence of an open");

    // Saying so does.
    let resp = h
        .app
        .clone()
        .oneshot(authed("POST", "/client/thread/t1/opened"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["opened"], 1);
    let rate = h
        .store
        .share_open_rate(h.acct, chrono::Utc::now() - chrono::Duration::days(1))
        .unwrap();
    assert_eq!(rate.opened, 1);

    // And saying it twice writes nothing, so the client can fire it on every
    // open without keeping books.
    let resp = h
        .app
        .clone()
        .oneshot(authed("POST", "/client/thread/t1/opened"))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["opened"], 0);
}

/// Both routes sit behind the bearer, like every other human-door route.
#[tokio::test]
async fn the_bearer_layer_wraps_both_routes() {
    let (control, _log) = spawn_control(vec![]).await;
    let (gmail, _g) = mock_gmail(0).await;
    let h = sharing_app(Some(control), gmail);

    for req in [
        axum::http::Request::builder()
            .method("GET")
            .uri("/client/invites")
            .body(Body::empty())
            .unwrap(),
        axum::http::Request::builder()
            .method("POST")
            .uri("/client/invites")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"recipients":["a@x.test"]}"#))
            .unwrap(),
    ] {
        let resp = h.app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

/// The capability rides on `/client/stats` too, so the app knows whether to
/// offer the button without a second request.
#[tokio::test]
async fn stats_reports_the_capability() {
    let (gmail, _g) = mock_gmail(0).await;
    let h = sharing_app(None, gmail.clone());
    h.store
        .upsert_message(&msg(h.acct, "g1", "t1", "hi", "body"))
        .unwrap();
    let body = body_json(
        h.app
            .clone()
            .oneshot(authed("GET", "/client/stats"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["invite_sharing"], false);

    let (control, _log) = spawn_control(vec![]).await;
    let h = sharing_app(Some(control), gmail);
    let body = body_json(
        h.app
            .clone()
            .oneshot(authed("GET", "/client/stats"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["invite_sharing"], true);
}

/// Pull the RFC822 back out of a captured `messages/send` request: the body is
/// `{"raw": "<base64url>"}`.
fn decode_sent_raw(request: &str) -> String {
    let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let raw = v["raw"].as_str().unwrap_or_default();
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let bytes = URL_SAFE_NO_PAD.decode(raw).unwrap_or_default();
    String::from_utf8_lossy(&bytes).to_string()
}
