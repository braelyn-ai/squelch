//! The control-plane wire: four routes and a health check.
//!
//! ```text
//! POST   /v1/tenants              -> 201 { port, pair_code, pair_url, deep_link }
//! GET    /v1/tenants/{label}      -> 200 { status, port } | 404
//! DELETE /v1/tenants/{label}      -> 204
//! POST   /v1/tenants/{label}/pair -> 200 { pair_code, pair_url, deep_link }
//! GET    /healthz                 -> 200 ok
//! ```
//!
//! Every handler is a thin shell: parse, hand to [`crate::provision::Warden`]
//! on a blocking thread, shape the answer. The provisioning work forks
//! processes and fsyncs files, so it must not run on an async worker.
//!
//! PRIVACY: a response body carries a port, a status word, and the pairing
//! handoff the control plane is going to show the user. It never carries a
//! path, an OS error, a mailbox address, or the ciphertext that came in. A 500
//! is a machine reason and nothing else; the detail behind it is in this box's
//! journal.

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
use crate::provision::{Pairing, ProvisionRequest, WardenError};

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
/// detail, because the detail is this box's business.
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
        WardenError::Conflict => (StatusCode::CONFLICT, "label_exists", None),
        WardenError::NotFound => (StatusCode::NOT_FOUND, "not_found", None),
        WardenError::Host { reason } => (StatusCode::INTERNAL_SERVER_ERROR, *reason, None),
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

/// Run blocking provisioning work off the async runtime.
///
/// A panic inside becomes a 500 with a machine reason rather than a dropped
/// connection: the control plane is mid-signup and needs an answer it can act
/// on. The panic itself is already on stderr via the default hook.
async fn blocking<T, F>(f: F) -> Result<T, WardenError>
where
    F: FnOnce() -> Result<T, WardenError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(error = %e, "warden task did not complete");
            Err(WardenError::host("task_failed"))
        }
    }
}

/// Parse a JSON body, returning the serde message on failure.
///
/// That message names the missing or mistyped FIELD, never the value, so it is
/// safe to hand back — and it is the only thing that makes a 400 here
/// debuggable from the other end.
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
    /// Age-armored ciphertext of the tenant's read credential, encrypted by the
    /// control plane to the box's recipient. The warden writes it and never
    /// reads it.
    cred_read_ciphertext: String,
}

#[derive(Debug, Serialize)]
struct CreateTenantResponse {
    port: u16,
    pair_code: String,
    pair_url: String,
    deep_link: String,
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
    port: u16,
}

