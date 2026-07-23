//! Shared LLM provider plumbing for BOTH triage stages.
//!
//! Stage-1 (small model, runs on every non-sealed, non-rule-decided email) and
//! Stage-2 (more capable escalation model) both talk to the SAME two providers
//! (Anthropic Messages API + OpenAI Chat Completions) with the SAME retry
//! discipline, the SAME structured-output request shape, and the SAME
//! injection-fenced prompt structure. This module owns that provider call so
//! neither stage forks the transport.
//!
//! What lives here: the wire request/response types, the retry policy, the
//! truncation-retry, the refusal/permanent-failure classification, and the
//! [`classify_llm`] entry point that returns the model's raw JSON verdict text
//! plus token usage. What lives in each stage: the (static, cache-friendly)
//! system prompt, the fenced user message, the output schema, and the parse +
//! apply of that stage's own verdict struct.
//!
//! REDACTION: this module NEVER logs email bodies, subjects, the API key, or raw
//! request/response bodies. Callers log counts, the model id, redacted error
//! types, and token-usage numbers (which are fine to log).

use crate::config::Stage2Provider;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Anthropic Messages API endpoint.
pub const API_URL: &str = "https://api.anthropic.com/v1/messages";
/// OpenAI Chat Completions API endpoint.
pub const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";
/// Pinned Anthropic API version header value.
const API_VERSION: &str = "2023-06-01";
/// Default max_tokens; a compact JSON object fits comfortably. A `max_tokens`
/// truncation is retried once at this doubled value.
pub const MAX_TOKENS: u32 = 400;
pub const MAX_TOKENS_RETRY: u32 = 800;
/// Retry policy for retryable statuses (429 / 5xx / 529).
const MAX_TRIES: u32 = 3;
pub const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// The URL for a provider's production endpoint.
pub fn provider_url(provider: Stage2Provider) -> &'static str {
    match provider {
        Stage2Provider::Anthropic => API_URL,
        Stage2Provider::OpenAI => OPENAI_API_URL,
    }
}

/// A single LLM classification request, shared by both stages. `system` is the
/// stage's STATIC system prompt (identical bytes every call so haiku prompt
/// caching can apply); `user` is the stage's fenced TRUSTED/UNTRUSTED message;
/// `schema` is the stage's structured-output JSON schema. `model` is written
/// verbatim into the request.
pub struct LlmRequest<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub schema: serde_json::Value,
}

/// The outcome of a single [`classify_llm`] call.
#[derive(Debug)]
pub enum LlmOutcome {
    /// The model's raw JSON verdict text (schema-valid per the provider) + usage.
    /// The caller parses this into its own stage verdict struct.
    Json(String, Option<Usage>),
    /// The model declined (`stop_reason == "refusal"` / OpenAI `refusal` field).
    Refused,
    /// A permanent (non-retryable, e.g. 400/401/truncation/parse) failure.
    /// Carries a redacted error type only.
    Failed(String),
}

/// A redacted classification error (transport / retry-exhaustion). Never carries
/// a body or key.
#[derive(Debug)]
pub struct ClassifyError {
    /// A short, redacted description (status code / error type only).
    pub kind: String,
    /// Whether this was a retryable class that exhausted its budget (vs. a hard
    /// transport error). Informational for logging.
    pub retryable: bool,
}

impl std::fmt::Display for ClassifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "classify error: {} (retryable={})",
            self.kind, self.retryable
        )
    }
}

/// Classify one email against the configured provider, returning the model's raw
/// JSON verdict text (the caller parses + validates it). Both providers consume
/// the IDENTICAL system prompt, fenced user message, and output schema.
///
/// Retry policy (shared): 429 (honor `retry-after`) and 529/5xx are retryable
/// with exponential backoff (cap 60s, max 3 tries); 400/401 are permanent. A
/// truncation is retried once at a higher token budget; a refusal yields
/// [`LlmOutcome::Refused`].
pub async fn classify_llm(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    provider: Stage2Provider,
    req: &LlmRequest<'_>,
) -> std::result::Result<LlmOutcome, ClassifyError> {
    match provider {
        Stage2Provider::Anthropic => classify_anthropic(http, url, api_key, req).await,
        Stage2Provider::OpenAI => classify_openai(http, url, api_key, req).await,
    }
}

