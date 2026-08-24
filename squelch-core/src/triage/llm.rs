//! Shared provider plumbing for both triage stages (Anthropic Messages API +
//! OpenAI Chat Completions): wire types, retry policy, truncation-retry,
//! refusal/permanent-failure classification, [`classify_llm`], and the shared
//! [`LlmOutcome`]/[`classify_into`] verdict shape. Each stage owns only its
//! system prompt, fenced user message, schema, and verdict validation.
//! REDACTION: never log email bodies, subjects, the API key, or raw bodies.

use crate::config::Stage2Provider;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Anthropic Messages API endpoint.
pub const API_URL: &str = "https://api.anthropic.com/v1/messages";
/// OpenAI Chat Completions API endpoint.
pub const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";
/// Pinned Anthropic API version header value.
const API_VERSION: &str = "2023-06-01";
/// The header a Bifrost gateway reads the per-tenant VIRTUAL KEY from.
///
/// Anthropic's own API takes the credential in `x-api-key`, and the gateway
/// does NOT read that header at all: with only `x-api-key` set it answers 401
/// "virtual key is required. Provide a virtual key via the x-bf-vk header",
/// i.e. it sees no key whatsoever. Sending the value in BOTH is what lets one
/// request shape serve a direct Anthropic endpoint and a fronting gateway,
/// which matters because self-host talks straight to Anthropic while every
/// hosted tenant goes through the gateway.
const GATEWAY_VK_HEADER: &str = "x-bf-vk";
/// Default max_tokens. The verdict itself is a compact JSON object, but on a
/// reasoning model THINKING TOKENS BILL AGAINST THIS CEILING TOO, and thinking
/// runs before the first output byte: a budget sized for the JSON alone
/// truncates every call before the verdict is ever written. A `max_tokens`
/// truncation is retried once at the doubled value.
pub const MAX_TOKENS: u32 = 8_000;
pub const MAX_TOKENS_RETRY: u32 = 16_000;
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

/// A single LLM classification request, shared by both stages. `system` must be
/// the stage's STATIC prompt (identical bytes every call, so prompt caching can
/// apply); `user` is the stage's fenced TRUSTED/UNTRUSTED message. `model` is
/// written verbatim into the request.
pub struct LlmRequest<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub schema: serde_json::Value,
    /// Reasoning depth, sent as `output_config.effort` on the Anthropic wire.
    /// This is the ONLY axis separating the two stages now that both run the
    /// same model: Stage-1 thinks briefly over a compact row, Stage-2 thinks
    /// hard over a fully contextualized one. `None` omits the field entirely,
    /// which is what a model without effort support requires.
    pub effort: Option<&'a str>,
}

