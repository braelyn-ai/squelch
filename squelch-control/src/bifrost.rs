//! The Bifrost governance client: one virtual key per tenant, minted at signup
//! and handed straight to the warden.
//!
//! Bifrost is the LLM gateway the hosted tier fronts every tenant daemon with.
//! This module speaks exactly two of its governance routes: mint a virtual key
//! with a monthly budget, and revoke one by id. The ADMIN token it presents can
//! mint unbounded spend, so it gets the same handling as the warden bearer:
//! presented on every request, never logged, redirects refused.
//!
//! THE KEY VALUE IS THE SECRET, AND THE ID IS THE RECORD. What Bifrost answers
//! with is a live `sk-bf-...` bearer plus an id naming it. The value exists in
//! this process only between the mint and the warden PUT that installs it; the
//! id is what the control store keeps, what a log line may carry, and what a
//! later revoke presents. [`VirtualKey`] deliberately derives nothing so the
//! value cannot ride out in a format string.
//!
//! Bifrost's answers are treated as UNTRUSTED INPUT even though the gateway is
//! ours: the id reaches a URL path (on revoke) and a store column, and the
//! value is forwarded to the warden, so both are shape-checked on arrival and
//! the body read is capped.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Ceiling on a governance response body. A mint answer is a few hundred
/// bytes; anything bigger is not the API we know.
const MAX_RESPONSE_BODY: usize = 64 * 1024;

/// Every minted key's budget resets monthly. Pinned rather than configurable:
/// the budget AMOUNT is the operator's knob, the cadence is the product's.
const BUDGET_RESET: &str = "1M";

/// Ceiling on a virtual-key id. Bifrost's are UUID-sized; this is slack.
const MAX_ID: usize = 128;

/// Ceiling on a key value, matching what the warden will accept
/// (`MAX_LLM_API_KEY` on its side of the wire).
const MAX_VALUE: usize = 4 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BifrostError {
    /// The gateway could not be reached, or answered something unreadable.
    #[error("the LLM gateway could not be reached")]
    Unreachable,
    /// 401/403. A deployment misconfiguration worth shouting about: no tenant
    /// gets a key until it is fixed.
    #[error("the LLM gateway refused our admin credentials")]
    Unauthorized,
    /// Any other non-success status.
    #[error("the LLM gateway refused the request")]
    Failed,
    /// A success status carrying a body this client will not store, log, or
    /// forward to the warden.
    #[error("the LLM gateway answered with an unusable virtual key")]
    BadKey,
    /// The caller handed revoke an id this client would never have accepted
    /// from a mint. A corrupt store row, caught before it reaches a URL path.
    #[error("refusing a virtual-key id this client would not have accepted")]
    BadId,
    /// The label failed validation. Should be unreachable (every caller
    /// validates first); it means the validators have drifted.
    #[error("refusing a label this client would not have validated")]
    BadLabel,
}

/// A freshly minted virtual key.
///
/// DELIBERATELY DERIVES NOTHING — no `Debug`, no `Clone`, no `Serialize` — so
/// the one copy of `value` lives from the mint to the warden PUT and cannot be
/// formatted, duplicated, or re-encoded on the way.
pub struct VirtualKey {
    /// Bifrost's name for the key. Recorded in the control store, safe in a
    /// log line, and what a later revoke presents.
    pub id: String,
    /// The live `sk-bf-...` bearer. NEVER stored, NEVER logged; held only for
    /// the call that installs it via the warden.
    pub value: String,
}

/// `POST /api/governance/virtual-keys` request body.
#[derive(Serialize)]
struct MintRequest {
    /// `tenant-<label>`, so the gateway's own listing names the tenant.
    name: String,
    budget: Budget,
    is_active: bool,
}

#[derive(Serialize)]
struct Budget {
    /// USD.
    max_limit: f64,
    reset_duration: &'static str,
}

/// The mint answer. NO Debug anywhere on this chain: `value` is live.
#[derive(Deserialize)]
struct MintResponse {
    virtual_key: WireKey,
}

#[derive(Deserialize)]
struct WireKey {
    id: String,
    value: String,
}

/// The real client: one reqwest client, one admin bearer, one base URL.
pub struct BifrostClient {
    base_url: String,
    admin_token: String,
    http: reqwest::Client,
}

