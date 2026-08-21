//! The signup flow end to end, against a MOCK Google and a MOCK warden that
//! speaks wire v2.
//!
//! Both mocks are real axum servers on ephemeral loopback ports, the way
//! squelch-core tests the APNs relay, because the properties under test are
//! properties OF THE WIRE:
//!
//! - provisioning is TWO calls, and the credential rides only on the second,
//! - the ciphertext is sealed to EXACTLY the recipient the first call answered
//!   with, and what is inside it is a credentials-file slot map (not a bare
//!   token, which would decrypt into an empty map on the daemon),
//! - a failure between the two calls leaves a retriable signup: the address is
//!   held for that mailbox, the invite is NOT burned, and the retry lands on the
//!   same recipient,
//! - Google receives the PKCE verifier that matches the challenge the consent
//!   URL carried,
//! - the success page carries the pairing code, the tenant URL, and the deep
//!   link.
//!
//! The MOCK WARDEN mints a per-tenant age identity, exactly as the real one
//! does, and keeps it. This test process is the only place both halves of a key
//! ever exist; the real control plane never sees an identity at all.
//!
//! Nothing here touches a real Google endpoint, a real cluster, a real port
//! 8848, or any store outside the throwaway Postgres schema `common` makes.
//! `Config` is constructed directly rather than read from the environment,
//! which is exactly why `token_url`/`profile_url` are fields:
//! `Config::from_env` pins Google's and nothing can move them.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    body::to_bytes,
    extract::State as AxumState,
    http::{HeaderMap, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use base64::Engine as _;
use serde_json::{Value, json};
use squelch_control::config::{
    BifrostConfig, Config, DEFAULT_ASSISTANT_BUDGET_USD, DEFAULT_LLM_BUDGET_USD,
};
use squelch_control::warden::HttpWarden;
use squelch_control::{ControlState, activation, invites, router, seal};
use tower::ServiceExt as _;

/// The throwaway Postgres schema every harness in this file is built on.
mod common;

/// The refresh token the mock Google hands out. Every assertion that this
/// never leaves the process in the clear greps for this exact string.
const REFRESH_TOKEN: &str = "1//THE-SECRET-REFRESH-TOKEN";
const ACCESS_TOKEN: &str = "ya29.THE-SECRET-ACCESS-TOKEN";
const MAILBOX: &str = "ada@example.com";
const READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";
const MODIFY: &str = "https://www.googleapis.com/auth/gmail.modify";
const SEND: &str = "https://www.googleapis.com/auth/gmail.send";

/// What a consent that left every box checked reports back, the way Google
/// spells a scope set: space delimited, in one string.
fn full_grant() -> String {
    format!("{READONLY} {MODIFY} {SEND}")
}

/// One tenant as the mock warden holds it: the identity it minted (which never
/// leaves the warden in production), the mailbox that reserved the label, and
/// whether a credential has been installed.
struct MockTenant {
    account_email: String,
    identity: age::x25519::Identity,
    recipient: String,
    provisioned: bool,
}

/// What the mocks recorded, so the test can assert on the bytes each service
/// actually received.
#[derive(Default)]
struct Recorder {
    /// Form fields of the token exchange.
    token_form: Vec<(String, String)>,
    /// Bodies posted to `POST /v1/tenants` (call 1).
    create_bodies: Vec<Value>,
    /// `(label, body)` seen on `PUT /v1/tenants/{label}/credentials` (call 2).
    /// Recorded BEFORE the mock decides whether to fail, so a failed install is
    /// still visible to the assertions.
    credential_puts: Vec<(String, Value)>,
    /// `(label, body)` seen on `PUT /v1/tenants/{label}/llm-key`. The whole
    /// body, so absence of a slot can be asserted, not just presence.
    llm_key_puts: Vec<(String, Value)>,
    /// Bodies posted to the mock Bifrost's mint route.
    bifrost_mint_bodies: Vec<Value>,
    /// Authorization headers the mock Bifrost saw, on every route. The live
    /// gateway takes HTTP Basic with the admin `username:password`.
    bifrost_auths: Vec<String>,
    /// Key NAMES the mock Bifrost should refuse to mint, so one of a signup's
    /// two mints can fail while the other lands.
    bifrost_fail_names: Vec<String>,
    /// Bearer values the warden saw, on every route.
    warden_bearers: Vec<String>,
    /// Labels asked about via `GET /v1/tenants/{label}`.
    status_lookups: Vec<String>,
    /// Labels the warden should report as ALREADY LIVE (somebody else's).
    taken: Vec<String>,
    /// Tenants the warden has minted an identity for, by label.
    tenants: BTreeMap<String, MockTenant>,
    /// When true, call 2 fails the way an apply that never went Ready does.
    fail_credentials: bool,
    /// When true, the llm-key install answers 503 `llm_not_configured`, the
    /// way a warden whose LLM wiring is absent refuses a key it cannot place.
    fail_llm_key: bool,
    /// When set, call 2 answers with this status instead of doing anything. For
    /// the statuses that are a WIRE disagreement rather than an outcome.
    credentials_status: Option<u16>,
    /// When set, the mock Google verifies PKCE the way Google does: the
    /// presented verifier must S256-hash to this challenge.
    expected_challenge: Option<String>,
    /// The `scope` the mock Google reports on the token. `None` is the whole set
    /// signup asks for; `Some` is a user who unchecked something.
    granted_scope: Option<String>,
    /// What `GET /v1/tenants/{label}/devices` answers, by label: a present
    /// entry is a 200 with this `first_paired_at` (None = the null body,
    /// nobody paired yet), an absent one is a 404. Labels listed in
    /// `devices_unavailable` answer 503 `not_running` first — the mid-roll
    /// tenant the poller must leave for the next pass.
    devices_paired: BTreeMap<String, Option<String>>,
    devices_unavailable: Vec<String>,
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
                    let (expected_challenge, granted_scope) = {
                        let mut r = rec.lock().unwrap();
                        r.token_form = form.clone();
                        (
                            r.expected_challenge.clone(),
                            r.granted_scope.clone().unwrap_or_else(full_grant),
                        )
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
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error":"invalid_grant"})),
                        )
                            .into_response();
                    }
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
            "/profile",
            get(|| async { Json(json!({ "emailAddress": MAILBOX })) }),
        )
        .with_state(rec);
    spawn(app).await
}

fn bearer_of(headers: &HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn str_field(v: &Value, name: &str) -> String {
    v.get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn json_status(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({ "error": reason }))).into_response()
}

