//! The waitlist end to end: the public form, the operator's page, and the code
//! that lands in somebody's inbox, against a MOCK Resend.
//!
//! The mock is a real axum server on an ephemeral loopback port, the way
//! `signup_flow.rs` mocks Google and the warden, because the properties under
//! test are properties OF THE WIRE and of the store underneath it:
//!
//! - the public route is not a membership oracle: a new address and one already
//!   on the list get the same 200, and every answer carries the CORS headers
//!   that make it readable from the site,
//! - approving is atomic, so a replayed POST mints nothing,
//! - the code that reaches Resend is a REAL invite: the test pulls it out of the
//!   captured message body, hashes it, and finds it available in the store, which
//!   is exactly what `POST /signup` will do to it,
//! - a refused send leaves a repairable row, and the repair REVOKES the code
//!   nobody received before minting the next one,
//! - with the feature unconfigured none of this is mounted at all,
//! - and the admin cookie is a different credential from the signup cookie,
//!   signed with the same key.
//!
//! Nothing here touches Resend, Google, a warden, or any store outside an
//! in-memory SQLite. `Config` is constructed directly, which is why `resend_url`
//! is a field: `Config::from_env` pins the real one.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    body::to_bytes,
    extract::State as AxumState,
    http::{HeaderMap, Request, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use serde_json::{Value, json};
use squelch_control::config::{Config, WaitlistConfig};
use squelch_control::cookie::{
    self, ADMIN_COOKIE_NAME, ADMIN_COOKIE_TTL_SECS, AdminClaim, COOKIE_NAME, SessionClaim,
};
use squelch_control::warden::HttpWarden;
use squelch_control::{ControlState, ControlStore, invites, router};
use tower::ServiceExt as _;

/// The operator's password. Thirty-two characters, the floor `from_env`
/// enforces, because a test that signs in with a short one would not be
/// exercising anything a deployment can have.
const ADMIN_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const RESEND_KEY: &str = "re_test_0123456789";
const INVITE_FROM: &str = "Passband <invites@passband.test>";
/// The one origin the public form may be posted from.
const ORIGIN: &str = "https://passband.test";
const COOKIE_KEY: &[u8] = &[42; 32];
const APPLICANT: &str = "ada@example.com";

/// What the mock Resend recorded.
#[derive(Default)]
struct Recorder {
    /// `(authorization, body)` for every `POST /emails`.
    sends: Vec<(String, Value)>,
    /// When true, the provider answers 500 the way an outage does.
    fail: bool,
}

type Shared = Arc<Mutex<Recorder>>;

/// A mock Resend: the one route this crate speaks.
async fn spawn_resend(rec: Shared) -> String {
    let app = Router::new()
        .route(
            "/emails",
            post(
                |AxumState(rec): AxumState<Shared>, headers: HeaderMap, body: String| async move {
                    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let mut r = rec.lock().unwrap();
                    r.sends.push((auth_of(&headers), parsed));
                    if r.fail {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "message": "internal_server_error" })),
                        )
                            .into_response()
                    } else {
                        (StatusCode::OK, Json(json!({ "id": "mock-message-id" }))).into_response()
                    }
                },
            ),
        )
        .with_state(rec);
    spawn(app).await
}

