//! BOTH LOGIN HOPS end to end, against a MOCK Google and a MOCK warden.
//!
//! One file for the two because they are one flow with one input's worth of
//! difference: the same identity-only consent, the same session table, the same
//! callback, and the same warden-minted pairing code as the ticket. What differs
//! is that `/console/auth` is TOLD a label and checks it against the mailbox,
//! while `/app/auth` is told nothing and LOOKS THE LABEL UP from the mailbox;
//! and that one ends at a tenant console while the other ends at a
//! `passband://pair` deep link. The mocks below serve both unchanged.
//!
//! What the CONSOLE flow promises, and therefore what is asserted here:
//!
//! - it asks Google for IDENTITY ONLY (`openid email`), never a Gmail scope and
//!   never offline access, because signing in to a console is not a reason to
//!   hold a key to somebody's mail,
//! - the mailbox Google names must be the one this control plane's store says
//!   owns the tenant, compared after normalization and constant time,
//! - what comes back is a 302 to a URL this service CONSTRUCTED from its own
//!   base domain, the validated label, and the warden's pairing code, with
//!   nothing caller-supplied anywhere in it,
//! - the hop is NOT an existence oracle: a well-formed label that nobody has
//!   provisioned is sent to Google exactly like a real one, and only the
//!   callback refuses, uniformly,
//! - every identity-shaped refusal is one page: a tenant that does not exist, a
//!   tenant that is not active, and the wrong Google account are
//!   indistinguishable from outside, and none of them offers the signup form,
//! - nothing reaches the warden until the mailbox has been proved.
//!
//! What the APP flow promises on top of that:
//!
//! - it takes NO input, so there is nothing for a stranger to guess and no
//!   parameter that could become a redirect,
//! - the tenant is found by REVERSE LOOKUP on the address Google verified, so
//!   the only mailbox anybody can ask about is the one they just proved they
//!   hold, and only an ACTIVE tenant answers,
//! - what comes back is a page carrying a `passband://pair` deep link built from
//!   this deployment's own tenant URL and the warden's code, with no token on it,
//! - a Google account with no mailbox here is told so plainly and costs the
//!   warden nothing,
//! - it shares the console hop's rate-limit bucket and the session table's login
//!   carve-out, so alternating the two cannot buy a flooder a second budget.
//!
//! Nothing here touches a real Google endpoint, a real cluster, a real port
//! 8848, or any store outside an in-memory SQLite.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    body::to_bytes,
    extract::State as AxumState,
    http::{HeaderMap, Request, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use base64::Engine as _;
use serde_json::{Value, json};
use squelch_control::config::Config;
use squelch_control::cookie::{self, SessionClaim};
use squelch_control::invites;
use squelch_control::ratelimit::CONSOLE_AUTH_REQUESTS_PER_MINUTE;
use squelch_control::warden::HttpWarden;
use squelch_control::{ControlState, ControlStore, router};
use tower::ServiceExt as _;

/// The access token the mock Google hands out. Nothing in this flow may ever
/// render or redirect with it, and the assertions grep for this exact string.
const ACCESS_TOKEN: &str = "ya29.THE-SECRET-ACCESS-TOKEN";
/// A refresh token the mock offers even though the console flow does not ask for
/// offline access. If it ever shows up anywhere, something asked for it.
const REFRESH_TOKEN: &str = "1//THE-SECRET-REFRESH-TOKEN";
const MAILBOX: &str = "ada@example.com";
const LABEL: &str = "ada";
const PAIR_CODE: &str = "ABCD-EFGH";
/// The cookie key the harness signs with, so a test can forge a claim with a
/// VALID MAC: the one forgery a signature cannot rule out on its own.
const COOKIE_KEY: [u8; 32] = [42; 32];

/// What the mocks recorded.
#[derive(Default)]
struct Recorder {
    /// Form fields of the token exchange.
    token_form: Vec<(String, String)>,
    /// Labels passed to `POST /v1/tenants/{label}/pair`.
    pair_calls: Vec<String>,
    /// Bearer values the warden saw.
    warden_bearers: Vec<String>,
    /// Who the mock Google says is signed in.
    signed_in_as: Option<String>,
    /// Whether that address is verified, as OIDC reports it. `None` leaves the
    /// claim OUT of the answer altogether, which is the case a provider (or
    /// anything answering as one) can produce by omission.
    email_verified: Option<serde_json::Value>,
    /// The `scope` the mock reports on the token response. Google echoes the
    /// canonical URL for `email` today, and has shipped the short name; both
    /// must satisfy the console flow.
    granted_scope: String,
    /// When set, the mock Google verifies PKCE the way Google does.
    expected_challenge: Option<String>,
    /// When set, the pair route answers with this status and mints nothing.
    pair_status: Option<u16>,
}

type Shared = Arc<Mutex<Recorder>>;

fn recorder() -> Shared {
    Arc::new(Mutex::new(Recorder {
        signed_in_as: Some(MAILBOX.to_string()),
        email_verified: Some(Value::Bool(true)),
        granted_scope: "openid https://www.googleapis.com/auth/userinfo.email".to_string(),
        ..Recorder::default()
    }))
}

/// A mock Google: the token endpoint and the OIDC userinfo endpoint. NOT the
/// Gmail profile endpoint, deliberately: a console login holds no Gmail scope,
/// so a flow that reached for it here would fail to route rather than quietly
/// pass.
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
                    let granted_scope = rec.lock().unwrap().granted_scope.clone();
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
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": "invalid_grant"})),
                        )
                            .into_response();
                    }
                    // The scope set Google reports for an `openid email` request.
                    // The short name goes out; which spelling comes back is the
                    // recorder's to choose, because Google has shipped both.
                    Json(json!({
                        "access_token": ACCESS_TOKEN,
                        "refresh_token": REFRESH_TOKEN,
                        "expires_in": 3599,
                        "token_type": "Bearer",
                        "scope": granted_scope,
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/userinfo",
            get(|AxumState(rec): AxumState<Shared>| async move {
                let r = rec.lock().unwrap();
                match &r.signed_in_as {
                    None => {
                        (StatusCode::UNAUTHORIZED, Json(json!({"error": "no"}))).into_response()
                    }
                    Some(email) => {
                        let mut answer = serde_json::Map::new();
                        answer.insert("sub".into(), json!("1234567890"));
                        answer.insert("email".into(), json!(email));
                        // ABSENT rather than false when the recorder says so:
                        // the omission case is the one the flow must not read
                        // as a yes.
                        if let Some(verified) = &r.email_verified {
                            answer.insert("email_verified".into(), verified.clone());
                        }
                        Json(Value::Object(answer)).into_response()
                    }
                }
            }),
        )
        .with_state(rec);
    spawn(app).await
}

/// A mock warden with the one route this flow uses.
async fn spawn_warden(rec: Shared) -> String {
    let app = Router::new()
        .route(
            "/v1/tenants/{label}/pair",
            post(
                |AxumState(rec): AxumState<Shared>,
                 axum::extract::Path(label): axum::extract::Path<String>,
                 headers: HeaderMap| async move {
                    let mut r = rec.lock().unwrap();
                    r.pair_calls.push(label.clone());
                    r.warden_bearers.push(
                        headers
                            .get(header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                    );
                    if let Some(status) = r.pair_status {
                        return (
                            StatusCode::from_u16(status).unwrap(),
                            Json(json!({"error": "refused"})),
                        )
                            .into_response();
                    }
                    Json(json!({
                        "pair_code": PAIR_CODE,
                        "pair_url": format!("https://{label}.passband.test"),
                        "deep_link": format!("passband://pair?url=x&code={PAIR_CODE}"),
                    }))
                    .into_response()
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

/// A whole control plane wired to the two mocks, with one provisioned tenant.
struct Harness {
    app: Router,
    rec: Shared,
    /// One live invite, so this file can prove the SIGNUP consent is unchanged
    /// by everything the console flow does to the shared code around it.
    invite_code: String,
}

impl Harness {
    async fn new() -> Self {
        let rec = recorder();
        let google = spawn_google(rec.clone()).await;
        let warden_url = spawn_warden(rec.clone()).await;

        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_url: "https://signup.passband.test".into(),
            redirect_uri: "https://signup.passband.test/oauth/callback".into(),
            base_domain: "passband.test".into(),
            client_id: "test-client-id".into(),
            client_secret: "test-client-secret".into(),
            cookie_key: COOKIE_KEY.to_vec(),
            warden_url: warden_url.clone(),
            warden_token: "warden-bearer".into(),
            db_path: ":memory:".into(),
            trusted_proxy_hops: 0,
            token_url: format!("{google}/token"),
            auth_url: format!("{google}/authorize"),
            profile_url: format!("{google}/profile"),
            userinfo_url: format!("{google}/userinfo"),
            // Console auth never touches the LLM gateway; feature off.
            bifrost: None,
            // Nor the waitlist: with it off, those routes are not mounted.
            waitlist: None,
        };

        let store = ControlStore::open_in_memory().unwrap();
        // The tenant that already exists. Capitalized on purpose: the store
        // normalizes, and so must the comparison on the way back.
        store.insert_tenant(LABEL, "Ada@Example.com").unwrap();
        let minted = invites::mint().unwrap();
        store
            .insert_invite(
                &minted.code_hash,
                chrono::Utc::now() + chrono::Duration::days(30),
            )
            .unwrap();

        let warden = Arc::new(
            HttpWarden::new(warden_url, "warden-bearer".into(), Duration::from_secs(5)).unwrap(),
        );
        Self {
            app: router(ControlState::new(config, store, warden).unwrap()),
            rec,
            invite_code: minted.code,
        }
    }

    /// A form POST, exactly as the signup page submits one.
    async fn post_form(&self, uri: &str, body: String) -> (StatusCode, HeaderMap, String) {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = String::from_utf8_lossy(&to_bytes(resp.into_body(), 1 << 20).await.unwrap())
            .to_string();
        (status, headers, body)
    }

    async fn get(&self, uri: &str, cookie: Option<&str>) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::builder().method("GET").uri(uri);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = String::from_utf8_lossy(&to_bytes(resp.into_body(), 1 << 20).await.unwrap())
            .to_string();
        (status, headers, body)
    }

    /// Start a console login and return `(consent url, cookie pair)`.
    async fn start_login(&self, tenant: &str) -> (String, String) {
        let (status, headers, body) = self
            .get(&format!("/console/auth?tenant={tenant}"), None)
            .await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        let location = headers[header::LOCATION].to_str().unwrap().to_string();
        let cookie = headers[header::SET_COOKIE].to_str().unwrap().to_string();
        (location, cookie.split(';').next().unwrap().to_string())
    }

    /// Walk one whole console login and return the callback's answer.
    async fn run_login(&self, tenant: &str) -> (StatusCode, HeaderMap, String) {
        let (consent, cookie) = self.start_login(tenant).await;
        self.finish_login(&consent, &cookie).await
    }

    /// Start an APP login and return `(consent url, cookie pair)`. No tenant
    /// argument, because the route takes none.
    async fn start_app_login(&self) -> (String, String) {
        let (status, headers, body) = self.get("/app/auth", None).await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        let location = headers[header::LOCATION].to_str().unwrap().to_string();
        let cookie = headers[header::SET_COOKIE].to_str().unwrap().to_string();
        (location, cookie.split(';').next().unwrap().to_string())
    }

    /// Come back from Google with the `state` this consent carried.
    async fn finish_login(&self, consent: &str, cookie: &str) -> (StatusCode, HeaderMap, String) {
        self.get(
            &format!(
                "/oauth/callback?code=the-auth-code&state={}",
                query_param(consent, "state")
            ),
            Some(cookie),
        )
        .await
    }

    /// Walk one whole app login and return the callback's answer.
    async fn run_app_login(&self) -> (StatusCode, HeaderMap, String) {
        let (consent, cookie) = self.start_app_login().await;
        self.finish_login(&consent, &cookie).await
    }
}

/// The PKCE S256 transform: base64url, unpadded, of SHA-256 over the verifier.
fn s256(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Percent-encode a form VALUE, so a minted invite code posts as itself.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn query_param(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// The happy path, asserted at every point the design makes a promise.
#[tokio::test]
async fn a_console_login_ends_at_the_tenants_own_console_with_a_pairing_code() {
    let h = Harness::new().await;

    let (consent, cookie) = h.start_login(LABEL).await;
    assert!(consent.contains("/authorize"), "{consent}");

    // IDENTITY ONLY. No Gmail scope of any kind, and no offline access: this is
    // a login, so Google is never asked for a credential that outlives it.
    let scope = query_param(&consent, "scope");
    assert_eq!(
        scope.split(' ').collect::<Vec<_>>(),
        vec!["openid", "email"]
    );
    assert!(!scope.contains("gmail"), "{scope}");
    assert!(!consent.contains("access_type"), "{consent}");
    assert_eq!(query_param(&consent, "code_challenge_method"), "S256");

    // Hold the exchange to the challenge that rode on THIS consent URL, so the
    // verifier is proven rather than merely present.
    h.rec.lock().unwrap().expected_challenge = Some(query_param(&consent, "code_challenge"));

    let (status, headers, body) = h
        .get(
            &format!(
                "/oauth/callback?code=the-auth-code&state={}",
                query_param(&consent, "state")
            ),
            Some(&cookie),
        )
        .await;
    assert_eq!(status, StatusCode::FOUND, "{body}");

    // THE CENTRAL ASSERTION: the destination is EXACTLY what this deployment
    // constructs from its own base domain, the validated label, and the warden's
    // code. Equality, not a `contains`, because the property under test is that
    // nothing else can get into this URL.
    let location = headers[header::LOCATION].to_str().unwrap();
    assert_eq!(
        location,
        format!("https://{LABEL}.passband.test/console/callback?code={PAIR_CODE}")
    );
    assert!(!location.contains(ACCESS_TOKEN));
    assert!(!location.contains(REFRESH_TOKEN));
    // A live pairing code sits in that URL, so nothing may cache it and it must
    // not ride out as a referer.
    assert_eq!(headers[header::CACHE_CONTROL], "no-store, no-cache");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");

    // The session cookie is cleared on the way out, exactly as the signup
    // callback clears its own.
    let set_cookie = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect::<Vec<_>>()
        .join("|");
    assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");

    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.pair_calls, vec![LABEL.to_string()]);
    assert_eq!(rec.warden_bearers, vec!["Bearer warden-bearer".to_string()]);
    // Google got the exchange, with PKCE and our own redirect URI.
    let form: std::collections::HashMap<_, _> = rec.token_form.iter().cloned().collect();
    assert_eq!(form.get("grant_type").unwrap(), "authorization_code");
    assert!(!form.get("code_verifier").unwrap().is_empty());
    assert_eq!(
        form.get("redirect_uri").unwrap(),
        "https://signup.passband.test/oauth/callback"
    );
}

/// THE ORACLE THAT IS NOT THERE: a well-formed label nobody has provisioned is
/// answered exactly like a real one, so this route cannot be asked which hosted
/// addresses exist.
///
/// Asserted parameter by parameter rather than by status alone: a 302 whose
/// scope set or prompt differed would be the same oracle wearing a different
/// hat. Only `state` and the PKCE challenge differ, and both are per-session by
/// construction.
#[tokio::test]
async fn an_unknown_tenant_is_sent_to_google_exactly_like_a_real_one() {
    let h = Harness::new().await;

    let (real_status, real_headers, _) =
        h.get(&format!("/console/auth?tenant={LABEL}"), None).await;
    let (unknown_status, unknown_headers, _) =
        h.get("/console/auth?tenant=nosuchtenant", None).await;

    assert_eq!(real_status, StatusCode::FOUND);
    assert_eq!(unknown_status, real_status, "an existence oracle");

    let shape = |headers: &HeaderMap| {
        let location = headers[header::LOCATION].to_str().unwrap().to_string();
        let url = url::Url::parse(&location).unwrap();
        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "state" && k != "code_challenge")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        pairs.sort();
        (
            url.origin().ascii_serialization(),
            url.path().to_string(),
            pairs,
        )
    };
    assert_eq!(shape(&real_headers), shape(&unknown_headers));

    // Even the cookie says nothing: both opened a session.
    assert!(real_headers.get(header::SET_COOKIE).is_some());
    assert!(unknown_headers.get(header::SET_COOKIE).is_some());

    assert!(
        h.rec.lock().unwrap().pair_calls.is_empty(),
        "nothing reaches the warden before the mailbox is proved"
    );
}

/// The ONLY thing this route refuses at the door is a label that could not be a
/// label at all, and every one of those is the same page. A shape refusal tells
/// a stranger nothing they did not type themselves.
#[tokio::test]
async fn only_a_malformed_label_is_refused_at_the_door() {
    let h = Harness::new().await;

    let mut answers = Vec::new();
    for tenant in [
        "",                 // no label at all
        "ab",               // too short
        "www",              // reserved, so it can never name a tenant
        "ada..x",           // outside the allowlist
        "../../etc/passwd", // a path, not a label
        "ada%00",
    ] {
        let (status, headers, body) = h.get(&format!("/console/auth?tenant={tenant}"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{tenant}: {body}");
        assert!(
            headers.get(header::SET_COOKIE).is_none(),
            "{tenant}: a refused login opens no session"
        );
        answers.push(body);
    }

    let first = &answers[0];
    for (i, body) in answers.iter().enumerate() {
        assert_eq!(body, first, "refusal {i} differs from the others");
    }
    assert!(first.contains("could not be completed"), "{first}");
    // Nothing about which rule was broken.
    assert!(!first.contains("reserved"), "{first}");
    assert!(!first.contains("too short"), "{first}");

    let rec = h.rec.lock().unwrap();
    assert!(rec.token_form.is_empty(), "nothing reached Google");
    assert!(rec.pair_calls.is_empty(), "nor the warden");
}

/// A label folds the way the label module folds it, and a real tenant reached
/// through another capitalization is still a 302 to Google.
#[tokio::test]
async fn a_label_is_folded_before_it_is_used() {
    let h = Harness::new().await;
    let (status, _, body) = h.get("/console/auth?tenant=ADA", None).await;
    assert_eq!(status, StatusCode::FOUND, "{body}");
}

/// The wrong Google account is refused with the SAME page a tenant that does not
/// exist gets, and the warden is never asked to mint anything.
#[tokio::test]
async fn a_login_from_another_mailbox_is_refused_uniformly() {
    let h = Harness::new().await;
    h.rec.lock().unwrap().signed_in_as = Some("someone-else@example.com".to_string());

    let (status, headers, body) = h.run_login(LABEL).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(headers.get(header::LOCATION).is_none(), "{body}");

    // Byte for byte the answer a tenant nobody provisioned gets, on the surface
    // where BOTH are decided: no oracle telling "wrong mailbox" from "no such
    // mailbox", which is what lets the hop itself answer every label alike.
    let (unknown_status, _, unknown_body) = h.run_login("nosuchtenant").await;
    assert_eq!(status, unknown_status);
    assert_eq!(body, unknown_body);
    // ...and nothing about the address that was signed in, or the one that owns
    // the mailbox, reaches the page.
    assert!(!body.contains("someone-else"), "{body}");
    assert!(!body.contains(MAILBOX), "{body}");

    let rec = h.rec.lock().unwrap();
    assert!(!rec.token_form.is_empty(), "the exchange was attempted");
    assert!(
        rec.pair_calls.is_empty(),
        "nothing reaches the warden until the mailbox is proved"
    );
}

/// The same mailbox in another capitalization is the same person. The store
/// normalizes on insert, and the comparison normalizes what Google says.
#[tokio::test]
async fn the_owning_mailbox_is_matched_however_google_spells_it() {
    let h = Harness::new().await;
    h.rec.lock().unwrap().signed_in_as = Some("ADA@Example.COM".to_string());

    let (status, headers, body) = h.run_login(LABEL).await;
    assert_eq!(status, StatusCode::FOUND, "{body}");
    assert_eq!(
        headers[header::LOCATION].to_str().unwrap(),
        format!("https://{LABEL}.passband.test/console/callback?code={PAIR_CODE}")
    );
}

/// An address Google itself will not vouch for is not an identity, and neither
/// is one it says NOTHING about: an absent `email_verified` is a missing
/// promise, not a quiet yes. Refused like any other, and nothing is minted.
#[tokio::test]
async fn an_unvouched_address_is_not_an_identity() {
    for unvouched in [
        Some(Value::Bool(false)),
        Some(Value::String("false".into())),
        // ABSENT. The claim is simply not in the answer.
        None,
    ] {
        let h = Harness::new().await;
        h.rec.lock().unwrap().email_verified = unvouched.clone();

        let (status, _, body) = h.run_login(LABEL).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{unvouched:?}: {body}");
        assert!(body.contains("could not be completed"), "{body}");
        assert!(h.rec.lock().unwrap().pair_calls.is_empty(), "{unvouched:?}");
    }
}

/// ...and both spellings of a PRESENT, true claim are accepted, because Google
/// has shipped each and a check that knew one would refuse every real login.
#[tokio::test]
async fn either_spelling_of_a_verified_claim_is_accepted() {
    for vouched in [Value::Bool(true), Value::String("true".into())] {
        let h = Harness::new().await;
        h.rec.lock().unwrap().email_verified = Some(vouched.clone());

        let (status, headers, body) = h.run_login(LABEL).await;
        assert_eq!(status, StatusCode::FOUND, "{vouched:?}: {body}");
        assert_eq!(
            headers[header::LOCATION].to_str().unwrap(),
            format!("https://{LABEL}.passband.test/console/callback?code={PAIR_CODE}")
        );
    }
}

/// The SCOPE Google echoes is the other place a spelling can differ: the short
/// name goes out on the request and the canonical URL comes back on the answer.
/// Either one satisfies the console flow's identity requirement.
#[tokio::test]
async fn either_email_scope_spelling_is_accepted() {
    for reported in [
        "openid https://www.googleapis.com/auth/userinfo.email",
        "openid email",
        // Google unions grants across a Cloud project, so more than was asked
        // for still covers what was asked for.
        "openid email profile",
    ] {
        let h = Harness::new().await;
        h.rec.lock().unwrap().granted_scope = reported.to_string();

        let (status, _, body) = h.run_login(LABEL).await;
        assert_eq!(status, StatusCode::FOUND, "{reported}: {body}");
    }

    // ...and a grant that is short of identity is still refused.
    let h = Harness::new().await;
    h.rec.lock().unwrap().granted_scope = "openid".to_string();
    let (status, _, body) = h.run_login(LABEL).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(h.rec.lock().unwrap().pair_calls.is_empty());
}

/// The console consent's own parameters, asserted where the flow is driven end
/// to end rather than only in the unit test that builds the URL.
///
/// `prompt=select_account` is the affordance that makes a two-account browser
/// work: without it Google silently picks one and the mailbox check refuses
/// somebody who did nothing wrong. NO `access_type`, so Google issues no refresh
/// token for a flow that has nowhere to put one.
#[tokio::test]
async fn the_console_consent_asks_google_to_pick_an_account_and_nothing_more() {
    let h = Harness::new().await;
    let (consent, _) = h.start_login(LABEL).await;

    assert_eq!(query_param(&consent, "prompt"), "select_account");
    let url = url::Url::parse(&consent).unwrap();
    assert!(
        url.query_pairs().all(|(k, _)| k != "access_type"),
        "{consent}"
    );
    let scope = query_param(&consent, "scope");
    assert_eq!(
        scope.split(' ').collect::<Vec<_>>(),
        vec!["openid", "email"]
    );
    assert!(!scope.contains("gmail"), "{scope}");
}

/// A warden that will not mint is not the user's fault and not an identity
/// answer: it gets its own retriable page, which says nothing about who they are
/// or whether the mailbox exists.
#[tokio::test]
async fn a_warden_that_cannot_mint_says_try_again() {
    let h = Harness::new().await;
    h.rec.lock().unwrap().pair_status = Some(500);

    let (status, headers, body) = h.run_login(LABEL).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(headers.get(header::LOCATION).is_none());
    assert!(body.contains("try again"), "{body}");
    assert_eq!(h.rec.lock().unwrap().pair_calls.len(), 1);
}

/// The console callback is one-shot, like every other: a replayed code and
/// cookie find nothing, and no second pairing code is minted.
#[tokio::test]
async fn a_replayed_console_callback_finds_nothing() {
    let h = Harness::new().await;
    let (consent, cookie) = h.start_login(LABEL).await;
    let uri = format!(
        "/oauth/callback?code=c1&state={}",
        query_param(&consent, "state")
    );

    let (first, _, _) = h.get(&uri, Some(&cookie)).await;
    assert_eq!(first, StatusCode::FOUND);
    let (second, _, body) = h.get(&uri, Some(&cookie)).await;
    assert_eq!(second, StatusCode::BAD_REQUEST);
    assert!(body.contains("could not be verified"), "{body}");
    assert_eq!(h.rec.lock().unwrap().pair_calls.len(), 1);
}

/// A callback whose `state` does not match the session is refused before
/// anything is exchanged.
#[tokio::test]
async fn a_console_state_mismatch_is_refused() {
    let h = Harness::new().await;
    let (_, cookie) = h.start_login(LABEL).await;

    let (status, _, body) = h
        .get("/oauth/callback?code=c1&state=not-the-state", Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("could not be verified"), "{body}");

    let rec = h.rec.lock().unwrap();
    assert!(rec.token_form.is_empty(), "no exchange was attempted");
    assert!(rec.pair_calls.is_empty());
}

/// A PERFECTLY SIGNED cookie that claims the wrong thing about its own session
/// is refused, which is the one forgery a valid MAC cannot rule out: the
/// server-side session is the authority on which flow this is, and a console
/// login that arrives claiming an invite row is not one.
#[tokio::test]
async fn a_cookie_that_claims_an_invite_cannot_drive_a_console_session() {
    let h = Harness::new().await;
    let (consent, cookie) = h.start_login(LABEL).await;

    // The session id from the real cookie, re-signed with a claim that says this
    // is a signup spending invite row 1.
    let value = cookie.split_once('=').unwrap().1;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.split_once('.').unwrap().0)
        .unwrap();
    let real: Value = serde_json::from_slice(&payload).unwrap();
    assert!(
        real.get("invite").is_none(),
        "a console claim carries no invite: {real}"
    );

    let forged = cookie::sign(
        &COOKIE_KEY,
        &SessionClaim {
            sid: real["sid"].as_str().unwrap().to_string(),
            label: LABEL.to_string(),
            invite: Some(1),
            app: false,
            iat: chrono::Utc::now().timestamp(),
        },
    );

    let (status, _, body) = h
        .get(
            &format!(
                "/oauth/callback?code=c1&state={}",
                query_param(&consent, "state")
            ),
            Some(&format!("passband_signup={forged}")),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("could not be verified"), "{body}");

    let rec = h.rec.lock().unwrap();
    assert!(rec.token_form.is_empty(), "nothing was exchanged");
    assert!(rec.pair_calls.is_empty());
}

/// The signup surface is untouched by any of this: the form still asks for the
/// three Gmail permissions, in the same words, on the same route, and the
/// consent it builds still carries the offline parameters that make a refresh
/// token exist.
///
/// A regression guard rather than a duplicate of `signup_flow.rs`. The scope set
/// and the consent parameters are chosen PER FLOW, and the way that goes wrong
/// is quietly, on the flow nobody was looking at: this file is where the console
/// flow is changed, so this is where the other one is held still.
#[tokio::test]
async fn the_signup_surface_is_unchanged() {
    let h = Harness::new().await;
    let (status, _, body) = h.get("/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("gmail.readonly"), "{body}");
    assert!(body.contains("gmail.modify"), "{body}");
    assert!(body.contains("gmail.send"), "{body}");
    assert!(body.contains(r#"action="/signup""#), "{body}");

    // ...and the consent a real post builds, parameter for parameter.
    let (status, headers, body) = h
        .post_form(
            "/signup",
            format!("label=newtenant&invite={}", urlencode(&h.invite_code)),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let consent = headers[header::LOCATION].to_str().unwrap().to_string();

    let scope = query_param(&consent, "scope");
    assert_eq!(
        scope.split(' ').collect::<Vec<_>>(),
        vec![
            "https://www.googleapis.com/auth/gmail.readonly",
            "https://www.googleapis.com/auth/gmail.modify",
            "https://www.googleapis.com/auth/gmail.send",
        ],
        "{consent}"
    );
    assert_eq!(query_param(&consent, "access_type"), "offline");
    assert_eq!(query_param(&consent, "prompt"), "consent");
    assert_eq!(query_param(&consent, "code_challenge_method"), "S256");
    assert_eq!(
        query_param(&consent, "redirect_uri"),
        "https://signup.passband.test/oauth/callback"
    );
    // The two flows share this file and share nothing else: a console consent
    // asks for identity, online, with a different prompt.
    let (console_consent, _) = h.start_login(LABEL).await;
    assert_ne!(
        query_param(&console_consent, "scope"),
        scope,
        "the console flow has taken the signup scope set"
    );
    assert_ne!(query_param(&console_consent, "prompt"), "consent");
}

/// THE REDIRECT IS CONSTRUCTED, NEVER ECHOED, and this is the hostile version of
/// the happy path's assertion: a tenant parameter that is trying to BE a URL
/// never reaches a `Location`.
///
/// The only two places a caller-influenced string could land are the consent hop
/// (which goes to Google) and the console hand-back (which goes to this
/// deployment's own base domain), so both are checked against every host an
/// attacker would want to see there.
#[tokio::test]
async fn a_hostile_tenant_parameter_never_reaches_a_location() {
    let h = Harness::new().await;

    for tenant in [
        "evil.test",
        "ada.evil.test",
        "%2F%2Fevil.test",
        "ada%2F..%2F..%2Fevil.test",
        "ada%00.evil.test",
        "ada%23@evil.test",
        "ada%3Fnext%3Dhttps%3A%2F%2Fevil.test",
    ] {
        let (status, headers, body) = h.get(&format!("/console/auth?tenant={tenant}"), None).await;
        let location = headers
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        assert!(!location.contains("evil.test"), "{tenant} -> {location}");
        match status {
            // A shape refusal, which is what most of these are.
            StatusCode::BAD_REQUEST => assert!(location.is_empty(), "{tenant}"),
            // Or a label that survived validation, in which case the only place
            // it may go is the mock Google's consent endpoint.
            StatusCode::FOUND => assert!(location.contains("/authorize"), "{tenant} -> {location}"),
            other => panic!("{tenant} answered {other}: {body}"),
        }
    }

    // And the hand-back on the ONE path that produces one is built from this
    // deployment's base domain and the warden's code, with nothing else in it.
    let (_, headers, _) = h.run_login(LABEL).await;
    assert_eq!(
        headers[header::LOCATION].to_str().unwrap(),
        format!("https://{LABEL}.passband.test/console/callback?code={PAIR_CODE}")
    );
}

/// NO CONSOLE REFUSAL SENDS ANYBODY TO THE SIGNUP FORM. Somebody signing in to a
/// mailbox they already own must never be handed the page for making a second
/// one, so every refusal on this flow is a console page: console copy, and the
/// only link (when there is one) back to their own console.
#[tokio::test]
async fn no_console_refusal_offers_the_signup_form() {
    let h = Harness::new().await;

    // A shape refusal at the door.
    let (_, _, malformed) = h.get("/console/auth?tenant=ab", None).await;

    // A session that cannot be verified: the state does not match.
    let (_, cookie) = h.start_login(LABEL).await;
    let (_, _, mismatched) = h
        .get("/oauth/callback?code=c1&state=not-the-state", Some(&cookie))
        .await;

    // A replayed callback: the session is one-shot and the second try finds
    // nothing at all.
    let (consent, cookie) = h.start_login(LABEL).await;
    let uri = format!(
        "/oauth/callback?code=c1&state={}",
        query_param(&consent, "state")
    );
    let (_, _, _) = h.get(&uri, Some(&cookie)).await;
    let (_, _, replayed) = h.get(&uri, Some(&cookie)).await;

    // The wrong Google account.
    h.rec.lock().unwrap().signed_in_as = Some("someone-else@example.com".to_string());
    let (_, _, wrong_account) = h.run_login(LABEL).await;

    for (name, body) in [
        ("malformed label", &malformed),
        ("state mismatch", &mismatched),
        ("replayed callback", &replayed),
        ("wrong account", &wrong_account),
    ] {
        assert!(
            !body.contains(r#"href="/""#),
            "{name} links to signup: {body}"
        );
        assert!(!body.contains("Start again"), "{name}: {body}");
        assert!(!body.contains("invite"), "{name}: {body}");
        assert!(!body.contains("signup"), "{name}: {body}");
    }

    // The two that know which console this was link back to it, which is the
    // only link that is ever right on this flow.
    for (name, body) in [("state mismatch", &mismatched), ("replayed", &replayed)] {
        assert!(
            body.contains(&format!("https://{LABEL}.passband.test")),
            "{name}: {body}"
        );
    }
}

/// `GET /console/auth` carries its OWN token bucket, tighter than signup's: it
/// is the one route that opens a server-side session with nothing presented at
/// all, so a stranger looping it must not be able to spend the budget (or the
/// session table) a paying signup needs.
#[tokio::test]
async fn the_console_hop_has_its_own_budget() {
    let h = Harness::new().await;

    let capacity = CONSOLE_AUTH_REQUESTS_PER_MINUTE as usize;
    let mut refused = 0;
    for _ in 0..(capacity + 5) {
        let (status, _, _) = h.get(&format!("/console/auth?tenant={LABEL}"), None).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            refused += 1;
        }
    }
    assert_eq!(refused, 5, "the bucket holds exactly its stated capacity");

    // ...and signup's own budget is untouched by the flood, because it is a
    // different bucket. (The post is refused on its invite, which is a 200 form
    // re-render: what matters here is that it is not a 429.)
    let (status, _, body) = h
        .post_form("/signup", "label=newtenant&invite=WRONG-CODE".to_string())
        .await;
    assert_eq!(status, StatusCode::OK, "signup paid for the console flood");
    assert!(body.contains("invite code"), "{body}");
}

// ---- the app hop ----------------------------------------------------------

/// The happy path, asserted at every point the design makes a promise.
///
/// The DEEP LINK is the assertion that matters. It is compared whole rather than
/// with a `contains`, because the property under test is that nothing except
/// this deployment's own tenant URL and the warden's own code can get into it.
#[tokio::test]
async fn an_app_login_ends_at_a_deep_link_for_the_mailboxs_own_tenant() {
    let h = Harness::new().await;

    // NO INPUT. The route was reached with no query string at all, which is why
    // there is nothing here for a stranger to guess at or to smuggle a redirect
    // through.
    let (consent, cookie) = h.start_app_login().await;
    assert!(consent.contains("/authorize"), "{consent}");

    // IDENTITY ONLY, the same ask a console login makes: no Gmail scope, no
    // offline access. Connecting an app is not a reason to hold a second key to
    // somebody's mail, and the daemon already holds the first.
    let scope = query_param(&consent, "scope");
    assert_eq!(
        scope.split(' ').collect::<Vec<_>>(),
        vec!["openid", "email"]
    );
    assert!(!scope.contains("gmail"), "{scope}");
    assert!(!consent.contains("access_type"), "{consent}");
    assert_eq!(query_param(&consent, "code_challenge_method"), "S256");

    // Hold the exchange to the challenge that rode on THIS consent URL.
    h.rec.lock().unwrap().expected_challenge = Some(query_param(&consent, "code_challenge"));

    let (status, headers, body) = h.finish_login(&consent, &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // THE CENTRAL ASSERTION.
    let link =
        format!("passband://pair?url=https%3A%2F%2F{LABEL}.passband.test&amp;code={PAIR_CODE}");
    assert!(body.contains(&link), "{body}");
    // The mailbox is named, because this is the one screen that can confirm
    // Google picked the account the person meant.
    assert!(body.contains(MAILBOX), "{body}");
    // ...and no token is on it, in either spelling.
    assert!(!body.contains(ACCESS_TOKEN));
    assert!(!body.contains(REFRESH_TOKEN));

    // A live pairing code is in that body, so nothing may cache it and it must
    // not ride out as a referer when the link is pressed.
    assert_eq!(headers[header::CACHE_CONTROL], "no-store, no-cache");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");

    // The session cookie is cleared on the way out, like every other terminal
    // answer from the callback.
    let set_cookie = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect::<Vec<_>>()
        .join("|");
    assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");

    // The warden minted for the label the LOOKUP found, which nobody sent.
    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.pair_calls, vec![LABEL.to_string()]);
    assert_eq!(rec.warden_bearers, vec!["Bearer warden-bearer".to_string()]);
}

/// The reverse lookup normalizes the way the store does, so the capitalized
/// answer Google actually gives finds the row rather than missing it. The tenant
/// in the harness is stored as `Ada@Example.com`; Google says `ada@example.com`.
#[tokio::test]
async fn an_app_login_finds_its_tenant_whatever_case_google_answers_in() {
    let h = Harness::new().await;
    h.rec.lock().unwrap().signed_in_as = Some("ADA@EXAMPLE.COM".into());

    let (status, _, body) = h.run_app_login().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains(PAIR_CODE), "{body}");
    assert_eq!(h.rec.lock().unwrap().pair_calls, vec![LABEL.to_string()]);
}

/// A real Google account with no mailbox here is told so PLAINLY, unlike every
/// refusal on the console hop.
///
/// That is safe precisely because the flow has no input: the only address this
/// answer can be asked about is one the asker just proved they hold at Google,
/// so there is no directory to walk and nobody else's mailbox to test. And
/// nothing reaches the warden, so a stranger cannot make a pairing code exist.
#[tokio::test]
async fn an_app_login_with_no_mailbox_here_says_so_and_mints_nothing() {
    let h = Harness::new().await;
    h.rec.lock().unwrap().signed_in_as = Some("grace@example.com".into());

    let (status, _, body) = h.run_app_login().await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("No Passband mailbox"), "{body}");
    // No code, no link, nothing minted.
    assert!(!body.contains(PAIR_CODE), "{body}");
    assert!(!body.contains("passband://"), "{body}");
    assert!(
        h.rec.lock().unwrap().pair_calls.is_empty(),
        "warden touched"
    );
}

/// An address Google will not VOUCH for is not an identity, so it is not an app
/// login either. Absent is not verified, which is the case a provider (or
/// anything answering as one) produces by leaving the claim out.
#[tokio::test]
async fn an_unvouched_google_answer_is_not_an_app_login() {
    for verified in [None, Some(Value::Bool(false))] {
        let h = Harness::new().await;
        h.rec.lock().unwrap().email_verified = verified.clone();

        let (status, _, body) = h.run_app_login().await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{verified:?}: {body}");
        assert!(h.rec.lock().unwrap().pair_calls.is_empty(), "{verified:?}");
    }
}

/// A warden that will not mint says TRY AGAIN, not "check who you are": the
/// person signed in correctly and this service is the thing that failed.
#[tokio::test]
async fn a_warden_that_will_not_mint_is_not_the_users_fault() {
    let h = Harness::new().await;
    h.rec.lock().unwrap().pair_status = Some(500);

    let (status, _, body) = h.run_app_login().await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("try again"), "{body}");
    assert!(!body.contains("passband://"), "{body}");
}

/// THE CROSS-FLOW FORGERY: a cookie with a VALID MAC, naming a live session, but
/// claiming to be the other kind of login.
///
/// The MAC cannot rule this out on its own, so the callback compares the flow
/// the cookie claims against the flow the session records and refuses on a
/// mismatch. Without that check, an app session's callback would be steered by
/// the cookie into rendering a console refusal that links to a label the cookie
/// chose.
#[tokio::test]
async fn a_cookie_cannot_change_which_login_a_session_is() {
    let h = Harness::new().await;
    let (consent, cookie) = h.start_app_login().await;

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cookie.split_once('=').unwrap().1.split_once('.').unwrap().0)
        .unwrap();
    let real: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(real["app"], Value::Bool(true), "{real}");
    assert_eq!(real["label"], "", "an app login names no tenant: {real}");

    // Same session id, same empty label, but claiming to be a console login.
    let forged = cookie::sign(
        &COOKIE_KEY,
        &SessionClaim {
            sid: real["sid"].as_str().unwrap().to_string(),
            label: String::new(),
            invite: None,
            app: false,
            iat: chrono::Utc::now().timestamp(),
        },
    );

    let (status, _, body) = h
        .get(
            &format!(
                "/oauth/callback?code=c1&state={}",
                query_param(&consent, "state")
            ),
            Some(&format!("passband_signup={forged}")),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(!body.contains("passband://"), "{body}");
    assert!(
        h.rec.lock().unwrap().pair_calls.is_empty(),
        "warden touched"
    );
}

/// ONE BUDGET FOR BOTH HOPS. They are the same errand through the same consent,
/// and a bucket each would let one client spend twice by alternating them.
#[tokio::test]
async fn the_two_login_hops_share_one_budget() {
    let h = Harness::new().await;

    let capacity = CONSOLE_AUTH_REQUESTS_PER_MINUTE as usize;
    let mut refused = 0;
    for i in 0..(capacity + 5) {
        // Alternating on purpose: what is under test is that the second route
        // does not come with a second allowance.
        let uri = if i % 2 == 0 {
            format!("/console/auth?tenant={LABEL}")
        } else {
            "/app/auth".to_string()
        };
        let (status, _, _) = h.get(&uri, None).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            refused += 1;
        }
    }
    assert_eq!(refused, 5, "the shared bucket holds one stated capacity");
}
