//! The Bifrost governance client: two virtual keys per tenant — triage
//! (`tenant-<label>`) and assistant (`tenant-<label>-assistant`), each with
//! its own budget and model list — minted at signup and handed straight to
//! the warden.
//!
//! Bifrost is the LLM gateway the hosted tier fronts every tenant daemon with.
//! This module speaks three of its routes: list the provider keys a virtual
//! key must be attached to, mint a virtual key with a monthly budget, and
//! revoke one by id. All three were verified against the deployed gateway
//! (Bifrost v1.6.9, 2026-08-13); where the docs and the wire disagreed, the
//! wire won:
//!
//! - The budget field is `budgets`, an ARRAY. A singular `budget` object is
//!   silently IGNORED and the key mints unbudgeted, which on our Anthropic key
//!   is unbounded spend. [`BifrostClient::mint_virtual_key`] therefore checks
//!   the echoed key and refuses (revoking the orphan) if the gateway did not
//!   attach the budget.
//! - A key with no `provider_configs` (explicit `key_ids` plus a non-empty
//!   `allowed_models`) cannot serve inference: empty `allowed_models` is
//!   deny-all and wildcard behavior is unreliable. The provider key ids are
//!   discovered from the gateway at mint time.
//! - Auth is HTTP BASIC with the admin `username:password`, not a session
//!   bearer: session tokens expire after 30 days, Basic works statically on
//!   `/api/*`. The credential can mint unbounded spend, so it gets the same
//!   handling as the warden bearer: presented on every request, never logged,
//!   redirects refused.
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
//! every body read is capped.

use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Ceiling on a governance response body. A mint answer is a few hundred
/// bytes; anything bigger is not the API we know.
const MAX_RESPONSE_BODY: usize = 64 * 1024;

/// Every minted key's budget resets monthly. Pinned rather than configurable:
/// the budget AMOUNT is the operator's knob, the cadence is the product's.
const BUDGET_RESET: &str = "1M";

/// The one upstream provider tenant keys are minted against.
const PROVIDER: &str = "anthropic";

/// Ceiling on a virtual-key id. Bifrost's are UUID-sized; this is slack. The
/// same bar is held to provider-key ids, which are names like
/// `ANTHROPIC_API_KEY_auto_detected`.
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
    /// 409 on a mint: the gateway already holds a key with this name and
    /// enforces unique names, so "mint a replacement, revoke the old one
    /// later" cannot work — the old key must go FIRST. `llm mint` revokes the
    /// keys the store records before minting; this surfaces only when the
    /// gateway holds a key the store never recorded (an orphaned mint).
    #[error(
        "the LLM gateway already holds a key with this name (409) and the store does not \
         record it; revoke the orphan in the gateway's own UI, then re-run"
    )]
    Conflict,
    /// Any other non-success status.
    #[error("the LLM gateway refused the request")]
    Failed,
    /// A success status carrying a body this client will not store, log, or
    /// forward to the warden.
    #[error("the LLM gateway answered with an unusable virtual key")]
    BadKey,
    /// The gateway lists no anthropic provider key to attach a virtual key to
    /// (or answered the listing with something unusable). A key minted without
    /// explicit `key_ids` cannot serve inference, so nothing was minted.
    #[error("the LLM gateway offered no usable anthropic provider key to attach")]
    NoProviderKeys,
    /// The gateway minted the key but the echo shows no budget or no provider
    /// config attached — the silent failure mode a singular `budget` field
    /// triggers. An unbudgeted tenant key is unbounded spend, so the mint is
    /// treated as FAILED and the orphan key is revoked best-effort.
    #[error("the LLM gateway minted a key without its budget or providers; refused and revoked")]
    Unbudgeted,
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

/// `POST /api/governance/virtual-keys` request body, in the shape the LIVE
/// gateway accepts (v1.6.9): `budgets` is an array — the documented singular
/// `budget` is silently ignored — and `provider_configs` must carry explicit
/// `key_ids` and a non-empty `allowed_models` or the key cannot serve.
#[derive(Serialize)]
struct MintRequest {
    /// `tenant-<label>` (triage) or `tenant-<label>-assistant`, so the
    /// gateway's own listing names the tenant and the key's job.
    name: String,
    description: String,
    provider_configs: Vec<ProviderConfig>,
    budgets: Vec<Budget>,
    is_active: bool,
}

