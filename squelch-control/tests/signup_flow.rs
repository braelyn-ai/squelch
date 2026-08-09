//! The signup flow end to end, against a MOCK Google and a MOCK warden.
//!
//! Both mocks are real axum servers on ephemeral loopback ports, the way
//! squelch-core tests the APNs relay, because the properties under test are
//! properties OF THE WIRE:
//!
//! - the warden receives age ARMOR and never a refresh token,
//! - Google receives the PKCE verifier that matches the challenge the consent
//!   URL carried,
//! - the success page carries the pairing code, the tenant URL, and the deep
//!   link.
//!
//! Nothing here touches a real Google endpoint, a real port 8848, or any store
//! outside an in-memory SQLite. `Config` is constructed directly rather than
//! read from the environment, which is exactly why `token_url`/`profile_url`
//! are fields: `Config::from_env` pins Google's and nothing can move them.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    body::to_bytes,
    extract::State as AxumState,
    http::{Request, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use base64::Engine as _;
use serde_json::{Value, json};
use squelch_control::config::Config;
use squelch_control::warden::HttpWarden;
use squelch_control::{ControlState, ControlStore, invites, router, seal};
use tower::ServiceExt as _;

/// The refresh token the mock Google hands out. Every assertion that this
/// never leaves the process in the clear greps for this exact string.
const REFRESH_TOKEN: &str = "1//THE-SECRET-REFRESH-TOKEN";
const ACCESS_TOKEN: &str = "ya29.THE-SECRET-ACCESS-TOKEN";
const MAILBOX: &str = "ada@example.com";
const READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";

/// What the mocks recorded, so the test can assert on the bytes each service
/// actually received.
#[derive(Default)]
struct Recorder {
    /// Form fields of the token exchange.
    token_form: Vec<(String, String)>,
    /// Bodies posted to `POST /v1/tenants`.
    provision_bodies: Vec<Value>,
    /// Bearer values the warden saw.
    warden_bearers: Vec<String>,
    /// Labels asked about via `GET /v1/tenants/{label}`.
    status_lookups: Vec<String>,
    /// Labels the warden should report as ALREADY TAKEN.
    taken: Vec<String>,
    /// When set, the mock Google verifies PKCE the way Google does: the
    /// presented verifier must S256-hash to this challenge.
    expected_challenge: Option<String>,
}

type Shared = Arc<Mutex<Recorder>>;

/// A mock Google: the token endpoint and the Gmail profile endpoint.
async fn spawn_google(rec: Shared) -> String {
    let app = Router::new()
        .route(
            "/token",
            post(
                |AxumState(rec): AxumState<Shared>, body: String| async move {
                    let form: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                    let expected_challenge = {
                        let mut r = rec.lock().unwrap();
                        r.token_form = form.clone();
                        r.expected_challenge.clone()
                    };

                    // PKCE is verified here, the way Google verifies it: the
                    // presented verifier must S256-hash to the challenge that
                    // rode on the consent URL. The test drives the real consent
                    // URL through, so this is a genuine check and not a
                    // rubber stamp.
                    let get = |k: &str| {
                        form.iter()
                            .find(|(f, _)| f == k)
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default()
                    };
                    let verifier = get("code_verifier");
                    let challenge_ok = match &expected_challenge {
                        None => !verifier.is_empty(),
                        Some(expected) => &s256(&verifier) == expected,
                    };
                    if get("grant_type") != "authorization_code" || !challenge_ok {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant"})))
                            .into_response();
                    }
                    Json(json!({
                        "access_token": ACCESS_TOKEN,
                        "refresh_token": REFRESH_TOKEN,
                        "expires_in": 3599,
                        "token_type": "Bearer",
                        "scope": READONLY,
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/profile",
            get(|| async { Json(json!({ "emailAddress": MAILBOX })) }),
        )
        .with_state(rec);
    spawn(app).await
}

/// A mock warden implementing the control -> warden contract.
async fn spawn_warden(rec: Shared) -> String {
    let app = Router::new()
        .route(
            "/v1/tenants",
            post(
                |AxumState(rec): AxumState<Shared>,
                 headers: axum::http::HeaderMap,
                 body: String| async move {
                    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let mut r = rec.lock().unwrap();
                    r.provision_bodies.push(parsed.clone());
                    r.warden_bearers.push(
                        headers
                            .get(header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                    );
                    let label = parsed
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if r.taken.contains(&label) {
                        return (StatusCode::CONFLICT, Json(json!({"error":"exists"})))
                            .into_response();
                    }
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "port": 9100,
                            "pair_code": "ABCD-EFGH",
                            "pair_url": format!("https://{label}.passband.test"),
                            "deep_link": "passband://pair?url=x&code=ABCD-EFGH",
                        })),
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/v1/tenants/{label}",
            get(
                |AxumState(rec): AxumState<Shared>,
                 axum::extract::Path(label): axum::extract::Path<String>| async move {
                    let mut r = rec.lock().unwrap();
                    r.status_lookups.push(label.clone());
                    if r.taken.contains(&label) {
                        Json(json!({"status":"active","port":9100})).into_response()
                    } else {
                        (StatusCode::NOT_FOUND, Json(json!({"error":"unknown"}))).into_response()
                    }
                },
            ),
        )
        .with_state(rec);
    spawn(app).await
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

/// A whole control plane wired to the two mocks, plus the identity that stands
/// in for the one on the VPS (this process never holds it in production).
struct Harness {
    app: Router,
    state: ControlState,
    rec: Shared,
    identity: age::x25519::Identity,
    invite_code: String,
}

impl Harness {
    async fn new() -> Self {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        let google = spawn_google(rec.clone()).await;
        let warden_url = spawn_warden(rec.clone()).await;

        let identity = age::x25519::Identity::generate();
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_url: "https://signup.passband.test".into(),
            redirect_uri: "https://signup.passband.test/oauth/callback".into(),
            base_domain: "passband.test".into(),
            client_id: "test-client-id".into(),
            client_secret: "test-client-secret".into(),
            cookie_key: vec![42; 32],
            age_recipient: identity.to_public().to_string(),
            warden_url: warden_url.clone(),
            warden_token: "warden-bearer".into(),
            db_path: ":memory:".into(),
            trusted_proxy_hops: 0,
            token_url: format!("{google}/token"),
            auth_url: format!("{google}/authorize"),
            profile_url: format!("{google}/profile"),
        };

        let store = ControlStore::open_in_memory().unwrap();
        let minted = invites::mint().unwrap();
        store.insert_invite(&minted.code_hash).unwrap();

        let warden = Arc::new(
            HttpWarden::new(
                warden_url,
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
            identity,
            invite_code: minted.code,
        }
    }

    /// Mint another invite straight into the control store, the way
    /// `squelch-control invite issue` does.
    fn issue_invite(&self) -> String {
        let minted = invites::mint().unwrap();
        self.state.store().insert_invite(&minted.code_hash).unwrap();
        minted.code
    }

    async fn get(&self, uri: &str, cookie: Option<&str>) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut req = Request::builder().method("GET").uri(uri);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        self.send(req.body(axum::body::Body::empty()).unwrap()).await
    }

    async fn post_form(&self, uri: &str, body: String) -> (StatusCode, axum::http::HeaderMap, String) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(body))
            .unwrap();
        self.send(req).await
    }

    async fn send(&self, req: Request<axum::body::Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = String::from_utf8_lossy(
            &to_bytes(resp.into_body(), 1 << 20).await.unwrap(),
        )
        .to_string();
        (status, headers, body)
    }

    /// Post the signup form and return `(consent url, cookie value)`.
    async fn start_signup(&self, label: &str) -> (String, String) {
        let code = self.invite_code.clone();
        self.start_signup_with(label, &code).await
    }

    async fn start_signup_with(&self, label: &str, invite: &str) -> (String, String) {
        let (status, headers, _) = self
            .post_form(
                "/signup",
                format!("invite={}&label={label}", urlencode(invite)),
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "signup should redirect");
        let location = headers[header::LOCATION].to_str().unwrap().to_string();
        let cookie = headers[header::SET_COOKIE].to_str().unwrap().to_string();
        // Only the cookie pair, as a browser would send it back.
        let cookie = cookie.split(';').next().unwrap().to_string();
        (location, cookie)
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The PKCE S256 transform: base64url, unpadded, of SHA-256 over the verifier.
fn s256(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn query_param(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn state_param(consent_url: &str) -> String {
    query_param(consent_url, "state")
}

/// The happy path, asserted at every point the design makes a promise.
#[tokio::test]
async fn signup_provisions_a_tenant_and_hands_back_a_pairing_code() {
    let h = Harness::new().await;

    let (consent_url, cookie) = h.start_signup("ada").await;
    // The consent URL is the mock's authorize endpoint, asking for readonly.
    assert!(consent_url.contains("/authorize"), "{consent_url}");
    assert!(consent_url.contains("gmail.readonly"));
    assert!(consent_url.contains("code_challenge_method=S256"));

    // Hold the exchange to the PKCE challenge that rode on THIS consent URL,
    // so the verifier is proven rather than merely present.
    h.rec.lock().unwrap().expected_challenge = Some(query_param(&consent_url, "code_challenge"));

    let state = state_param(&consent_url);
    let (status, headers, body) = h
        .get(
            &format!("/oauth/callback?code=the-auth-code&state={state}"),
            Some(&cookie),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The success page: code, tenant URL, deep link, and pairing instructions.
    assert!(body.contains("ABCD-EFGH"), "{body}");
    assert!(body.contains("https://ada.passband.test"), "{body}");
    assert!(
        body.contains("passband://pair?url=https%3A%2F%2Fada.passband.test&amp;code=ABCD-EFGH"),
        "{body}"
    );
    assert!(body.contains("passband.app"));
    // No token, ever, on a page.
    assert!(!body.contains(REFRESH_TOKEN));
    assert!(!body.contains(ACCESS_TOKEN));
    // The session cookie is cleared on the way out.
    let set_cookie = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect::<Vec<_>>()
        .join("|");
    assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");

    let rec = h.rec.lock().unwrap();

    // Google got the PKCE verifier and our redirect URI, and the exchange was
    // an authorization_code grant.
    let form: std::collections::HashMap<_, _> = rec.token_form.iter().cloned().collect();
    assert_eq!(form.get("grant_type").unwrap(), "authorization_code");
    assert_eq!(form.get("code").unwrap(), "the-auth-code");
    assert!(!form.get("code_verifier").unwrap().is_empty());
    assert_eq!(
        form.get("redirect_uri").unwrap(),
        "https://signup.passband.test/oauth/callback"
    );

    // THE CENTRAL ASSERTION: what reached the warden is armor, addressed to the
    // box, and contains no token in the clear.
    assert_eq!(rec.provision_bodies.len(), 1);
    let sent = &rec.provision_bodies[0];
    assert_eq!(sent["label"], "ada");
    assert_eq!(sent["account_email"], MAILBOX);
    let ciphertext = sent["cred_read_ciphertext"].as_str().unwrap();
    assert!(ciphertext.starts_with(seal::ARMOR_HEADER), "{ciphertext}");
    assert!(!ciphertext.contains(REFRESH_TOKEN));
    assert!(!ciphertext.contains(ACCESS_TOKEN));
    let whole_body = serde_json::to_string(sent).unwrap();
    assert!(!whole_body.contains(REFRESH_TOKEN), "{whole_body}");
    assert!(!whole_body.contains(ACCESS_TOKEN), "{whole_body}");

    // ...and it is the tenant's real credential: only the box's identity opens
    // it, and what comes out is the StoredToken the daemon expects.
    let decryptor =
        age::Decryptor::new(age::armor::ArmoredReader::new(ciphertext.as_bytes())).unwrap();
    let mut reader = decryptor
        .decrypt(std::iter::once(&h.identity as &dyn age::Identity))
        .unwrap();
    let mut plaintext = String::new();
    std::io::Read::read_to_string(&mut reader, &mut plaintext).unwrap();
    let token: squelch_core::credentials::StoredToken = serde_json::from_str(&plaintext).unwrap();
    assert_eq!(token.refresh_token.as_deref(), Some(REFRESH_TOKEN));
    assert_eq!(token.access_token, ACCESS_TOKEN);

    // The bearer went on the wire, and availability was checked before consent.
    assert_eq!(rec.warden_bearers[0], "Bearer warden-bearer");
    assert_eq!(rec.status_lookups, vec!["ada".to_string()]);
}

/// One mailbox, one daemon. The mock Google always names the same mailbox, so
/// a second signup under a different label and a fresh invite is the duplicate
/// case, and it is refused at the callback, after consent and before anything
/// is provisioned.
#[tokio::test]
async fn a_mailbox_gets_one_daemon() {
    let h = Harness::new().await;
    let (consent, cookie) = h.start_signup("ada").await;
    let (status, _, _) = h
        .get(
            &format!("/oauth/callback?code=c1&state={}", state_param(&consent)),
            Some(&cookie),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let second_code = h.issue_invite();
    let (consent2, cookie2) = h.start_signup_with("grace", &second_code).await;
    let (status, _, body) = h
        .get(
            &format!("/oauth/callback?code=c2&state={}", state_param(&consent2)),
            Some(&cookie2),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("already"), "{body}");

    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.provision_bodies.len(), 1, "only the first provisioned");
}

/// An invite is spent by the signup that used it, and the code stops working
/// the moment the tenant exists.
#[tokio::test]
async fn an_invite_code_works_once() {
    let h = Harness::new().await;
    let (consent, cookie) = h.start_signup("ada").await;
    let (status, _, _) = h
        .get(
            &format!("/oauth/callback?code=c1&state={}", state_param(&consent)),
            Some(&cookie),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, body) = h
        .post_form(
            "/signup",
            format!("invite={}&label=grace", urlencode(&h.invite_code)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the form re-renders");
    assert!(body.contains("not usable"), "{body}");
}

/// If the verifier does not match the challenge, Google refuses the exchange
/// and nothing is provisioned. The control plane says so without leaking why.
#[tokio::test]
async fn an_exchange_google_refuses_provisions_nothing() {
    let h = Harness::new().await;
    let (consent, cookie) = h.start_signup("ada").await;
    // A challenge no verifier this process holds can satisfy.
    h.rec.lock().unwrap().expected_challenge = Some("a-challenge-from-another-flow".to_string());

    let (status, _, body) = h
        .get(
            &format!("/oauth/callback?code=c1&state={}", state_param(&consent)),
            Some(&cookie),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("did not complete"), "{body}");

    let rec = h.rec.lock().unwrap();
    assert!(!rec.token_form.is_empty(), "the exchange was attempted");
    assert!(rec.provision_bodies.is_empty(), "and nothing was provisioned");
}

/// A callback whose `state` does not match the session is refused, and nothing
/// is exchanged or provisioned.
#[tokio::test]
async fn a_state_mismatch_is_refused() {
    let h = Harness::new().await;
    let (_, cookie) = h.start_signup("ada").await;

    let (status, _, body) = h
        .get("/oauth/callback?code=c1&state=not-the-state", Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("could not be verified"), "{body}");

    let rec = h.rec.lock().unwrap();
    assert!(rec.token_form.is_empty(), "no exchange was attempted");
    assert!(rec.provision_bodies.is_empty());
}

/// A tampered cookie fails its MAC and is indistinguishable from no cookie.
#[tokio::test]
async fn a_tampered_cookie_is_refused() {
    let h = Harness::new().await;
    let (consent, cookie) = h.start_signup("ada").await;
    let state = state_param(&consent);

    // Flip the label inside the signed payload, keeping the original MAC.
    let value = cookie.split_once('=').unwrap().1;
    let (payload_b64, mac_b64) = value.split_once('.').unwrap();
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut claim: Value = serde_json::from_slice(&engine.decode(payload_b64).unwrap()).unwrap();
    claim["label"] = json!("www");
    let forged = format!(
        "passband_signup={}.{}",
        engine.encode(serde_json::to_vec(&claim).unwrap()),
        mac_b64
    );

    let (status, _, body) = h
        .get(
            &format!("/oauth/callback?code=c1&state={state}"),
            Some(&forged),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("could not be verified"), "{body}");

    // ...and so is no cookie at all: one message for both.
    let (status_no_cookie, _, body_no_cookie) = h
        .get(&format!("/oauth/callback?code=c1&state={state}"), None)
        .await;
    assert_eq!(status_no_cookie, status);
    assert_eq!(body_no_cookie, body);

    let rec = h.rec.lock().unwrap();
    assert!(rec.token_form.is_empty());
    assert!(rec.provision_bodies.is_empty());
}

/// A callback is one-shot: replaying the same code and cookie gets the same
/// refusal a stranger would get.
#[tokio::test]
async fn a_replayed_callback_finds_nothing() {
    let h = Harness::new().await;
    let (consent, cookie) = h.start_signup("ada").await;
    let state = state_param(&consent);
    let uri = format!("/oauth/callback?code=c1&state={state}");

    let (first, _, _) = h.get(&uri, Some(&cookie)).await;
    assert_eq!(first, StatusCode::OK);
    let (second, _, body) = h.get(&uri, Some(&cookie)).await;
    assert_eq!(second, StatusCode::BAD_REQUEST);
    assert!(body.contains("could not be verified"), "{body}");

    assert_eq!(h.rec.lock().unwrap().provision_bodies.len(), 1);
}

/// A label the warden already knows about is refused BEFORE the user is sent
/// to Google, so nobody spends a consent on an address they cannot have.
#[tokio::test]
async fn a_taken_label_is_refused_before_consent() {
    let h = Harness::new().await;
    h.rec.lock().unwrap().taken.push("ada".to_string());

    let (status, _, body) = h
        .post_form(
            "/signup",
            format!("invite={}&label=ada", urlencode(&h.invite_code)),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("already taken"), "{body}");
    assert!(h.rec.lock().unwrap().token_form.is_empty());
}

/// Invalid labels and unusable invite codes never reach Google either, and the
/// invite refusal says the same thing whatever was wrong with the code.
#[tokio::test]
async fn the_form_refuses_bad_input_without_leaving_the_page() {
    let h = Harness::new().await;

    for (body, needle) in [
        (
            format!("invite={}&label=ab", urlencode(&h.invite_code)),
            "too short",
        ),
        (
            format!("invite={}&label=www", urlencode(&h.invite_code)),
            "reserved",
        ),
        (
            format!("invite={}&label=-ada", urlencode(&h.invite_code)),
            "hyphen",
        ),
        (
            format!("invite={}&label=ada..x", urlencode(&h.invite_code)),
            "lowercase letters",
        ),
        ("invite=ZZZZ-ZZZZ&label=ada".to_string(), "not usable"),
        ("invite=nonsense&label=ada".to_string(), "not usable"),
        ("invite=&label=ada".to_string(), "not usable"),
    ] {
        let (status, _, page) = h.post_form("/signup", body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(page.contains(needle), "{body} -> {page}");
    }

    let rec = h.rec.lock().unwrap();
    assert!(rec.token_form.is_empty(), "nothing reached Google");
    assert!(rec.provision_bodies.is_empty());
}

/// The form and the health check are the only things reachable without a
/// session, and the form states the grant it is about to ask for.
#[tokio::test]
async fn the_landing_page_states_the_grant() {
    let h = Harness::new().await;
    let (status, _, body) = h.get("/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("gmail.readonly"), "{body}");
    assert!(body.contains(".passband.test"), "{body}");

    let (status, _, body) = h.get("/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}