fn auth_of(headers: &HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

/// A whole control plane wired to the mock mailer.
struct Harness {
    app: Router,
    state: ControlState,
    rec: Shared,
}

impl Harness {
    /// The feature ON, pointed at a live mock.
    async fn new() -> Self {
        Self::build(true).await
    }

    /// The feature OFF: the trio unset, so nothing waitlist-shaped is mounted.
    async fn without_waitlist() -> Self {
        Self::build(false).await
    }

    async fn build(waitlist: bool) -> Self {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        let resend_url = spawn_resend(rec.clone()).await;

        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_url: "https://signup.passband.test".into(),
            redirect_uri: "https://signup.passband.test/oauth/callback".into(),
            base_domain: "passband.test".into(),
            client_id: "test-client-id".into(),
            client_secret: "test-client-secret".into(),
            cookie_key: COOKIE_KEY.to_vec(),
            // Nothing on these routes reaches the warden or Google. Port 1 is
            // where a request would land if one ever did, which is a connection
            // refused rather than a silently mocked success.
            warden_url: "http://127.0.0.1:1".into(),
            warden_token: "warden-bearer".into(),
            db_path: ":memory:".into(),
            trusted_proxy_hops: 0,
            token_url: "http://127.0.0.1:1/token".into(),
            auth_url: "http://127.0.0.1:1/authorize".into(),
            profile_url: "http://127.0.0.1:1/profile".into(),
            userinfo_url: "http://127.0.0.1:1/userinfo".into(),
            bifrost: None,
            waitlist: waitlist.then(|| WaitlistConfig {
                admin_token: ADMIN_TOKEN.into(),
                resend_api_key: RESEND_KEY.into(),
                invite_from: INVITE_FROM.into(),
                allowed_origin: ORIGIN.into(),
                // The struct is constructed directly, so an http mock URL is
                // fine here; `from_env` is where the real origin is pinned.
                resend_url,
            }),
        };

        let store = ControlStore::open_in_memory().unwrap();
        let warden = Arc::new(
            HttpWarden::new(
                "http://127.0.0.1:1".into(),
                "warden-bearer".into(),
                Duration::from_secs(5),
            )
            .unwrap(),
        );
        let state = ControlState::new(config, store, warden).unwrap();
        Self {
            app: router(state.clone()),
            state,
            rec,
        }
    }

    async fn get(&self, uri: &str, cookie: Option<&str>) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::builder().method("GET").uri(uri);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        self.send(req.body(axum::body::Body::empty()).unwrap()).await
    }

    async fn post_form(
        &self,
        uri: &str,
        body: String,
        cookie: Option<&str>,
    ) -> (StatusCode, HeaderMap, String) {
        self.post_from(uri, body, cookie, None).await
    }

    /// The same, said by a browser that names where the form was: what a real
    /// one sends, and what an attacker's page cannot forge.
    async fn post_from(
        &self,
        uri: &str,
        body: String,
        cookie: Option<&str>,
        origin: Option<&str>,
    ) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        if let Some(o) = origin {
            req = req.header(header::ORIGIN, o);
        }
        self.send(req.body(axum::body::Body::from(body)).unwrap())
            .await
    }

    async fn send(&self, req: Request<axum::body::Body>) -> (StatusCode, HeaderMap, String) {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = String::from_utf8_lossy(&to_bytes(resp.into_body(), 1 << 20).await.unwrap())
            .to_string();
        (status, headers, body)
    }

    /// Join the list, the way the site's form does.
    async fn join(&self, email: &str) -> (StatusCode, HeaderMap, String) {
        self.post_form("/waitlist", format!("email={}", urlencode(email)), None)
            .await
    }

    /// Sign in and return the cookie pair a browser would send back.
    async fn sign_in(&self) -> String {
        let (status, headers, body) = self
            .post_form("/admin/login", format!("token={ADMIN_TOKEN}"), None)
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        let cookie = headers[header::SET_COOKIE].to_str().unwrap().to_string();
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        cookie.split(';').next().unwrap().to_string()
    }

    /// The id of the single waitlist row.
    fn only_row_id(&self) -> i64 {
        let rows = self.state.store().list_waitlist().unwrap();
        assert_eq!(rows.len(), 1, "expected exactly one waitlist row");
        rows[0].id
    }

    /// `(status, invite_id, notified)` for one row, read from the store.
    fn row_state(&self, id: i64) -> (String, Option<i64>, bool) {
        let row = self
            .state
            .store()
            .waitlist_entry(id)
            .unwrap()
            .expect("the row is there");
        (row.status.clone(), row.invite_id, row.notified_at.is_some())
    }

    fn invite_ids(&self) -> Vec<i64> {
        self.state
            .store()
            .list_invites()
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect()
    }

    /// Whether a plaintext code is redeemable right now, asked exactly the way
    /// `POST /signup` asks it.
    fn code_is_available(&self, code: &str) -> bool {
        self.state
            .store()
            .find_available_invite(&invites::hash(code), chrono::Utc::now())
            .unwrap()
            .is_some()
    }

    fn sends(&self) -> Vec<(String, Value)> {
        self.rec.lock().unwrap().sends.clone()
    }

    fn fail_sends(&self, fail: bool) {
        self.rec.lock().unwrap().fail = fail;
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The code out of a captured message, from the line it sits on by itself.
/// `XXXX-XXXX-XXXX-XXXX` is 19 characters.
fn code_in(body: &Value) -> String {
    let text = body["text"].as_str().expect("the message has a text part");
    text.lines()
        .map(str::trim)
        .find(|line| line.len() == 19 && invites::is_plausible(line))
        .expect("the invite email carries the code on its own line")
        .to_string()
}

fn header_of(headers: &HeaderMap, name: header::HeaderName) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// A signed admin cookie with an `iat` of the test's choosing.
fn admin_cookie_at(iat: i64) -> String {
    let value = cookie::sign_admin(COOKIE_KEY, &AdminClaim::new(ADMIN_TOKEN, iat));
    format!("{ADMIN_COOKIE_NAME}={value}")
}

/// One submission makes one row, a second makes none, and neither is told
/// which it was.
#[tokio::test]
async fn a_submission_lands_on_the_list_once() {
    let h = Harness::new().await;

    let (status, headers, body) = h.join("Ada@Example.com").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"ok":true}"#);
    assert_eq!(
        header_of(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN),
        ORIGIN
    );
    assert_eq!(header_of(&headers, header::VARY), "origin");
    assert_eq!(header_of(&headers, header::CACHE_CONTROL), "no-store");

    // The same address again, spelled differently. Same answer, same one row:
    // an answer that differed would say who is already on the list.
    let (status, _, body) = h.join("ADA@EXAMPLE.COM").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"ok":true}"#);

    let rows = h.state.store().list_waitlist().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].email, APPLICANT, "stored normalized");
    assert_eq!(rows[0].status, "pending");
}