#[derive(Serialize)]
struct ProviderConfig {
    provider: &'static str,
    weight: u32,
    /// Never empty: empty means deny-all, and wildcards are unreliable.
    allowed_models: Vec<String>,
    /// The provider keys listed by the gateway at mint time. Explicit because
    /// a key minted without them answers "no keys found" at inference.
    key_ids: Vec<String>,
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
    /// Echoed attachments. Only their PRESENCE is checked — a mint whose echo
    /// shows no budget or no provider config is the silent-ignore failure
    /// mode, and installing that key would be unbounded spend.
    #[serde(default)]
    budgets: Vec<Attached>,
    #[serde(default)]
    provider_configs: Vec<Attached>,
}

/// An echoed attachment whose contents this client does not read: presence is
/// the guarantee being checked, and not deserializing the fields keeps
/// whatever rides in them out of this process.
#[derive(Deserialize)]
struct Attached {}

/// `GET /api/providers/anthropic/keys` answer: the provider keys a virtual
/// key must name in `key_ids`.
#[derive(Deserialize)]
struct ProviderKeysResponse {
    keys: Vec<ProviderKey>,
}

#[derive(Deserialize)]
struct ProviderKey {
    id: String,
}

/// The real client: one reqwest client, one admin credential, one base URL,
/// and the allow-list of models every minted key carries.
pub struct BifrostClient {
    base_url: String,
    /// `Basic base64(username:password)`, precomputed once. As secret as the
    /// admin credential it encodes: never logged.
    auth_header: String,
    /// `allowed_models` for every minted TRIAGE key; the assistant's list
    /// arrives per mint. Config guarantees non-empty.
    models: Vec<String>,
    http: reqwest::Client,
}