/// A mock warden implementing the control -> warden contract, v2.
async fn spawn_warden(rec: Shared) -> String {
    let app = Router::new()
        .route(
            "/v1/tenants",
            post(
                |AxumState(rec): AxumState<Shared>, headers: HeaderMap, body: String| async move {
                    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let mut r = rec.lock().unwrap();
                    r.create_bodies.push(parsed.clone());
                    r.warden_bearers.push(bearer_of(&headers));

                    let label = str_field(&parsed, "label");
                    let account_email = str_field(&parsed, "account_email");
                    if label.is_empty() || account_email.is_empty() {
                        return json_status(StatusCode::UNPROCESSABLE_ENTITY, "invalid");
                    }
                    if r.taken.contains(&label) {
                        return json_status(StatusCode::CONFLICT, "exists");
                    }
                    // THE IDEMPOTENCY THE CONTRACT PINS: a second create for a
                    // PENDING label from the SAME mailbox answers with the SAME
                    // recipient. A provisioned label, or a different mailbox,
                    // is a duplicate and gets 409.
                    if let Some(existing) = r.tenants.get(&label) {
                        if existing.provisioned || existing.account_email != account_email {
                            return json_status(StatusCode::CONFLICT, "exists");
                        }
                        return (
                            StatusCode::CREATED,
                            Json(json!({ "recipient": existing.recipient })),
                        )
                            .into_response();
                    }

                    let identity = age::x25519::Identity::generate();
                    let recipient = identity.to_public().to_string();
                    r.tenants.insert(
                        label,
                        MockTenant {
                            account_email,
                            identity,
                            recipient: recipient.clone(),
                            provisioned: false,
                        },
                    );
                    (StatusCode::CREATED, Json(json!({ "recipient": recipient }))).into_response()
                },
            ),
        )
        .route(
            "/v1/tenants/{label}/credentials",
            put(
                |AxumState(rec): AxumState<Shared>,
                 axum::extract::Path(label): axum::extract::Path<String>,
                 headers: HeaderMap,
                 body: String| async move {
                    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let mut r = rec.lock().unwrap();
                    r.credential_puts.push((label.clone(), parsed));
                    r.warden_bearers.push(bearer_of(&headers));

                    if let Some(status) = r.credentials_status {
                        let status = StatusCode::from_u16(status).unwrap();
                        return json_status(status, "refused");
                    }
                    if r.fail_credentials {
                        // The apply failed, or the pod never went Ready. The
                        // tenant stays pending and keeps its identity.
                        return json_status(StatusCode::INTERNAL_SERVER_ERROR, "apply_failed");
                    }
                    match r.tenants.get_mut(&label) {
                        None => json_status(StatusCode::NOT_FOUND, "unknown"),
                        Some(t) if t.provisioned => {
                            json_status(StatusCode::CONFLICT, "provisioned")
                        }
                        Some(t) => {
                            t.provisioned = true;
                            Json(json!({
                                "pair_code": "ABCD-EFGH",
                                "pair_url": format!("https://{label}.passband.test"),
                                "deep_link": "passband://pair?url=x&code=ABCD-EFGH",
                            }))
                            .into_response()
                        }
                    }
                },
            ),
        )
        .route(
            "/v1/tenants/{label}/llm-key",
            put(
                |AxumState(rec): AxumState<Shared>,
                 axum::extract::Path(label): axum::extract::Path<String>,
                 headers: HeaderMap,
                 body: String| async move {
                    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let mut r = rec.lock().unwrap();
                    // Recorded BEFORE the mock decides whether to fail, so a
                    // refused install is still visible to the assertions.
                    r.llm_key_puts.push((label, parsed));
                    r.warden_bearers.push(bearer_of(&headers));
                    if r.fail_llm_key {
                        return json_status(StatusCode::SERVICE_UNAVAILABLE, "llm_not_configured");
                    }
                    Json(json!({})).into_response()
                },
            ),
        )
        .route(
            "/v1/tenants/{label}",
            get(
                |AxumState(rec): AxumState<Shared>,
                 axum::extract::Path(label): axum::extract::Path<String>,
                 headers: HeaderMap| async move {
                    let mut r = rec.lock().unwrap();
                    r.status_lookups.push(label.clone());
                    r.warden_bearers.push(bearer_of(&headers));
                    if r.taken.contains(&label) {
                        return Json(json!({"status":"active"})).into_response();
                    }
                    match r.tenants.get(&label) {
                        Some(t) if t.provisioned => {
                            Json(json!({"status":"active"})).into_response()
                        }
                        Some(_) => Json(json!({"status":"pending"})).into_response(),
                        None => json_status(StatusCode::NOT_FOUND, "unknown"),
                    }
                },
            ),
        )
        .route(
            "/v1/tenants/{label}/devices",
            get(
                |AxumState(rec): AxumState<Shared>,
                 axum::extract::Path(label): axum::extract::Path<String>,
                 headers: HeaderMap| async move {
                    let mut r = rec.lock().unwrap();
                    r.warden_bearers.push(bearer_of(&headers));
                    if r.devices_unavailable.contains(&label) {
                        return json_status(StatusCode::SERVICE_UNAVAILABLE, "not_running");
                    }
                    match r.devices_paired.get(&label) {
                        Some(at) => Json(json!({ "first_paired_at": at })).into_response(),
                        None => json_status(StatusCode::NOT_FOUND, "not_found"),
                    }
                },
            ),
        )
        .with_state(rec);
    spawn(app).await
}

/// The id and value the mock Bifrost mints, derived from the requested key
/// NAME so a signup's two mints — `tenant-<label>` and
/// `tenant-<label>-assistant` — stay distinguishable end to end. The control
/// plane must forward each value to the warden and keep no other copy.
fn vk_id_for(name: &str) -> String {
    format!("vk-{name}")
}

fn vk_value_for(name: &str) -> String {
    format!("sk-bf-KEY-FOR-{name}")
}

/// The Bifrost admin credential the harness configures: `username:password`,
/// sent by the client as HTTP Basic on every governance call.
const BIFROST_ADMIN: &str = "bifrost-admin:the-basic-password";

/// The provider key id the mock gateway lists, mirroring the one the live
/// gateway auto-detects.
const PROVIDER_KEY_ID: &str = "ANTHROPIC_API_KEY_auto_detected";

fn bifrost_basic_auth() -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(BIFROST_ADMIN)
    )
}

