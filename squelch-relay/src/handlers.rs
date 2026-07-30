//! `GET /healthz` and `POST /v1/push`.
//!
//! Each token's APNs status passes through VERBATIM — `410 Unregistered`
//! especially, since the daemon owns dead-token cleanup. A malformed request
//! 4xxs the whole batch; a failed token is data (`status: 0`), never a batch
//! failure. PRIVACY: no token value, payload, bearer, or APNs JWT is logged.

use std::time::{Duration, Instant};

use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Environment;
use crate::state::RelayState;

/// An unbounded token list would let one call hold a connection open forever.
const MAX_TOKENS: usize = 100;
/// APNs tokens are 32 bytes (64 hex) today; the bounds are deliberately loose so
/// a token-format change does not require a relay release.
const TOKEN_MIN_LEN: usize = 16;
const TOKEN_MAX_LEN: usize = 200;
/// APNs caps `apns-collapse-id` at 64 bytes and 400s anything longer.
const MAX_COLLAPSE_ID: usize = 64;
/// Unbounded, an opaque id would turn one request into a 100x fan-out of
/// megabyte bodies.
const MAX_EVENT_ID: usize = 256;
/// APNs' own payload ceiling for an alert push; the backstop that cannot drift
/// as fields are added to the payload.
const MAX_PAYLOAD: usize = 4096;
/// Concurrent APNs requests per batch.
const FANOUT: usize = 8;
/// Wall-clock budget for the whole fan-out, so an APNs host that blackholes
/// rather than resets cannot pin a connection for ~130s. Unfinished tokens
/// report `status: 0` / `timeout`.
const BATCH_BUDGET: Duration = Duration::from_secs(30);

/// The generic fallback alert, shown when the Notification Service Extension
/// cannot reach the user's daemon to fetch real content. It must say nothing.
const ALERT_TITLE: &str = "squelch";
const ALERT_BODY: &str = "New mail surfaced";

/// A handler error carrying an HTTP status and a client-safe message.
#[derive(Debug)]
pub struct RelayError {
    status: StatusCode,
    message: String,
}

impl RelayError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

/// An opaque event id, forwarded into the APNs payload VERBATIM — the relay
/// never interprets it. Anything non-scalar is a 400.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum EventId {
    Int(i64),
    Str(String),
}

#[derive(Debug, Deserialize)]
pub struct PushRequest {
    pub device_tokens: Vec<String>,
    pub event_id: EventId,
    #[serde(default)]
    pub collapse_id: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
}

/// One token's outcome. `status` is the APNs HTTP status verbatim, or 0 when
/// there was no HTTP response at all.
#[derive(Debug, Serialize)]
pub struct PushResult {
    pub token: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apns_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PushResponse {
    pub results: Vec<PushResult>,
}

pub async fn healthz() -> &'static str {
    "ok"
}

/// A request that has passed validation.
struct Validated {
    tokens: Vec<String>,
    payload: Vec<u8>,
    topic: String,
    collapse_id: Option<String>,
    base_url: String,
    environment: Environment,
}