impl BifrostClient {
    /// `base_url` is a canonical origin (no trailing slash); `admin_token` is
    /// the gateway admin's `username:password` (validated by config), sent as
    /// HTTP Basic; `models` becomes `allowed_models` on every minted triage
    /// key.
    pub fn new(
        base_url: String,
        admin_token: String,
        models: Vec<String>,
        timeout: Duration,
    ) -> Result<Self, BifrostError> {
        let http = reqwest::Client::builder()
            // Redirects refused: every request carries the ADMIN credential
            // and the mint answer carries a live key, and a redirect is how
            // either ends up at a host nobody chose.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| BifrostError::Unreachable)?;
        let auth_header = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(admin_token)
        );
        Ok(Self {
            base_url,
            auth_header,
            models,
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// The provider keys a mint must attach, fetched at mint time so a key
    /// rotated on the gateway is picked up without a redeploy. All listed ids
    /// are used (there is exactly one today).
    async fn provider_key_ids(&self) -> Result<Vec<String>, BifrostError> {
        let resp = self
            .http
            .get(self.url(&format!("/api/providers/{PROVIDER}/keys")))
            .header(reqwest::header::AUTHORIZATION, &self.auth_header)
            .send()
            .await
            .map_err(|_| BifrostError::Unreachable)?;
        match resp.status().as_u16() {
            200 => {
                let body = read_capped(resp).await?;
                let parsed: ProviderKeysResponse =
                    serde_json::from_slice(&body).map_err(|_| BifrostError::NoProviderKeys)?;
                let ids: Vec<String> = parsed.keys.into_iter().map(|k| k.id).collect();
                // An empty listing, or an id shaped like nothing this client
                // would put in a request, is the same refusal: no key mints
                // without a usable provider key behind it.
                if ids.is_empty() || !ids.iter().all(|id| is_id(id)) {
                    return Err(BifrostError::NoProviderKeys);
                }
                Ok(ids)
            }
            401 | 403 => Err(BifrostError::Unauthorized),
            _ => Err(BifrostError::Failed),
        }
    }

    /// Mint the TRIAGE virtual key, named `tenant-<label>`, with a monthly
    /// budget of `budget_usd`, attached to every provider key the gateway
    /// lists and allowed exactly the configured triage models.
    pub async fn mint_virtual_key(
        &self,
        label: &str,
        budget_usd: f64,
    ) -> Result<VirtualKey, BifrostError> {
        let models = self.models.clone();
        self.mint(label, "", &models, budget_usd).await
    }

    /// Mint the ASSISTANT virtual key, named `tenant-<label>-assistant`: the
    /// second key every tenant gets, with its own budget and model list,
    /// because the assistant's on-demand spend must not eat triage's budget
    /// (or vice versa) and wants models triage never calls.
    pub async fn mint_assistant_key(
        &self,
        label: &str,
        models: &[String],
        budget_usd: f64,
    ) -> Result<VirtualKey, BifrostError> {
        self.mint(label, "-assistant", models, budget_usd).await
    }

    /// One mint for both key kinds, so the unbudgeted-echo guardrail and the
    /// shape checks below cannot drift between them. `suffix` is this crate's
    /// own constant (`""` or `"-assistant"`), never input.
    async fn mint(
        &self,
        label: &str,
        suffix: &str,
        models: &[String],
        budget_usd: f64,
    ) -> Result<VirtualKey, BifrostError> {
        // Validated upstream, asserted here: the label lands verbatim in the
        // gateway's key listing.
        crate::labels::validate(label).map_err(|_| BifrostError::BadLabel)?;

        let key_ids = self.provider_key_ids().await?;

        let resp = self
            .http
            .post(self.url("/api/governance/virtual-keys"))
            .header(reqwest::header::AUTHORIZATION, &self.auth_header)
            .json(&MintRequest {
                name: format!("tenant-{label}{suffix}"),
                description: format!("Passband hosted tenant {label}{suffix}"),
                provider_configs: vec![ProviderConfig {
                    provider: PROVIDER,
                    weight: 1,
                    allowed_models: models.to_vec(),
                    key_ids,
                }],
                budgets: vec![Budget {
                    max_limit: budget_usd,
                    reset_duration: BUDGET_RESET,
                }],
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
                // THE GUARDRAIL: the gateway ignores fields it does not
                // recognize, and what a singular `budget` bought us once was a
                // key with no budget at all. If the echo does not show the
                // budget AND the provider config attached, this mint FAILED —
                // an unbudgeted tenant key is unbounded spend on our Anthropic
                // key and must never be installed. The orphan is revoked
                // best-effort; if the revoke also fails, the error still
                // stands and the id dies with this scope, unnamed anywhere.
                if parsed.virtual_key.budgets.is_empty()
                    || parsed.virtual_key.provider_configs.is_empty()
                {
                    let _ = self.revoke_virtual_key(&key.id).await;
                    return Err(BifrostError::Unbudgeted);
                }
                Ok(key)
            }
            401 | 403 => Err(BifrostError::Unauthorized),
            409 => Err(BifrostError::Conflict),
            _ => Err(BifrostError::Failed),
        }
    }

    /// Revoke a virtual key by the id a mint answered with.
    ///
    /// A 404 is SUCCESS: the key is already gone, and treating that as failure
    /// would wedge a stale store pointer forever (the revoke path only clears
    /// the pointer after this returns Ok).
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
            // Already gone is the outcome revoke wanted.
            404 => Ok(()),
            401 | 403 => Err(BifrostError::Unauthorized),
            _ => Err(BifrostError::Failed),
        }
    }

    /// The revoke wire, isolated so fixing it is one line.
    ///
    /// `DELETE /api/governance/virtual-keys/{id}` answers 200 on a live key —
    /// verified against the deployed gateway (Bifrost v1.6.9, 2026-08-13).
    /// What a delete of a MISSING id answers was not exercised, so the caller
    /// keeps its tolerant status mapping.
    fn revoke_request(&self, id: &str) -> reqwest::RequestBuilder {
        self.http
            .delete(self.url(&format!("/api/governance/virtual-keys/{id}")))
            .header(reqwest::header::AUTHORIZATION, &self.auth_header)
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
        routing::{delete, get, post},
    };
    use serde_json::{Value, json};

    use super::*;

    /// The credential the mock is spoken to with, and the header it must
    /// arrive as: Basic, base64 of the whole `username:password`.
    const ADMIN_TOKEN: &str = "admin:the-admin-password";