/// A mock Bifrost: the two routes a mint speaks (the provider-key listing and
/// the governance create), answering the LIVE gateway's shapes — the mint echo
/// carries the attached `budgets` and `provider_configs`, which the client
/// refuses to trust a key without.
async fn spawn_bifrost(rec: Shared) -> String {
    let app = Router::new()
        .route(
            "/api/providers/anthropic/keys",
            get(
                |AxumState(rec): AxumState<Shared>, headers: HeaderMap| async move {
                    let mut r = rec.lock().unwrap();
                    r.bifrost_auths.push(bearer_of(&headers));
                    Json(json!({ "keys": [{ "id": PROVIDER_KEY_ID, "models": [] }] }))
                        .into_response()
                },
            ),
        )
        .route(
            "/api/governance/virtual-keys",
            post(
                |AxumState(rec): AxumState<Shared>, headers: HeaderMap, body: String| async move {
                    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let name = str_field(&parsed, "name");
                    let mut r = rec.lock().unwrap();
                    // Recorded BEFORE the mock decides whether to fail, so a
                    // refused mint is still visible to the assertions.
                    r.bifrost_mint_bodies.push(parsed);
                    r.bifrost_auths.push(bearer_of(&headers));
                    if r.bifrost_fail_names.contains(&name) {
                        return json_status(StatusCode::INTERNAL_SERVER_ERROR, "mint_failed");
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "message": "Virtual key created successfully",
                            "virtual_key": {
                                "id": vk_id_for(&name),
                                "value": vk_value_for(&name),
                                "budgets": [{ "max_limit": DEFAULT_LLM_BUDGET_USD, "reset_duration": "1M" }],
                                "provider_configs": [{ "provider": "anthropic", "weight": 1 }],
                            },
                        })),
                    )
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

/// A whole control plane wired to the two mocks.
struct Harness {
    app: Router,
    state: ControlState,
    rec: Shared,
    invite_code: String,
    /// The schema this harness's store lives in, for the assertions that read a
    /// column no method returns. `ControlStore::client` is crate-private, so a
    /// test needing raw SQL opens its own connection and can only land on the
    /// same schema if it is given the same URL.
    db_url: String,
}

/// How a harness is wired to Bifrost: not at all (the trio unset), a live
/// mock, or a configured gateway that is DOWN (the fail-soft case).
enum Bifrost {
    Off,
    Mock,
    Down,
}

impl Harness {
    async fn new() -> Self {
        Self::with_bifrost(Bifrost::Off).await
    }

    async fn with_bifrost(bifrost: Bifrost) -> Self {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        let google = spawn_google(rec.clone()).await;
        let warden_url = spawn_warden(rec.clone()).await;
        let bifrost_url = match bifrost {
            Bifrost::Off => None,
            Bifrost::Mock => Some(spawn_bifrost(rec.clone()).await),
            // Port 1: nothing listens, so every mint fails the way an outage
            // fails.
            Bifrost::Down => Some("http://127.0.0.1:1".to_string()),
        };

        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_url: "https://signup.passband.test".into(),
            redirect_uri: "https://signup.passband.test/oauth/callback".into(),
            base_domain: "passband.test".into(),
            client_id: "test-client-id".into(),
            client_secret: "test-client-secret".into(),
            cookie_key: vec![42; 32],
            warden_url: warden_url.clone(),
            warden_token: "warden-bearer".into(),
            database_url: "postgres://unused".into(),
            trusted_proxy_hops: 0,
            token_url: format!("{google}/token"),
            auth_url: format!("{google}/authorize"),
            profile_url: format!("{google}/profile"),
            // Never reached on this flow: a signup's mailbox comes from Gmail's
            // profile endpoint, which is what its own grant permits. The console
            // login is the one that reads userinfo (`tests/console_auth.rs`).
            userinfo_url: format!("{google}/userinfo"),
            // The struct is constructed directly, so an http mock URL is fine
            // here; `from_env` is where https is enforced.
            bifrost: bifrost_url.map(|url| BifrostConfig {
                url,
                admin_token: BIFROST_ADMIN.into(),
                budget_usd: DEFAULT_LLM_BUDGET_USD,
                models: vec!["claude-haiku-4-5".into(), "claude-sonnet-5".into()],
                assistant_budget_usd: DEFAULT_ASSISTANT_BUDGET_USD,
                assistant_models: vec!["claude-haiku-4-5".into(), "claude-opus-4-8".into()],
            }),
            // The signup flow does not touch the waitlist; feature off, so
            // those routes are not mounted at all.
            waitlist: None,
        };

        let (store, db_url) = common::fresh_store().await;
        let minted = invites::mint().unwrap();
        store
            .insert_invite(&minted.code_hash, default_expiry())
            .await
            .unwrap();

        let warden = Arc::new(
            HttpWarden::new(warden_url, "warden-bearer".into(), Duration::from_secs(5)).unwrap(),
        );
        // The Bifrost client is DERIVED inside `ControlState::new` from
        // `config.bifrost`; there is no second wiring point to disagree with.
        let state = ControlState::new(config, store, warden).unwrap();
        Self {
            app: router(state.clone()),
            state,
            rec,
            invite_code: minted.code,
            db_url,
        }
    }

    /// Every funnel row, as `(email, account_email, tenant_label, signed_up_at,
    /// analytics_id)`. Read raw because `analytics_id` is deliberately not on
    /// `UserRow`: it must never travel beside an address.
    async fn funnel(&self) -> Vec<(String, Option<String>, Option<String>, Option<String>, String)> {
        common::raw_client(&self.db_url)
            .await
            .query(
                "SELECT email, account_email, tenant_label, signed_up_at, analytics_id
                   FROM users ORDER BY id",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
            .collect()
    }

    /// Mint another invite straight into the control store, the way
    /// `squelch-control invite issue` does.
    async fn issue_invite(&self) -> String {
        self.issue_invite_expiring(default_expiry()).await
    }

    /// ...with an expiry of the test's choosing. A past one is a code that ran
    /// out before the test started.
    async fn issue_invite_expiring(&self, expires_at: chrono::DateTime<chrono::Utc>) -> String {
        let minted = invites::mint().unwrap();
        self.state
            .store()
            .insert_invite(&minted.code_hash, expires_at)
            .await
            .unwrap();
        minted.code
    }

    /// Whether the code the harness minted is available to be held right now.
    /// The reservation is invisible from the outside, so this is how a test
    /// asserts a failed signup handed it back.
    async fn invite_is_available(&self) -> bool {
        self.state
            .store()
            .find_available_invite(&invites::hash(&self.invite_code), chrono::Utc::now())
            .await
            .unwrap()
            .is_some()
    }

    /// How the store recorded the invite: `(used_by_label, still available)`.
    async fn invite_row(&self) -> (Option<String>, bool) {
        let rows = self.state.store().list_invites().await.unwrap();
        let row = rows.last().expect("the harness minted one").clone();
        (row.used_by_label, self.invite_is_available().await)
    }

    /// The recipient the mock warden minted for `label`.
    fn recipient_of(&self, label: &str) -> String {
        self.rec.lock().unwrap().tenants[label].recipient.clone()
    }

    /// Open a ciphertext with the identity the WARDEN kept for `label`. This is
    /// the assertion the whole per-tenant-key design rests on: if the control
    /// plane sealed to anything other than the recipient call 1 answered with,
    /// this fails.
    fn open_with_tenant_identity(&self, label: &str, armor: &str) -> String {
        let rec = self.rec.lock().unwrap();
        let tenant = rec
            .tenants
            .get(label)
            .expect("the warden minted an identity for this tenant");
        let decryptor =
            age::Decryptor::new(age::armor::ArmoredReader::new(armor.as_bytes())).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(&tenant.identity as &dyn age::Identity))
            .unwrap();
        let mut plaintext = String::new();
        std::io::Read::read_to_string(&mut reader, &mut plaintext).unwrap();
        plaintext
    }

    /// The armor the control plane sent on the nth `PUT .../credentials`.
    fn credential_armor(&self, n: usize) -> (String, String) {
        let rec = self.rec.lock().unwrap();
        let (label, body) = &rec.credential_puts[n];
        (
            label.clone(),
            body["cred_read_ciphertext"]
                .as_str()
                .expect("the credential body carries armor")
                .to_string(),
        )
    }

    async fn get(
        &self,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut req = Request::builder().method("GET").uri(uri);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        self.send(req.body(axum::body::Body::empty()).unwrap())
            .await
    }

    async fn post_form(
        &self,
        uri: &str,
        body: String,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(body))
            .unwrap();
        self.send(req).await
    }

    async fn send(
        &self,
        req: Request<axum::body::Body>,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = String::from_utf8_lossy(&to_bytes(resp.into_body(), 1 << 20).await.unwrap())
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

    /// Walk one whole signup: form post, consent, callback.
    async fn run_signup(&self, label: &str, invite: &str) -> (StatusCode, String) {
        let (consent, cookie) = self.start_signup_with(label, invite).await;
        let (status, _, body) = self
            .get(
                &format!(
                    "/oauth/callback?code=the-auth-code&state={}",
                    state_param(&consent)
                ),
                Some(&cookie),
            )
            .await;
        (status, body)
    }
}