pub async fn push(
    State(state): State<RelayState>,
    body: Bytes,
) -> Result<Json<PushResponse>, RelayError> {
    let started = Instant::now();

    // Parsed by hand so every malformed body gets the same 400 + `{"error": …}`
    // shape as a failed bound check. serde's Display embeds the offending value
    // verbatim, which would reflect device tokens into a body an upstream proxy
    // might log — only the error class and position go back.
    let req: PushRequest = serde_json::from_slice(&body).map_err(|e| {
        RelayError::bad_request(format!(
            "invalid request body: {:?} error at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;
    let v = validate(&state, req)?;

    let jwt = state.signer().token().map_err(|_| {
        // The error detail could name the key file; keep it out of the log and
        // the response.
        tracing::error!("failed to sign APNs provider token");
        RelayError::new(StatusCode::INTERNAL_SERVER_ERROR, "apns token unavailable")
    })?;

    let total = v.tokens.len();
    // `buffered`, not `buffer_unordered`: results come back in request order, so
    // the response array lines up with `device_tokens` index for index.
    let mut results: Vec<PushResult> = Vec::with_capacity(total);
    let fanout = async {
        let mut stream = stream::iter(v.tokens.iter().cloned())
            .map(|token| {
                let state = &state;
                let v = &v;
                let jwt = jwt.clone();
                async move { send_one(state, v, &jwt, token).await }
            })
            .buffered(FANOUT);
        while let Some(r) = stream.next().await {
            results.push(r);
        }
    };
    // Results arrive in request order, so everything past `results.len()` at
    // expiry is exactly the set that never finished.
    let timed_out = tokio::time::timeout(BATCH_BUDGET, fanout).await.is_err();
    if timed_out {
        for token in v.tokens.iter().skip(results.len()) {
            results.push(PushResult {
                token: token.clone(),
                status: 0,
                apns_id: None,
                reason: Some("timeout".to_string()),
            });
        }
    }

    // PRIVACY: counts, statuses, timing. Never token values or the payload.
    tracing::info!(
        tokens = total,
        statuses = %summarize(&results),
        topic = %v.topic,
        environment = v.environment.as_str(),
        collapsed = v.collapse_id.is_some(),
        timed_out,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "push"
    );

    Ok(Json(PushResponse { results }))
}

/// Bounds-check the request and build everything the fan-out needs.
fn validate(state: &RelayState, req: PushRequest) -> Result<Validated, RelayError> {
    let cfg = state.config();

    if req.device_tokens.is_empty() {
        return Err(RelayError::bad_request("device_tokens must not be empty"));
    }
    if req.device_tokens.len() > MAX_TOKENS {
        return Err(RelayError::bad_request(format!(
            "device_tokens exceeds the {MAX_TOKENS}-token limit"
        )));
    }
    for t in &req.device_tokens {
        // Hex-only is what keeps a token from reshaping the APNs URL: no
        // slashes, dots, query, or percent-escapes survive it.
        if t.len() < TOKEN_MIN_LEN || t.len() > TOKEN_MAX_LEN {
            return Err(RelayError::bad_request(format!(
                "device token length must be {TOKEN_MIN_LEN}..={TOKEN_MAX_LEN} hex characters"
            )));
        }
        if !t.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RelayError::bad_request("device token must be hex"));
        }
    }

    let collapse_id = match req.collapse_id {
        None => None,
        Some(c) => {
            if c.is_empty() {
                return Err(RelayError::bad_request("collapse_id must not be empty"));
            }
            if c.len() > MAX_COLLAPSE_ID {
                return Err(RelayError::bad_request(format!(
                    "collapse_id exceeds {MAX_COLLAPSE_ID} bytes"
                )));
            }
            // It becomes a header value; non-graphic bytes would fail header
            // construction later — a 500 for what is really a bad request.
            if !c.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(RelayError::bad_request(
                    "collapse_id must be printable ASCII with no spaces",
                ));
            }
            Some(c)
        }
    };

    // An unbounded id is the one way to buy a 100x amplifier with a single
    // rate-limited request.
    if let EventId::Str(s) = &req.event_id
        && s.len() > MAX_EVENT_ID
    {
        return Err(RelayError::bad_request(format!(
            "event_id exceeds {MAX_EVENT_ID} bytes"
        )));
    }

    let topic = cfg
        .resolve_topic(req.topic.as_deref())
        .ok_or_else(|| RelayError::bad_request("topic is not in this relay's allowlist"))?
        .to_string();

    let environment = match req.environment.as_deref() {
        None => cfg.apns_env,
        Some(e) => Environment::parse(e).ok_or_else(|| {
            RelayError::bad_request("environment must be `production` or `sandbox`")
        })?,
    };
    let base_url = match cfg.apns_url_override.as_deref() {
        Some(u) => u.to_string(),
        None => environment.base_url().to_string(),
    };

    // Content-free by design: an opaque id plus a generic alert.
    let payload = serde_json::to_vec(&json!({
        "aps": {
            "alert": { "title": ALERT_TITLE, "body": ALERT_BODY },
            "mutable-content": 1,
            "sound": "default",
        },
        "event_id": req.event_id,
    }))
    .map_err(|_| RelayError::new(StatusCode::INTERNAL_SERVER_ERROR, "payload encode failed"))?;

    // Backstop: whatever the fields grow into, a body Apple would reject never
    // gets multiplied by the token count.
    if payload.len() > MAX_PAYLOAD {
        return Err(RelayError::bad_request(format!(
            "APNs payload exceeds {MAX_PAYLOAD} bytes"
        )));
    }

    Ok(Validated {
        tokens: req.device_tokens,
        payload,
        topic,
        collapse_id,
        base_url,
        environment,
    })
}

/// Push one token. Never returns an error: a transport failure is reported as
/// `status: 0` so the rest of the batch still gets through.
async fn send_one(state: &RelayState, v: &Validated, jwt: &str, token: String) -> PushResult {
    // `FANOUT` bounds concurrency within one batch; this bounds it across all
    // in-flight batches. Waiting counts against the batch budget, so load sheds
    // as `timeout` rather than queueing without limit.
    let _permit = state.apns_slot().await;

    let url = format!("{}/3/device/{}", v.base_url, token);
    let mut req = state
        .http()
        .post(&url)
        .header(reqwest::header::AUTHORIZATION, format!("bearer {jwt}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("apns-topic", &v.topic)
        // A visible alert, not `content-available`: background pushes get
        // throttled, while a mutable alert runs the NSE reliably.
        .header("apns-push-type", "alert")
        .header("apns-priority", "10");
    if let Some(c) = &v.collapse_id {
        req = req.header("apns-collapse-id", c);
    }

    let resp = match req.body(v.payload.clone()).send().await {
        Ok(r) => r,
        Err(_) => {
            // The error carries the URL, which carries the device token.
            return PushResult {
                token,
                status: 0,
                apns_id: None,
                reason: Some("unreachable".to_string()),
            };
        }
    };

    let status = resp.status().as_u16();
    let apns_id = resp
        .headers()
        .get("apns-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    // APNs returns a body only on error, shaped `{"reason": "...", ...}`.
    let reason = match resp.bytes().await {
        Ok(b) if !b.is_empty() => serde_json::from_slice::<serde_json::Value>(&b)
            .ok()
            .and_then(|j| j.get("reason").and_then(|r| r.as_str()).map(String::from)),
        _ => None,
    };

    PushResult {
        token,
        status,
        apns_id,
        reason,
    }
}

/// Compact `status x count` summary for the log line, e.g. `200x3,410x1`.
fn summarize(results: &[PushResult]) -> String {
    let mut counts: Vec<(u16, usize)> = Vec::new();
    for r in results {
        match counts.iter_mut().find(|(s, _)| *s == r.status) {
            Some((_, n)) => *n += 1,
            None => counts.push((r.status, 1)),
        }
    }
    counts.sort_unstable();
    counts
        .iter()
        .map(|(s, n)| format!("{s}x{n}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn state() -> RelayState {
        let config = Config {
            bind: crate::config::DEFAULT_BIND.parse().unwrap(),
            apns_key_pem: include_str!("../tests/fixture_test_key.p8").to_string(),
            apns_key_id: "KEYID12345".into(),
            apns_team_id: "TEAMID6789".into(),
            apns_topics: vec!["dev.squelch.ios".into(), "dev.squelch.ios.beta".into()],
            apns_env: Environment::Production,
            auth_token: None,
            apns_url_override: None,
        };
        RelayState::new(config).unwrap()
    }

    fn parse(body: serde_json::Value) -> Result<Validated, RelayError> {
        let req: PushRequest = serde_json::from_value(body)
            .map_err(|e| RelayError::bad_request(format!("invalid request body: {e}")))?;
        validate(&state(), req)
    }

    fn hex(n: usize) -> String {
        "ab".repeat(n / 2)
    }

    fn base(tokens: serde_json::Value) -> serde_json::Value {
        json!({ "device_tokens": tokens, "event_id": 42 })
    }

    #[test]
    fn accepts_a_minimal_request() {
        let v = parse(base(json!([hex(64)]))).unwrap();
        assert_eq!(v.tokens.len(), 1);
        assert_eq!(v.topic, "dev.squelch.ios");
        assert_eq!(v.environment, Environment::Production);
        assert!(v.collapse_id.is_none());
        assert_eq!(v.base_url, "https://api.push.apple.com");
    }

    #[test]
    fn payload_is_content_free_and_forwards_the_event_id() {
        let v = parse(base(json!([hex(64)]))).unwrap();
        let p: serde_json::Value = serde_json::from_slice(&v.payload).unwrap();
        assert_eq!(p["aps"]["alert"]["title"], "squelch");
        assert_eq!(p["aps"]["alert"]["body"], "New mail surfaced");
        assert_eq!(p["aps"]["mutable-content"], 1);
        assert_eq!(p["aps"]["sound"], "default");
        assert_eq!(p["event_id"], 42);
    }

    #[test]
    fn string_event_ids_survive_verbatim() {
        let v = parse(json!({ "device_tokens": [hex(64)], "event_id": "01J8ZK" })).unwrap();
        let p: serde_json::Value = serde_json::from_slice(&v.payload).unwrap();
        assert_eq!(p["event_id"], "01J8ZK");
    }

    #[test]
    fn rejects_non_scalar_event_ids() {
        for bad in [
            json!(true),
            json!(null),
            json!([1]),
            json!({"id": 1}),
            json!(1.5),
        ] {
            let r = parse(json!({ "device_tokens": [hex(64)], "event_id": bad }));
            assert!(r.is_err(), "event_id {bad} should be rejected");
        }
    }

    /// An unbounded id is a 100x amplifier: the payload is cloned per token.
    #[test]
    fn bounds_the_string_event_id() {
        let ok = "a".repeat(MAX_EVENT_ID);
        let v = parse(json!({ "device_tokens": [hex(64)], "event_id": ok })).unwrap();
        assert!(v.payload.len() <= MAX_PAYLOAD);

        for bad in [MAX_EVENT_ID + 1, 500_000] {
            let r = parse(json!({ "device_tokens": [hex(64)], "event_id": "b".repeat(bad) }));
            assert!(r.is_err(), "a {bad}-byte event_id should be rejected");
        }
    }

    /// Whatever the payload grows to hold, a body APNs would reject never
    /// reaches the fan-out.
    #[test]
    fn the_encoded_payload_stays_under_the_apns_limit() {
        let v = parse(base(json!([hex(64)]))).unwrap();
        assert!(v.payload.len() <= MAX_PAYLOAD);
    }

    #[test]
    fn rejects_a_missing_event_id() {
        assert!(parse(json!({ "device_tokens": [hex(64)] })).is_err());
    }

    #[test]
    fn enforces_token_count_bounds() {
        assert!(parse(base(json!([]))).is_err());
        let many: Vec<String> = (0..MAX_TOKENS).map(|_| hex(64)).collect();
        assert!(parse(base(json!(many))).is_ok());
        let too_many: Vec<String> = (0..MAX_TOKENS + 1).map(|_| hex(64)).collect();
        assert!(parse(base(json!(too_many))).is_err());
    }

    #[test]
    fn enforces_token_length_bounds() {
        assert!(parse(base(json!([hex(TOKEN_MIN_LEN)]))).is_ok());
        assert!(parse(base(json!([hex(TOKEN_MIN_LEN - 2)]))).is_err());
        assert!(parse(base(json!([hex(TOKEN_MAX_LEN)]))).is_ok());
        assert!(parse(base(json!([hex(TOKEN_MAX_LEN + 2)]))).is_err());
        assert!(parse(base(json!([""]))).is_err());
    }

    #[test]
    fn rejects_non_hex_tokens() {
        assert!(parse(base(json!([hex(62) + "zz"]))).is_err());
        // A path traversal attempt cannot reshape the APNs URL.
        assert!(parse(base(json!([hex(60) + "/../x"]))).is_err());
        assert!(parse(base(json!([hex(60) + "?a=b"]))).is_err());
    }

    #[test]
    fn rejects_a_bad_token_anywhere_in_the_batch() {
        assert!(parse(base(json!([hex(64), "nope"]))).is_err());
    }

    #[test]
    fn enforces_the_topic_allowlist() {
        let v = parse(json!({
            "device_tokens": [hex(64)], "event_id": 1, "topic": "dev.squelch.ios.beta"
        }))
        .unwrap();
        assert_eq!(v.topic, "dev.squelch.ios.beta");

        assert!(
            parse(json!({
                "device_tokens": [hex(64)], "event_id": 1, "topic": "com.evil.app"
            }))
            .is_err()
        );
    }

    #[test]
    fn enforces_collapse_id_bounds() {
        let ok = "c".repeat(MAX_COLLAPSE_ID);
        let v = parse(json!({
            "device_tokens": [hex(64)], "event_id": 1, "collapse_id": ok
        }))
        .unwrap();
        assert_eq!(v.collapse_id.as_deref(), Some(ok.as_str()));

        for bad in ["", &"c".repeat(MAX_COLLAPSE_ID + 1), "has space"] {
            assert!(
                parse(json!({
                    "device_tokens": [hex(64)], "event_id": 1, "collapse_id": bad
                }))
                .is_err(),
                "collapse_id {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn environment_selects_the_apns_host() {
        let v = parse(json!({
            "device_tokens": [hex(64)], "event_id": 1, "environment": "sandbox"
        }))
        .unwrap();
        assert_eq!(v.environment, Environment::Sandbox);
        assert_eq!(v.base_url, "https://api.sandbox.push.apple.com");

        assert!(
            parse(json!({
                "device_tokens": [hex(64)], "event_id": 1, "environment": "staging"
            }))
            .is_err()
        );
    }

    #[test]
    fn summarizes_statuses_by_count() {
        let mk = |status| PushResult {
            token: "x".into(),
            status,
            apns_id: None,
            reason: None,
        };
        assert_eq!(summarize(&[mk(200), mk(410), mk(200)]), "200x2,410x1");
        assert_eq!(summarize(&[]), "");
    }
}