impl BifrostClient {
    /// `base_url` is a canonical origin (no trailing slash).
    pub fn new(
        base_url: String,
        admin_token: String,
        timeout: Duration,
    ) -> Result<Self, BifrostError> {
        let http = reqwest::Client::builder()
            // Redirects refused: every request carries the ADMIN bearer and
            // the mint answer carries a live key, and a redirect is how either
            // ends up at a host nobody chose.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| BifrostError::Unreachable)?;
        Ok(Self {
            base_url,
            admin_token,
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Mint a virtual key named `tenant-<label>` with a monthly budget of
    /// `budget_usd`.
    pub async fn mint_virtual_key(
        &self,
        label: &str,
        budget_usd: f64,
    ) -> Result<VirtualKey, BifrostError> {
        // Validated upstream, asserted here: the label lands verbatim in the
        // gateway's key listing.
        crate::labels::validate(label).map_err(|_| BifrostError::BadLabel)?;

        let resp = self
            .http
            .post(self.url("/api/governance/virtual-keys"))
            .bearer_auth(&self.admin_token)
            .json(&MintRequest {
                name: format!("tenant-{label}"),
                budget: Budget {
                    max_limit: budget_usd,
                    reset_duration: BUDGET_RESET,
                },
                is_active: true,
            })
            .send()
            .await
            .map_err(|_| BifrostError::Unreachable)?;

        match resp.status().as_u16() {
            // Bifrost has answered both on a create, depending on version.
            200 | 201 => {
                let body = read_capped(resp).await?;
                let parsed: MintResponse =
                    serde_json::from_slice(&body).map_err(|_| BifrostError::BadKey)?;
                let key = VirtualKey {
                    id: parsed.virtual_key.id,
                    value: parsed.virtual_key.value,
                };
                // The id is about to be stored and, one day, put in a URL
                // path; the value is about to be forwarded to the warden.
                // Both are held to a shape HERE, before either goes anywhere.
                if !is_id(&key.id) || !is_value(&key.value) {
                    return Err(BifrostError::BadKey);
                }
                Ok(key)
            }
            401 | 403 => Err(BifrostError::Unauthorized),
            _ => Err(BifrostError::Failed),
        }
    }

    /// Revoke a virtual key by the id a mint answered with.
    pub async fn revoke_virtual_key(&self, id: &str) -> Result<(), BifrostError> {
        // The id goes into a URL PATH. It was shape-checked when it was
        // minted, and it is shape-checked again here, because between the two
        // it sat in a database row.
        if !is_id(id) {
            return Err(BifrostError::BadId);
        }
        let resp = self
            .revoke_request(id)
            .send()
            .await
            .map_err(|_| BifrostError::Unreachable)?;
        match resp.status().as_u16() {
            200 | 202 | 204 => Ok(()),
            401 | 403 => Err(BifrostError::Unauthorized),
            _ => Err(BifrostError::Failed),
        }
    }

    /// The revoke wire, isolated so fixing it is one line.
    ///
    /// UNCONFIRMED: the Bifrost docs we hold do not pin the revoke endpoint.
    /// `DELETE /api/governance/virtual-keys/{id}` is the LIKELY shape (it
    /// mirrors the mint route), but it MUST be confirmed against the deployed
    /// Bifrost before the first real revoke; a PUT/PATCH that flips
    /// `is_active` is the plausible alternative. Whichever it is, the change
    /// lands here and nowhere else.
    fn revoke_request(&self, id: &str) -> reqwest::RequestBuilder {
        self.http
            .delete(self.url(&format!("/api/governance/virtual-keys/{id}")))
            .bearer_auth(&self.admin_token)
    }
}

/// The shape of a key id: what UUIDs and slugs are made of, and NOTHING that
/// could restructure a URL path or a log line.
fn is_id(id: &str) -> bool {
    (1..=MAX_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The shape of a key value: printable ASCII, bounded. Held to the same bar
/// the warden enforces, so a garbage answer is refused here rather than
/// discovered as a 422 there.
fn is_value(v: &str) -> bool {
    (1..=MAX_VALUE).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_graphic())
}

async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>, BifrostError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| BifrostError::Unreachable)? {
        if out.len() + chunk.len() > MAX_RESPONSE_BODY {
            return Err(BifrostError::Unreachable);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Json, Router,
        extract::State as AxumState,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
        routing::{delete, post},
    };
    use serde_json::{Value, json};

    use super::*;

    /// What the mock gateway recorded: the bearer and the body of every mint,
    /// and the path of every revoke.
    #[derive(Default)]
    struct Recorder {
        bearers: Vec<String>,
        mint_bodies: Vec<Value>,
        revoked_ids: Vec<String>,
        /// When set, the mint route answers this instead of a key.
        mint_response: Option<(u16, String)>,
    }

    type Shared = Arc<Mutex<Recorder>>;

    async fn spawn_gateway(rec: Shared) -> String {
        let app = Router::new()
            .route(
                "/api/governance/virtual-keys",
                post(
                    |AxumState(rec): AxumState<Shared>, headers: HeaderMap, body: String| async move {
                        let mut r = rec.lock().unwrap();
                        r.bearers.push(
                            headers
                                .get(header::AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_string(),
                        );
                        r.mint_bodies
                            .push(serde_json::from_str(&body).unwrap_or(Value::Null));
                        if let Some((status, body)) = r.mint_response.clone() {
                            return (StatusCode::from_u16(status).unwrap(), body).into_response();
                        }
                        (
                            StatusCode::CREATED,
                            Json(json!({
                                "message": "Virtual key created successfully",
                                "virtual_key": { "id": "vk-123", "value": "sk-bf-THE-KEY-VALUE" },
                            })),
                        )
                            .into_response()
                    },
                ),
            )
            .route(
                "/api/governance/virtual-keys/{id}",
                delete(
                    |AxumState(rec): AxumState<Shared>,
                     axum::extract::Path(id): axum::extract::Path<String>,
                     headers: HeaderMap| async move {
                        let mut r = rec.lock().unwrap();
                        r.revoked_ids.push(id);
                        r.bearers.push(
                            headers
                                .get(header::AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_string(),
                        );
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(rec);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    async fn client_for(rec: &Shared) -> BifrostClient {
        let url = spawn_gateway(rec.clone()).await;
        BifrostClient::new(url, "the-admin-token".into(), Duration::from_secs(5)).unwrap()
    }

    /// The happy path: the request carries the bearer, the tenant-named key,
    /// the budget, and the monthly reset; the answer parses into id + value.
    #[tokio::test]
    async fn mints_a_key_the_way_the_governance_api_expects() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        let c = client_for(&rec).await;

        let key = c.mint_virtual_key("ada", 5.0).await.unwrap();
        assert_eq!(key.id, "vk-123");
        assert_eq!(key.value, "sk-bf-THE-KEY-VALUE");

        let r = rec.lock().unwrap();
        assert_eq!(r.bearers, vec!["Bearer the-admin-token".to_string()]);
        let body = &r.mint_bodies[0];
        assert_eq!(body["name"], "tenant-ada");
        assert_eq!(body["budget"]["max_limit"], 5.0);
        assert_eq!(body["budget"]["reset_duration"], "1M");
        assert_eq!(body["is_active"], true);
    }

    /// A success status carrying garbage — not JSON, the wrong shape, or a key
    /// this client will not store or forward — is refused as one error.
    #[tokio::test]
    async fn refuses_a_mint_answer_it_will_not_use() {
        for bad in [
            "not json at all".to_string(),
            json!({"message": "ok"}).to_string(),
            json!({"virtual_key": {"id": "", "value": "sk-bf-x"}}).to_string(),
            json!({"virtual_key": {"id": "vk/../123", "value": "sk-bf-x"}}).to_string(),
            json!({"virtual_key": {"id": "vk-123", "value": ""}}).to_string(),
            json!({"virtual_key": {"id": "vk-123", "value": "with space"}}).to_string(),
            json!({"virtual_key": {"id": "a".repeat(MAX_ID + 1), "value": "sk-bf-x"}}).to_string(),
        ] {
            let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
            rec.lock().unwrap().mint_response = Some((200, bad.clone()));
            let c = client_for(&rec).await;
            assert!(
                matches!(c.mint_virtual_key("ada", 5.0).await, Err(BifrostError::BadKey)),
                "{bad:?}"
            );
        }
    }

    /// A body past the cap is refused mid-read, exactly like the warden
    /// client's: an answer that size is not the API we know.
    #[tokio::test]
    async fn refuses_an_oversized_answer() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        rec.lock().unwrap().mint_response =
            Some((200, format!("{{\"pad\":\"{}\"}}", "x".repeat(MAX_RESPONSE_BODY))));
        let c = client_for(&rec).await;
        assert!(matches!(
            c.mint_virtual_key("ada", 5.0).await,
            Err(BifrostError::Unreachable)
        ));
    }

    /// 401/403 is its own error: nothing will mint until the admin token is
    /// fixed, and the caller's log line should say so.
    #[tokio::test]
    async fn a_refused_admin_token_is_distinct() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        rec.lock().unwrap().mint_response = Some((401, json!({"error":"nope"}).to_string()));
        let c = client_for(&rec).await;
        assert!(matches!(
            c.mint_virtual_key("ada", 5.0).await,
            Err(BifrostError::Unauthorized)
        ));
    }

    /// Revoke presents the stored id on the likely wire, and refuses an id it
    /// would never have accepted from a mint — before any socket is opened.
    #[tokio::test]
    async fn revokes_by_id_and_refuses_a_corrupt_one() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        let c = client_for(&rec).await;

        c.revoke_virtual_key("vk-123").await.unwrap();
        assert_eq!(rec.lock().unwrap().revoked_ids, vec!["vk-123".to_string()]);

        for bad in ["", "../admin", "vk/123", "vk 123", "vk%2f"] {
            assert!(
                matches!(c.revoke_virtual_key(bad).await, Err(BifrostError::BadId)),
                "{bad:?}"
            );
        }
    }

    /// The label reaches the gateway's key listing verbatim; one this crate
    /// would not have validated is refused before the socket is opened.
    #[tokio::test]
    async fn refuses_a_label_it_would_not_have_validated() {
        let c = BifrostClient::new(
            // Port 1: nothing listens, so reaching the socket would fail
            // rather than pass by accident.
            "http://127.0.0.1:1".into(),
            "token".into(),
            Duration::from_millis(50),
        )
        .unwrap();
        for bad in ["../../etc", "WWW", "a b", ""] {
            assert!(
                matches!(
                    c.mint_virtual_key(bad, 5.0).await,
                    Err(BifrostError::BadLabel)
                ),
                "{bad:?}"
            );
        }
    }
}