/// Whether a string is shaped like the UUIDv4 the store mints for
/// `analytics_id`: 8-4-4-4-12 lowercase hex, version nibble 4, variant in
/// `89ab`. The app holds an adopted id to this shape before it will adopt it.
fn is_uuid_shaped(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts.iter().map(|p| p.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12]
        && parts
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
        && parts[2].starts_with('4')
        && matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b')
}

/// What `squelch-control invite issue` would stamp on a code minted now.
fn default_expiry() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() + chrono::Duration::days(invites::DEFAULT_TTL_DAYS)
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
    // The consent URL is the mock's authorize endpoint, asking for all three
    // grants in ONE consent: hosted Passband ships the actions, and the daemon's
    // write path has no other way to be filled.
    assert!(consent_url.contains("/authorize"), "{consent_url}");
    let asked = query_param(&consent_url, "scope");
    assert_eq!(
        asked.split(' ').collect::<Vec<_>>(),
        vec![READONLY, MODIFY, SEND],
        "{consent_url}"
    );
    assert!(consent_url.contains("code_challenge_method=S256"));
    // ...and offline access, which is what makes Google return the refresh token
    // this whole flow exists to seal. Asserted here because the scope set and
    // these parameters are now chosen per flow, and the console login asks for
    // neither.
    assert_eq!(query_param(&consent_url, "access_type"), "offline");
    assert_eq!(query_param(&consent_url, "prompt"), "consent");

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

    // THE FUNNEL, THROUGH THE CLI DOOR. The harness's invite is the shape
    // `squelch-control invite issue` mints: a code addressed to nobody, so no
    // row named this person before they redeemed it. One appears at the
    // callback, keyed on the Google account, which is the only identity this
    // door ever learns.
    //
    // BEFORE the recorder lock below, which is held to the end of this test: a
    // std `MutexGuard` may not be alive across an await, and reading the funnel
    // is one.
    let funnel = h.funnel().await;
    assert_eq!(funnel.len(), 1, "one person, one row: {funnel:?}");
    let (email, account, label, signed_up, analytics_id) = &funnel[0];
    assert_eq!(email, MAILBOX, "known by the account that redeemed it");
    assert_eq!(account.as_deref(), Some(MAILBOX));
    assert_eq!(label.as_deref(), Some("ada"));
    assert!(signed_up.is_some(), "stamped at the callback");
    assert!(is_uuid_shaped(analytics_id), "{analytics_id}");

    // ---- what the warden actually received ----
    let (put_label, armor) = h.credential_armor(0);
    let plaintext = h.open_with_tenant_identity("ada", &armor);
    let recipient = h.recipient_of("ada");
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

    // CALL 1 carries the label and the mailbox and NO credential: at that point
    // the recipient it would be sealed to does not exist yet.
    assert_eq!(rec.create_bodies.len(), 1);
    let created = &rec.create_bodies[0];
    assert_eq!(created["label"], "ada");
    assert_eq!(created["account_email"], MAILBOX);
    assert!(
        created.get("cred_read_ciphertext").is_none(),
        "the credential must not ride on call 1: {created}"
    );

    // CALL 2 carries armor, on the right label, and no token in the clear.
    assert_eq!(rec.credential_puts.len(), 1);
    assert_eq!(put_label, "ada");
    assert!(armor.starts_with(seal::ARMOR_HEADER), "{armor}");
    assert!(!armor.contains(REFRESH_TOKEN));
    assert!(!armor.contains(ACCESS_TOKEN));
    let whole_body = serde_json::to_string(&rec.credential_puts[0].1).unwrap();
    assert!(!whole_body.contains(REFRESH_TOKEN), "{whole_body}");
    assert!(!whole_body.contains(ACCESS_TOKEN), "{whole_body}");
    // What call 1 answered with is a PUBLIC key. The private half never left the
    // mock warden, which is the only reason this test process can decrypt at all.
    assert!(recipient.starts_with("age1"), "{recipient}");

    // THE CENTRAL ASSERTION: the ciphertext opens with the identity the warden
    // minted for THIS tenant (proved by `open_with_tenant_identity` above), and
    // what is inside is the credentials-file SLOT MAP the daemon reads, not a
    // bare token (which would decrypt into an empty map and fail days later).
    //
    // BOTH SLOTS, from the one consent: `email` is what sync and triage load,
    // `email#write` is what the action handlers load. A blob with only the first
    // provisions a mailbox whose Archive and Send buttons fail forever.
    let parsed: Value = serde_json::from_str(&plaintext).unwrap();
    let write_slot = format!("{MAILBOX}#write");
    assert_eq!(
        parsed["slots"].as_object().map(|s| s.len()),
        Some(2),
        "{plaintext}"
    );
    for slot in [MAILBOX, write_slot.as_str()] {
        assert_eq!(
            parsed["slots"][slot]["refresh_token"], REFRESH_TOKEN,
            "{slot}"
        );
        assert_eq!(
            parsed["slots"][slot]["access_token"], ACCESS_TOKEN,
            "{slot}"
        );
    }
    assert!(parsed.get("refresh_token").is_none(), "{plaintext}");

    // ...and it is sealed to EXACTLY that recipient: another key opens nothing.
    let stranger = age::x25519::Identity::generate();
    let decryptor = age::Decryptor::new(age::armor::ArmoredReader::new(armor.as_bytes())).unwrap();
    assert!(
        decryptor
            .decrypt(std::iter::once(&stranger as &dyn age::Identity))
            .is_err()
    );

    // The bearer went on every warden call, and availability was checked before
    // consent.
    assert!(!rec.warden_bearers.is_empty());
    assert!(
        rec.warden_bearers
            .iter()
            .all(|b| b == "Bearer warden-bearer"),
        "{:?}",
        rec.warden_bearers
    );
    assert_eq!(rec.status_lookups, vec!["ada".to_string()]);
}