/// `POST /v1/tenants` — provision a tenant and hand back its first pairing.
pub async fn create_tenant(State(state): State<WardenState>, body: Bytes) -> Response {
    let req: CreateTenant = match parse_json(&body) {
        Ok(req) => req,
        Err(detail) => return malformed(detail),
    };
    let warden = state.warden();
    let result = blocking(move || {
        warden.provision(ProvisionRequest {
            label: req.label,
            account_email: req.account_email,
            cred_read_ciphertext: req.cred_read_ciphertext,
        })
    })
    .await;

    match result {
        Ok(done) => (
            StatusCode::CREATED,
            Json(CreateTenantResponse {
                port: done.port,
                pair_code: done.pairing.pair_code,
                pair_url: done.pairing.pair_url,
                deep_link: done.pairing.deep_link,
            }),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

/// `GET /v1/tenants/{label}` — what systemd says about this tenant.
pub async fn get_tenant(State(state): State<WardenState>, Path(label): Path<String>) -> Response {
    let warden = state.warden();
    match blocking(move || warden.status(&label)).await {
        Ok(view) => (
            StatusCode::OK,
            Json(StatusResponse {
                status: view.status,
                port: view.port,
            }),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

/// `DELETE /v1/tenants/{label}` — stop and disable, keep the data.
pub async fn delete_tenant(
    State(state): State<WardenState>,
    Path(label): Path<String>,
) -> Response {
    let warden = state.warden();
    match blocking(move || warden.deprovision(&label)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

/// `POST /v1/tenants/{label}/pair` — re-mint a pairing code for a later
/// device. No body: the tenant is the whole request.
pub async fn repair_tenant(
    State(state): State<WardenState>,
    Path(label): Path<String>,
) -> Response {
    let warden = state.warden();
    match blocking(move || warden.repair(&label)).await {
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

    use crate::testing::{Harness, TEST_TOKEN, armored};

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
            "cred_read_ciphertext": armored(label),
        })
        .to_string()
    }

    #[tokio::test]
    async fn creates_reads_repairs_and_deletes_a_tenant() {
        let h = Harness::new();

        let (status, body) = call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["port"], 9100);
        assert_eq!(body["pair_code"], "ABCD-1234");
        assert_eq!(body["pair_url"], "https://alice.passband.email");
        assert!(
            body["deep_link"]
                .as_str()
                .unwrap()
                .starts_with("passband://pair?url=")
        );
        // Nothing about the request comes back out.
        assert!(!body.to_string().contains("example.com"));
        assert!(!body.to_string().contains("AGE ENCRYPTED"));

        let (status, body) = call(&h, authed("GET", "/v1/tenants/alice", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "active");
        assert_eq!(body["port"], 9100);

        h.runner.script(
            "squelchd pair",
            crate::host::CmdOutput::success(crate::testing::pair_stdout(
                "WXYZ-9876",
                "https://alice.passband.email",
            )),
        );
        let (status, body) = call(&h, authed("POST", "/v1/tenants/alice/pair", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pair_code"], "WXYZ-9876");
        assert_eq!(body["pair_url"], "https://alice.passband.email");

        let (status, body) = call(&h, authed("DELETE", "/v1/tenants/alice", "")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(body, Value::Null);
    }

    #[tokio::test]
    async fn duplicate_is_409_and_invalid_is_422() {
        let h = Harness::new();
        let (status, _) = call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, body) = call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "label_exists");

        for label in ["ab", "-alice", "ALICE!", "mcp", &"a".repeat(31)] {
            let (status, body) = call(&h, authed("POST", "/v1/tenants", &create_body(label))).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{label}");
            assert_eq!(body["error"], "invalid_label", "{label}");
            // The constraint is named; the value is not echoed.
            assert!(body["detail"].is_string());
        }

        let plaintext = serde_json::json!({
            "label": "bob",
            "account_email": "bob@example.com",
            "cred_read_ciphertext": "{\"refresh_token\":\"1//0gREAL\"}",
        })
        .to_string();
        let (status, body) = call(&h, authed("POST", "/v1/tenants", &plaintext)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "invalid_cred_read_ciphertext");
    }

    #[tokio::test]
    async fn unknown_tenant_is_404_on_get_and_204_on_delete() {
        let h = Harness::new();
        let (status, body) = call(&h, authed("GET", "/v1/tenants/nobody", "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");

        let (status, _) = call(&h, authed("POST", "/v1/tenants/nobody/pair", "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // DELETE is idempotent: the control plane retries it on its own unwind
        // paths and must not have to special-case a 404 there.
        let (status, _) = call(&h, authed("DELETE", "/v1/tenants/nobody", "")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_broken_box_is_a_500_with_a_machine_reason_and_no_detail() {
        let h = Harness::new();
        h.runner.fail_on("systemctl enable");

        let (status, body) = call(&h, authed("POST", "/v1/tenants", &create_body("alice"))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], "unit_start_failed");
        assert!(body.get("detail").is_none());
        // No path from this box on the wire.
        assert!(!body.to_string().contains("/var/lib"));
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
    }

    /// Every gated route, every way of getting the bearer wrong. The answer is
    /// always the same bare 401.
    #[tokio::test]
    async fn every_route_is_gated() {
        let h = Harness::new();
        let routes = [
            ("POST", "/v1/tenants"),
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
            // Wrong, and a prefix of the real one: the compare is constant
            // time and length-checked, so neither is closer than the other.
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
        // Nothing got past the gate: no command ran and no file was written.
        assert!(h.runner.calls().is_empty());
        assert!(h.fs.paths().is_empty());
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
        let huge = serde_json::json!({
            "label": "alice",
            "account_email": "alice@example.com",
            "cred_read_ciphertext": "A".repeat(super::MAX_BODY + 1),
        })
        .to_string();
        let (status, _) = call(&h, authed("POST", "/v1/tenants", &huge)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(h.fs.paths().is_empty());
    }
}