/// A refusal is a refusal ONLY for a string that is not an address, and it
/// carries the same CORS headers, or the site cannot read it.
#[tokio::test]
async fn a_malformed_address_is_refused_readably() {
    let h = Harness::new().await;
    for bad in ["ada", "ada@example", "@example.com", "ada@@example.com", ""] {
        let (status, headers, body) = h.join(bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}");
        assert_eq!(body, r#"{"ok":false,"error":"invalid_email"}"#);
        assert_eq!(
            header_of(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN),
            ORIGIN,
            "{bad:?}"
        );
        assert_eq!(header_of(&headers, header::CACHE_CONTROL), "no-store");
    }
    assert!(h.state.store().list_waitlist().unwrap().is_empty());

    // The preflight the site does not currently need, answered anyway.
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/waitlist")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        header_of(resp.headers(), header::ACCESS_CONTROL_ALLOW_ORIGIN),
        ORIGIN
    );
}

/// The public write has its own small bucket: a loop fills the list with junk
/// otherwise.
#[tokio::test]
async fn the_public_form_is_rate_limited() {
    let h = Harness::new().await;
    for i in 0..10 {
        let (status, _, _) = h.join(&format!("ada{i}@example.com")).await;
        assert_eq!(status, StatusCode::OK, "submission {i} should pass");
    }
    let (status, _, _) = h.join("one-too-many@example.com").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

/// The door: closed with nothing, closed with the wrong token, open with the
/// right one, and the refusal says only that.
#[tokio::test]
async fn the_admin_page_opens_only_to_the_token() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;

    let (status, _, body) = h.get("/admin", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"type="password""#), "{body}");
    assert!(!body.contains(APPLICANT), "{body}");

    let (status, headers, body) = h
        .post_form("/admin/login", "token=not-the-token".into(), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("not accepted"), "{body}");
    assert!(!headers.contains_key(header::SET_COOKIE), "no session");

    let cookie = h.sign_in().await;
    let (status, _, body) = h.get("/admin", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(APPLICANT), "{body}");
    assert!(body.contains("Approve and email invite"), "{body}");
}

/// An address that is an injection attempt is rendered as text, on the one page
/// that renders what a stranger typed.
///
/// TWO LAYERS, and this asserts both. The angle brackets never get past the
/// shape check, because `name-addr` syntax in an address is a lie told to the
/// operator reading the row. What DOES get in still carries characters with a
/// meaning in HTML, and those are escaped rather than trusted to be harmless.
#[tokio::test]
async fn the_dashboard_escapes_what_a_stranger_typed() {
    let h = Harness::new().await;
    let (status, _, _) = h.join(r#""><script>alert(1)</script>@evil.test"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "brackets never reach a row");

    let hostile = "a&'b@evil.test";
    let (status, _, _) = h.join(hostile).await;
    assert_eq!(status, StatusCode::OK);

    let cookie = h.sign_in().await;
    let (_, _, body) = h.get("/admin", Some(&cookie)).await;
    assert!(!body.contains(hostile), "{body}");
    assert!(body.contains("a&amp;&#39;b@evil.test"), "{body}");
}

/// The whole point of the feature: one click mints a real code, mails it, and
/// stamps the row.
#[tokio::test]
async fn approving_mails_a_code_that_redeems() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;
    let id = h.only_row_id();
    let cookie = h.sign_in().await;

    let (status, headers, _) = h
        .post_form("/admin/approve", format!("id={id}"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(header_of(&headers, header::LOCATION), "/admin");

    let (row_status, invite_id, notified) = h.row_state(id);
    assert_eq!(row_status, "approved");
    assert!(notified, "the provider accepted it, so the row is stamped");
    assert_eq!(h.invite_ids(), vec![invite_id.expect("an invite was minted")]);

    let sends = h.sends();
    assert_eq!(sends.len(), 1);
    let (auth, body) = &sends[0];
    assert_eq!(auth, &format!("Bearer {RESEND_KEY}"));
    assert_eq!(body["from"], INVITE_FROM);
    assert_eq!(body["to"][0], APPLICANT);
    assert_eq!(body["subject"], "Your Passband invite");

    // THE ASSERTION THE FEATURE RESTS ON: what was mailed is a live invite,
    // asked of the store exactly as `POST /signup` will ask it.
    let code = code_in(body);
    assert!(h.code_is_available(&code), "the mailed code must redeem");
    assert!(
        body["html"].as_str().unwrap().contains(&code),
        "both parts carry it"
    );
    // ...and the link in it is a plain one. No code in a URL, ever.
    assert!(!body["html"].as_str().unwrap().contains(&format!("={code}")));

    // The dashboard never shows the code back, because nothing kept it.
    let (_, _, page) = h.get("/admin", Some(&cookie)).await;
    assert!(!page.contains(&code), "{page}");
    assert!(page.contains("Invited"), "{page}");
}

/// A double-clicked button, a refreshed POST, two operators: the store's guard
/// makes all three one approval.
#[tokio::test]
async fn a_replayed_approval_mints_nothing() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;
    let id = h.only_row_id();
    let cookie = h.sign_in().await;

    h.post_form("/admin/approve", format!("id={id}"), Some(&cookie))
        .await;
    let after_first = h.invite_ids();

    let (status, _, body) = h
        .post_form("/admin/approve", format!("id={id}"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::OK, "the replay renders, it does not act");
    assert!(body.contains("already approved"), "{body}");
    assert_eq!(h.invite_ids(), after_first, "no second code");
    assert_eq!(h.sends().len(), 1, "no second email");
}

/// A provider outage is a repairable row, not a lost applicant: approved, not
/// stamped, badged, and one button away from a fresh code.
#[tokio::test]
async fn a_failed_send_leaves_a_row_the_operator_can_repair() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;
    let id = h.only_row_id();
    let cookie = h.sign_in().await;

    h.fail_sends(true);
    h.post_form("/admin/approve", format!("id={id}"), Some(&cookie))
        .await;

    let (row_status, first_invite, notified) = h.row_state(id);
    assert_eq!(row_status, "approved", "the person is approved either way");
    assert!(!notified, "nothing was accepted, so nothing is stamped");
    let first_invite = first_invite.expect("the code was minted before the send");

    let (_, _, page) = h.get("/admin", Some(&cookie)).await;
    assert!(page.contains("email not sent"), "{page}");
    assert!(page.contains("Send new invite"), "{page}");

    // The repair: the code nobody received is revoked, and a new one is minted
    // in its place. Two live invites for one approval would be one mailbox more
    // than anybody approved.
    h.fail_sends(false);
    let (status, _, _) = h
        .post_form("/admin/send", format!("id={id}"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, second_invite, notified) = h.row_state(id);
    let second_invite = second_invite.expect("a fresh code");
    assert_ne!(second_invite, first_invite);
    assert_eq!(h.invite_ids(), vec![second_invite], "the old one is gone");
    assert!(notified, "the second send was accepted");

    let sends = h.sends();
    assert_eq!(sends.len(), 2);
    let mailed = code_in(&sends[1].1);
    assert!(h.code_is_available(&mailed));
    assert_ne!(mailed, code_in(&sends[0].1), "a re-send is a new code");
}

/// A spent invite is not replaced. Somebody set a mailbox up with it, and a
/// fresh code would be a second one nobody approved.
#[tokio::test]
async fn a_spent_invite_is_not_replaced() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;
    let id = h.only_row_id();
    let cookie = h.sign_in().await;
    h.post_form("/admin/approve", format!("id={id}"), Some(&cookie))
        .await;

    // Spend it, the way a finished signup does.
    let code = code_in(&h.sends()[0].1);
    let invite_id = h
        .state
        .store()
        .reserve_invite(
            &invites::hash(&code),
            "holder",
            chrono::Utc::now(),
            chrono::Utc::now() + chrono::Duration::minutes(10),
        )
        .unwrap()
        .unwrap();
    h.state
        .store()
        .consume_invite(invite_id, "ada", "holder")
        .unwrap();

    let (status, _, body) = h
        .post_form("/admin/send", format!("id={id}"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("already been used"), "{body}");
    assert_eq!(h.sends().len(), 1, "no second email");
    assert_eq!(h.invite_ids(), vec![invite_id], "the spent row is untouched");
}

/// A code an operator revoked from the CLI is a row pointing at nothing, not a
/// spent invite. The button has to work, or the person waits forever behind a
/// message that is not true.
#[tokio::test]
async fn a_revoked_invite_is_replaced_rather_than_refused() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;
    let id = h.only_row_id();
    let cookie = h.sign_in().await;
    h.post_form("/admin/approve", format!("id={id}"), Some(&cookie))
        .await;

    let (_, first, _) = h.row_state(id);
    assert!(h.state.store().revoke_invite(first.unwrap()).unwrap());
    assert!(h.invite_ids().is_empty());

    let (status, _, _) = h
        .post_form("/admin/send", format!("id={id}"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, second, notified) = h.row_state(id);
    assert_ne!(second, first);
    assert!(notified);
    assert_eq!(h.sends().len(), 2);
    assert!(h.code_is_available(&code_in(&h.sends()[1].1)));
}

/// Unconfigured means UNMOUNTED. A deployment with no admin token answers 404
/// at an admin URL: there is nothing there to guess at.
#[tokio::test]
async fn an_unconfigured_deployment_has_no_waitlist_and_no_admin() {
    let h = Harness::without_waitlist().await;
    for (method, uri) in [
        ("POST", "/waitlist"),
        ("GET", "/admin"),
        ("POST", "/admin/login"),
        ("POST", "/admin/approve"),
        ("POST", "/admin/send"),
    ] {
        let (status, _, _) = match method {
            "GET" => h.get(uri, None).await,
            _ => h.post_form(uri, String::new(), None).await,
        };
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
    // The signup surface is untouched by any of it.
    let (status, _, _) = h.get("/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
}

/// One key signs both cookies, so the DOCUMENT has to be what tells them apart.
/// A signup claim presented as an admin session is refused.
#[tokio::test]
async fn a_signup_cookie_is_not_an_admin_cookie() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;
    let id = h.only_row_id();

    let signup = cookie::sign(
        COOKIE_KEY,
        &SessionClaim {
            sid: "s".repeat(43),
            label: "ada".into(),
            invite: Some(1),
            iat: chrono::Utc::now().timestamp(),
        },
    );
    for value in [
        format!("{ADMIN_COOKIE_NAME}={signup}"),
        format!("{COOKIE_NAME}={signup}"),
    ] {
        let (status, _, body) = h.get("/admin", Some(&value)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"type="password""#), "{body}");
        assert!(!body.contains(APPLICANT), "{body}");

        let (status, _, _) = h
            .post_form("/admin/approve", format!("id={id}"), Some(&value))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(h.row_state(id).0, "pending", "nothing was approved");
    assert!(h.invite_ids().is_empty());
}

/// A session that aged out is a session: the action is refused, the stale
/// cookie is cleared, and the operator is sent back to the door.
#[tokio::test]
async fn an_expired_admin_cookie_is_refused() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;
    let id = h.only_row_id();
    let now = chrono::Utc::now().timestamp();

    let stale = admin_cookie_at(now - ADMIN_COOKIE_TTL_SECS - 1);
    let (status, headers, body) = h
        .post_form("/admin/approve", format!("id={id}"), Some(&stale))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("session has ended"), "{body}");
    assert!(
        header_of(&headers, header::SET_COOKIE).contains("Max-Age=0"),
        "the stale cookie is cleared"
    );
    assert_eq!(h.row_state(id).0, "pending");

    // A tampered one fails the same way, and one inside the window works.
    let forged = format!("{}x", admin_cookie_at(now));
    let (status, _, _) = h
        .post_form("/admin/approve", format!("id={id}"), Some(&forged))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = h
        .post_form(
            "/admin/approve",
            format!("id={id}"),
            Some(&admin_cookie_at(now - ADMIN_COOKIE_TTL_SECS + 60)),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

/// THE COOKIE IS NOT THE WHOLE CSRF STORY, and this is the half it misses.
///
/// `SameSite=Strict` scopes a cookie to the SITE, and every `passband` name is
/// one site: the marketing page, the warden, and the gateway are all siblings of
/// this service. A page on any of them can post this form and the browser will
/// attach a live admin cookie, so the origin is checked as well and a POST from
/// a sibling mints nothing.
#[tokio::test]
async fn a_sibling_subdomain_cannot_press_the_buttons() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;
    let id = h.only_row_id();
    let cookie = h.sign_in().await;

    for origin in ["https://warden.passband.test", "https://passband.test"] {
        let (status, _, _) = h
            .post_from(
                "/admin/approve",
                format!("id={id}"),
                Some(&cookie),
                Some(origin),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "from {origin}");
    }
    assert_eq!(h.row_state(id).0, "pending", "nothing was approved");
    assert!(h.invite_ids().is_empty(), "and nothing was minted");
    assert!(h.sends().is_empty(), "and nothing was mailed");

    // The service's own page is the one that may.
    let (status, _, _) = h
        .post_from(
            "/admin/approve",
            format!("id={id}"),
            Some(&cookie),
            Some("https://signup.passband.test"),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(h.row_state(id).0, "approved");
    assert_eq!(h.sends().len(), 1);
}

/// Two presses of "send fresh invite" mint two codes and exactly one of them is
/// mailed: the compare-and-swap on the row decides, and the loser takes its own
/// mint back rather than leaving a live code no row names.
#[tokio::test]
async fn two_racing_sends_mail_one_code() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;
    let id = h.only_row_id();
    let cookie = h.sign_in().await;
    h.post_form("/admin/approve", format!("id={id}"), Some(&cookie))
        .await;
    let first = h.row_state(id).1.expect("approved with an invite");
    assert_eq!(h.sends().len(), 1);

    // Both presses read the pointer the approval wrote. The store is serialized,
    // so the race is staged: the row is moved on before the second one lands,
    // which is exactly the state the loser of a real race would find.
    let (status, _, _) = h
        .post_form("/admin/send", format!("id={id}"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let second = h.row_state(id).1.expect("a fresh invite");
    assert_ne!(second, first, "a re-send replaces the code");
    assert_eq!(h.sends().len(), 2);

    // Now the loser: its expectation names the invite that has already been
    // replaced.
    let store = h.state.store();
    let expires = chrono::Utc::now() + chrono::Duration::days(30);
    let loser = store
        .insert_invite(&invites::hash("LOSE-RRRR-RRRR-RRRR"), expires)
        .unwrap();
    assert!(
        !store.set_waitlist_invite(id, loser, Some(first)).unwrap(),
        "the pointer moved, so this write is refused"
    );
    assert_eq!(
        h.row_state(id).1,
        Some(second),
        "and the row still names the winner"
    );
}

/// A re-send while somebody is at Google's consent screen would delete the code
/// their session is holding, and they cannot grant that consent twice. So the
/// button refuses instead.
#[tokio::test]
async fn a_held_invite_is_left_alone() {
    let h = Harness::new().await;
    h.join(APPLICANT).await;
    let id = h.only_row_id();
    let cookie = h.sign_in().await;
    h.post_form("/admin/approve", format!("id={id}"), Some(&cookie))
        .await;
    let invite_id = h.row_state(id).1.unwrap();

    // The applicant pastes the code and is redirected to Google.
    let code = code_in(&h.sends()[0].1);
    let now = chrono::Utc::now();
    let held = h
        .state
        .store()
        .reserve_invite(
            &invites::hash(&code),
            "some-session",
            now,
            now + chrono::Duration::minutes(10),
        )
        .unwrap();
    assert_eq!(held, Some(invite_id), "the store handed out the hold");

    let (status, _, body) = h
        .post_form("/admin/send", format!("id={id}"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::OK, "a banner, not a redirect");
    assert!(body.contains("redeeming that code right now"), "{body}");
    assert_eq!(h.row_state(id).1, Some(invite_id), "the hold survived");
    assert_eq!(h.invite_ids(), vec![invite_id], "and nothing was minted");
    assert_eq!(h.sends().len(), 1, "and nothing extra was mailed");
}

/// The throttle's refusal is the one a browser is most likely to meet, so it
/// carries the CORS header too. Without it the fetch rejects as a network error
/// and the form cannot tell "slow down" from "we are down".
#[tokio::test]
async fn a_throttled_answer_is_still_readable_from_the_site() {
    let h = Harness::new().await;
    let mut throttled = None;
    for i in 0..40 {
        let (status, headers, _) = h.join(&format!("person{i}@example.com")).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            throttled = Some(headers);
            break;
        }
    }
    let headers = throttled.expect("the bucket empties well inside 40 submissions");
    assert_eq!(
        header_of(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN),
        ORIGIN
    );
    assert_eq!(header_of(&headers, header::VARY), "origin");
}