/// THE OTHER DOOR, and the case that made `account_email` a column: an invite
/// MAILED to one address, redeemed by a Google account that is a different
/// address. A bearer code cannot promise otherwise, and before this the board
/// showed the invited address for a mailbox belonging to somebody else.
///
/// The row is seeded through the store the way the admin page seeds it —
/// `invite_directly`, then `set_user_invite` with the id the mint returned — so
/// the pointer under test is the one production writes.
#[tokio::test]
async fn a_mailed_invite_stamps_the_row_it_was_minted_for() {
    let h = Harness::new().await;
    let store = h.state.store();

    const INVITED_AT: &str = "ada@work.example.com";
    let user_id = store
        .invite_directly(INVITED_AT, chrono::Utc::now())
        .await
        .unwrap()
        .expect("a fresh address is approved by this call");
    let minted = invites::mint().unwrap();
    let invite_id = store
        .insert_invite(&minted.code_hash, default_expiry())
        .await
        .unwrap();
    assert!(
        store
            .set_user_invite(user_id, invite_id, None)
            .await
            .unwrap()
    );
    let before = h.funnel().await;
    assert_eq!(before.len(), 1);
    let analytics_id = before[0].4.clone();

    // The mock Google always answers with MAILBOX, which is NOT the address the
    // invite was addressed to. That is the whole point of this test.
    let (status, body) = h.run_signup("ada", &minted.code).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let funnel = h.funnel().await;
    assert_eq!(
        funnel.len(),
        1,
        "the SAME row was stamped, not a second one: {funnel:?}"
    );
    let (email, account, label, signed_up, id_after) = &funnel[0];
    assert_eq!(
        email, INVITED_AT,
        "still known by the address the invite went to"
    );
    assert_eq!(
        account.as_deref(),
        Some(MAILBOX),
        "and the account that actually signed up is recorded beside it"
    );
    assert_ne!(email.as_str(), MAILBOX, "the mismatch this column exists for");
    assert_eq!(label.as_deref(), Some("ada"));
    assert!(signed_up.is_some());
    assert_eq!(
        id_after, &analytics_id,
        "signing up is not a new person: the id is the one they were minted"
    );
}

/// A failure on call 2 is the state wire v2 invents, so it gets the most
/// assertions: the address is held, the invite is NOT burned, the control plane
/// records nothing, and the retry lands on the SAME per-tenant key.
#[tokio::test]
async fn a_failed_credential_install_leaves_a_retriable_signup() {
    let h = Harness::new().await;
    h.rec.lock().unwrap().fail_credentials = true;

    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("not finished"), "{body}");
    assert!(body.contains("has not been used"), "{body}");
    // Honest copy: it does NOT claim nothing was set up, because the address is
    // being held.
    assert!(!body.contains("Nothing was set up"), "{body}");

    // Nothing was recorded here: the tenant does not exist as far as this
    // control plane is concerned, so its own label check will not block a retry.
    assert!(!h.state.store().label_exists("ada").await.unwrap());
    // ...and the code was handed back THE MOMENT the provision failed, not left
    // held until the session would have expired ten minutes from now. The page
    // above tells this person to start again with the same code; this is what
    // makes that sentence true.
    assert!(
        h.invite_is_available().await,
        "the failed signup released its hold"
    );
    {
        let rec = h.rec.lock().unwrap();
        assert_eq!(rec.create_bodies.len(), 1);
        assert_eq!(rec.credential_puts.len(), 1, "the install was attempted");
        assert!(!rec.tenants["ada"].provisioned, "and it did not take");
    }

    // THE RETRY, with the same code and the same address. That the form accepts
    // it at all proves two things: the invite was never spent, and a PENDING
    // label passes the pre-consent availability check.
    h.rec.lock().unwrap().fail_credentials = false;
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ABCD-EFGH"), "{body}");

    // ONE identity for this tenant across both attempts, and both ciphertexts
    // open with it: the retry re-used the same recipient rather than orphaning
    // the first key.
    let (_, first) = h.credential_armor(0);
    let (_, second) = h.credential_armor(1);
    assert_ne!(first, second, "each seal is its own ciphertext");
    for armor in [&first, &second] {
        let plaintext = h.open_with_tenant_identity("ada", armor);
        let parsed: Value = serde_json::from_str(&plaintext).unwrap();
        assert_eq!(parsed["slots"][MAILBOX]["refresh_token"], REFRESH_TOKEN);
        assert_eq!(
            parsed["slots"][format!("{MAILBOX}#write")]["refresh_token"],
            REFRESH_TOKEN
        );
    }

    {
        let rec = h.rec.lock().unwrap();
        assert_eq!(rec.tenants.len(), 1, "no orphaned second tenant");
        assert_eq!(rec.create_bodies.len(), 2, "call 1 ran again");
        assert!(rec.tenants["ada"].provisioned);
    }
    // The invite is spent now, and only now.
    assert!(h.state.store().label_exists("ada").await.unwrap());
    assert_eq!(h.invite_row().await, (Some("ada".to_string()), false));
}