/// The outcome of a single LLM call, generic over the verdict `T`. Every stage
/// and extractor shares this one shape: [`classify_llm`] yields
/// `LlmOutcome<String>` (the raw JSON verdict text) and [`classify_into`] turns
/// that into the caller's own parsed type, so the Refused/Failed halves are
/// never re-declared per stage.
#[derive(Debug)]
pub enum LlmOutcome<T> {
    /// The model's verdict + usage.
    Ok(T, Option<Usage>),
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
    /// transport error). Informational only.
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
/// JSON verdict text for the caller to parse and validate. Both providers consume
/// the IDENTICAL system prompt, fenced user message, and output schema.
///
/// Retry policy: 429 (honors `retry-after`) and 529/5xx retry with exponential
/// backoff (60s cap, 3 tries); 400/401 are permanent; a truncation retries once
/// at a higher token budget.
pub async fn classify_llm(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    provider: Stage2Provider,
    req: &LlmRequest<'_>,
) -> std::result::Result<LlmOutcome<String>, ClassifyError> {
    match provider {
        Stage2Provider::Anthropic => classify_anthropic(http, url, api_key, req).await,
        Stage2Provider::OpenAI => classify_openai(http, url, api_key, req).await,
    }
}

/// [`classify_llm`] plus the verdict parse: deserialize the raw text into `T`,
/// then hand it to `finish` for the caller's own validation/repackaging (its
/// `Err` is a redacted failure kind). Refusals and permanent failures pass
/// through untouched, and a malformed verdict becomes `Failed("json_parse")` —
/// the shape every stage and extractor shares.
pub async fn classify_into<T, U>(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    provider: Stage2Provider,
    req: &LlmRequest<'_>,
    finish: impl FnOnce(T) -> std::result::Result<U, String>,
) -> std::result::Result<LlmOutcome<U>, ClassifyError>
where
    T: serde::de::DeserializeOwned,
{
    let raw = classify_llm(http, url, api_key, provider, req).await?;
    Ok(match raw {
        LlmOutcome::Ok(text, usage) => match serde_json::from_str::<T>(&text) {
            Ok(parsed) => match finish(parsed) {
                Ok(value) => LlmOutcome::Ok(value, usage),
                Err(kind) => LlmOutcome::Failed(kind),
            },
            Err(_) => LlmOutcome::Failed("json_parse".into()),
        },
        LlmOutcome::Refused => LlmOutcome::Refused,
        LlmOutcome::Failed(kind) => LlmOutcome::Failed(kind),
    })
}

/// Emit a stage's/extractor's production `classify`: the delegation to its own
/// `classify_at` at the RESOLVED endpoint — [`crate::config::ResolvedLlm::url`],
/// which is [`provider_url`] unless the operator routed the Anthropic wire
/// through a gateway. Identical for all four call sites, so it is written once.
/// Leading doc attributes are forwarded.
macro_rules! classify_entrypoint {
    ($(#[$attr:meta])* $cfg:ty, $input:ty, $outcome:ty $(,)?) => {
        $(#[$attr])*
        pub async fn classify(
            http: &reqwest::Client,
            url: &str,
            api_key: &str,
            cfg: &$cfg,
            provider: $crate::config::Stage2Provider,
            input: &$input,
        ) -> std::result::Result<$outcome, $crate::triage::llm::ClassifyError> {
            classify_at(http, url, api_key, cfg, provider, input).await
        }
    };
}
pub(crate) use classify_entrypoint;

#[derive(Debug, Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: Vec<SystemBlock<'a>>,
    messages: Vec<RequestMessage<'a>>,
    output_config: OutputConfig<'a>,
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
struct OutputConfig<'a> {
    format: OutputFormat,
    /// Omitted when the configured model has no effort support; sending it to
    /// such a model is a 400, so absence has to stay expressible.
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'a str>,
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
            // OpenAI's wire has no prompt-cache split; the ledger records zero.
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
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

/// Token usage — numbers are fine to log. Requests set `cache_control:
/// ephemeral` on the system block, so Anthropic reports the prompt split three
/// ways: `input_tokens` is the UNCACHED remainder only, with cache writes and
/// reads in their own fields — dropping them under-reports the ledger.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

/// The ledger records exactly the split the API reports, so a usage block maps
/// field-for-field onto the store's bump-usage argument.
impl From<Usage> for crate::store::UsageTokens {
    fn from(u: Usage) -> Self {
        Self {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_creation: u.cache_creation_input_tokens,
            cache_read: u.cache_read_input_tokens,
        }
    }
}

async fn classify_anthropic(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    req: &LlmRequest<'_>,
) -> std::result::Result<LlmOutcome<String>, ClassifyError> {
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
                effort: req.effort,
            },
        };

        let build = || {
            let req = http
                .post(url)
                .header("x-api-key", api_key)
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json");
            // See GATEWAY_VK_HEADER: a fronting gateway ignores `x-api-key`
            // entirely, so the credential has to ride this header too or every
            // hosted call 401s. Only when a gateway is actually in front —
            // Anthropic has no use for it, and a credential should not be
            // copied into extra headers on a request that does not need it.
            let req = if is_gateway_url(url) {
                req.header(GATEWAY_VK_HEADER, api_key)
            } else {
                req
            };
            req.json(&body)
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
        return Ok(LlmOutcome::Ok(text, parsed.usage));
    }
}

async fn classify_openai(
    http: &reqwest::Client,
    url: &str,
    api_key: &str,
    req: &LlmRequest<'_>,
) -> std::result::Result<LlmOutcome<String>, ClassifyError> {
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
        return Ok(LlmOutcome::Ok(
            text,
            parsed.usage.map(OpenAiUsage::into_usage),
        ));
    }
}

/// The two success shapes from [`send_with_retry`].
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

/// POST the request, applying the retry policy for retryable statuses.
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

