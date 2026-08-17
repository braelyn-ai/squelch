//! The control-plane wire: eight routes and a health check.
//!
//! ```text
//! POST   /v1/tenants                     -> 201 { recipient }
//! PUT    /v1/tenants/{label}/credentials -> 200 { pair_code, pair_url, deep_link }
//! PUT    /v1/tenants/{label}/llm-key     -> 200 {}
//! GET    /v1/tenants/{label}             -> 200 { status } | 404
//! GET    /v1/tenants/{label}/drift       -> 200 { status, deployment_present, foreign, changes }
//! POST   /v1/tenants/{label}/reconcile   -> 200 { deployment, status } | 409
//! POST   /v1/tenants/{label}/pair        -> 200 { pair_code, pair_url, deep_link }
//! DELETE /v1/tenants/{label}             -> 204
//! GET    /healthz                        -> 200 ok
//! ```
//!
//! Every handler is a thin shell: parse, hand to [`crate::provision::Warden`],
//! shape the answer. Nothing here decides anything.
//!
//! PRIVACY: a response body carries a public age recipient, a status word, or
//! the pairing handoff the control plane is about to show the user. It never
//! carries an identity, a path, an API error, a mailbox address, or the
//! ciphertext that came in. A 500 is a machine reason and nothing else; the
//! detail behind it is in this pod's log.
//!
//! The drift report is the one body that quotes cluster state back, and what
//! it quotes is a Deployment spec: field paths, image tags, mount points, and
//! Secret references BY NAME. A Deployment spec holds no secret material - the
//! kubelet is what resolves a reference into a value - and it names no
//! mailbox. Anything that would carry more than a spec does not belong in it.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::WardenState;
use crate::pair::Pairing;
use crate::provision::WardenError;

/// Ceiling on a request body. The only large field is the age-armored
/// credential, which [`crate::validate::MAX_CIPHERTEXT`] caps at 64 KiB; this
/// leaves room for the JSON around it and nothing more.
pub const MAX_BODY: usize = 96 * 1024;

pub async fn healthz() -> &'static str {
    "ok"
}

/// Turn a domain error into the wire's version of it.
///
/// The 4xx bodies name the constraint that was violated, because the control
/// plane shows a person a form again; the 5xx body is a machine reason with no
/// detail, because the detail is this cluster's business.
fn error_response(e: &WardenError) -> Response {
    let (status, error, detail) = match e {
        WardenError::InvalidLabel(inner) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_label",
            Some(inner.to_string()),
        ),
        WardenError::InvalidEmail(inner) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_account_email",
            Some(inner.to_string()),
        ),
        WardenError::InvalidCiphertext(inner) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_cred_read_ciphertext",
            Some(inner.to_string()),
        ),
        WardenError::InvalidApiKey(inner) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_api_key",
            Some(inner.to_string()),
        ),
        // The llm-key body parsed but named neither slot; a well-formed
        // request that violates the "at least one key" constraint, so a 422
        // that names it, not a 400.
        WardenError::NoKeys => (StatusCode::UNPROCESSABLE_ENTITY, "no_keys", None),
        WardenError::Conflict => (StatusCode::CONFLICT, "label_exists", None),
        WardenError::NotFound => (StatusCode::NOT_FOUND, "not_found", None),
        // 409 rather than 404: the tenant is real, and it is its state that
        // has no workload to converge. The control plane's answer is a
        // different call (finish the signup, or re-consent), not a retry.
        WardenError::NotReconcilable => (StatusCode::CONFLICT, "not_reconcilable", None),
        // 503, not 422: the request was fine, this deployment is what lacks
        // the LLM gateway. The control plane should not be calling here at all.
        WardenError::LlmNotConfigured => {
            (StatusCode::SERVICE_UNAVAILABLE, "llm_not_configured", None)
        }
        WardenError::Cluster { reason } => (StatusCode::INTERNAL_SERVER_ERROR, *reason, None),
    };
    match detail {
        Some(detail) => (status, Json(json!({ "error": error, "detail": detail }))).into_response(),
        None => (status, Json(json!({ "error": error }))).into_response(),
    }
}

