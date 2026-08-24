//! The tenant door end to end: a daemon presenting a share token and getting
//! back a real invite code.
//!
//! The properties under test are properties OF THE WIRE and of the store
//! underneath it, which is why this drives the router rather than the handler:
//!
//! - the credential is the whole gate, and every way it can be bad is ONE
//!   answer, so nothing here can be used to learn which tenants exist,
//! - what comes back is a REAL invite: the test hashes the code out of the
//!   response and finds it available in the store, which is exactly what
//!   `POST /signup` will do to it,
//! - the quota is per tenant, counted, and enforced BEFORE anything is written,
//!   so a refused mint leaves no row behind,
//! - a revoked token stops working immediately, without the pod having to be
//!   told anything,
//! - and the recipient is not on this wire at all. There is no field for one,
//!   which is the design (see `squelch_control::tenant`).
//!
//! Nothing here touches Google, a warden, or Resend: the tenant door talks to
//! none of them. It talks to the store, and the store is a throwaway Postgres
//! schema `common` makes.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    body::to_bytes,
    http::{Request, StatusCode, header},
};
use serde_json::Value;
use squelch_control::config::{Config, WaitlistConfig};
use squelch_control::warden::HttpWarden;
use squelch_control::{ControlState, invites, router, share};
use tower::ServiceExt as _;

/// The throwaway Postgres schema every harness in this file is built on.
mod common;

const ADMIN_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const COOKIE_KEY: &[u8] = &[42; 32];
const LABEL: &str = "ada";
const ACCOUNT: &str = "ada@example.com";

struct Harness {
    app: Router,
    state: ControlState,
    /// The schema-scoped URL the store was opened on, so a test can reach the
    /// one column no store method writes: a tenant's status. Nothing in the
    /// control plane closes a tenant today (teardown is the warden's), and a
    /// store method invented for a test would be a production API nothing
    /// calls.
    db_url: String,
}

impl Harness {
    /// A deployment with the invite policy configured, which is what makes the
    /// tenant door answer at all.
    async fn new() -> Self {
        Self::build(true).await
    }

    /// The same deployment with no waitlist config: no expiry policy, no
    /// operator, and so no invites to lend a tenant a piece of.
    async fn without_invites() -> Self {
        Self::build(false).await
    }

    async fn build(waitlist: bool) -> Self {
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_url: "https://signup.passband.test".into(),
            redirect_uri: "https://signup.passband.test/oauth/callback".into(),
            base_domain: "passband.test".into(),
            client_id: "test-client-id".into(),
            client_secret: "test-client-secret".into(),
            cookie_key: COOKIE_KEY.to_vec(),
            // Nothing on this route reaches the warden, Google, or Resend. Port
            // 1 is where a request would land if one ever did, which is a
            // connection refused rather than a silently mocked success.
            warden_url: "http://127.0.0.1:1".into(),
            warden_token: "warden-bearer".into(),
            database_url: "postgres://unused".into(),
            trusted_proxy_hops: 0,
            token_url: "http://127.0.0.1:1/token".into(),
            auth_url: "http://127.0.0.1:1/authorize".into(),
            profile_url: "http://127.0.0.1:1/profile".into(),
            userinfo_url: "http://127.0.0.1:1/userinfo".into(),
            bifrost: None,
            waitlist: waitlist.then(|| WaitlistConfig {
                admin_token: ADMIN_TOKEN.into(),
                resend_api_key: "re_test_0123456789".into(),
                invite_from: "Passband <invites@passband.test>".into(),
                allowed_origin: "https://passband.test".into(),
                resend_url: "http://127.0.0.1:1".into(),
            }),
        };

        let (store, db_url) = common::fresh_store().await;
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
            db_url,
        }
    }

    /// Close the tenant, the way a teardown eventually would.
    async fn cancel_tenant(&self) {
        common::raw_client(&self.db_url)
            .await
            .execute("UPDATE tenants SET status = 'cancelled'", &[])
            .await
            .unwrap();
    }

    /// Provision a tenant and give it a share token, the way `share mint`
    /// does. Returns the plaintext, which exists only here and in the header
    /// the tests present it in.
    async fn sharing_tenant(&self) -> String {
        self.state
            .store()
            .insert_tenant(LABEL, ACCOUNT)
            .await
            .unwrap();
        let minted = share::mint().unwrap();
        assert!(
            self.state
                .store()
                .set_tenant_share_token(LABEL, &minted.token_hash)
                .await
                .unwrap()
        );
        minted.token
    }

    /// `POST /tenant/invite` with whatever bearer the caller wants to present,
    /// including none.
    async fn mint(&self, bearer: Option<&str>) -> (StatusCode, Value) {
        let mut req = Request::builder().method("POST").uri("/tenant/invite");
        if let Some(bearer) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
}