/// Is this Anthropic-wire request going somewhere OTHER than Anthropic's own
/// endpoint — i.e. through an operator-configured gateway?
///
/// Pure and URL-only on purpose: the credential decision in
/// [`classify_anthropic`] must be derivable from the resolved endpoint alone,
/// so no call site can send a virtual key to Anthropic (where it is useless) or
/// withhold one from the gateway (where its absence is a 401 on every row).
/// [`crate::config::Stage2Config::resolve_llm`] is what puts a gateway URL here
/// in the first place, and it only does so when the operator set a base URL.
pub fn is_gateway_url(url: &str) -> bool {
    url != API_URL
}

/// True when a permanent-failure kind is a CONFIG-LEVEL failure: the request
/// was rejected for a reason shared by every queued row, never a fact about the
/// message being classified. A wrong credential (`http_401`/`http_403`), an
/// exhausted gateway budget (`http_402`), a bad endpoint (`http_404`) — and
/// `http_400`, because this daemon builds every request from the same schema,
/// capped body, and configured model, so a 400 means the CONFIGURATION is
/// rejected (the canonical case: a gateway virtual key whose allowed-models
/// list does not contain the configured model answers 400 in 0ms, before any
/// upstream call). Callers must leave the row queued and stop the pass: the
/// remaining rows would fail identically, and a row marked processed here is
/// foreclosed from triage even after the config is fixed. The 2026-08-19
/// fleet outage foreclosed two days of hosted mail exactly that way — every
/// verdict silently fell back to the ingest heuristic.
///
/// Row-level permanent kinds (`json_parse`, `max_tokens_truncation`,
/// `response_decode`, `importance_out_of_range`) stay outside this predicate:
/// those ARE facts about the row's response and must mark processed so the row
/// cannot loop.
pub fn is_config_failure(kind: &str) -> bool {
    ["http_400", "http_401", "http_402", "http_403", "http_404"]
        .iter()
        .any(|prefix| kind.starts_with(prefix))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The gateway takes its virtual key ONLY in `x-bf-vk` — with just
    /// `x-api-key` set it answers 401 "virtual key is required", meaning it saw
    /// no credential at all. Anthropic direct is the opposite: `x-api-key` is
    /// the credential and the gateway header means nothing. So the endpoint,
    /// and only the endpoint, decides.
    #[test]
    fn only_a_gateway_endpoint_gets_the_virtual_key_header() {
        assert!(!is_gateway_url(API_URL));
        assert!(is_gateway_url(
            "https://bifrost.passband.app/anthropic/v1/messages"
        ));
        // A self-hosted gateway on a LAN box is still a gateway.
        assert!(is_gateway_url("http://10.0.0.4:8080/v1/messages"));
    }

    /// The config split is what keeps a bad credential, budget, or model list
    /// from eating the queue: config-shaped 4xx leaves rows queued for after
    /// the fix, while row-level kinds (a malformed response IS a fact about
    /// this row) mark processed so the row cannot loop.
    #[test]
    fn config_failures_split_from_the_row_level_permanent_class_by_kind() {
        assert!(is_config_failure("http_401:authentication_error"));
        assert!(is_config_failure("http_403:permission_error"));
        // The 2026-08-19 outage class: a gateway virtual key rejecting the
        // configured model answers 400 for every row alike.
        assert!(is_config_failure("http_400:invalid_request_error"));
        // Gateway budget exhausted: month-scoped config, not a row fact.
        assert!(is_config_failure("http_402:payment_required"));
        assert!(is_config_failure("http_404:not_found_error"));
        assert!(!is_config_failure("max_tokens_truncation"));
        assert!(!is_config_failure("json_parse"));
        assert!(!is_config_failure("response_decode"));
        assert!(!is_config_failure("importance_out_of_range"));
    }

    #[test]
    fn usage_deserializes_cache_token_fields() {
        let u: Usage = serde_json::from_str(
            r#"{"input_tokens":10,"output_tokens":5,
                "cache_creation_input_tokens":700,"cache_read_input_tokens":9000}"#,
        )
        .unwrap();
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 5);
        assert_eq!(u.cache_creation_input_tokens, 700);
        assert_eq!(u.cache_read_input_tokens, 9000);
    }

    #[test]
    fn usage_without_cache_fields_defaults_them_to_zero() {
        let u: Usage = serde_json::from_str(r#"{"input_tokens":10,"output_tokens":5}"#).unwrap();
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
    }

    #[test]
    fn openai_usage_leaves_cache_fields_zero() {
        let u = OpenAiUsage {
            prompt_tokens: 42,
            completion_tokens: 7,
        }
        .into_usage();
        assert_eq!(u.input_tokens, 42);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
    }
}
