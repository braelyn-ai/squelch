//! The control-plane wire: six routes and a health check.
//!
//! ```text
//! POST   /v1/tenants                     -> 201 { recipient }
//! PUT    /v1/tenants/{label}/credentials -> 200 { pair_code, pair_url, deep_link }
//! PUT    /v1/tenants/{label}/llm-key     -> 200 {}
//! GET    /v1/tenants/{label}             -> 200 { status } | 404
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
        WardenError::Conflict => (StatusCode::CONFLICT, "label_exists", None),
        WardenError::NotFound => (StatusCode::NOT_FOUND, "not_found", None),
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

#[derive(Debug, Deserialize)]
struct SetLlmKey {
    /// The tenant's LLM gateway virtual key, minted by the control plane.
    /// Stored verbatim in the tenant's Secret and never read back here.
    api_key: String,
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
/// gateway virtual key. A running tenant is rolled onto it; a pending one
/// picks it up when the workload is applied.
pub async fn set_llm_key(
    State(state): State<WardenState>,
    Path(label): Path<String>,
    body: Bytes,
) -> Response {
    let req: SetLlmKey = match parse_json(&body) {
        Ok(req) => req,
        Err(detail) => return malformed(detail),
    };
    match state.warden().set_llm_key(&label, &req.api_key).await {
        // Nothing to hand back: the key came in, and it never goes out.
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

        // A missing field is a caller bug, not a validation refusal.
        let (status, _) = call(&h, authed("PUT", "/v1/tenants/alice/llm-key", "{}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // And a label nobody minted has nothing to key.
        let body = serde_json::json!({ "api_key": "sk-vk-abc123" }).to_string();
        let (status, _) = call(&h, authed("PUT", "/v1/tenants/nobody/llm-key", &body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
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

    #[tokio::test]
    async fn unknown_tenants_404_except_on_delete() {
        let h = Harness::new();
        let (status, body) = call(&h, authed("GET", "/v1/tenants/nobody", "")).await;
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