// ===========================================================================
// Anthropic Messages API wire types.
// ===========================================================================

#[derive(Debug, Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: Vec<SystemBlock<'a>>,
    messages: Vec<RequestMessage<'a>>,
    output_config: OutputConfig,
}

#[derive(Debug, Serialize)]
struct SystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct RequestMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct OutputConfig {
    format: OutputFormat,
}

#[derive(Debug, Serialize)]
struct OutputFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    schema: serde_json::Value,
}

// ---- OpenAI Chat Completions wire types -----------------------------------

#[derive(Debug, Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage<'a>>,
    max_completion_tokens: u32,
    response_format: OpenAiResponseFormat,
}

#[derive(Debug, Serialize)]
struct OpenAiMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct OpenAiResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    json_schema: OpenAiJsonSchema,
}

#[derive(Debug, Serialize)]
struct OpenAiJsonSchema {
    name: &'static str,
    strict: bool,
    schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    message: Option<OpenAiChoiceMessage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

impl OpenAiUsage {
    fn into_usage(self) -> Usage {
        Usage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
        }
    }
}

/// The subset of the Anthropic response we consume.
#[derive(Debug, Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

/// Token usage — numbers are fine to log.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

// ===========================================================================
// Provider paths.
// ===========================================================================

async fn classify_anthropic(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    req: &LlmRequest<'_>,
) -> std::result::Result<LlmOutcome, ClassifyError> {
    let mut max_tokens = MAX_TOKENS;
    let mut allow_token_retry = true;

    loop {
        let body = MessagesRequest {
            model: req.model,
            max_tokens,
            system: vec![SystemBlock {
                kind: "text",
                text: req.system,
                cache_control: Some(CacheControl { kind: "ephemeral" }),
            }],
            messages: vec![RequestMessage {
                role: "user",
                content: req.user,
            }],
            output_config: OutputConfig {
                format: OutputFormat {
                    kind: "json_schema",
                    schema: req.schema.clone(),
                },
            },
        };

        let build = || {
            http.post(url)
                .header("x-api-key", api_key)
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json")
                .json(&body)
        };
        let parsed: MessagesResponse = match send_with_retry(build).await? {
            SendOk::Body(b) => b,
            SendOk::PermanentFailure(kind) => return Ok(LlmOutcome::Failed(kind)),
        };

        match parsed.stop_reason.as_deref() {
            Some("refusal") => return Ok(LlmOutcome::Refused),
            Some("max_tokens") if allow_token_retry => {
                max_tokens = MAX_TOKENS_RETRY;
                allow_token_retry = false;
                continue;
            }
            Some("max_tokens") => {
                return Ok(LlmOutcome::Failed("max_tokens_truncation".into()));
            }
            _ => {}
        }

        let text = parsed
            .content
            .iter()
            .find(|b| b.kind == "text")
            .and_then(|b| b.text.as_deref());
        let text = match text {
            Some(t) => t.to_string(),
            None => return Ok(LlmOutcome::Failed("no_text_block".into())),
        };
        return Ok(LlmOutcome::Json(text, parsed.usage));
    }
}