impl IntoResponse for WardenError {
    fn into_response(self) -> Response {
        error_response(&self)
    }
}

/// Parse a JSON body, returning the serde message on failure.
///
/// That message names the missing or mistyped FIELD, never the value, so it is
/// safe to hand back, and it is the only thing that makes a 400 here debuggable
/// from the other end.
fn parse_json<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, String> {
    serde_json::from_slice(body).map_err(|e| e.to_string())
}

/// The 400 for a body that did not parse.
fn malformed(detail: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "malformed_request", "detail": detail })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct CreateTenant {
    label: String,
    account_email: String,
}

#[derive(Debug, Serialize)]
struct CreateTenantResponse {
    /// `age1...`. Public by construction: it is the half of the pair that
    /// exists to be handed out.
    recipient: String,
}

#[derive(Debug, Deserialize)]
struct SetCredentials {
    /// Age-armored ciphertext of the tenant's read credential, sealed by the
    /// control plane to the recipient this warden minted. Stored verbatim and
    /// never read here.
    cred_read_ciphertext: String,
}

/// No `Debug`: both fields are live virtual keys, and a derived formatter is
/// how one ends up in a log line by accident.
#[derive(Deserialize)]
struct SetLlmKey {
    /// The tenant's LLM gateway virtual key, minted by the control plane.
    /// Stored verbatim in the tenant's Secret and never read back here.
    /// Defaulted, matching the control plane's wire (it skips absent fields):
    /// a half-failed mint installs the half that exists, and an absent slot
    /// means "leave the installed key as it is", never "clear it".
    #[serde(default)]
    api_key: Option<String>,
    /// The assistant relay virtual key the daemon proxies the Passband
    /// assistant through, when the control plane has minted one. Defaulted so
    /// a control plane from before the assistant era still parses: absent
    /// means the triage slot only, with the same leave-it-alone semantics.
    #[serde(default)]
    assistant_api_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct PairResponse {
    pair_code: String,
    pair_url: String,
    deep_link: String,
}

impl From<Pairing> for PairResponse {
    fn from(p: Pairing) -> Self {
        Self {
            pair_code: p.pair_code,
            pair_url: p.pair_url,
            deep_link: p.deep_link,
        }
    }
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    status: &'static str,
}

/// `POST /v1/tenants` - mint this tenant's key pair and hand back the public
/// half. Nothing runs yet.
pub async fn create_tenant(State(state): State<WardenState>, body: Bytes) -> Response {
    let req: CreateTenant = match parse_json(&body) {
        Ok(req) => req,
        Err(detail) => return malformed(detail),
    };
    match state
        .warden()
        .create_tenant(&req.label, &req.account_email)
        .await
    {
        Ok(created) => (
            StatusCode::CREATED,
            Json(CreateTenantResponse {
                recipient: created.recipient,
            }),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

/// `PUT /v1/tenants/{label}/credentials` - store the sealed blob, bring the
/// tenant up, and hand back its first pairing.
pub async fn set_credentials(
    State(state): State<WardenState>,
    Path(label): Path<String>,
    body: Bytes,
) -> Response {
    let req: SetCredentials = match parse_json(&body) {
        Ok(req) => req,
        Err(detail) => return malformed(detail),
    };
    match state
        .warden()
        .set_credentials(&label, &req.cred_read_ciphertext)
        .await
    {
        Ok(pairing) => (StatusCode::OK, Json(PairResponse::from(pairing))).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `PUT /v1/tenants/{label}/llm-key` - store or rotate the tenant's LLM
/// gateway virtual keys (triage and/or the assistant relay; at least one). An
/// absent slot is left exactly as it is. A running tenant is rolled onto the
/// result; a pending one picks it up when the workload is applied.
pub async fn set_llm_key(
    State(state): State<WardenState>,
    Path(label): Path<String>,
    body: Bytes,
) -> Response {
    let req: SetLlmKey = match parse_json(&body) {
        Ok(req) => req,
        Err(detail) => return malformed(detail),
    };
    match state
        .warden()
        .set_llm_key(
            &label,
            req.api_key.as_deref(),
            req.assistant_api_key.as_deref(),
        )
        .await
    {
        // Nothing to hand back: the keys came in, and they never go out.
        Ok(()) => (StatusCode::OK, Json(json!({}))).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `GET /v1/tenants/{label}` - what the cluster says about this tenant.
pub async fn get_tenant(State(state): State<WardenState>, Path(label): Path<String>) -> Response {
    match state.warden().status(&label).await {
        Ok(status) => (
            StatusCode::OK,
            Json(StatusResponse {
                status: status.as_str(),
            }),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

/// `GET /v1/tenants/{label}/drift` - who else owns part of this tenant's
/// Deployment, and what an apply of today's render would change. Read-only:
/// the apply it makes is a dry run.
pub async fn get_drift(State(state): State<WardenState>, Path(label): Path<String>) -> Response {
    match state.warden().drift(&label).await {
        Ok(report) => (StatusCode::OK, Json(report)).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `POST /v1/tenants/{label}/reconcile` - put a running tenant back onto
/// today's render, deleting and recreating its Deployment if another field
/// manager owns part of it. No body: the tenant is the whole request.
pub async fn reconcile_tenant(
    State(state): State<WardenState>,
    Path(label): Path<String>,
) -> Response {
    match state.warden().reconcile(&label).await {
        Ok(reconciled) => (StatusCode::OK, Json(reconciled)).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `DELETE /v1/tenants/{label}` - take the workload down, keep the data.
pub async fn delete_tenant(
    State(state): State<WardenState>,
    Path(label): Path<String>,
) -> Response {
    match state.warden().delete(&label).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

/// `POST /v1/tenants/{label}/pair` - re-mint a pairing code for a later
/// device. No body: the tenant is the whole request.
pub async fn repair_tenant(
    State(state): State<WardenState>,
    Path(label): Path<String>,
) -> Response {
    match state.warden().repair(&label).await {
        Ok(pairing) => (StatusCode::OK, Json(PairResponse::from(pairing))).into_response(),
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::testing::{Harness, TEST_TOKEN, armored, llm_test_config};

    /// Send a request through the real router, with the real bearer.
    async fn call(h: &Harness, req: Request<Body>) -> (StatusCode, Value) {
        let response = crate::router(h.state()).oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, body)
    }

    fn authed(method: &str, path: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn create_body(label: &str) -> String {
        serde_json::json!({
            "label": label,
            "account_email": format!("{label}@example.com"),
        })
        .to_string()
    }

    fn credential_body(label: &str) -> String {
        serde_json::json!({ "cred_read_ciphertext": armored(label) }).to_string()
    }

    #[tokio::test]
    async fn walks_a_tenant_through_both_phases_and_back_out() {
        let h = Harness::new();

        let (status, body) = call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        assert_eq!(status, StatusCode::CREATED);
        let recipient = body["recipient"].as_str().unwrap().to_string();
        assert!(recipient.starts_with("age1"));
        // The 201 carries the public half and nothing else.
        assert_eq!(body.as_object().unwrap().len(), 1);
        assert!(!body.to_string().contains("AGE-SECRET-KEY"));
        assert!(!body.to_string().contains("example.com"));

        let (status, body) = call(&h, authed("GET", "/v1/tenants/alice", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "pending");

        let (status, body) = call(
            &h,
            authed(
                "PUT",
                "/v1/tenants/alice/credentials",
                &credential_body("alice"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pair_code"], "ABCD-1234");
        assert_eq!(body["pair_url"], "https://alice.passband.email");
        assert!(
            body["deep_link"]
                .as_str()
                .unwrap()
                .starts_with("passband://pair?url=")
        );
        // Nothing about the request comes back out.
        assert!(!body.to_string().contains("AGE ENCRYPTED"));

        let (status, body) = call(&h, authed("GET", "/v1/tenants/alice", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "active");

        h.cluster.exec_prints(&crate::testing::pair_stdout(
            "WXYZ-9876",
            "https://alice.passband.email",
        ));
        let (status, body) = call(&h, authed("POST", "/v1/tenants/alice/pair", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pair_code"], "WXYZ-9876");

        let (status, body) = call(&h, authed("DELETE", "/v1/tenants/alice", "")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(body, Value::Null);

        let (status, body) = call(&h, authed("GET", "/v1/tenants/alice", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "stopped");
    }

    #[tokio::test]
    async fn a_pending_label_is_idempotent_and_a_live_one_is_a_409() {
        let h = Harness::new();
        let (status, first) = call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        assert_eq!(status, StatusCode::CREATED);

        // The retry after a lost response: same label, same address, same key.
        let (status, second) = call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(first["recipient"], second["recipient"]);

        call(
            &h,
            authed(
                "PUT",
                "/v1/tenants/alice/credentials",
                &credential_body("alice"),
            ),
        )
        .await;
        let (status, body) = call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "label_exists");
    }

    #[tokio::test]
    async fn invalid_input_is_a_422_that_names_the_constraint() {
        let h = Harness::new();
        for label in ["ab", "-alice", "ALICE!", "mcp", &"a".repeat(31)] {
            let (status, body) = call(&h, authed("POST", "/v1/tenants", &create_body(label))).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{label}");
            assert_eq!(body["error"], "invalid_label", "{label}");
            // The constraint is named, and the request is not echoed: the
            // address in particular must not travel back to a log aggregator
            // on the other side of this call.
            assert!(body["detail"].is_string());
            assert!(!body.to_string().contains("example.com"), "{label}");
        }

        let bad_email =
            serde_json::json!({ "label": "bob", "account_email": "nobody" }).to_string();
        let (status, body) = call(&h, authed("POST", "/v1/tenants", &bad_email)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid_account_email");

        // The one that matters: a plaintext credential is refused before it can
        // be stored anywhere.
        call(&h, authed("POST", "/v1/tenants", &create_body("carol"))).await;
        let plaintext =
            serde_json::json!({ "cred_read_ciphertext": "{\"refresh_token\":\"1//0gREAL\"}" })
                .to_string();
        let (status, body) = call(
            &h,
            authed("PUT", "/v1/tenants/carol/credentials", &plaintext),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid_cred_read_ciphertext");
        assert_eq!(h.cluster.applied_names(), vec!["carol-identity"]);
    }

    /// The llm-key route, end to end on the wire: a 200 with an empty object,
    /// and never the key back out.
    #[tokio::test]
    async fn the_llm_key_route_stores_and_says_nothing() {
        let h = Harness::with_config(llm_test_config());
        call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;

        let body = serde_json::json!({ "api_key": "sk-vk-abc123" }).to_string();
        let (status, body) = call(&h, authed("PUT", "/v1/tenants/alice/llm-key", &body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({}));
        assert!(!body.to_string().contains("sk-vk"));

        // A key that would break the env value it becomes is a 422 that names
        // the constraint and never echoes the key.
        let bad = serde_json::json!({ "api_key": "sk\nvk" }).to_string();
        let (status, body) = call(&h, authed("PUT", "/v1/tenants/alice/llm-key", &bad)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid_api_key");
        assert!(!body.to_string().contains("sk\nvk"));

        // An empty one is the same class of refusal.
        let empty = serde_json::json!({ "api_key": "" }).to_string();
        let (status, _) = call(&h, authed("PUT", "/v1/tenants/alice/llm-key", &empty)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        // Both fields are optional on the wire — absent means "leave that
        // slot alone" — so an empty object parses, and the refusal is the
        // named constraint: a PUT that names neither slot installs nothing.
        let (status, body) = call(&h, authed("PUT", "/v1/tenants/alice/llm-key", "{}")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "no_keys");

        // And a label nobody minted has nothing to key.
        let body = serde_json::json!({ "api_key": "sk-vk-abc123" }).to_string();
        let (status, _) = call(&h, authed("PUT", "/v1/tenants/nobody/llm-key", &body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The extended llm-key body: an assistant key rides beside the triage
    /// key, a body without the field — an old control plane — still lands,
    /// and a broken assistant key is the same 422 as a broken triage key.
    #[tokio::test]
    async fn the_llm_key_route_takes_an_optional_assistant_key() {
        let h = Harness::with_config(llm_test_config());
        call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;

        // Backward compat: no assistant_api_key field at all is a 200, and
        // the stored Secret has no assistant entry.
        let body = serde_json::json!({ "api_key": "sk-vk-triage" }).to_string();
        let (status, _) = call(&h, authed("PUT", "/v1/tenants/alice/llm-key", &body)).await;
        assert_eq!(status, StatusCode::OK);
        let stored = h.cluster.secret("alice-llm").unwrap();
        assert!(!stored.data.unwrap().contains_key("assistant-api-key"));

        // Both keys: still a 200 with an empty object, never a key back out.
        let body = serde_json::json!({
            "api_key": "sk-vk-triage",
            "assistant_api_key": "sk-vk-assistant",
        })
        .to_string();
        let (status, body) = call(&h, authed("PUT", "/v1/tenants/alice/llm-key", &body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({}));
        let stored = h.cluster.secret("alice-llm").unwrap();
        assert!(stored.data.unwrap().contains_key("assistant-api-key"));

        // An assistant-only body — the shape a re-run after a half-failed
        // mint sends — is a 200, and the triage slot it did not name is left
        // exactly as it was.
        let body = serde_json::json!({ "assistant_api_key": "sk-vk-assistant-2" }).to_string();
        let (status, _) = call(&h, authed("PUT", "/v1/tenants/alice/llm-key", &body)).await;
        assert_eq!(status, StatusCode::OK);
        let stored = h.cluster.secret("alice-llm").unwrap().data.unwrap();
        assert!(stored.contains_key("api-key"));
        assert!(stored.contains_key("assistant-api-key"));

        // A broken assistant key is refused with the constraint named and
        // neither key echoed.
        let bad = serde_json::json!({
            "api_key": "sk-vk-triage",
            "assistant_api_key": "sk\nassistant",
        })
        .to_string();
        let (status, body) = call(&h, authed("PUT", "/v1/tenants/alice/llm-key", &bad)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid_api_key");
        assert!(!body.to_string().contains("sk-vk"));
        assert!(!body.to_string().contains("assistant"));
    }

    /// A warden with no LLM gateway configured refuses the llm-key route: a
    /// 503 naming the reason, no key stored, and never the key echoed back.
    #[tokio::test]
    async fn the_llm_key_route_refuses_when_the_gateway_is_not_configured() {
        let h = Harness::new();
        call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;

        let body = serde_json::json!({ "api_key": "sk-vk-abc123" }).to_string();
        let (status, body) = call(&h, authed("PUT", "/v1/tenants/alice/llm-key", &body)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "llm_not_configured");
        assert!(!body.to_string().contains("sk-vk"));
        assert!(h.cluster.secret("alice-llm").is_none());
    }

    /// Edit the stored Deployment the way a person with kubectl does: a field
    /// the render disagrees with, and a ledger entry saying somebody else owns
    /// part of the object now. The two findings are independent, which is why
    /// this stamps both.
    async fn hand_edit_the_deployment(h: &Harness) {
        use crate::cluster::{Cluster, Kind, Object};
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::{FieldsV1, ManagedFieldsEntry};

        let Some(Object::Deployment(mut deployment)) = h.cluster.object(Kind::Deployment, "alice")
        else {
            panic!("no deployment");
        };
        deployment
            .spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap()
            .containers[0]
            .image = Some("ghcr.io/braelyn-ai/squelchd:hand-edited".to_string());
        deployment.metadata.managed_fields = Some(vec![ManagedFieldsEntry {
            manager: Some("kubectl-set".to_string()),
            operation: Some("Update".to_string()),
            fields_v1: Some(FieldsV1(serde_json::json!({
                "f:spec": { "f:template": { "f:spec": { "f:containers": {
                    "k:{\"name\":\"squelchd\"}": { "f:env": {} }
                }}}}
            }))),
            ..Default::default()
        }]);
        h.cluster
            .apply(Object::Deployment(deployment))
            .await
            .unwrap();
    }

    /// The drift route on the wire: a clean tenant, then the same tenant after
    /// somebody edited it by hand.
    #[tokio::test]
    async fn the_drift_route_reports_a_hand_edited_tenant() {
        let h = Harness::new();
        call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        call(
            &h,
            authed(
                "PUT",
                "/v1/tenants/alice/credentials",
                &credential_body("alice"),
            ),
        )
        .await;

        let (status, body) = call(&h, authed("GET", "/v1/tenants/alice/drift", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "active");
        assert_eq!(body["deployment_present"], true);
        assert_eq!(body["foreign"], serde_json::json!([]));
        assert_eq!(body["changes"], serde_json::json!([]));

        hand_edit_the_deployment(&h).await;
        let (status, body) = call(&h, authed("GET", "/v1/tenants/alice/drift", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["foreign"][0]["manager"], "kubectl-set");
        assert_eq!(body["foreign"][0]["operation"], "Update");
        assert_eq!(
            body["foreign"][0]["paths"][0],
            "spec.template.spec.containers[squelchd].env"
        );
        assert_eq!(
            body["changes"][0]["path"],
            "spec.template.spec.containers[squelchd].image"
        );
        assert_eq!(
            body["changes"][0]["live"],
            "ghcr.io/braelyn-ai/squelchd:hand-edited"
        );
        // From the config rather than spelled out: what this asserts is that
        // the render's image is what a re-apply would restore, not which tag
        // this deployment happens to pin.
        assert_eq!(body["changes"][0]["rendered"], h.config.image);
        // A spec is field names and references by name. Nothing about this
        // tenant's person, and nothing that came in on a request.
        let rendered = body.to_string();
        assert!(!rendered.contains("example.com"));
        assert!(!rendered.contains("AGE ENCRYPTED"));
    }

    /// A tenant with no workload answers 200 with an honest empty report,
    /// rather than a 404 that would read as "no such tenant".
    #[tokio::test]
    async fn the_drift_route_answers_for_a_tenant_that_is_not_running() {
        let h = Harness::new();
        call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        let (status, body) = call(&h, authed("GET", "/v1/tenants/alice/drift", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "pending");
        assert_eq!(body["deployment_present"], false);
        assert_eq!(body["foreign"], serde_json::json!([]));
        assert_eq!(body["changes"], serde_json::json!([]));

        // And a label that is not a label never reaches the cluster.
        let (status, body) = call(&h, authed("GET", "/v1/tenants/-nope-/drift", "")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid_label");
    }

    /// The reconcile route on the wire: the two words the control plane reads,
    /// and the recreate that a hand edit forces.
    #[tokio::test]
    async fn the_reconcile_route_converges_and_then_recreates() {
        let h = Harness::new();
        call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        call(
            &h,
            authed(
                "PUT",
                "/v1/tenants/alice/credentials",
                &credential_body("alice"),
            ),
        )
        .await;

        let (status, body) = call(&h, authed("POST", "/v1/tenants/alice/reconcile", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({ "deployment": "converged", "status": "active" })
        );
        assert!(h.cluster.deleted().is_empty());

        hand_edit_the_deployment(&h).await;
        let (status, body) = call(&h, authed("POST", "/v1/tenants/alice/reconcile", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deployment"], "recreated");
        assert_eq!(body["status"], "active");
        // The body is two fixed words. Nothing about this tenant's person, and
        // nothing about this cluster.
        assert!(!body.to_string().contains("example.com"));
    }

    /// A tenant with no workload is a 409, not a 404: the tenant is real, and
    /// what it needs is a different call.
    #[tokio::test]
    async fn the_reconcile_route_refuses_a_tenant_with_nothing_running() {
        let h = Harness::new();
        call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;

        let (status, body) = call(&h, authed("POST", "/v1/tenants/alice/reconcile", "")).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body, serde_json::json!({ "error": "not_reconcilable" }));

        call(
            &h,
            authed(
                "PUT",
                "/v1/tenants/alice/credentials",
                &credential_body("alice"),
            ),
        )
        .await;
        call(&h, authed("DELETE", "/v1/tenants/alice", "")).await;
        let (status, body) = call(&h, authed("POST", "/v1/tenants/alice/reconcile", "")).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "not_reconcilable");

        let (status, body) = call(&h, authed("POST", "/v1/tenants/-nope-/reconcile", "")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid_label");
    }

    #[tokio::test]
    async fn unknown_tenants_404_except_on_delete() {
        let h = Harness::new();
        let (status, body) = call(&h, authed("GET", "/v1/tenants/nobody", "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");

        let (status, body) = call(&h, authed("GET", "/v1/tenants/nobody/drift", "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");

        let (status, body) = call(&h, authed("POST", "/v1/tenants/nobody/reconcile", "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");

        let (status, _) = call(&h, authed("POST", "/v1/tenants/nobody/pair", "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = call(
            &h,
            authed(
                "PUT",
                "/v1/tenants/nobody/credentials",
                &credential_body("nobody"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // DELETE is idempotent: the control plane retries it on its own unwind
        // paths and must not have to special-case a 404 there.
        let (status, _) = call(&h, authed("DELETE", "/v1/tenants/nobody", "")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_broken_cluster_is_a_500_with_a_machine_reason_and_no_detail() {
        let h = Harness::new();
        call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        h.cluster.never_ready();

        let (status, body) = call(
            &h,
            authed(
                "PUT",
                "/v1/tenants/alice/credentials",
                &credential_body("alice"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], "not_ready");
        assert!(body.get("detail").is_none());
        // Nothing about this cluster crosses the wire.
        let rendered = body.to_string();
        for leak in ["tenants", "kube", "namespace", "10.0.0.0"] {
            assert!(!rendered.contains(leak), "{leak} leaked: {rendered}");
        }
    }

    #[tokio::test]
    async fn malformed_json_is_a_400() {
        let h = Harness::new();
        let (status, body) = call(&h, authed("POST", "/v1/tenants", "{nope")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "malformed_request");

        // A missing field is the same class of caller bug.
        let (status, _) = call(&h, authed("POST", "/v1/tenants", r#"{"label":"alice"}"#)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = call(&h, authed("PUT", "/v1/tenants/alice/credentials", "{}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Every gated route, every way of getting the bearer wrong. The answer is
    /// always the same bare 401.
    #[tokio::test]
    async fn every_route_is_gated() {
        let h = Harness::new();
        let routes = [
            ("POST", "/v1/tenants"),
            ("PUT", "/v1/tenants/alice/credentials"),
            ("PUT", "/v1/tenants/alice/llm-key"),
            ("GET", "/v1/tenants/alice"),
            ("GET", "/v1/tenants/alice/drift"),
            ("POST", "/v1/tenants/alice/reconcile"),
            ("DELETE", "/v1/tenants/alice"),
            ("POST", "/v1/tenants/alice/pair"),
        ];
        let bad_headers = [
            None,
            Some(String::new()),
            Some("Bearer".to_string()),
            Some("Bearer ".to_string()),
            Some("Basic hunter2".to_string()),
            // Wrong, and a prefix of the real one: the compare is constant time
            // and length-checked, so neither is closer than the other.
            Some(format!("Bearer {}", &TEST_TOKEN[..TEST_TOKEN.len() - 1])),
            Some(format!("Bearer {TEST_TOKEN}x")),
            Some("Bearer short".to_string()),
        ];

        for (method, path) in routes {
            for header_value in &bad_headers {
                let mut req = Request::builder().method(method).uri(path);
                if let Some(value) = header_value {
                    req = req.header(header::AUTHORIZATION, value);
                }
                let req = req.body(Body::from(create_body("alice"))).unwrap();
                let response = crate::router(h.state()).oneshot(req).await.unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::UNAUTHORIZED,
                    "{method} {path} with {header_value:?}"
                );
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                assert!(bytes.is_empty(), "401 must have no body");
            }
        }
        // Nothing got past the gate: the cluster was never touched.
        assert!(h.cluster.applied().is_empty());
        assert!(h.cluster.deleted().is_empty());
    }

    #[tokio::test]
    async fn healthz_answers_without_a_token() {
        let h = Harness::new();
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let response = crate::router(h.state()).oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_before_it_is_parsed() {
        let h = Harness::new();
        call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        let huge = serde_json::json!({
            "cred_read_ciphertext": "A".repeat(super::MAX_BODY + 1),
        })
        .to_string();
        let (status, _) = call(&h, authed("PUT", "/v1/tenants/alice/credentials", &huge)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        // The identity from phase one, and nothing more.
        assert_eq!(h.cluster.applied_names(), vec!["alice-identity"]);
    }
}