/// The happy path, and the assertion that matters most: the thing that came
/// back is a code `POST /signup` would accept.
#[tokio::test]
async fn a_share_token_mints_a_code_signup_would_take() {
    let h = Harness::new().await;
    let token = h.sharing_tenant().await;

    let (status, body) = h.mint(Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let code = body["code"].as_str().expect("a code came back");
    // Shaped like every other invite: the daemon puts this in a mail a human
    // reads, so it has to be the dashed, typeable form.
    assert!(invites::is_plausible(code), "{code}");
    assert_eq!(code.matches('-').count(), 3, "{code}");

    // AND IT IS REAL: available in the store under its hash, which is the
    // lookup the signup route makes.
    assert!(
        h.state
            .store()
            .find_available_invite(&invites::hash(code), chrono::Utc::now())
            .await
            .unwrap()
            .is_some()
    );

    // The daemon is told where to send people and how long it lasts, so its
    // copy never has to hardcode either.
    assert_eq!(body["signup_url"], "https://signup.passband.test");
    assert!(body["expires_at"].as_str().is_some());
    assert_eq!(body["remaining"], share::QUOTA_PER_WINDOW - 1);
}

/// Every way a credential can be bad is ONE answer, and none of them says
/// anything about which tenants exist. Nothing is written on any of these
/// paths.
#[tokio::test]
async fn every_bad_credential_is_the_same_refusal() {
    let h = Harness::new().await;
    let token = h.sharing_tenant().await;

    let unknown = share::mint().unwrap().token;
    for bearer in [
        None,
        Some(""),
        Some("not-a-token"),
        Some(unknown.as_str()),
        // A real invite code is not a share token: two credentials, and
        // pasting one into the other's slot is a plain refusal.
        Some("ABCD-EFGH-JKMN-PQRS"),
        // Length is bounded before anything is hashed.
        Some(&*"x".repeat(4096)),
    ] {
        let (status, body) = h.mint(bearer).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer:?} -> {body}");
        assert_eq!(body["error"], "unauthorized", "{bearer:?}");
    }

    // A revoked token joins them, with no pod told anything.
    assert!(
        h.state
            .store()
            .clear_tenant_share_token(LABEL)
            .await
            .unwrap()
    );
    let (status, body) = h.mint(Some(&token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthorized");
}

/// A tenant that is no longer active cannot mint, however good its token was
/// yesterday. The token is not revoked here: the STATUS is what refuses it.
#[tokio::test]
async fn a_torn_down_tenant_cannot_mint() {
    let h = Harness::new().await;
    let token = h.sharing_tenant().await;
    h.cancel_tenant().await;

    let (status, body) = h.mint(Some(&token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthorized");
}

/// The quota: counted per tenant, refused with its own status so the app can
/// say something true about it, and enforced BEFORE the write, so the refusal
/// leaves nothing behind.
#[tokio::test]
async fn the_quota_bounds_one_tenant_and_writes_nothing_when_it_refuses() {
    let h = Harness::new().await;
    let token = h.sharing_tenant().await;

    for i in 0..share::QUOTA_PER_WINDOW {
        let (status, body) = h.mint(Some(&token)).await;
        assert_eq!(status, StatusCode::OK, "mint {i}: {body}");
        assert_eq!(body["remaining"], share::QUOTA_PER_WINDOW - 1 - i);
    }

    let before = h.state.store().list_invites().await.unwrap().len();
    let (status, body) = h.mint(Some(&token)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"], "quota_exhausted");
    assert!(
        body["code"].is_null(),
        "a refusal hands back no credential: {body}"
    );
    assert_eq!(
        h.state.store().list_invites().await.unwrap().len(),
        before,
        "a refused mint writes nothing"
    );
}

/// One tenant's spending is not another's. The second tenant mints against a
/// full quota with the first one exhausted.
#[tokio::test]
async fn one_tenants_quota_is_not_another_tenants() {
    let h = Harness::new().await;
    let ada = h.sharing_tenant().await;
    for _ in 0..share::QUOTA_PER_WINDOW {
        assert_eq!(h.mint(Some(&ada)).await.0, StatusCode::OK);
    }
    assert_eq!(h.mint(Some(&ada)).await.0, StatusCode::TOO_MANY_REQUESTS);

    h.state
        .store()
        .insert_tenant("grace", "grace@example.com")
        .await
        .unwrap();
    let grace = share::mint().unwrap();
    h.state
        .store()
        .set_tenant_share_token("grace", &grace.token_hash)
        .await
        .unwrap();

    let (status, body) = h.mint(Some(&grace.token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["remaining"], share::QUOTA_PER_WINDOW - 1);
}

/// A deployment with no invite policy answers 503 rather than 404: the route
/// exists, so a daemon can tell "this control plane does not do invites" from
/// "that URL is wrong", and the app's copy needs that difference.
#[tokio::test]
async fn a_deployment_without_invites_says_so() {
    let h = Harness::without_invites().await;
    let token = h.sharing_tenant().await;
    let (status, body) = h.mint(Some(&token)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "unavailable");
}
