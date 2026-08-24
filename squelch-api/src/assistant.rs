//! The hosted assistant relay: the Passband app posts its Anthropic-shaped
//! streaming request here, and the daemon forwards it to the gateway with a
//! DAEMON-HELD credential, streaming the SSE bytes back verbatim. The app never
//! sees a key — self-host BYOK lives in the app, and this route simply does not
//! exist (404) without a gateway to relay to.
//!
//! HUMAN DOOR ONLY, like every route that spends tenant money. The body is the
//! user's own conversation and is treated like mail content: never logged in
//! either direction, never inspected here.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use squelch_core::config::ResolvedAssistant;
use tokio_stream::StreamExt;

use crate::handlers::audit_action;
use crate::state::ApiState;

/// Slots for in-flight assistant streams: a burst of streams must not pile
/// unbounded connections onto the gateway. Bounded by waiting, like
/// [`crate::auth::DEVICE_AUTH_CONCURRENCY`], and each permit is held for the
/// WHOLE stream, not just the handler's own await.
const ASSISTANT_CONCURRENCY: usize = 4;

/// The Anthropic wire version pinned on every relayed request, so the daemon —
/// not whatever the app happens to send — decides the dialect the gateway sees.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Connect budget for reaching the gateway. There is deliberately NO total
/// timeout on the client: a streamed completion legitimately runs for minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Budget from "request sent" to "headers back". Separate from
/// [`CONNECT_TIMEOUT`], which only covers the dial: a gateway that accepts the
/// connection and then stalls before answering would otherwise park this
/// request forever, and the permit it holds is one of
/// [`ASSISTANT_CONCURRENCY`] — four wedged requests and the relay is dead for
/// everyone. 30s is generous for headers (the first token can lag, headers do
/// not) and short enough that a wedged slot frees itself.
const HEADERS_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on the SILENCE between streamed chunks, not on the stream: a
/// completion may run for many minutes as long as bytes keep arriving. This
/// guards the same permit as [`HEADERS_TIMEOUT`], for a gateway that wedges
/// MID-stream. 180s sits comfortably above Bifrost's own 120s stream idle
/// timeout, so in the normal case the gateway cancels first and this fires
/// only when the gateway itself is the thing that hung.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// The gateway credential + endpoint plus the client that posts to it. No
/// `Debug` on purpose: `api_key` is key material and must never reach a log
/// line (see [`ResolvedAssistant`]).
pub struct AssistantRelay {
    http: reqwest::Client,
    url: String,
    api_key: String,
    /// See [`ASSISTANT_CONCURRENCY`].
    slots: Arc<tokio::sync::Semaphore>,
}

impl AssistantRelay {
    pub fn new(resolved: ResolvedAssistant) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // Redirects refused: every request carries the assistant credential,
            // and a redirect is how it ends up at a host nobody chose.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client build");
        Self {
            http,
            url: resolved.url,
            api_key: resolved.api_key,
            slots: Arc::new(tokio::sync::Semaphore::new(ASSISTANT_CONCURRENCY)),
        }
    }
}

/// POST /client/assistant/messages — forward the raw body to the gateway and
/// stream the answer back byte-for-byte. No re-framing: the app's SSE parser
/// depends on exact framing, so the response body is the upstream body. A
/// non-2xx upstream (JSON error) flows through the same path with its status
/// and content-type mirrored.
pub(crate) async fn assistant_messages(State(state): State<ApiState>, body: Bytes) -> Response {
    let Some(relay) = state.assistant().cloned() else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "assistant_relay_unavailable" })),
        )
            .into_response();
    };

    // Take a stream slot BEFORE dialing upstream; the permit rides inside the
    // response stream below so it lives until the last byte, not until this
    // handler returns.
    let permit = relay
        .slots
        .clone()
        .acquire_owned()
        .await
        .expect("assistant semaphore is never closed");

    // ONLY these headers cross: the daemon's credential and the wire pins. The
    // client's Authorization (its device token) and anything else it sent stop
    // here.
    let mut req = relay
        .http
        .post(&relay.url)
        .header("x-api-key", relay.api_key.as_str())
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "text/event-stream");
    // The gateway reads its virtual key ONLY from `x-bf-vk` and ignores
    // `x-api-key` outright, so without this every assistant turn is a 401 —
    // the same bug that silently took hosted TRIAGE down (see
    // `squelch_core::triage::llm::GATEWAY_VK_HEADER`). This relay is
    // gateway-only by construction (`resolve_assistant` returns None without a
    // base URL), but the condition is kept so ONE rule decides who gets a
    // virtual key, here and on the triage wire.
    if squelch_core::triage::llm::is_gateway_url(&relay.url) {
        req = req.header("x-bf-vk", relay.api_key.as_str());
    }
    let upstream = tokio::time::timeout(HEADERS_TIMEOUT, req.body(body).send()).await;

    let upstream = match upstream {
        Ok(Ok(resp)) => resp,
        // A transport error and a stall before headers get the same answer:
        // either way the gateway did not respond. The error itself is never
        // surfaced or logged: a reqwest error can embed the URL, and the 502
        // body says everything the app can act on.
        Ok(Err(_)) | Err(_) => {
            audit_action(&state, "assistant_relay", None, "failed:transport").await;
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(json!({ "error": "assistant_relay_unreachable" })),
            )
                .into_response();
        }
    };

    // Spend honesty: one audit row per request that reached the gateway, with
    // the upstream status and NO content detail in either direction.
    let detail = format!("status:{}", upstream.status().as_u16());
    audit_action(&state, "assistant_relay", None, &detail).await;

    let mut builder = Response::builder().status(upstream.status());
    // The header whitelist, deliberately exactly two: content-type so the app
    // can tell SSE from a JSON error, retry-after so an upstream 429/529
    // backoff hint survives the relay. Everything else the gateway says about
    // itself stops here.
    for name in [header::CONTENT_TYPE, header::RETRY_AFTER] {
        if let Some(value) = upstream.headers().get(&name) {
            builder = builder.header(name, value.clone());
        }
    }
    // The idle guard on the body, chunk by chunk: an elapsed gap becomes an
    // io::Error, which terminates the axum Body, closes the client's stream,
    // and — the part that matters for the fleet — drops the permit. The
    // upstream's own mid-stream error is flattened to a static message for
    // the same reason as above: a reqwest error can embed the URL.
    let stream = upstream
        .bytes_stream()
        .timeout(STREAM_IDLE_TIMEOUT)
        .map(move |chunk| {
            // Capture the permit so the slot is held for the stream's lifetime.
            let _ = &permit;
            match chunk {
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(_)) => Err(std::io::Error::other("assistant upstream stream failed")),
                Err(_elapsed) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "assistant upstream stream stalled",
                )),
            }
        });
    builder
        .body(Body::from_stream(stream))
        .expect("status and whitelisted headers mirrored from a parsed response are valid")
}