async fn classify_openai(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    req: &LlmRequest<'_>,
) -> std::result::Result<LlmOutcome, ClassifyError> {
    let mut max_completion_tokens = MAX_TOKENS;
    let mut allow_token_retry = true;

    loop {
        let body = OpenAiRequest {
            model: req.model,
            messages: vec![
                OpenAiMessage {
                    role: "system",
                    content: req.system,
                },
                OpenAiMessage {
                    role: "user",
                    content: req.user,
                },
            ],
            max_completion_tokens,
            response_format: OpenAiResponseFormat {
                kind: "json_schema",
                json_schema: OpenAiJsonSchema {
                    name: "triage",
                    strict: true,
                    schema: req.schema.clone(),
                },
            },
        };

        let build = || {
            http.post(url)
                .bearer_auth(api_key)
                .header("content-type", "application/json")
                .json(&body)
        };
        let parsed: OpenAiResponse = match send_with_retry(build).await? {
            SendOk::Body(b) => b,
            SendOk::PermanentFailure(kind) => return Ok(LlmOutcome::Failed(kind)),
        };

        let choice = match parsed.choices.into_iter().next() {
            Some(c) => c,
            None => return Ok(LlmOutcome::Failed("no_choice".into())),
        };
        let message = match choice.message {
            Some(m) => m,
            None => return Ok(LlmOutcome::Failed("no_message".into())),
        };

        if message.refusal.is_some() {
            return Ok(LlmOutcome::Refused);
        }

        match choice.finish_reason.as_deref() {
            Some("length") if allow_token_retry => {
                max_completion_tokens = MAX_TOKENS_RETRY;
                allow_token_retry = false;
                continue;
            }
            Some("length") => {
                return Ok(LlmOutcome::Failed("max_tokens_truncation".into()));
            }
            _ => {}
        }

        let text = match message.content {
            Some(t) => t,
            None => return Ok(LlmOutcome::Failed("no_text_block".into())),
        };
        return Ok(LlmOutcome::Json(
            text,
            parsed.usage.map(OpenAiUsage::into_usage),
        ));
    }
}

/// Internal: the two success shapes from [`send_with_retry`].
enum SendOk<T> {
    Body(T),
    /// A permanent (400/401) failure with a redacted kind string.
    PermanentFailure(String),
}

/// Provider error-body shape. Both Anthropic and OpenAI use
/// `{"error": {"type": ...}}`; only the redacted `type` is read.
#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    error: Option<ApiErrorInner>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorInner {
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// POST the request, applying the SHARED retry policy for retryable statuses.
async fn send_with_retry<T, F>(build: F) -> std::result::Result<SendOk<T>, ClassifyError>
where
    T: serde::de::DeserializeOwned,
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let send = build().send().await;

        let resp = match send {
            Ok(r) => r,
            Err(_) => {
                if attempt >= MAX_TRIES {
                    return Err(ClassifyError {
                        kind: "transport".into(),
                        retryable: true,
                    });
                }
                sleep_backoff(attempt, None).await;
                continue;
            }
        };

        let status = resp.status();
        if status.is_success() {
            let parsed: T = resp.json().await.map_err(|_| ClassifyError {
                kind: "response_decode".into(),
                retryable: false,
            })?;
            return Ok(SendOk::Body(parsed));
        }

        let code = status.as_u16();
        let retryable = code == 429 || code == 529 || (500..600).contains(&code);
        if retryable {
            if attempt >= MAX_TRIES {
                return Err(ClassifyError {
                    kind: format!("http_{code}"),
                    retryable: true,
                });
            }
            let retry_after = parse_retry_after(&resp);
            sleep_backoff(attempt, retry_after).await;
            continue;
        }

        let kind = resp
            .json::<ApiErrorBody>()
            .await
            .ok()
            .and_then(|b| b.error)
            .and_then(|e| e.kind)
            .unwrap_or_else(|| "unknown".to_string());
        return Ok(SendOk::PermanentFailure(format!("http_{code}:{kind}")));
    }
}

/// Parse a `retry-after` header (seconds) if present.
fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Exponential backoff with a 60s cap; honors a server `retry-after` when given.
async fn sleep_backoff(attempt: u32, retry_after: Option<Duration>) {
    let base = retry_after.unwrap_or_else(|| {
        let secs = 1u64 << (attempt.min(6) - 1); // 1,2,4,8,16,32...
        Duration::from_secs(secs)
    });
    tokio::time::sleep(base.min(BACKOFF_CAP)).await;
}