/// THE RACE: one code, two tabs. The second is refused at the form, before it
/// can spend a consent, and the first is unaffected. Without the hold both
/// sessions passed the check and provisioned a tenant each, and only the second
/// consume lost.
#[tokio::test]
async fn one_code_opens_one_signup_at_a_time() {
    let h = Harness::new().await;
    let invite = h.invite_code.clone();

    // Tab one is at Google.
    let (consent, cookie) = h.start_signup("ada").await;

    // Tab two posts the same code. One message, the same one a wrong code gets.
    let (status, _, body) = h
        .post_form(
            "/signup",
            format!("invite={}&label=grace", urlencode(&invite)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the form re-renders");
    assert!(body.contains("not usable"), "{body}");
    {
        let rec = h.rec.lock().unwrap();
        assert!(rec.token_form.is_empty(), "tab two reached nothing");
        assert!(rec.create_bodies.is_empty());
    }

    // Tab one finishes normally: being raced cost it nothing.
    let (status, _, body) = h
        .get(
            &format!("/oauth/callback?code=c1&state={}", state_param(&consent)),
            Some(&cookie),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.create_bodies.len(), 1, "one code, one tenant");
    assert_eq!(rec.credential_puts.len(), 1);
}

/// A hold lapses with the session that took it, so a signup abandoned at Google
/// does not strand the code for the person who was sent it.
#[tokio::test]
async fn a_lapsed_hold_does_not_strand_the_code() {
    let h = Harness::new().await;
    let hash = invites::hash(&h.invite_code);

    // Somebody posted the form twenty minutes ago, went to Google, and closed
    // the tab. Their session is long gone; only the row remains.
    let then = chrono::Utc::now() - chrono::Duration::minutes(20);
    h.state
        .store()
        .reserve_invite(
            &hash,
            "an-abandoned-session",
            then,
            then + chrono::Duration::minutes(10),
        )
        .await
        .unwrap()
        .expect("the abandoned signup held it");
    assert!(
        h.invite_is_available().await,
        "and the hold has since lapsed"
    );

    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(h.invite_row().await, (Some("ada".to_string()), false));
}

/// A code that ran out is refused exactly the way a wrong one is, and nothing
/// downstream is asked about it.
#[tokio::test]
async fn an_expired_code_is_refused_like_any_other() {
    let h = Harness::new().await;
    let expired = h
        .issue_invite_expiring(chrono::Utc::now() - chrono::Duration::seconds(1))
        .await;

    let (status, _, body) = h
        .post_form(
            "/signup",
            format!("invite={}&label=ada", urlencode(&expired)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the form re-renders");
    assert!(body.contains("not usable"), "{body}");

    let rec = h.rec.lock().unwrap();
    assert!(rec.token_form.is_empty(), "nothing reached Google");
    assert!(rec.status_lookups.is_empty(), "nor the warden");
}

/// The shape codes are minted in now: sixteen Crockford symbols in four groups,
/// working end to end through the form.
#[tokio::test]
async fn a_minted_code_signs_up_in_the_shape_it_is_printed() {
    let h = Harness::new().await;
    assert_eq!(h.invite_code.len(), 19, "{}", h.invite_code);
    assert_eq!(h.invite_code.matches('-').count(), 3, "{}", h.invite_code);

    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Retyped the way a person retypes it: no dashes, wrong case, extra space.
    // Same credential, and it is spent.
    let sloppy = format!(" {} ", invite.replace('-', "").to_lowercase());
    let (status, _, page) = h
        .post_form(
            "/signup",
            format!("invite={}&label=grace", urlencode(&sloppy)),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("not usable"), "{page}");
}

/// A label that goes live while the user is at Google is refused on call 1,
/// after consent and before anything is sealed. The invite survives.
#[tokio::test]
async fn a_label_taken_during_consent_does_not_burn_the_invite() {
    let h = Harness::new().await;
    let (consent, cookie) = h.start_signup("ada").await;
    h.rec.lock().unwrap().taken.push("ada".to_string());

    let (status, _, body) = h
        .get(
            &format!("/oauth/callback?code=c1&state={}", state_param(&consent)),
            Some(&cookie),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("just taken"), "{body}");
    assert!(body.contains("has not been used"), "{body}");
    {
        let rec = h.rec.lock().unwrap();
        assert_eq!(rec.create_bodies.len(), 1, "call 1 was attempted");
        assert!(rec.credential_puts.is_empty(), "and nothing was sealed");
    }

    // The invite still works: the form takes it and sends the user to Google.
    // `start_signup_with` asserts the 303 itself, so reaching the end of this
    // test is the assertion.
    let invite = h.invite_code.clone();
    h.start_signup_with("grace", &invite).await;
}

/// One mailbox, one daemon. The mock Google always names the same mailbox, so
/// a second signup under a different label and a fresh invite is the duplicate
/// case, and it is refused at the callback, after consent and before anything
/// is provisioned.
#[tokio::test]
async fn a_mailbox_gets_one_daemon() {
    let h = Harness::new().await;
    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let second_code = h.issue_invite().await;
    let (status, body) = h.run_signup("grace", &second_code).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("already"), "{body}");

    let rec = h.rec.lock().unwrap();
    assert_eq!(rec.create_bodies.len(), 1, "only the first was created");
    assert_eq!(rec.credential_puts.len(), 1);
}

/// An invite is spent by the signup that used it, and the code stops working
/// the moment the tenant exists.
#[tokio::test]
async fn an_invite_code_works_once() {
    let h = Harness::new().await;
    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // THE INVARIANT the hold buys: the session that provisioned held the code
    // the whole way, so the consume at the end cannot have lost a race. If it
    // had, the row would carry no label and this signup would have made a
    // tenant for free.
    assert_eq!(h.invite_row().await, (Some("ada".to_string()), false));

    let (status, _, body) = h
        .post_form(
            "/signup",
            format!("invite={}&label=grace", urlencode(&invite)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the form re-renders");
    assert!(body.contains("not usable"), "{body}");
}

/// A 422 from call 2 is the two sides of the wire disagreeing, not something the
/// person who typed the address did. It reads as the generic retriable failure,
/// and NOTHING on the page blames their input.
#[tokio::test]
async fn a_refused_credential_body_is_not_reported_as_an_address_problem() {
    let h = Harness::new().await;
    h.rec.lock().unwrap().credentials_status = Some(422);

    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("not finished"), "{body}");
    assert!(!body.contains("taken"), "{body}");
    assert!(!body.contains("address is already"), "{body}");
    assert!(!body.contains("refused"), "{body}");

    // Retriable means retriable: the code went straight back.
    assert!(h.invite_is_available().await);
    assert_eq!(h.rec.lock().unwrap().credential_puts.len(), 1);
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
    assert!(rec.create_bodies.is_empty(), "and no tenant was created");
    assert!(rec.credential_puts.is_empty());
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
    assert!(rec.create_bodies.is_empty());
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
    assert!(rec.create_bodies.is_empty());
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

    assert_eq!(h.rec.lock().unwrap().credential_puts.len(), 1);
}

/// A label the warden already serves is refused BEFORE the user is sent to
/// Google, so nobody spends a consent on an address they cannot have.
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
    assert!(rec.create_bodies.is_empty());
}

/// A consent that left a box unchecked provisions NOTHING. Whichever of the
/// three is missing, the answer is the same page, the invite is handed back, and
/// no half-capable tenant is created.
#[tokio::test]
async fn a_partial_consent_provisions_nothing_and_says_so() {
    for partial in [
        READONLY.to_string(),
        format!("{READONLY} {MODIFY}"),
        format!("{READONLY} {SEND}"),
        format!("{MODIFY} {SEND}"),
        String::new(),
    ] {
        let h = Harness::new().await;
        h.rec.lock().unwrap().granted_scope = Some(partial.clone());

        let invite = h.invite_code.clone();
        let (status, body) = h.run_signup("ada", &invite).await;
        assert_eq!(status, StatusCode::OK, "{partial:?} -> {body}");
        // ONE wording, whichever box it was, and it names what is needed.
        assert!(body.contains("all three Gmail permissions"), "{body}");
        assert!(body.contains("has not been used"), "{body}");
        assert!(body.contains("Nothing was set up"), "{body}");

        // The exchange happened; nothing past it did.
        {
            let rec = h.rec.lock().unwrap();
            assert!(!rec.token_form.is_empty(), "the exchange was attempted");
            assert!(rec.create_bodies.is_empty(), "no tenant was created");
            assert!(rec.credential_puts.is_empty(), "nothing was sealed");
        }
        assert!(!h.state.store().label_exists("ada").await.unwrap());
        // ...and the code is spendable again, which is what "start again" means.
        assert!(h.invite_is_available().await, "{partial:?}");
    }
}

/// The union case, on the wire: a Google account that has granted this project
/// more than signup asks for reports MORE, and that is a pass, not a refusal.
#[tokio::test]
async fn a_wider_grant_than_we_asked_for_is_accepted() {
    let h = Harness::new().await;
    h.rec.lock().unwrap().granted_scope = Some(format!(
        "{} https://www.googleapis.com/auth/userinfo.email",
        full_grant()
    ));

    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ABCD-EFGH"), "{body}");
}

/// With the Bifrost trio configured, signup mints TWO virtual keys per tenant
/// — triage and assistant, each with its own name, budget, and model list —
/// hands both VALUES to the warden in one PUT, and keeps only the IDS. The
/// values never reach the page or the store.
#[tokio::test]
async fn signup_mints_and_installs_a_tenant_llm_key() {
    let h = Harness::with_bifrost(Bifrost::Mock).await;
    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body.contains(&vk_value_for("tenant-ada")),
        "the key value must never reach a page"
    );
    assert!(
        !body.contains(&vk_value_for("tenant-ada-assistant")),
        "the assistant key value must never reach a page"
    );

    {
        let rec = h.rec.lock().unwrap();
        // FOUR governance requests — a provider-key listing then a mint, twice —
        // all carrying HTTP Basic with the admin `username:password`.
        assert_eq!(rec.bifrost_auths, vec![bifrost_basic_auth(); 4]);
        assert_eq!(rec.bifrost_mint_bodies.len(), 2);
        let mint = &rec.bifrost_mint_bodies[0];
        assert_eq!(mint["name"], "tenant-ada");
        // `budgets` is an ARRAY on the live gateway; a singular `budget` object
        // is silently ignored and mints an unbudgeted key.
        assert_eq!(mint["budgets"].as_array().unwrap().len(), 1);
        assert_eq!(mint["budgets"][0]["max_limit"], DEFAULT_LLM_BUDGET_USD);
        assert_eq!(mint["budgets"][0]["reset_duration"], "1M");
        assert_eq!(mint["is_active"], true);
        // The provider config pins the listed key ids and a non-empty model
        // allow-list; without both the key cannot serve inference.
        let pc = &mint["provider_configs"][0];
        assert_eq!(pc["provider"], "anthropic");
        assert_eq!(pc["key_ids"], json!([PROVIDER_KEY_ID]));
        assert_eq!(
            pc["allowed_models"],
            json!(["claude-haiku-4-5", "claude-sonnet-5"])
        );
        // The SECOND mint is the assistant's: its own name, its own budget, its
        // own model list.
        let mint = &rec.bifrost_mint_bodies[1];
        assert_eq!(mint["name"], "tenant-ada-assistant");
        assert_eq!(mint["budgets"].as_array().unwrap().len(), 1);
        assert_eq!(
            mint["budgets"][0]["max_limit"],
            DEFAULT_ASSISTANT_BUDGET_USD
        );
        assert_eq!(mint["budgets"][0]["reset_duration"], "1M");
        let pc = &mint["provider_configs"][0];
        assert_eq!(pc["provider"], "anthropic");
        assert_eq!(pc["key_ids"], json!([PROVIDER_KEY_ID]));
        assert_eq!(
            pc["allowed_models"],
            json!(["claude-haiku-4-5", "claude-opus-4-8"])
        );
        // BOTH values went to the warden, in ONE put, for this tenant.
        assert_eq!(rec.llm_key_puts.len(), 1);
        let (label, put) = &rec.llm_key_puts[0];
        assert_eq!(label, "ada");
        assert_eq!(put["api_key"], vk_value_for("tenant-ada"));
        assert_eq!(
            put["assistant_api_key"],
            vk_value_for("tenant-ada-assistant")
        );
    }
    // The store kept the IDS, and only the IDS.
    assert_eq!(
        h.state.store().tenant_vk("ada").await.unwrap(),
        Some(vk_id_for("tenant-ada"))
    );
    assert_eq!(
        h.state.store().tenant_assistant_vk("ada").await.unwrap(),
        Some(vk_id_for("tenant-ada-assistant"))
    );
}

/// THE ORPHAN PROMISE: Bifrost minted the keys but the warden would not take
/// them. The signup still completes — fail-soft, exactly like an outage — AND
/// the vk ids are still recorded on the tenant row, because a key that is live
/// in Bifrost with no record is one no operator can ever find to revoke.
#[tokio::test]
async fn a_failed_key_install_still_records_the_vk_id() {
    let h = Harness::with_bifrost(Bifrost::Mock).await;
    h.rec.lock().unwrap().fail_llm_key = true;

    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ABCD-EFGH"), "{body}");
    assert!(
        !body.contains(&vk_value_for("tenant-ada")),
        "the key value must never reach a page"
    );

    {
        let rec = h.rec.lock().unwrap();
        assert_eq!(rec.bifrost_mint_bodies.len(), 2, "both mints happened");
        assert_eq!(rec.llm_key_puts.len(), 1, "the install was attempted");
        assert_eq!(
            rec.credential_puts.len(),
            1,
            "the mailbox still provisioned"
        );
    }
    // The ids are on the row for the manual `llm revoke` or `llm mint` the log
    // line points the operator at.
    assert_eq!(
        h.state.store().tenant_vk("ada").await.unwrap(),
        Some(vk_id_for("tenant-ada"))
    );
    assert_eq!(
        h.state.store().tenant_assistant_vk("ada").await.unwrap(),
        Some(vk_id_for("tenant-ada-assistant"))
    );
    // ...and the signup was a real one: the invite is spent.
    assert_eq!(h.invite_row().await, (Some("ada".to_string()), false));
}

/// THE FAIL-SOFT PROMISE: a Bifrost outage costs the tenant its LLM keys,
/// never the signup. Triage is not mail custody, and `llm mint` backfills.
#[tokio::test]
async fn a_bifrost_outage_does_not_refuse_a_signup() {
    let h = Harness::with_bifrost(Bifrost::Down).await;
    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ABCD-EFGH"), "{body}");

    {
        let rec = h.rec.lock().unwrap();
        assert!(rec.llm_key_puts.is_empty(), "no key existed to install");
        assert_eq!(
            rec.credential_puts.len(),
            1,
            "the mailbox still provisioned"
        );
    }
    // Nothing was minted, so nothing is recorded: both pointers stay empty
    // rather than naming keys that do not exist.
    assert_eq!(h.state.store().tenant_vk("ada").await.unwrap(), None);
    assert_eq!(
        h.state.store().tenant_assistant_vk("ada").await.unwrap(),
        None
    );
    // ...and the signup was a real one: the invite is spent.
    assert_eq!(h.invite_row().await, (Some("ada".to_string()), false));
}

/// THE INDEPENDENCE PROMISE, one way: the assistant mint fails, and the tenant
/// still signs up WITH its triage key installed. The one warden PUT carries
/// only the key that exists — no `assistant_api_key` slot at all, not a null —
/// and no assistant id is recorded, so nothing points at a key that was never
/// minted.
#[tokio::test]
async fn an_assistant_mint_failure_still_installs_the_triage_key() {
    let h = Harness::with_bifrost(Bifrost::Mock).await;
    h.rec
        .lock()
        .unwrap()
        .bifrost_fail_names
        .push("tenant-ada-assistant".to_string());

    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ABCD-EFGH"), "{body}");

    {
        let rec = h.rec.lock().unwrap();
        assert_eq!(
            rec.bifrost_mint_bodies.len(),
            2,
            "both mints were attempted"
        );
        assert_eq!(rec.llm_key_puts.len(), 1, "one PUT, with what succeeded");
        let (label, put) = &rec.llm_key_puts[0];
        assert_eq!(label, "ada");
        assert_eq!(put["api_key"], vk_value_for("tenant-ada"));
        assert!(
            put.get("assistant_api_key").is_none(),
            "the missing key must be absent, not null: {put}"
        );
    }
    assert_eq!(
        h.state.store().tenant_vk("ada").await.unwrap(),
        Some(vk_id_for("tenant-ada"))
    );
    assert_eq!(
        h.state.store().tenant_assistant_vk("ada").await.unwrap(),
        None
    );
    assert_eq!(h.invite_row().await, (Some("ada".to_string()), false));
}

/// ...and the other way: the triage mint fails, and the assistant key is still
/// minted, installed, and recorded. The PUT carries only `assistant_api_key`.
#[tokio::test]
async fn a_triage_mint_failure_still_installs_the_assistant_key() {
    let h = Harness::with_bifrost(Bifrost::Mock).await;
    h.rec
        .lock()
        .unwrap()
        .bifrost_fail_names
        .push("tenant-ada".to_string());

    let invite = h.invite_code.clone();
    let (status, body) = h.run_signup("ada", &invite).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("ABCD-EFGH"), "{body}");

    {
        let rec = h.rec.lock().unwrap();
        assert_eq!(
            rec.bifrost_mint_bodies.len(),
            2,
            "both mints were attempted"
        );
        assert_eq!(rec.llm_key_puts.len(), 1, "one PUT, with what succeeded");
        let (label, put) = &rec.llm_key_puts[0];
        assert_eq!(label, "ada");
        assert!(
            put.get("api_key").is_none(),
            "the missing key must be absent, not null: {put}"
        );
        assert_eq!(
            put["assistant_api_key"],
            vk_value_for("tenant-ada-assistant")
        );
    }
    assert_eq!(h.state.store().tenant_vk("ada").await.unwrap(), None);
    assert_eq!(
        h.state.store().tenant_assistant_vk("ada").await.unwrap(),
        Some(vk_id_for("tenant-ada-assistant"))
    );
    assert_eq!(h.invite_row().await, (Some("ada".to_string()), false));
}

/// The form and the health check are the only things reachable without a
/// session, and the form states the grants it is about to ask for.
#[tokio::test]
async fn the_landing_page_states_the_grant() {
    let h = Harness::new().await;
    let (status, _, body) = h.get("/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("gmail.readonly"), "{body}");
    assert!(body.contains("gmail.modify"), "{body}");
    assert!(body.contains("gmail.send"), "{body}");
    assert!(body.contains(".passband.test"), "{body}");

    let (status, _, body) = h.get("/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}

/// The activation poller end to end against the mock warden: a paired tenant
/// is stamped ONCE with the daemon's own timestamp, a mid-roll tenant (503)
/// is left for the next pass, and a stamped user leaves the candidate set
/// forever — the follow-up pass stamps nobody and moves nothing.
#[tokio::test]
async fn the_activation_poller_stamps_once_and_quiesces() {
    let h = Harness::new().await;

    // Two signed-up people, seeded the way record_signup's CLI door works:
    // no invite pointer, keyed on the Google account that redeemed.
    let now = chrono::Utc::now();
    h.state.store().insert_tenant("ada", "ada@example.com").await.unwrap();
    h.state.store().insert_tenant("bea", "bea@example.com").await.unwrap();
    h.state.store().record_signup(9001, "ada@example.com", "ada", now).await.unwrap();
    h.state.store().record_signup(9002, "bea@example.com", "bea", now).await.unwrap();

    // ada's daemon reports a pairing at ITS OWN timestamp; bea's pod is
    // mid-roll and answers 503.
    let paired_at = "2026-08-20T09:30:00Z";
    {
        let mut r = h.rec.lock().unwrap();
        r.devices_paired.insert("ada".into(), Some(paired_at.into()));
        r.devices_unavailable.push("bea".into());
    }
    assert_eq!(activation::poll_first_paired(&h.state).await, 1);

    let stamps = |rows: Vec<tokio_postgres::Row>| -> Vec<(Option<String>, Option<String>)> {
        rows.iter()
            .map(|r| (r.get("tenant_label"), r.get("first_paired_at")))
            .collect()
    };
    let read = || async {
        stamps(
            common::raw_client(&h.db_url)
                .await
                .query("SELECT tenant_label, first_paired_at FROM users ORDER BY id", &[])
                .await
                .unwrap(),
        )
    };
    let after_first = read().await;
    assert_eq!(after_first.len(), 2);
    let ada_stamp = after_first[0].1.clone().expect("ada stamped");
    // The DAEMON's instant survives re-serialization; the poller never
    // substitutes its own clock.
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&ada_stamp).unwrap(),
        chrono::DateTime::parse_from_rfc3339(paired_at).unwrap()
    );
    assert_eq!(after_first[1], ("bea".to_string().into(), None));

    // Next pass: ada has left the candidate set, and bea's warden now answers
    // the null body — asked again, still honestly unstamped.
    {
        let mut r = h.rec.lock().unwrap();
        r.devices_unavailable.clear();
        r.devices_paired.insert("bea".into(), None);
    }
    assert_eq!(activation::poll_first_paired(&h.state).await, 0);
    assert_eq!(read().await, after_first);

    // bea finally pairs; only bea is stamped, and ada's first stamp stands.
    h.rec
        .lock()
        .unwrap()
        .devices_paired
        .insert("bea".into(), Some("2026-08-21T00:00:00Z".into()));
    assert_eq!(activation::poll_first_paired(&h.state).await, 1);
    let after_third = read().await;
    assert_eq!(after_third[0].1.as_deref(), Some(ada_stamp.as_str()));
    assert!(after_third[1].1.is_some());
}
