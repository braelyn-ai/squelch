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

/// Qualify the request's `model` with its provider, returning `None` when the
/// body needs no change and the ORIGINAL BYTES should go out untouched.
///
/// This is the one place the relay looks inside the body, and it is the same
/// argument that pins `anthropic-version` above: the daemon, not the app,
/// decides the dialect the gateway sees. The app cannot make this call itself
/// because one setting drives both transports — through this relay a bare id
/// is a 400 "could not auto resolve a provider", and through BYOK straight to
/// Anthropic the qualified id is the one that is invalid. Only the daemon knows
/// which endpoint is downstream, and it already gates the virtual-key header on
/// exactly that question.
///
/// EVERYTHING ELSE IS LEFT ALONE. An unparseable body, a missing or non-string
/// `model`, or an id that already names a provider all return `None` so the
/// original bytes are forwarded verbatim and the gateway's own error reaches
/// the app unedited. Re-serialization happens only when there is a change to
/// make, so the common case once apps send qualified ids costs a parse and no
/// copy. The conversation itself is never read, logged, or inspected here —
/// only the one top-level field.
fn qualify_model_for_gateway(body: &Bytes) -> Option<Bytes> {
    let mut parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let model = parsed.get("model")?.as_str()?;
    let qualified = squelch_core::triage::llm::qualify_gateway_model(model)?;
    *parsed.get_mut("model")? = serde_json::Value::String(qualified);
    Some(Bytes::from(serde_json::to_vec(&parsed).ok()?))
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
    // ...and, for the same reason and behind the same gate, the model id is
    // qualified with its provider. See [`qualify_model_for_gateway`].
    let body = if squelch_core::triage::llm::is_gateway_url(&relay.url) {
        req = req.header("x-bf-vk", relay.api_key.as_str());
        qualify_model_for_gateway(&body).unwrap_or(body)
    } else {
        body
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(body: &str) -> Option<String> {
        qualify_model_for_gateway(&Bytes::from(body.to_string()))
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
    }

    /// The bug this exists for: the app sends a bare id, the gateway cannot
    /// resolve a provider from it, and the turn 400s before the virtual key is
    /// ever consulted.
    #[test]
    fn a_bare_model_gets_its_provider() {
        let out = rewrite(r#"{"model":"claude-opus-5","max_tokens":16}"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["model"], "anthropic/claude-opus-5");
        // Everything else survives the round trip untouched.
        assert_eq!(v["max_tokens"], 16);
    }

    /// An id that already names a provider is left alone AND the original
    /// bytes are forwarded: `None` means "do not re-serialize".
    #[test]
    fn an_already_qualified_model_is_not_touched() {
        assert!(rewrite(r#"{"model":"anthropic/claude-opus-5"}"#).is_none());
        // Another provider's spelling is the caller's choice, not ours to fix.
        assert!(rewrite(r#"{"model":"openai/gpt-5"}"#).is_none());
    }

    /// Nothing about a malformed or surprising body becomes this function's
    /// problem: it declines, the original bytes go out, and the gateway's own
    /// error reaches the app unedited.
    #[test]
    fn anything_unexpected_is_forwarded_verbatim() {
        assert!(rewrite("not json at all").is_none());
        assert!(rewrite(r#"{"messages":[]}"#).is_none(), "no model field");
        assert!(
            rewrite(r#"{"model":123}"#).is_none(),
            "model is not a string"
        );
        assert!(rewrite(r#"{"model":""}"#).is_none(), "empty model");
        assert!(rewrite("[1,2,3]").is_none(), "not an object");
    }

    /// The conversation rides through unchanged. This is the property that
    /// makes looking inside the body acceptable at all: one top-level field is
    /// read, and the user's own words are neither inspected nor reordered.
    #[test]
    fn the_conversation_survives_the_rewrite() {
        let body = r#"{"model":"claude-haiku-4-5","stream":true,"messages":[{"role":"user","content":"what did Ada send me?"}],"system":[{"type":"text","text":"be brief"}]}"#;
        let out = rewrite(body).unwrap();
        let before: serde_json::Value = serde_json::from_str(body).unwrap();
        let after: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(after["model"], "anthropic/claude-haiku-4-5");
        for k in ["stream", "messages", "system"] {
            assert_eq!(before[k], after[k], "{k} changed");
        }
    }
}