    fn expected_auth() -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(ADMIN_TOKEN)
        )
    }

    fn test_models() -> Vec<String> {
        vec!["claude-haiku-4-5".into(), "claude-sonnet-5".into()]
    }

    /// What the mock gateway recorded: the Authorization header of every
    /// request, the body of every mint, and the path of every revoke.
    #[derive(Default)]
    struct Recorder {
        auths: Vec<String>,
        mint_bodies: Vec<Value>,
        revoked_ids: Vec<String>,
        /// When set, the mint route answers this instead of a key.
        mint_response: Option<(u16, String)>,
        /// When set, the provider-keys route answers this instead of the one
        /// auto-detected key.
        keys_response: Option<(u16, String)>,
        /// When set, the revoke route answers this status.
        revoke_status: Option<u16>,
    }

    type Shared = Arc<Mutex<Recorder>>;

    fn auth_of(headers: &HeaderMap) -> String {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    async fn spawn_gateway(rec: Shared) -> String {
        let app = Router::new()
            .route(
                "/api/providers/anthropic/keys",
                get(
                    |AxumState(rec): AxumState<Shared>, headers: HeaderMap| async move {
                        let mut r = rec.lock().unwrap();
                        r.auths.push(auth_of(&headers));
                        if let Some((status, body)) = r.keys_response.clone() {
                            return (StatusCode::from_u16(status).unwrap(), body).into_response();
                        }
                        Json(json!({
                            "keys": [{ "id": "ANTHROPIC_API_KEY_auto_detected", "models": [] }],
                        }))
                        .into_response()
                    },
                ),
            )
            .route(
                "/api/governance/virtual-keys",
                post(
                    |AxumState(rec): AxumState<Shared>, headers: HeaderMap, body: String| async move {
                        let mut r = rec.lock().unwrap();
                        r.auths.push(auth_of(&headers));
                        r.mint_bodies
                            .push(serde_json::from_str(&body).unwrap_or(Value::Null));
                        if let Some((status, body)) = r.mint_response.clone() {
                            return (StatusCode::from_u16(status).unwrap(), body).into_response();
                        }
                        // The LIVE gateway echoes the attachments; the client
                        // must see both before it trusts the key.
                        (
                            StatusCode::OK,
                            Json(json!({
                                "message": "Virtual key created successfully",
                                "virtual_key": {
                                    "id": "vk-123",
                                    "value": "sk-bf-THE-KEY-VALUE",
                                    "budgets": [{ "max_limit": 5.0, "reset_duration": "1M" }],
                                    "provider_configs": [{ "provider": "anthropic", "weight": 1 }],
                                },
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
                        r.auths.push(auth_of(&headers));
                        StatusCode::from_u16(r.revoke_status.unwrap_or(200)).unwrap()
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
        BifrostClient::new(
            url,
            ADMIN_TOKEN.into(),
            test_models(),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    /// The happy path: the key listing is fetched and the mint carries Basic
    /// auth, the tenant-named key, the budgets ARRAY with the monthly reset,
    /// and a provider config naming the listed key ids and the configured
    /// models; the answer parses into id + value.
    #[tokio::test]
    async fn mints_a_key_the_way_the_live_governance_api_expects() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        let c = client_for(&rec).await;

        let key = c.mint_virtual_key("ada", 5.0).await.unwrap();
        assert_eq!(key.id, "vk-123");
        assert_eq!(key.value, "sk-bf-THE-KEY-VALUE");

        let r = rec.lock().unwrap();
        // Two requests — the provider-key listing, then the mint — both
        // carrying HTTP Basic, never a bearer.
        assert_eq!(r.auths, vec![expected_auth(), expected_auth()]);
        let body = &r.mint_bodies[0];
        assert_eq!(body["name"], "tenant-ada");
        assert!(body["description"].as_str().is_some_and(|d| !d.is_empty()));
        // `budgets` is an ARRAY: the singular `budget` the docs describe is
        // silently ignored by the live gateway.
        assert_eq!(body["budgets"].as_array().unwrap().len(), 1);
        assert_eq!(body["budgets"][0]["max_limit"], 5.0);
        assert_eq!(body["budgets"][0]["reset_duration"], "1M");
        assert_eq!(body["is_active"], true);
        // One provider config: anthropic, explicit key ids from the listing,
        // and a NON-EMPTY allowed_models (empty means deny-all).
        assert_eq!(body["provider_configs"].as_array().unwrap().len(), 1);
        let pc = &body["provider_configs"][0];
        assert_eq!(pc["provider"], "anthropic");
        assert_eq!(pc["weight"], 1);
        assert_eq!(pc["key_ids"], json!(["ANTHROPIC_API_KEY_auto_detected"]));
        assert_eq!(
            pc["allowed_models"],
            json!(["claude-haiku-4-5", "claude-sonnet-5"])
        );
    }

    /// The assistant mint rides the same wire with its OWN name, models, and
    /// budget: `tenant-<label>-assistant`, the per-mint list rather than the
    /// client's, and the amount this call names.
    #[tokio::test]
    async fn mints_an_assistant_key_with_its_own_name_models_and_budget() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        let c = client_for(&rec).await;

        let models = vec!["claude-opus-4-8".to_string()];
        let key = c.mint_assistant_key("ada", &models, 10.0).await.unwrap();
        assert_eq!(key.id, "vk-123");
        assert_eq!(key.value, "sk-bf-THE-KEY-VALUE");

        let r = rec.lock().unwrap();
        assert_eq!(r.auths, vec![expected_auth(), expected_auth()]);
        let body = &r.mint_bodies[0];
        assert_eq!(body["name"], "tenant-ada-assistant");
        assert_eq!(body["budgets"].as_array().unwrap().len(), 1);
        assert_eq!(body["budgets"][0]["max_limit"], 10.0);
        assert_eq!(body["budgets"][0]["reset_duration"], "1M");
        let pc = &body["provider_configs"][0];
        assert_eq!(pc["provider"], "anthropic");
        assert_eq!(pc["key_ids"], json!(["ANTHROPIC_API_KEY_auto_detected"]));
        // NOT the client's triage list: the assistant models the caller named.
        assert_eq!(pc["allowed_models"], json!(["claude-opus-4-8"]));
    }

    /// The unbudgeted-echo guardrail covers the assistant mint too: shared
    /// wire, shared refusal, and the orphan is revoked the same way.
    #[tokio::test]
    async fn refuses_and_revokes_an_assistant_key_minted_without_its_budget() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        rec.lock().unwrap().mint_response = Some((
            200,
            json!({"virtual_key": {"id": "vk-123", "value": "sk-bf-x"}}).to_string(),
        ));
        let c = client_for(&rec).await;
        let models = vec!["claude-opus-4-8".to_string()];
        assert!(matches!(
            c.mint_assistant_key("ada", &models, 10.0).await,
            Err(BifrostError::Unbudgeted)
        ));
        assert_eq!(rec.lock().unwrap().revoked_ids, vec!["vk-123".to_string()]);
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
                matches!(
                    c.mint_virtual_key("ada", 5.0).await,
                    Err(BifrostError::BadKey)
                ),
                "{bad:?}"
            );
        }
    }

    /// THE GUARDRAIL: a well-shaped key whose echo shows no budgets or no
    /// provider_configs attached is the silent-ignore failure mode. The mint
    /// is reported FAILED and the orphan key is revoked, because installing it
    /// would be unbounded spend.
    #[tokio::test]
    async fn refuses_and_revokes_a_key_minted_without_its_budget() {
        for bad in [
            // No attachments at all.
            json!({"virtual_key": {"id": "vk-123", "value": "sk-bf-x"}}).to_string(),
            // Budget attached, providers not.
            json!({"virtual_key": {
                "id": "vk-123", "value": "sk-bf-x",
                "budgets": [{"max_limit": 5.0}], "provider_configs": [],
            }})
            .to_string(),
            // Providers attached, budget not.
            json!({"virtual_key": {
                "id": "vk-123", "value": "sk-bf-x",
                "budgets": [], "provider_configs": [{"provider": "anthropic"}],
            }})
            .to_string(),
        ] {
            let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
            rec.lock().unwrap().mint_response = Some((200, bad.clone()));
            let c = client_for(&rec).await;
            assert!(
                matches!(
                    c.mint_virtual_key("ada", 5.0).await,
                    Err(BifrostError::Unbudgeted)
                ),
                "{bad:?}"
            );
            // The best-effort revoke went out for the orphan.
            assert_eq!(
                rec.lock().unwrap().revoked_ids,
                vec!["vk-123".to_string()],
                "{bad:?}"
            );
        }
    }

    /// No usable provider key means no mint: an empty listing, garbage, or an
    /// id this client would not put in a request body all name the cause
    /// distinctly, and the mint route is never called.
    #[tokio::test]
    async fn a_gateway_with_no_provider_keys_refuses_the_mint() {
        for bad in [
            json!({"keys": []}).to_string(),
            "not json".to_string(),
            json!({"keys": [{"id": "bad key id"}]}).to_string(),
        ] {
            let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
            rec.lock().unwrap().keys_response = Some((200, bad.clone()));
            let c = client_for(&rec).await;
            assert!(
                matches!(
                    c.mint_virtual_key("ada", 5.0).await,
                    Err(BifrostError::NoProviderKeys)
                ),
                "{bad:?}"
            );
            assert!(rec.lock().unwrap().mint_bodies.is_empty(), "{bad:?}");
        }
    }

    /// A body past the cap is refused mid-read, exactly like the warden
    /// client's: an answer that size is not the API we know.
    #[tokio::test]
    async fn refuses_an_oversized_answer() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        rec.lock().unwrap().mint_response = Some((
            200,
            format!("{{\"pad\":\"{}\"}}", "x".repeat(MAX_RESPONSE_BODY)),
        ));
        let c = client_for(&rec).await;
        assert!(matches!(
            c.mint_virtual_key("ada", 5.0).await,
            Err(BifrostError::Unreachable)
        ));
    }

    /// 401/403 is its own error: nothing will mint until the admin credential
    /// is fixed, and the caller's log line should say so. Asserted on both
    /// routes the mint path speaks.
    #[tokio::test]
    async fn a_refused_admin_credential_is_distinct() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        rec.lock().unwrap().mint_response = Some((401, json!({"error":"nope"}).to_string()));
        let c = client_for(&rec).await;
        assert!(matches!(
            c.mint_virtual_key("ada", 5.0).await,
            Err(BifrostError::Unauthorized)
        ));

        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        rec.lock().unwrap().keys_response = Some((403, json!({"error":"nope"}).to_string()));
        let c = client_for(&rec).await;
        assert!(matches!(
            c.mint_virtual_key("ada", 5.0).await,
            Err(BifrostError::Unauthorized)
        ));
    }

    /// Revoke presents the stored id on the verified wire with Basic auth, and
    /// refuses an id it would never have accepted from a mint — before any
    /// socket is opened.
    #[tokio::test]
    async fn revokes_by_id_and_refuses_a_corrupt_one() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        let c = client_for(&rec).await;

        c.revoke_virtual_key("vk-123").await.unwrap();
        {
            let r = rec.lock().unwrap();
            assert_eq!(r.revoked_ids, vec!["vk-123".to_string()]);
            assert_eq!(r.auths, vec![expected_auth()]);
        }

        for bad in ["", "../admin", "vk/123", "vk 123", "vk%2f"] {
            assert!(
                matches!(c.revoke_virtual_key(bad).await, Err(BifrostError::BadId)),
                "{bad:?}"
            );
        }
    }

    /// A 404 on revoke is already-revoked, which is success: anything else
    /// would wedge a stale store pointer forever, since the pointer is only
    /// cleared after revoke returns Ok.
    #[tokio::test]
    async fn revoking_a_missing_key_is_already_done() {
        let rec: Shared = Arc::new(Mutex::new(Recorder::default()));
        rec.lock().unwrap().revoke_status = Some(404);
        let c = client_for(&rec).await;
        c.revoke_virtual_key("vk-stale").await.unwrap();
        assert_eq!(
            rec.lock().unwrap().revoked_ids,
            vec!["vk-stale".to_string()]
        );
    }

    /// The label reaches the gateway's key listing verbatim; one this crate
    /// would not have validated is refused before the socket is opened.
    #[tokio::test]
    async fn refuses_a_label_it_would_not_have_validated() {
        let c = BifrostClient::new(
            // Port 1: nothing listens, so reaching the socket would fail
            // rather than pass by accident.
            "http://127.0.0.1:1".into(),
            ADMIN_TOKEN.into(),
            test_models(),
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
