//! Configuration: `~/.config/squelch/config.toml`, with env-var overrides that
//! always win. Every threshold and path is a [`Config`] field with a default, so
//! a missing config file still yields a working system.

use crate::error::CoreError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Canonical env var for the SQLite path (matches [`Config`]'s `db_path`).
pub const ENV_DB_PATH: &str = "SQUELCH_DB_PATH";
/// Legacy alias for [`ENV_DB_PATH`], silently accepted with a deprecation note.
pub const ENV_DB_PATH_LEGACY: &str = "SQUELCH_DB";
/// Canonical env var for the account email (matches [`Config`]'s `account_email`).
pub const ENV_ACCOUNT_EMAIL: &str = "SQUELCH_ACCOUNT_EMAIL";
/// Legacy alias for [`ENV_ACCOUNT_EMAIL`], silently accepted with a deprecation note.
pub const ENV_ACCOUNT_EMAIL_LEGACY: &str = "SQUELCH_ACCOUNT";
/// Comma-separated extra hostnames for the agent door's DNS-rebinding guard,
/// additive to the loopback defaults (a `tailscale serve` proxy rewrites `Host`).
pub const ENV_MCP_ALLOWED_HOSTS: &str = "SQUELCH_MCP_ALLOWED_HOSTS";

/// The one canonical default SQLite path, `~/.local/share/squelch/squelch.db`:
/// every binary resolves here when no path is configured, so they cannot drift
/// onto different db files. CWD-relative only when `HOME` is unset.
pub fn default_db_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let dir = PathBuf::from(home).join(".local/share/squelch");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("squelch.db");
    }
    PathBuf::from("squelch.db")
}

/// Read a canonical env var, falling back to a legacy alias with a deprecation
/// note to stderr (the note carries no value).
fn env_with_legacy(canonical: &str, legacy: &str) -> Option<String> {
    if let Ok(v) = std::env::var(canonical)
        && !v.is_empty()
    {
        return Some(v);
    }
    if let Ok(v) = std::env::var(legacy)
        && !v.is_empty()
    {
        eprintln!(
            "squelch: {legacy} is deprecated; please use {canonical} instead (still honored for now)"
        );
        return Some(v);
    }
    None
}

/// The SQLite path for ALL binaries: `SQUELCH_DB_PATH` > legacy `SQUELCH_DB` >
/// [`default_db_path`]. Single source of truth, so binaries cannot drift.
pub fn resolve_db_path() -> PathBuf {
    env_with_legacy(ENV_DB_PATH, ENV_DB_PATH_LEGACY)
        .map(PathBuf::from)
        .unwrap_or_else(default_db_path)
}

/// The account email for ALL binaries: `SQUELCH_ACCOUNT_EMAIL` > legacy
/// `SQUELCH_ACCOUNT` > `default_email`.
pub fn resolve_account_email(default_email: &str) -> String {
    env_with_legacy(ENV_ACCOUNT_EMAIL, ENV_ACCOUNT_EMAIL_LEGACY)
        .unwrap_or_else(|| default_email.to_string())
}

/// The agent-door DNS-rebinding allow-list: rmcp's loopback defaults PLUS
/// `SQUELCH_MCP_ALLOWED_HOSTS`. Strictly additive — the loopback entries are
/// never dropped and the guard is never widened to "any host". Entries may be
/// bare hosts or `host:port` authorities; blanks are ignored.
pub fn mcp_allowed_hosts() -> Vec<String> {
    let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if let Ok(raw) = std::env::var(ENV_MCP_ALLOWED_HOSTS) {
        for h in raw.split(',') {
            let h = h.trim();
            if !h.is_empty() {
                hosts.push(h.to_string());
            }
        }
    }
    hosts
}

/// The READ scope. This is all the sync daemon + triage ever REQUEST, and that
/// much is a hard invariant, hence a `const`. See [`WRITE_SCOPES`] for the
/// separate, opt-in action credential.
///
/// What Google ISSUES is a different question. Grants are unioned per Cloud
/// project: with incremental authorization an access token also covers every
/// scope the user has already granted the project, even when those grants came
/// from a different client. So once a user has run `squelchd auth --write`, the
/// token behind the read credential carries modify+send too, and is capable of
/// more than readonly however narrow the request was.
///
/// The consequence, stated plainly: token scope is defense in depth here, not
/// the thing that enforces the two-door split. What enforces it is that the
/// agent door exposes no write tools, that [`WRITE_SCOPES`] credentials are
/// loaded only by human-door action handlers, and that sealed rows are absent
/// from every serving query. See `docs/SECURITY.md` §4.
pub const GMAIL_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";

/// The WRITE scopes: requested ONLY by `squelchd auth --write`, loaded ONLY by
/// human-door action endpoints, never by sync/triage. Deliberately a separate
/// constant from [`GMAIL_READONLY_SCOPE`] so the two can never be conflated.
pub const GMAIL_MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
pub const GMAIL_SEND_SCOPE: &str = "https://www.googleapis.com/auth/gmail.send";

/// Convenience: the full set of scopes for the write credential.
pub const WRITE_SCOPES: &[&str] = &[GMAIL_MODIFY_SCOPE, GMAIL_SEND_SCOPE];

/// Which backend persists OAuth tokens: the OS secret service, or a mode-0600
/// JSON file (the only option on a headless box with no desktop keyring).
/// Defaults to keyring on macOS, file on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialBackend {
    Keyring,
    File,
}

impl Default for CredentialBackend {
    fn default() -> Self {
        // Headless Linux typically has no Secret Service.
        if cfg!(target_os = "macos") {
            CredentialBackend::Keyring
        } else {
            CredentialBackend::File
        }
    }
}

impl CredentialBackend {
    /// Case-insensitive parse of `credential_backend` / `SQUELCH_CRED_BACKEND`;
    /// an unknown value leaves the caller on the platform default.
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "keyring" => Some(CredentialBackend::Keyring),
            "file" => Some(CredentialBackend::File),
            _ => None,
        }
    }
}

/// Which LLM provider Stage-2 talks to. Selected by KEY PREFIX at resolution
/// time (see [`Stage2Config::resolve_llm`]) unless forced via the
/// `stage2_provider` config field / `SQUELCH_STAGE2_PROVIDER` env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage2Provider {
    Anthropic,
    OpenAI,
}

impl Stage2Provider {
    /// Case-insensitive parse of `stage2_provider` / `SQUELCH_STAGE2_PROVIDER`;
    /// an unknown value leaves the caller on prefix sniffing.
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Some(Stage2Provider::Anthropic),
            "openai" => Some(Stage2Provider::OpenAI),
            _ => None,
        }
    }

    /// A stable lowercase label for the provider, surfaced on `/client/usage`.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage2Provider::Anthropic => "anthropic",
            Stage2Provider::OpenAI => "openai",
        }
    }

    /// Default cost-ledger prices (USD per MTok in, out) — change with the model.
    pub fn default_prices(self) -> (f64, f64) {
        match self {
            Stage2Provider::Anthropic => (1.0, 5.0),
            Stage2Provider::OpenAI => (0.15, 0.60),
        }
    }
}

/// Sync tunables.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// How many days of history to backfill on the initial sync.
    pub backfill_days: u32,
    /// How often (seconds) the incremental poll loop calls `history.list`; one
    /// poll batch is the coalesced batch.
    pub poll_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            backfill_days: 30,
            poll_secs: 45,
        }
    }
}

/// Notification-event tunables: these two numbers are the whole non-structural
/// policy for what earns a row in the `events` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// Importance at or above which a message earns an event on score alone
    /// (past_due/deadline tiers bypass it entirely). Default 50, deliberately the
    /// same number as the TUI's starting squelch line, so "notified" and "above
    /// the line" mean the same thing. Env: `SQUELCH_NOTIFY_MIN_IMPORTANCE`.
    pub min_importance: u8,
    /// THE STORM GUARD: mail received longer ago than this can never produce an
    /// event, whatever its verdict — that is what makes "never on initial
    /// backfill" hold across restarts and re-syncs, instead of trusting a code
    /// path to know which pass it is on. Mail dated in the FUTURE is out of the
    /// window too, so a sender-controlled `Date:` cannot buy freshness (see
    /// [`crate::triage::events::is_fresh`]). Default 900.
    pub freshness_window_secs: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            min_importance: 50,
            freshness_window_secs: 900,
        }
    }
}

/// APNs pusher config; see [`crate::push`] for the task itself.
///
/// `relay_url` IS THE FEATURE FLAG: absent (the default), the pusher is never
/// spawned and no socket is ever opened toward anyone. Nothing here configures
/// content, because the relay is blind — the push carries an event id and a
/// collapse id and nothing else.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PusherConfig {
    /// Base URL of the squelch relay (e.g. `https://relay.example.com`). The
    /// pusher POSTs to `{relay_url}/v1/push`. Env: `SQUELCH_RELAY_URL`.
    /// `None` => the pusher task is not spawned at all.
    pub relay_url: Option<String>,
    /// Bearer presented to the relay; it is the value the relay itself validates
    /// as `SQUELCH_RELAY_AUTH_TOKEN`. Env: `SQUELCH_RELAY_TOKEN`. Prefer the env
    /// var over config.toml — this is secret material, and it is NEVER logged.
    pub relay_token: Option<String>,
    /// Optional `apns-topic` (bundle id) override, forwarded verbatim. Must be in
    /// the relay's own allowlist. Env: `SQUELCH_RELAY_TOPIC`.
    pub topic: Option<String>,
    /// Optional APNs environment override (`production` | `sandbox`), forwarded
    /// verbatim; the relay validates it. Env: `SQUELCH_RELAY_APNS_ENV`.
    pub environment: Option<String>,
}

/// Outbound read-tracking config; see [`crate::tracking`].
///
/// `base_url` IS THE FEATURE FLAG for MINTING: absent (the default), no send
/// ever gets a pixel no matter what the client asks for, and the human door
/// reports tracking as unconfigured so the client can hide the toggle. It is
/// deliberately separate from `[pusher] relay_url` — the pixel has to be
/// reachable from a stranger's mail client, which the relay may or may not be.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TrackingConfig {
    /// PUBLIC base URL that reaches this daemon's `/t/:token` route (e.g.
    /// `https://track.example.com`); the pixel is `{base_url}/t/{token}`.
    /// Env: `SQUELCH_TRACK_URL`. `None` => tracking is off for every send.
    pub base_url: Option<String>,
}

/// Prometheus scrape-endpoint config; see [`crate::metrics`].
///
/// `bind` IS THE FEATURE FLAG: absent (the default), the listener is never
/// opened and nothing can scrape this daemon. It is a SEPARATE address from the
/// doors on purpose — `/metrics` carries no credential, so it is reachable only
/// from wherever the operator points it (loopback, or a private interface a
/// scraper reaches), never from whatever fronts `/client/*`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MetricsConfig {
    /// Host:port for the metrics listener (e.g. `127.0.0.1:9848`).
    /// Env: `SQUELCH_METRICS_BIND`. `None` => no listener is opened at all.
    pub bind: Option<String>,
}

/// Default embedding-weights cache dir, a sibling of the sqlite db under the
/// XDG data dir; CWD-relative only when `HOME` is unset.
pub fn default_embed_cache_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local/share/squelch")
            .join("models");
    }
    PathBuf::from("squelch-models")
}

/// On-box semantic-recall tunables (fastembed, ONNX, CPU; weights download once
/// to `cache_dir`). `model` and `dims` MUST agree with each other and with the
/// `message_vecs` vec0 `float[N]` in `store/schema.sql` — the store asserts this
/// at open time, and changing `dims` means resetting the db.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedConfig {
    /// fastembed model name/alias. Default: BGE-small-en-v1.5 (384-dim, small,
    /// English). Accepts the fastembed `model_code` or a friendly alias.
    pub model: String,
    /// Embedding dimensionality; must match `model` and the vec0 table width.
    pub dims: usize,
    /// Where ONNX weights cache on disk. Default: [`default_embed_cache_dir`].
    pub cache_dir: PathBuf,
    /// Characters of `subject + body` fed to the embedder per message.
    pub max_chars: usize,
    /// Backfill batch size: how many missing-vector messages to embed per pass.
    pub backfill_batch: usize,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            model: "bge-small-en-v1.5".to_string(),
            dims: 384,
            cache_dir: default_embed_cache_dir(),
            max_chars: crate::embed::DEFAULT_EMBED_MAX_CHARS,
            backfill_batch: 64,
        }
    }
}

impl EmbedConfig {
    /// Build the resolved [`crate::embed::EmbedSettings`] the embedder needs.
    pub fn settings(&self) -> crate::embed::EmbedSettings {
        crate::embed::EmbedSettings {
            model_name: self.model.clone(),
            dims: self.dims,
            cache_dir: self.cache_dir.clone(),
        }
    }
}

/// Resolved (present) OAuth client credentials.
#[derive(Debug, Clone)]
pub struct OAuthClientConfig {
    pub client_id: String,
    pub client_secret: String,
}

/// Importance scores (0-100) assigned by the Stage-1 rules engine per rung.
/// Tunable so operators can bias what surfaces without recompiling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Stage1Config {
    /// Bills/past-due always surface via their tier; this is the raw score.
    pub bill_importance: u8,
    /// A message matched a `Surface` sender rule.
    pub rule_surface_importance: u8,
    /// A message matched a `Squelch` sender rule.
    pub rule_squelch_importance: u8,
    /// A message matched a `Filtered` rule (deferred to Stage-2).
    pub rule_filtered_importance: u8,
    /// Sender appears in the user's Sent mail (known contact).
    pub known_contact_importance: u8,
    /// Ops/monitoring alert from an automated sender.
    pub alert_importance: u8,
    /// Newsletter / receipt / cold-sales noise.
    pub noise_importance: u8,
    /// Ambiguous fall-through (unknown sender, no pattern) -> Stage-2.
    pub fallthrough_importance: u8,
    /// Bill-shaped mail from an UNKNOWN sender. Deliberately moderate: a scam
    /// "past-due" must surface for a Stage-2 look, never land confident PastDue.
    pub bill_unknown_sender_importance: u8,
    /// Sanity dampener: an extracted bill amount strictly greater than this
    /// (dollars) is treated as absurd and shaves confidence (never raises tier).
    /// Default $50,000 — a real household bill essentially never exceeds this.
    pub bill_absurd_amount_threshold: f64,

    // ---- Stage-1 LLM pass (the small model run on every non-rule email) ----
    // The heuristic fields above are its seed/fallback. Key, provider, and
    // endpoint come from [`Stage2Config::resolve_llm`]; only the fields below
    // are Stage-1's own.
    /// The Stage-1 model id string. Default `claude-haiku-4-5` (a small, cheap
    /// model — it sees nearly every email). Env: `SQUELCH_STAGE1_MODEL`.
    pub model: String,
    /// Cap on the flattened email body (chars) fed into the UNTRUSTED block.
    /// Env: `SQUELCH_STAGE1_MAX_BODY_CHARS`.
    pub max_body_chars: usize,
    /// How many queued rows to refine per sync cycle. Env:
    /// `SQUELCH_STAGE1_BATCH_PER_CYCLE`.
    pub batch_per_cycle: usize,
    /// Global per-account-per-day call cap — the only cap Stage-1 has, since it
    /// must see every email. Env: `SQUELCH_STAGE1_GLOBAL_DAILY_CAP`.
    pub global_daily_cap: u32,
    /// Per-million-input-token price (USD) for the Stage-1 model. Default 1.0
    /// (claude-haiku-4-5). Env: `SQUELCH_STAGE1_PRICE_IN_PER_MTOK`.
    pub price_in_per_mtok: f64,
    /// Per-million-output-token price (USD) for the Stage-1 model. Default 5.0
    /// (claude-haiku-4-5). Env: `SQUELCH_STAGE1_PRICE_OUT_PER_MTOK`.
    pub price_out_per_mtok: f64,
}

impl Default for Stage1Config {
    fn default() -> Self {
        Self {
            bill_importance: 95,
            rule_surface_importance: 80,
            rule_squelch_importance: 10,
            rule_filtered_importance: 30,
            known_contact_importance: 70,
            alert_importance: 75,
            noise_importance: 15,
            fallthrough_importance: 40,
            bill_unknown_sender_importance: 55,
            bill_absurd_amount_threshold: 50_000.0,
            // Stage-1 LLM defaults.
            model: "claude-haiku-4-5".to_string(),
            max_body_chars: 1500,
            batch_per_cycle: 10,
            global_daily_cap: 1000,
            price_in_per_mtok: 1.0,
            price_out_per_mtok: 5.0,
        }
    }
}

/// Stage-2 LLM triage tunables. The pass runs only over rows Stage-1 refined but
/// left non-confident: `stage1_model_used IS NOT NULL AND needs_stage2=1 AND
/// model_used IS NULL AND sensitivity='normal'` — that last clause is what keeps
/// sealed mail away from the model. Enabled by key presence alone; env overrides
/// are `SQUELCH_MODEL` and `SQUELCH_STAGE2_*`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Stage2Config {
    /// Anthropic API key; the `ANTHROPIC_API_KEY` env var wins over it. Absent,
    /// Stage-2 disables gracefully and rows stay queued. NEVER logged.
    pub anthropic_api_key: Option<String>,
    /// Force the provider (`anthropic` / `openai`), overriding key-prefix
    /// sniffing. Env: `SQUELCH_STAGE2_PROVIDER`.
    pub stage2_provider: Option<Stage2Provider>,
    /// Anthropic-wire base URL override: when set (and the resolved provider is
    /// Anthropic), every LLM call posts to `<base>/v1/messages` instead of
    /// api.anthropic.com. This is the hosted fleet's knob for routing tenant
    /// daemons through our Bifrost gateway. It exists in production while
    /// `SyncEngine::with_api_base` stays `#[cfg(test)]`-only because the threat
    /// model differs: that hook would re-aim the tenant's GOOGLE BEARER (an
    /// exfiltration primitive), whereas this one redirects a scoped, revocable,
    /// budget-capped virtual key WE issue. It is operator-set, https-enforced
    /// (plain http only for loopback, so tests/dev mocks work), and never
    /// logged; a non-conforming value is announced once on stderr and treated
    /// as absent. Env: `SQUELCH_ANTHROPIC_BASE_URL`.
    pub anthropic_base_url: Option<String>,
    /// Model id, written verbatim into the request's `model` field and stored as
    /// `model_used` on applied rows.
    pub model: String,
    /// Cap on the flattened email body (chars) fed into the UNTRUSTED block.
    /// The body is truncated to this and the truncation is noted in-band.
    pub max_body_chars: usize,
    /// How many queued rows to process per sync cycle (fetch cap).
    pub batch_per_cycle: usize,
    /// Per-thread-per-day API-call cap (the circuit breaker). Incremented
    /// BEFORE the call so retry storms can't exceed it.
    pub thread_daily_cap: u32,
    /// Global per-account-per-day API-call cap. Same increment-before
    /// discipline, counted via a `thread_id='__global__'` sentinel row in
    /// `wake_budget`.
    pub global_daily_cap: u32,
    /// Per-sender-per-day API-call cap, so one chatty sender fanning many
    /// threads cannot burn the budget. Counted via a `thread_id='sender:<addr>'`
    /// sentinel row in `wake_budget` (no real Gmail thread id starts with
    /// `sender:`). Env: `SQUELCH_STAGE2_SENDER_DAILY_CAP`.
    pub sender_daily_cap: u32,
    /// Queued rows older than this are marked `model_used='stale-skip'` instead
    /// of spending a call: they keep their Stage-1 values, and neither consume
    /// budget nor sit queued forever. Env: `SQUELCH_STAGE2_MAX_AGE_DAYS`.
    pub max_age_days: u32,
    /// Per-MTok input price (USD), used only for the `est_cost_usd_today` figure
    /// on `/client/stats`. Change-with-model, together with
    /// `price_out_per_mtok`. Env: `SQUELCH_STAGE2_PRICE_IN_PER_MTOK`.
    pub price_in_per_mtok: f64,
    /// Per-MTok output price (USD). Change-with-model. Env:
    /// `SQUELCH_STAGE2_PRICE_OUT_PER_MTOK`.
    pub price_out_per_mtok: f64,
}

impl Default for Stage2Config {
    fn default() -> Self {
        Self {
            anthropic_api_key: None,
            stage2_provider: None,
            anthropic_base_url: None,
            // Stage-2 is the ESCALATION pass on a MORE CAPABLE model.
            model: "claude-sonnet-5".to_string(),
            max_body_chars: 1500,
            batch_per_cycle: 10,
            thread_daily_cap: 3,
            global_daily_cap: 200,
            sender_daily_cap: 5,
            max_age_days: 7,
            // claude-sonnet-5 per-MTok (input / output).
            price_in_per_mtok: 3.0,
            price_out_per_mtok: 15.0,
        }
    }
}

/// A fully resolved LLM destination: the key, the wire it speaks, and the exact
/// endpoint URL every call posts to. Produced once at startup by
/// [`Stage2Config::resolve_llm`] so no call site re-derives (and none can
/// disagree on) where the key is sent. No `Debug` on purpose: `api_key` is key
/// material and must never reach a log line.
pub struct ResolvedLlm {
    pub api_key: String,
    pub provider: Stage2Provider,
    pub url: String,
}

impl Stage2Config {
    /// Resolve the LLM key, provider, and endpoint in one shot. Key source,
    /// first match wins: `SQUELCH_STAGE2_API_KEY` > `ANTHROPIC_API_KEY` >
    /// `OPENAI_API_KEY` > config `anthropic_api_key`. Provider precedence:
    /// explicit `stage2_provider` override > `sk-ant-` prefix sniff > (a valid
    /// `anthropic_base_url` is set => Anthropic) > OpenAI — the base-URL arm
    /// exists because a gateway virtual key (`sk-bf-...`) is not `sk-ant-`
    /// shaped, yet an operator who pointed the daemon at an Anthropic-compatible
    /// gateway has already declared the wire. The URL is the provider's
    /// production endpoint unless the Anthropic wire carries a valid
    /// `anthropic_base_url`, in which case it is `<base>/v1/messages`; the
    /// override never applies to OpenAI. Empty strings count as absent, and key
    /// material is never logged.
    pub fn resolve_llm(&self) -> Option<ResolvedLlm> {
        // Validate the override BEFORE provider inference: a rejected URL is
        // absent everywhere, so it cannot flip a key onto the Anthropic wire
        // and then fall back to posting it at api.anthropic.com.
        let base_url = self.validated_base_url();

        let (key, inferred) = if let Some(key) = env_nonempty("SQUELCH_STAGE2_API_KEY") {
            // Explicit var: sniff the provider from the prefix; a set base URL
            // breaks the tie for non-Anthropic-shaped keys (see above).
            let provider = if key.starts_with("sk-ant-") || base_url.is_some() {
                Stage2Provider::Anthropic
            } else {
                Stage2Provider::OpenAI
            };
            (key, provider)
        } else if let Some(key) = env_nonempty("ANTHROPIC_API_KEY") {
            (key, Stage2Provider::Anthropic)
        } else if let Some(key) = env_nonempty("OPENAI_API_KEY") {
            (key, Stage2Provider::OpenAI)
        } else if let Some(key) = self.anthropic_api_key.clone().filter(|s| !s.is_empty()) {
            (key, Stage2Provider::Anthropic)
        } else {
            return None;
        };

        // Config force-override wins over the inferred provider.
        let provider = self.stage2_provider.unwrap_or(inferred);
        let url = match (provider, base_url) {
            (Stage2Provider::Anthropic, Some(base)) => {
                format!("{}/v1/messages", base.trim_end_matches('/'))
            }
            _ => crate::triage::llm::provider_url(provider).to_string(),
        };
        Some(ResolvedLlm {
            api_key: key,
            provider,
            url,
        })
    }

    /// The `anthropic_base_url` override, or `None` when it is unset, blank, or
    /// fails the transport check: https anywhere, plain http only for loopback
    /// (dev/test mocks). A rejected value is announced on stderr and then
    /// treated as absent, so a typo'd override disables the gateway loudly
    /// instead of sending our key in cleartext.
    fn validated_base_url(&self) -> Option<&str> {
        let base = self
            .anthropic_base_url
            .as_deref()
            .filter(|s| !s.is_empty())?;
        if base_url_transport_ok(base) {
            Some(base)
        } else {
            eprintln!(
                "squelch: stage2.anthropic_base_url / SQUELCH_ANTHROPIC_BASE_URL must be https \
                 (plain http is loopback-only) with no query or fragment — override ignored, \
                 using the provider's production endpoint"
            );
            None
        }
    }

    /// Key + provider only; compat shim over [`Stage2Config::resolve_llm`].
    pub fn resolve_key_and_provider(&self) -> Option<(String, Stage2Provider)> {
        self.resolve_llm().map(|r| (r.api_key, r.provider))
    }

    /// Just the API key, for callers that only need presence.
    pub fn resolve_api_key(&self) -> Option<String> {
        self.resolve_llm().map(|r| r.api_key)
    }

    /// Stage-2 is enabled iff an API key is resolvable.
    pub fn enabled(&self) -> bool {
        self.resolve_llm().is_some()
    }
}

/// `true` when `base` is safe to send an API key to: https anywhere, or http
/// terminating on this machine (127.0.0.0/8, `::1`, `localhost`). Anything that
/// does not parse as a URL fails too, as does a base carrying a query or
/// fragment — `/v1/messages` is appended to this string, so a `?` or `#` in it
/// would produce a mangled join; better to fail loudly than post at a typo.
fn base_url_transport_ok(base: &str) -> bool {
    let Ok(parsed) = url::Url::parse(base) else {
        return false;
    };
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return false;
    }
    match parsed.scheme() {
        "https" => true,
        "http" => match parsed.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            None => false,
        },
        _ => false,
    }
}

/// Read an env var, returning `None` when unset or empty.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// Overwrite `slot` from env var `name` when it is set, non-empty, and parses as
/// `T`. EMPTY IS "UNSET": an exported-but-blank var must never clear a
/// configured value. The value is used verbatim — no trimming — so a var whose
/// whitespace matters keeps it.
fn env_override<T: std::str::FromStr>(name: &str, slot: &mut T) {
    if let Ok(v) = std::env::var(name)
        && !v.is_empty()
        && let Ok(parsed) = v.parse::<T>()
    {
        *slot = parsed;
    }
}

/// [`env_override`] for an `Option<T>` slot: a usable value sets `Some`, and an
/// unset/blank/unparseable var leaves whatever the config file supplied.
fn env_override_opt<T: std::str::FromStr>(name: &str, slot: &mut Option<T>) {
    if let Ok(v) = std::env::var(name)
        && !v.is_empty()
        && let Ok(parsed) = v.parse::<T>()
    {
        *slot = Some(parsed);
    }
}

// ---- Stage-2 daily-cap runtime-override plumbing ---------------------------
// Three layers, highest wins: an `app_settings` runtime override (applied
// without a restart) > config/env > [`Stage2Config::default`]. These constants
// are the shared `app_settings.key` names, so store, sync pass, and API cannot
// drift.

/// `app_settings.key` for the per-thread-per-day Stage-2 cap override.
pub const APP_SETTING_THREAD_DAILY_CAP: &str = "stage2_thread_daily_cap";
/// `app_settings.key` for the per-sender-per-day Stage-2 cap override.
pub const APP_SETTING_SENDER_DAILY_CAP: &str = "stage2_sender_daily_cap";
/// `app_settings.key` for the global-per-account-per-day Stage-2 cap override.
pub const APP_SETTING_GLOBAL_DAILY_CAP: &str = "stage2_global_daily_cap";
/// `app_settings.key` for the global-per-account-per-day Stage-1 cap override.
pub const APP_SETTING_STAGE1_GLOBAL_DAILY_CAP: &str = "stage1_global_daily_cap";

/// Inclusive bounds a Stage-2 daily-cap value must fall within (validated by the
/// human door before persisting an override).
pub const STAGE2_CAP_MIN: u32 = 1;
pub const STAGE2_CAP_MAX: u32 = 100_000;

/// Which layer supplied a Stage-2 daily cap. `Config` covers both TOML and env
/// (both mean "operator-set"); the runtime override layer is reported separately
/// by the API, so it has no variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapSource {
    Default,
    Config,
}

impl CapSource {
    /// Stable lowercase label for the wire (`"default"` / `"config"`).
    pub fn as_str(self) -> &'static str {
        match self {
            CapSource::Default => "default",
            CapSource::Config => "config",
        }
    }
}

/// The config/env-layer source of each Stage-2 daily cap, computed at load and
/// threaded to the human door ("override" is decided later from `app_settings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stage2CapSources {
    pub thread_daily_cap: CapSource,
    pub sender_daily_cap: CapSource,
    pub global_daily_cap: CapSource,
    /// The config/env-layer source of the Stage-1 global daily cap.
    pub stage1_global_daily_cap: CapSource,
}

impl Default for Stage2CapSources {
    fn default() -> Self {
        Self {
            thread_daily_cap: CapSource::Default,
            sender_daily_cap: CapSource::Default,
            global_daily_cap: CapSource::Default,
            stage1_global_daily_cap: CapSource::Default,
        }
    }
}

/// A cap is `Config`-sourced if the TOML `[stage2]` table carries its key OR its
/// env var is set (non-empty); otherwise it fell through to the built-in default.
fn cap_source(stage2_tbl: Option<&toml::Table>, key: &str, env_var: &str) -> CapSource {
    let in_toml = stage2_tbl.map(|t| t.contains_key(key)).unwrap_or(false);
    let in_env = std::env::var(env_var)
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if in_toml || in_env {
        CapSource::Config
    } else {
        CapSource::Default
    }
}

/// [`Stage2CapSources`] for a (possibly absent) config path. A missing or
/// unparseable file contributes no TOML keys; env can still promote a cap.
fn stage2_cap_sources_for(path: Option<&std::path::Path>) -> Stage2CapSources {
    let parsed: Option<toml::Table> = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| text.parse::<toml::Table>().ok());
    let stage2_tbl: Option<toml::Table> = parsed
        .as_ref()
        .and_then(|t| t.get("stage2").and_then(|v| v.as_table()).cloned());
    let stage1_tbl: Option<toml::Table> = parsed
        .as_ref()
        .and_then(|t| t.get("stage1").and_then(|v| v.as_table()).cloned());
    let s = stage2_tbl.as_ref();
    Stage2CapSources {
        thread_daily_cap: cap_source(s, "thread_daily_cap", "SQUELCH_STAGE2_THREAD_DAILY_CAP"),
        sender_daily_cap: cap_source(s, "sender_daily_cap", "SQUELCH_STAGE2_SENDER_DAILY_CAP"),
        global_daily_cap: cap_source(s, "global_daily_cap", "SQUELCH_STAGE2_GLOBAL_DAILY_CAP"),
        stage1_global_daily_cap: cap_source(
            stage1_tbl.as_ref(),
            "global_daily_cap",
            "SQUELCH_STAGE1_GLOBAL_DAILY_CAP",
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Google OAuth client id (from your own GCP "Desktop app" client).
    pub client_id: Option<String>,
    /// Google OAuth client secret.
    pub client_secret: Option<String>,
    /// The single Gmail account this v0 instance manages. Also the keyring key.
    pub account_email: Option<String>,

    /// Path to the SQLite store.
    pub db_path: PathBuf,
    /// Which backend persists OAuth tokens (`keyring` or `file`). Defaults per
    /// platform (keyring on macOS, file on Linux). Override with
    /// `SQUELCH_CRED_BACKEND`.
    pub credential_backend: CredentialBackend,
    /// Path to the JSON credentials file used by the `file` backend. Defaults to
    /// `~/.config/squelch/credentials.json`. Ignored by the keyring backend.
    pub credentials_path: Option<PathBuf>,
    /// Default minimum importance for surfacing updates.
    pub default_min_importance: u8,
    /// How aggressively to squelch. Placeholder; the triage agent owns semantics.
    pub squelch_level: u8,
    /// Stage-1 rules-engine tuning.
    pub stage1: Stage1Config,
    /// Stage-2 LLM triage tuning (Anthropic API, budgets).
    pub stage2: Stage2Config,
    /// Sync tunables (backfill window, poll interval).
    pub sync: SyncConfig,
    /// On-box semantic-recall (v1) tunables (embedding model, dims, cache dir).
    pub embed: EmbedConfig,
    /// Notification-event emission policy (threshold + the freshness storm guard).
    pub notify: NotifyConfig,
    /// APNs pusher: the blind relay's URL + bearer. Absent `relay_url` means the
    /// task is never spawned.
    pub pusher: PusherConfig,
    /// Outbound read tracking. Absent `base_url` means no send is ever tracked.
    pub tracking: TrackingConfig,
    /// Prometheus scrape endpoint. Absent `bind` means the listener is never
    /// opened.
    pub metrics: MetricsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            account_email: None,
            db_path: default_db_path(),
            credential_backend: CredentialBackend::default(),
            credentials_path: None,
            default_min_importance: 0,
            squelch_level: 0,
            stage1: Stage1Config::default(),
            stage2: Stage2Config::default(),
            sync: SyncConfig::default(),
            embed: EmbedConfig::default(),
            notify: NotifyConfig::default(),
            pusher: PusherConfig::default(),
            tracking: TrackingConfig::default(),
            metrics: MetricsConfig::default(),
        }
    }
}

impl Config {
    /// Default config path: `~/.config/squelch/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| h.join(".config").join("squelch").join("config.toml"))
    }

    /// Load config from the default path (if present), applying env-var
    /// overrides. A missing file is not an error — defaults are used.
    pub fn load() -> Self {
        let mut cfg = match Self::default_path() {
            Some(p) => Self::from_path(&p).unwrap_or_default(),
            None => Self::default(),
        };
        cfg.apply_env_overrides();
        cfg
    }

    /// [`Config::load`], plus where each Stage-2 daily cap came from — the human
    /// door reports that on `/client/triage-config`.
    pub fn load_with_cap_sources() -> (Self, Stage2CapSources) {
        let path = Self::default_path();
        let mut cfg = match &path {
            Some(p) => Self::from_path(p).unwrap_or_default(),
            None => Self::default(),
        };
        // Sources are read from the raw TOML + env BEFORE env is folded into cfg
        // (folding is lossy — it collapses "explicitly set" into the value).
        let sources = stage2_cap_sources_for(path.as_deref());
        cfg.apply_env_overrides();
        (cfg, sources)
    }

    /// [`Config::load_from`], plus the cap sources. See
    /// [`Config::load_with_cap_sources`].
    pub fn load_from_with_cap_sources(path: &std::path::Path) -> (Self, Stage2CapSources) {
        let mut cfg = Self::from_path(path).unwrap_or_default();
        let sources = stage2_cap_sources_for(Some(path));
        cfg.apply_env_overrides();
        (cfg, sources)
    }

    /// Parse a config from a specific TOML file. Returns `None` if the file is
    /// absent or unparseable (callers fall back to defaults).
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
    }

    /// Env-var overrides (highest precedence). Env always wins over the file so
    /// operators can override without editing config.
    fn apply_env_overrides(&mut self) {
        // The two legacy-alias vars keep their own helper: they accept a
        // deprecated second name and note the deprecation on stderr.
        if let Some(p) = env_with_legacy(ENV_DB_PATH, ENV_DB_PATH_LEGACY) {
            self.db_path = PathBuf::from(p);
        }
        env_override("SQUELCH_MIN_IMPORTANCE", &mut self.default_min_importance);
        env_override_opt("SQUELCH_CLIENT_ID", &mut self.client_id);
        env_override_opt("SQUELCH_CLIENT_SECRET", &mut self.client_secret);
        if let Some(v) = env_with_legacy(ENV_ACCOUNT_EMAIL, ENV_ACCOUNT_EMAIL_LEGACY) {
            self.account_email = Some(v);
        }
        env_override("SQUELCH_BACKFILL_DAYS", &mut self.sync.backfill_days);
        env_override("SQUELCH_POLL_SECS", &mut self.sync.poll_secs);
        env_override("SQUELCH_SQUELCH_LEVEL", &mut self.squelch_level);
        env_override(
            "SQUELCH_NOTIFY_MIN_IMPORTANCE",
            &mut self.notify.min_importance,
        );
        // ---- APNs pusher (blind relay) -------------------------------------
        // The relay token is never echoed anywhere.
        for (name, slot) in [
            ("SQUELCH_RELAY_URL", &mut self.pusher.relay_url),
            ("SQUELCH_RELAY_TOKEN", &mut self.pusher.relay_token),
            ("SQUELCH_RELAY_TOPIC", &mut self.pusher.topic),
            ("SQUELCH_RELAY_APNS_ENV", &mut self.pusher.environment),
            // ---- Outbound read tracking ------------------------------------
            // Same rule as the relay block: trimmed, and a blank value is
            // "unset" rather than "configured with nothing", so an exported-but-
            // empty var cannot mint pixels pointing at "/t/<token>".
            ("SQUELCH_TRACK_URL", &mut self.tracking.base_url),
            // ---- Prometheus scrape endpoint --------------------------------
            // Same rule again: an exported-but-empty var leaves the listener
            // closed rather than trying to bind an empty address.
            ("SQUELCH_METRICS_BIND", &mut self.metrics.bind),
        ] {
            if let Ok(v) = std::env::var(name) {
                let v = v.trim();
                if !v.is_empty() {
                    *slot = Some(v.to_string());
                }
            }
        }

        // Lenient enum parse (trim + lowercase, unknown value keeps the current
        // one), so not the strict `FromStr` helper.
        if let Ok(v) = std::env::var("SQUELCH_CRED_BACKEND")
            && let Some(b) = CredentialBackend::from_str_lenient(&v)
        {
            self.credential_backend = b;
        }
        // `var_os`, not `var`: a credentials path may be non-UTF-8, and an empty
        // value is honored as-is here rather than treated as unset.
        if let Some(p) = std::env::var_os("SQUELCH_CREDENTIALS_PATH") {
            self.credentials_path = Some(PathBuf::from(p));
        }

        // ---- Stage-2 overrides ---------------------------------------------
        // The API key itself is resolved lazily via env in
        // `Stage2Config::resolve_llm`; no need to copy it here.
        if let Ok(v) = std::env::var("SQUELCH_STAGE2_PROVIDER")
            && let Some(p) = Stage2Provider::from_str_lenient(&v)
        {
            self.stage2.stage2_provider = Some(p);
        }
        env_override_opt(
            "SQUELCH_ANTHROPIC_BASE_URL",
            &mut self.stage2.anthropic_base_url,
        );
        env_override("SQUELCH_MODEL", &mut self.stage2.model);
        env_override(
            "SQUELCH_STAGE2_MAX_BODY_CHARS",
            &mut self.stage2.max_body_chars,
        );
        env_override(
            "SQUELCH_STAGE2_BATCH_PER_CYCLE",
            &mut self.stage2.batch_per_cycle,
        );
        env_override(
            "SQUELCH_STAGE2_THREAD_DAILY_CAP",
            &mut self.stage2.thread_daily_cap,
        );
        env_override(
            "SQUELCH_STAGE2_GLOBAL_DAILY_CAP",
            &mut self.stage2.global_daily_cap,
        );
        env_override(
            "SQUELCH_STAGE2_SENDER_DAILY_CAP",
            &mut self.stage2.sender_daily_cap,
        );
        env_override("SQUELCH_STAGE2_MAX_AGE_DAYS", &mut self.stage2.max_age_days);
        env_override(
            "SQUELCH_STAGE2_PRICE_IN_PER_MTOK",
            &mut self.stage2.price_in_per_mtok,
        );
        env_override(
            "SQUELCH_STAGE2_PRICE_OUT_PER_MTOK",
            &mut self.stage2.price_out_per_mtok,
        );

        // ---- Stage-1 LLM overrides -----------------------------------------
        env_override("SQUELCH_STAGE1_MODEL", &mut self.stage1.model);
        env_override(
            "SQUELCH_STAGE1_MAX_BODY_CHARS",
            &mut self.stage1.max_body_chars,
        );
        env_override(
            "SQUELCH_STAGE1_BATCH_PER_CYCLE",
            &mut self.stage1.batch_per_cycle,
        );
        env_override(
            "SQUELCH_STAGE1_GLOBAL_DAILY_CAP",
            &mut self.stage1.global_daily_cap,
        );
        env_override(
            "SQUELCH_STAGE1_PRICE_IN_PER_MTOK",
            &mut self.stage1.price_in_per_mtok,
        );
        env_override(
            "SQUELCH_STAGE1_PRICE_OUT_PER_MTOK",
            &mut self.stage1.price_out_per_mtok,
        );

        // Range-guard the caps, matching POST /client/triage-config's
        // validation: a cap of 0 would silently block EVERY row each cycle
        // (`used >= cap` holds at 0). Clamps with a warning rather than
        // erroring, and runs last so it guards the TOML and env layers both.
        for (name, cap) in [
            ("stage2.thread_daily_cap", &mut self.stage2.thread_daily_cap),
            ("stage2.sender_daily_cap", &mut self.stage2.sender_daily_cap),
            ("stage2.global_daily_cap", &mut self.stage2.global_daily_cap),
            ("stage1.global_daily_cap", &mut self.stage1.global_daily_cap),
        ] {
            if !(STAGE2_CAP_MIN..=STAGE2_CAP_MAX).contains(cap) {
                let clamped = (*cap).clamp(STAGE2_CAP_MIN, STAGE2_CAP_MAX);
                eprintln!(
                    "squelch: config {name}={cap} is out of range \
                     ({STAGE2_CAP_MIN}..={STAGE2_CAP_MAX}); clamping to {clamped}"
                );
                *cap = clamped;
            }
        }
    }

    /// Resolve the credentials-file path for the `file` backend: the configured
    /// path if set, else `~/.config/squelch/credentials.json`.
    pub fn resolve_credentials_path(&self) -> PathBuf {
        if let Some(p) = &self.credentials_path {
            return p.clone();
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| h.join(".config").join("squelch").join("credentials.json"))
            .unwrap_or_else(|| PathBuf::from("credentials.json"))
    }

    /// Load config from an explicit path (if present), then apply env overrides.
    /// A missing file is fine — you can drive everything from the environment.
    pub fn load_from(path: &std::path::Path) -> Self {
        let mut cfg = Self::from_path(path).unwrap_or_default();
        cfg.apply_env_overrides();
        cfg
    }

    /// Fetch OAuth client credentials, erroring with a helpful message if the
    /// user hasn't set up their GCP client yet.
    pub fn oauth_client(&self) -> Result<OAuthClientConfig, CoreError> {
        let client_id = self.client_id.clone().filter(|s| !s.is_empty());
        let client_secret = self.client_secret.clone().filter(|s| !s.is_empty());
        match (client_id, client_secret) {
            (Some(client_id), Some(client_secret)) => Ok(OAuthClientConfig {
                client_id,
                client_secret,
            }),
            _ => Err(CoreError::Credential(format!(
                "missing OAuth client credentials. Create a Google Cloud \"Desktop app\" \
                 OAuth client (with the Gmail API enabled) and set client_id/client_secret in {} \
                 or via SQUELCH_CLIENT_ID / SQUELCH_CLIENT_SECRET.",
                Self::default_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.config/squelch/config.toml".to_string())
            ))),
        }
    }

    /// The configured account email, erroring helpfully if unset.
    pub fn require_account_email(&self) -> Result<String, CoreError> {
        self.account_email
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CoreError::Credential(
                    "no account_email configured (set account_email in config or \
                     SQUELCH_ACCOUNT_EMAIL)"
                        .to_string(),
                )
            })
    }
}

/// Mirror config-representable `.env` pairs into the TOML config at `path`, so a
/// repo-root `.env` reaches binaries launched from any CWD.
///
/// ONLY keys that are real [`Config`] fields are written — env-only secrets
/// (`SQUELCH_API_TOKEN`, `ANTHROPIC_API_KEY`, …) never land on disk here.
/// Unrelated keys are preserved, `.env` wins on conflicts, and an unparseable
/// file is refused rather than clobbered. `Ok(true)` if the file was rewritten.
pub fn mirror_env_pairs_to_config(
    pairs: &[(String, String)],
    path: &std::path::Path,
) -> std::io::Result<bool> {
    let get = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    // Canonical name wins over its legacy alias, same as apply_env_overrides.
    let get2 = |canonical: &str, legacy: &str| get(canonical).or_else(|| get(legacy));

    let mut mapped: Vec<(&str, toml::Value)> = Vec::new();
    if let Some(v) = get("SQUELCH_CLIENT_ID") {
        mapped.push(("client_id", toml::Value::String(v)));
    }
    if let Some(v) = get("SQUELCH_CLIENT_SECRET") {
        mapped.push(("client_secret", toml::Value::String(v)));
    }
    if let Some(v) = get2(ENV_ACCOUNT_EMAIL, ENV_ACCOUNT_EMAIL_LEGACY) {
        mapped.push(("account_email", toml::Value::String(v)));
    }
    if let Some(v) = get2(ENV_DB_PATH, ENV_DB_PATH_LEGACY) {
        mapped.push(("db_path", toml::Value::String(v)));
    }
    if let Some(b) =
        get("SQUELCH_CRED_BACKEND").and_then(|v| CredentialBackend::from_str_lenient(&v))
    {
        let s = match b {
            CredentialBackend::Keyring => "keyring",
            CredentialBackend::File => "file",
        };
        mapped.push(("credential_backend", toml::Value::String(s.to_string())));
    }
    if let Some(v) = get("SQUELCH_CREDENTIALS_PATH") {
        mapped.push(("credentials_path", toml::Value::String(v)));
    }
    if mapped.is_empty() {
        return Ok(false);
    }

    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let mut table: toml::Table = match &existing {
        Some(text) => text.parse().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("refusing to rewrite unparseable {}: {e}", path.display()),
            )
        })?,
        None => toml::Table::new(),
    };

    let mut changed = false;
    for (key, val) in mapped {
        if table.get(key) != Some(&val) {
            table.insert(key.to_string(), val);
            changed = true;
        }
    }
    if !changed {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let rendered = toml::to_string_pretty(&table)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    // Write-then-rename so a crash can't leave a half-written config we would
    // then refuse to touch. The tmp is CREATED 0600 — client_secret lives here
    // and must never exist world-readable, not even for the instant before a
    // chmod. Any failure removes the tmp file.
    let tmp = path.with_extension("toml.tmp");
    // A leftover tmp from a crashed prior run may carry old (possibly 0644)
    // permissions; mode(0o600) only applies at CREATE, so clear it first.
    let _ = std::fs::remove_file(&tmp);
    let write_tmp = || -> std::io::Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(rendered.as_bytes())?;
        f.sync_all()
    };
    if let Err(e) = write_tmp().and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that touch process-wide env must not run concurrently.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn sync_defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.sync.backfill_days, 30);
        assert_eq!(c.sync.poll_secs, 45);
        assert!(c.client_id.is_none());
    }

    #[test]
    fn mirror_env_pairs_creates_config_with_mapped_keys_only() {
        let dir = std::env::temp_dir().join(format!("squelch-mirror-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::remove_file(&path).ok();

        let pairs: Vec<(String, String)> = [
            ("SQUELCH_CLIENT_ID", "abc.apps.googleusercontent.com"),
            ("SQUELCH_CLIENT_SECRET", "sekret"),
            ("SQUELCH_ACCOUNT_EMAIL", "you@gmail.com"),
            ("SQUELCH_DB_PATH", "/tmp/squelch.db"),
            // env-only values must NOT be mirrored to disk
            ("SQUELCH_API_TOKEN", "supersecret"),
            ("ANTHROPIC_API_KEY", "sk-ant-xxx"),
            ("SQUELCH_MCP_ALLOWED_HOSTS", "box.tailnet.ts.net"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .into();

        assert!(mirror_env_pairs_to_config(&pairs, &path).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("supersecret"));
        assert!(!text.contains("sk-ant-xxx"));
        assert!(!text.contains("ts.net"));

        let c = Config::from_path(&path).unwrap();
        assert_eq!(
            c.client_id.as_deref(),
            Some("abc.apps.googleusercontent.com")
        );
        assert_eq!(c.client_secret.as_deref(), Some("sekret"));
        assert_eq!(c.account_email.as_deref(), Some("you@gmail.com"));
        assert_eq!(c.db_path, PathBuf::from("/tmp/squelch.db"));

        // Second mirror with identical pairs is a no-op.
        assert!(!mirror_env_pairs_to_config(&pairs, &path).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mirror_env_pairs_merges_and_preserves_unrelated_keys() {
        let dir = std::env::temp_dir().join(format!("squelch-mirror2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "account_email = \"old@gmail.com\"\nsquelch_level = 3\n\n[sync]\nbackfill_days = 90\n",
        )
        .unwrap();

        let pairs = vec![(
            "SQUELCH_ACCOUNT_EMAIL".to_string(),
            "new@gmail.com".to_string(),
        )];
        assert!(mirror_env_pairs_to_config(&pairs, &path).unwrap());

        let c = Config::from_path(&path).unwrap();
        assert_eq!(c.account_email.as_deref(), Some("new@gmail.com"));
        assert_eq!(c.squelch_level, 3);
        assert_eq!(c.sync.backfill_days, 90);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mirror_env_pairs_refuses_unparseable_config() {
        let dir = std::env::temp_dir().join(format!("squelch-mirror3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is [not toml").unwrap();

        let pairs = vec![("SQUELCH_ACCOUNT_EMAIL".to_string(), "x@y.com".to_string())];
        assert!(mirror_env_pairs_to_config(&pairs, &path).is_err());
        // The broken file is left exactly as it was.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is [not toml");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_from_toml() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("squelch-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
client_id = "abc.apps.googleusercontent.com"
client_secret = "sekret"
account_email = "you@gmail.com"
db_path = "/tmp/squelch.db"

[sync]
backfill_days = 90
"#,
        )
        .unwrap();
        let c = Config::load_from(&path);
        assert_eq!(
            c.client_id.as_deref(),
            Some("abc.apps.googleusercontent.com")
        );
        assert_eq!(c.account_email.as_deref(), Some("you@gmail.com"));
        assert_eq!(c.db_path, PathBuf::from("/tmp/squelch.db"));
        assert_eq!(c.sync.backfill_days, 90);
        // unspecified sync field falls back to default
        assert_eq!(c.sync.poll_secs, 45);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_default() {
        let _g = ENV_LOCK.lock().unwrap();
        // No env overrides in play => the canonical default path.
        unsafe {
            std::env::remove_var("SQUELCH_DB_PATH");
            std::env::remove_var("SQUELCH_DB");
        }
        let c = Config::load_from(std::path::Path::new("/nonexistent/squelch/config.toml"));
        assert_eq!(c.db_path, default_db_path());
    }

    #[test]
    fn db_path_precedence_canonical_over_legacy_over_default() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_DB_PATH");
            std::env::remove_var("SQUELCH_DB");
        }
        // Neither set => canonical default.
        assert_eq!(resolve_db_path(), default_db_path());

        // Legacy only => legacy value (with a deprecation note to stderr).
        unsafe {
            std::env::set_var("SQUELCH_DB", "/tmp/legacy.db");
        }
        assert_eq!(resolve_db_path(), PathBuf::from("/tmp/legacy.db"));

        // Canonical set => canonical WINS over legacy.
        unsafe {
            std::env::set_var("SQUELCH_DB_PATH", "/tmp/canonical.db");
        }
        assert_eq!(resolve_db_path(), PathBuf::from("/tmp/canonical.db"));

        unsafe {
            std::env::remove_var("SQUELCH_DB_PATH");
            std::env::remove_var("SQUELCH_DB");
        }
    }

    /// `SQUELCH_TRACK_URL` follows the relay block's rules exactly: it overrides
    /// the file, it is trimmed, and a blank value leaves tracking OFF rather than
    /// configured with nothing (which would mint pixels pointing at `/t/<token>`).
    #[test]
    fn track_url_env_override_matches_the_relay_block() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_TRACK_URL");
        }
        let mut c = Config::default();
        c.apply_env_overrides();
        assert_eq!(c.tracking.base_url, None, "unset => tracking is off");

        unsafe {
            std::env::set_var("SQUELCH_TRACK_URL", "   ");
        }
        let mut c = Config::default();
        c.tracking.base_url = Some("https://from-file.example".to_string());
        c.apply_env_overrides();
        assert_eq!(
            c.tracking.base_url.as_deref(),
            Some("https://from-file.example"),
            "a blank env value does not clobber the file"
        );

        unsafe {
            std::env::set_var("SQUELCH_TRACK_URL", "  https://track.example.com  ");
        }
        let mut c = Config::default();
        c.tracking.base_url = Some("https://from-file.example".to_string());
        c.apply_env_overrides();
        assert_eq!(
            c.tracking.base_url.as_deref(),
            Some("https://track.example.com")
        );

        unsafe {
            std::env::remove_var("SQUELCH_TRACK_URL");
        }
    }

    /// `SQUELCH_METRICS_BIND` rides the same block, and the blank case matters
    /// as much here: an empty value must leave the scrape listener closed, not
    /// try to bind "".
    #[test]
    fn metrics_bind_env_override_matches_the_relay_block() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_METRICS_BIND");
        }
        let mut c = Config::default();
        c.apply_env_overrides();
        assert_eq!(c.metrics.bind, None, "unset => no metrics listener");

        unsafe {
            std::env::set_var("SQUELCH_METRICS_BIND", "   ");
        }
        let mut c = Config::default();
        c.metrics.bind = Some("127.0.0.1:9848".to_string());
        c.apply_env_overrides();
        assert_eq!(
            c.metrics.bind.as_deref(),
            Some("127.0.0.1:9848"),
            "a blank env value does not clobber the file"
        );

        unsafe {
            std::env::set_var("SQUELCH_METRICS_BIND", "  0.0.0.0:9999  ");
        }
        let mut c = Config::default();
        c.metrics.bind = Some("127.0.0.1:9848".to_string());
        c.apply_env_overrides();
        assert_eq!(c.metrics.bind.as_deref(), Some("0.0.0.0:9999"));

        unsafe {
            std::env::remove_var("SQUELCH_METRICS_BIND");
        }
    }

    #[test]
    fn account_email_precedence_canonical_over_legacy_over_default() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_ACCOUNT_EMAIL");
            std::env::remove_var("SQUELCH_ACCOUNT");
        }
        assert_eq!(resolve_account_email("me@localhost"), "me@localhost");

        unsafe {
            std::env::set_var("SQUELCH_ACCOUNT", "legacy@x.com");
        }
        assert_eq!(resolve_account_email("me@localhost"), "legacy@x.com");

        unsafe {
            std::env::set_var("SQUELCH_ACCOUNT_EMAIL", "canon@x.com");
        }
        assert_eq!(resolve_account_email("me@localhost"), "canon@x.com");

        unsafe {
            std::env::remove_var("SQUELCH_ACCOUNT_EMAIL");
            std::env::remove_var("SQUELCH_ACCOUNT");
        }
    }

    #[test]
    fn legacy_db_env_flows_through_config() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_DB_PATH");
            std::env::set_var("SQUELCH_DB", "/tmp/legacy-cfg.db");
        }
        let mut c = Config::default();
        c.apply_env_overrides();
        assert_eq!(c.db_path, PathBuf::from("/tmp/legacy-cfg.db"));
        unsafe {
            std::env::remove_var("SQUELCH_DB");
        }
    }

    #[test]
    fn mcp_allowed_hosts_are_additive_to_loopback() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_MCP_ALLOWED_HOSTS");
        }
        let base = mcp_allowed_hosts();
        assert!(base.contains(&"localhost".to_string()));
        assert!(base.contains(&"127.0.0.1".to_string()));
        assert!(base.contains(&"::1".to_string()));

        unsafe {
            std::env::set_var(
                "SQUELCH_MCP_ALLOWED_HOSTS",
                " braelyns-mbp.tail15becf.ts.net , example.com:8080 ,",
            );
        }
        let hosts = mcp_allowed_hosts();
        // Loopback defaults preserved...
        assert!(hosts.contains(&"localhost".to_string()));
        assert!(hosts.contains(&"127.0.0.1".to_string()));
        // ...and the extras are added, trimmed, blanks dropped.
        assert!(hosts.contains(&"braelyns-mbp.tail15becf.ts.net".to_string()));
        assert!(hosts.contains(&"example.com:8080".to_string()));
        assert_eq!(hosts.len(), 5);
        unsafe {
            std::env::remove_var("SQUELCH_MCP_ALLOWED_HOSTS");
        }
    }

    #[test]
    fn env_overrides_file() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK so no other test reads env concurrently.
        unsafe {
            std::env::set_var("SQUELCH_CLIENT_ID", "env-id");
            std::env::set_var("SQUELCH_BACKFILL_DAYS", "7");
        }
        let mut c = Config {
            client_id: Some("file-id".to_string()),
            ..Config::default()
        };
        c.apply_env_overrides();
        assert_eq!(c.client_id.as_deref(), Some("env-id"));
        assert_eq!(c.sync.backfill_days, 7);
        unsafe {
            std::env::remove_var("SQUELCH_CLIENT_ID");
            std::env::remove_var("SQUELCH_BACKFILL_DAYS");
        }
    }

    #[test]
    fn stage2_defaults_are_sane() {
        let c = Stage2Config::default();
        assert_eq!(c.model, "claude-sonnet-5");
        assert_eq!(c.max_body_chars, 1500);
        assert_eq!(c.batch_per_cycle, 10);
        assert_eq!(c.thread_daily_cap, 3);
        assert_eq!(c.global_daily_cap, 200);
        assert_eq!(c.sender_daily_cap, 5);
        assert_eq!(c.max_age_days, 7);
        assert_eq!(c.price_in_per_mtok, 3.0);
        assert_eq!(c.price_out_per_mtok, 15.0);
    }

    #[test]
    fn stage1_llm_defaults_are_sane() {
        let c = Stage1Config::default();
        assert_eq!(c.model, "claude-haiku-4-5");
        assert_eq!(c.global_daily_cap, 1000);
        assert_eq!(c.batch_per_cycle, 10);
        assert_eq!(c.max_body_chars, 1500);
        assert_eq!(c.price_in_per_mtok, 1.0);
        assert_eq!(c.price_out_per_mtok, 5.0);
    }

    #[test]
    fn stage1_env_overrides_and_range_guard() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_STAGE1_MODEL", "claude-haiku-4-5-20251001");
            std::env::set_var("SQUELCH_STAGE1_GLOBAL_DAILY_CAP", "0"); // below min -> clamp to 1
            std::env::set_var("SQUELCH_STAGE1_PRICE_IN_PER_MTOK", "0.8");
            std::env::set_var("SQUELCH_STAGE1_BATCH_PER_CYCLE", "25");
        }
        let mut c = Config::default();
        c.apply_env_overrides();
        assert_eq!(c.stage1.model, "claude-haiku-4-5-20251001");
        assert_eq!(c.stage1.global_daily_cap, 1, "0 clamps to STAGE2_CAP_MIN");
        assert_eq!(c.stage1.price_in_per_mtok, 0.8);
        assert_eq!(c.stage1.batch_per_cycle, 25);
        unsafe {
            std::env::remove_var("SQUELCH_STAGE1_MODEL");
            std::env::remove_var("SQUELCH_STAGE1_GLOBAL_DAILY_CAP");
            std::env::remove_var("SQUELCH_STAGE1_PRICE_IN_PER_MTOK");
            std::env::remove_var("SQUELCH_STAGE1_BATCH_PER_CYCLE");
        }
    }

    #[test]
    fn stage1_cap_source_default_then_config() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_STAGE1_GLOBAL_DAILY_CAP");
        }
        let missing = std::path::Path::new("/nonexistent/squelch/config.toml");
        let (_, src) = Config::load_from_with_cap_sources(missing);
        assert_eq!(src.stage1_global_daily_cap, CapSource::Default);

        unsafe {
            std::env::set_var("SQUELCH_STAGE1_GLOBAL_DAILY_CAP", "500");
        }
        let (cfg, src) = Config::load_from_with_cap_sources(missing);
        assert_eq!(src.stage1_global_daily_cap, CapSource::Config);
        assert_eq!(cfg.stage1.global_daily_cap, 500);
        unsafe {
            std::env::remove_var("SQUELCH_STAGE1_GLOBAL_DAILY_CAP");
        }
    }

    /// Clear every Stage-2 key/provider/endpoint env var. Caller must hold
    /// `ENV_LOCK`.
    fn clear_stage2_env() {
        // SAFETY: caller holds ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_STAGE2_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("SQUELCH_STAGE2_PROVIDER");
            std::env::remove_var("SQUELCH_ANTHROPIC_BASE_URL");
        }
    }

    #[test]
    fn stage2_enabled_by_key_presence() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_stage2_env();
        let mut c = Stage2Config::default();
        assert!(!c.enabled(), "no key => disabled");
        // Config-file key enables.
        c.anthropic_api_key = Some("sk-config".into());
        assert!(c.enabled());
        assert_eq!(c.resolve_api_key().as_deref(), Some("sk-config"));
        // Env wins over config-file key.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "sk-env");
        }
        assert_eq!(c.resolve_api_key().as_deref(), Some("sk-env"));
        // Empty config-file key is treated as absent.
        c.anthropic_api_key = Some(String::new());
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        assert!(!c.enabled());
        clear_stage2_env();
    }

    #[test]
    fn stage2_provider_prefix_sniff_and_precedence() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_stage2_env();
        let c = Stage2Config::default();

        // Nothing set => disabled.
        assert!(c.resolve_key_and_provider().is_none());

        // 1. Explicit SQUELCH_STAGE2_API_KEY, sk-ant- prefix => Anthropic.
        unsafe {
            std::env::set_var("SQUELCH_STAGE2_API_KEY", "sk-ant-abc123");
        }
        assert_eq!(
            c.resolve_key_and_provider(),
            Some(("sk-ant-abc123".to_string(), Stage2Provider::Anthropic))
        );

        // Explicit var, non-anthropic prefix => OpenAI (sniff).
        unsafe {
            std::env::set_var("SQUELCH_STAGE2_API_KEY", "sk-proj-openai");
        }
        assert_eq!(
            c.resolve_key_and_provider(),
            Some(("sk-proj-openai".to_string(), Stage2Provider::OpenAI))
        );

        // Explicit var WINS over ANTHROPIC_API_KEY and OPENAI_API_KEY.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-other");
            std::env::set_var("OPENAI_API_KEY", "sk-openai-other");
        }
        assert_eq!(
            c.resolve_key_and_provider(),
            Some(("sk-proj-openai".to_string(), Stage2Provider::OpenAI))
        );

        // 2. Without the explicit var, ANTHROPIC_API_KEY wins over OPENAI_API_KEY.
        unsafe {
            std::env::remove_var("SQUELCH_STAGE2_API_KEY");
        }
        assert_eq!(
            c.resolve_key_and_provider(),
            Some(("sk-ant-other".to_string(), Stage2Provider::Anthropic))
        );

        // 3. OPENAI_API_KEY only => OpenAI.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        assert_eq!(
            c.resolve_key_and_provider(),
            Some(("sk-openai-other".to_string(), Stage2Provider::OpenAI))
        );

        // 4. Config-file key (no env) => Anthropic.
        clear_stage2_env();
        let c2 = Stage2Config {
            anthropic_api_key: Some("sk-config".into()),
            ..Stage2Config::default()
        };
        assert_eq!(
            c2.resolve_key_and_provider(),
            Some(("sk-config".to_string(), Stage2Provider::Anthropic))
        );

        clear_stage2_env();
    }

    #[test]
    fn stage2_provider_force_override() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_stage2_env();

        // An sk-ant- key would sniff Anthropic, but the config override forces
        // OpenAI (e.g. a proxy that accepts an anthropic-shaped key).
        unsafe {
            std::env::set_var("SQUELCH_STAGE2_API_KEY", "sk-ant-shaped");
        }
        let c = Stage2Config {
            stage2_provider: Some(Stage2Provider::OpenAI),
            ..Stage2Config::default()
        };
        assert_eq!(
            c.resolve_key_and_provider(),
            Some(("sk-ant-shaped".to_string(), Stage2Provider::OpenAI))
        );

        // And the reverse: OPENAI_API_KEY forced to Anthropic.
        clear_stage2_env();
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-openai-x");
        }
        let c2 = Stage2Config {
            stage2_provider: Some(Stage2Provider::Anthropic),
            ..Stage2Config::default()
        };
        assert_eq!(
            c2.resolve_key_and_provider(),
            Some(("sk-openai-x".to_string(), Stage2Provider::Anthropic))
        );

        clear_stage2_env();
    }

    #[test]
    fn stage2_provider_env_override_folds_into_config() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_stage2_env();
        unsafe {
            std::env::set_var("SQUELCH_STAGE2_PROVIDER", "openai");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.stage2.stage2_provider, Some(Stage2Provider::OpenAI));
        clear_stage2_env();
    }

    #[test]
    fn anthropic_base_url_env_override_folds_into_config() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_ANTHROPIC_BASE_URL", "https://gw.example.com");
        }
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(
            cfg.stage2.anthropic_base_url.as_deref(),
            Some("https://gw.example.com")
        );

        // Blank is "unset", never "set to empty": the config-file layer stays.
        unsafe {
            std::env::set_var("SQUELCH_ANTHROPIC_BASE_URL", "");
        }
        let mut cfg = Config {
            stage2: Stage2Config {
                anthropic_base_url: Some("https://file.example.com".to_string()),
                ..Stage2Config::default()
            },
            ..Config::default()
        };
        cfg.apply_env_overrides();
        assert_eq!(
            cfg.stage2.anthropic_base_url.as_deref(),
            Some("https://file.example.com")
        );
        unsafe {
            std::env::remove_var("SQUELCH_ANTHROPIC_BASE_URL");
        }
    }

    #[test]
    fn anthropic_base_url_rewrites_the_anthropic_endpoint() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_stage2_env();
        let with_base = |base: &str| Stage2Config {
            anthropic_api_key: Some("sk-config".into()),
            anthropic_base_url: Some(base.to_string()),
            ..Stage2Config::default()
        };

        let r = with_base("https://gw.example.com").resolve_llm().unwrap();
        assert_eq!(r.provider, Stage2Provider::Anthropic);
        assert_eq!(r.url, "https://gw.example.com/v1/messages");

        // A trailing slash never doubles.
        assert_eq!(
            with_base("https://gw.example.com/").resolve_llm().unwrap().url,
            "https://gw.example.com/v1/messages"
        );

        // Absent (and blank, the house convention) => production endpoint.
        let c = Stage2Config {
            anthropic_api_key: Some("sk-config".into()),
            ..Stage2Config::default()
        };
        assert_eq!(c.resolve_llm().unwrap().url, crate::triage::llm::API_URL);
        assert_eq!(
            with_base("").resolve_llm().unwrap().url,
            crate::triage::llm::API_URL
        );
        clear_stage2_env();
    }

    #[test]
    fn anthropic_base_url_requires_https_off_loopback() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_stage2_env();
        let with_base = |base: &str| Stage2Config {
            anthropic_api_key: Some("sk-config".into()),
            anthropic_base_url: Some(base.to_string()),
            ..Stage2Config::default()
        };

        // Cleartext off-box (or garbage): the override is treated as absent —
        // triage stays on the production endpoint rather than sending the key
        // over http. A query or fragment is rejected too: `/v1/messages` is
        // appended to the base, so a typo'd `?` or `#` would mangle the join.
        for bad in [
            "http://gw.example.com",
            "http://10.0.0.7:8080",
            "not a url",
            "https://gw.example.com?token=x",
            "https://gw.example.com/#anchor",
        ] {
            let r = with_base(bad).resolve_llm().unwrap();
            assert_eq!(r.url, crate::triage::llm::API_URL, "{bad} must be ignored");
        }

        // Loopback http is the dev/test carve-out.
        for host in ["127.0.0.1:8080", "localhost:8080", "[::1]:8080"] {
            let base = format!("http://{host}");
            assert_eq!(
                with_base(&base).resolve_llm().unwrap().url,
                format!("{base}/v1/messages")
            );
        }
        clear_stage2_env();
    }

    #[test]
    fn base_url_pins_a_gateway_shaped_key_to_the_anthropic_wire() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_stage2_env();
        // A Bifrost virtual key is sk-bf- shaped, so the prefix sniff alone
        // would put it on the OpenAI wire — but an operator who set the base
        // URL has already declared the wire is Anthropic.
        unsafe {
            std::env::set_var("SQUELCH_STAGE2_API_KEY", "sk-bf-virtual");
        }
        let c = Stage2Config {
            anthropic_base_url: Some("https://gw.example.com".to_string()),
            ..Stage2Config::default()
        };
        let r = c.resolve_llm().unwrap();
        assert_eq!(r.provider, Stage2Provider::Anthropic);
        assert_eq!(r.url, "https://gw.example.com/v1/messages");

        // A REJECTED override must not flip the wire either: still OpenAI.
        let c = Stage2Config {
            anthropic_base_url: Some("http://gw.example.com".to_string()),
            ..Stage2Config::default()
        };
        assert_eq!(c.resolve_llm().unwrap().provider, Stage2Provider::OpenAI);

        // The explicit provider override outranks everything, and the OpenAI
        // wire ignores the base URL entirely.
        let c = Stage2Config {
            stage2_provider: Some(Stage2Provider::OpenAI),
            anthropic_base_url: Some("https://gw.example.com".to_string()),
            ..Stage2Config::default()
        };
        let r = c.resolve_llm().unwrap();
        assert_eq!(r.provider, Stage2Provider::OpenAI);
        assert_eq!(r.url, crate::triage::llm::OPENAI_API_URL);
        clear_stage2_env();
    }

    #[test]
    fn stage2_provider_default_prices() {
        assert_eq!(Stage2Provider::Anthropic.default_prices(), (1.0, 5.0));
        assert_eq!(Stage2Provider::OpenAI.default_prices(), (0.15, 0.60));
    }

    #[test]
    fn stage2_env_overrides() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_MODEL", "claude-opus-4-8");
            std::env::set_var("SQUELCH_STAGE2_THREAD_DAILY_CAP", "7");
            std::env::set_var("SQUELCH_STAGE2_GLOBAL_DAILY_CAP", "500");
            std::env::set_var("SQUELCH_STAGE2_BATCH_PER_CYCLE", "25");
            std::env::set_var("SQUELCH_STAGE2_MAX_BODY_CHARS", "8000");
            std::env::set_var("SQUELCH_STAGE2_SENDER_DAILY_CAP", "9");
            std::env::set_var("SQUELCH_STAGE2_MAX_AGE_DAYS", "14");
            std::env::set_var("SQUELCH_STAGE2_PRICE_IN_PER_MTOK", "3.0");
            std::env::set_var("SQUELCH_STAGE2_PRICE_OUT_PER_MTOK", "15.0");
        }
        let mut c = Config::default();
        c.apply_env_overrides();
        assert_eq!(c.stage2.model, "claude-opus-4-8");
        assert_eq!(c.stage2.thread_daily_cap, 7);
        assert_eq!(c.stage2.global_daily_cap, 500);
        assert_eq!(c.stage2.batch_per_cycle, 25);
        assert_eq!(c.stage2.max_body_chars, 8000);
        assert_eq!(c.stage2.sender_daily_cap, 9);
        assert_eq!(c.stage2.max_age_days, 14);
        assert_eq!(c.stage2.price_in_per_mtok, 3.0);
        assert_eq!(c.stage2.price_out_per_mtok, 15.0);
        unsafe {
            std::env::remove_var("SQUELCH_MODEL");
            std::env::remove_var("SQUELCH_STAGE2_THREAD_DAILY_CAP");
            std::env::remove_var("SQUELCH_STAGE2_GLOBAL_DAILY_CAP");
            std::env::remove_var("SQUELCH_STAGE2_BATCH_PER_CYCLE");
            std::env::remove_var("SQUELCH_STAGE2_MAX_BODY_CHARS");
            std::env::remove_var("SQUELCH_STAGE2_SENDER_DAILY_CAP");
            std::env::remove_var("SQUELCH_STAGE2_MAX_AGE_DAYS");
            std::env::remove_var("SQUELCH_STAGE2_PRICE_IN_PER_MTOK");
            std::env::remove_var("SQUELCH_STAGE2_PRICE_OUT_PER_MTOK");
        }
    }

    /// An exported-but-blank var is "unset", never "set to empty": otherwise a
    /// stray `SQUELCH_MODEL=` in a shell would wipe the configured model. Same
    /// rule for an unparseable value — the config-file layer survives both.
    #[test]
    fn blank_or_unparseable_env_never_clobbers_a_configured_value() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_MODEL", "");
            std::env::set_var("SQUELCH_STAGE1_MODEL", "");
            std::env::set_var("SQUELCH_CLIENT_ID", "");
            std::env::set_var("SQUELCH_STAGE2_BATCH_PER_CYCLE", "not-a-number");
        }
        let mut c = Config {
            client_id: Some("file-id".to_string()),
            stage2: Stage2Config {
                model: "file-model".to_string(),
                batch_per_cycle: 42,
                ..Stage2Config::default()
            },
            stage1: Stage1Config {
                model: "file-stage1-model".to_string(),
                ..Stage1Config::default()
            },
            ..Config::default()
        };
        c.apply_env_overrides();
        assert_eq!(c.stage2.model, "file-model");
        assert_eq!(c.stage1.model, "file-stage1-model");
        assert_eq!(c.client_id.as_deref(), Some("file-id"));
        assert_eq!(c.stage2.batch_per_cycle, 42);
        unsafe {
            std::env::remove_var("SQUELCH_MODEL");
            std::env::remove_var("SQUELCH_STAGE1_MODEL");
            std::env::remove_var("SQUELCH_CLIENT_ID");
            std::env::remove_var("SQUELCH_STAGE2_BATCH_PER_CYCLE");
        }
    }

    #[test]
    fn notify_defaults_match_the_squelch_line_and_env_overrides_the_threshold() {
        let _g = ENV_LOCK.lock().unwrap();
        // The default threshold IS the TUI's starting squelch line (50), so
        // "notified" and "above the line" mean the same thing to a user.
        let c = Config::default();
        assert_eq!(c.notify.min_importance, 50);
        assert_eq!(c.notify.freshness_window_secs, 900);

        // SAFETY: guarded by ENV_LOCK.
        unsafe { std::env::set_var("SQUELCH_NOTIFY_MIN_IMPORTANCE", "85") }
        let mut c = Config::default();
        c.apply_env_overrides();
        assert_eq!(c.notify.min_importance, 85);
        // Unrelated: the generic SQUELCH_MIN_IMPORTANCE knob is the READ default,
        // not the notify threshold; they must not alias.
        assert_eq!(c.default_min_importance, 0);
        unsafe { std::env::remove_var("SQUELCH_NOTIFY_MIN_IMPORTANCE") }
    }

    /// The pusher is OFF unless an operator names a relay. `relay_url` is the
    /// whole feature flag, and env beats config exactly like everywhere else.
    #[test]
    fn pusher_is_absent_by_default_and_env_names_the_relay() {
        let _g = ENV_LOCK.lock().unwrap();
        let c = Config::default();
        assert_eq!(c.pusher, PusherConfig::default());
        assert!(c.pusher.relay_url.is_none(), "no relay => no pusher task");

        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_RELAY_URL", "https://relay.example.com");
            std::env::set_var("SQUELCH_RELAY_TOKEN", "s3cret");
            std::env::set_var("SQUELCH_RELAY_TOPIC", "dev.squelch.ios");
            std::env::set_var("SQUELCH_RELAY_APNS_ENV", "sandbox");
        }
        let mut c: Config = toml::from_str("[pusher]\nrelay_url = \"http://from-file\"\n").unwrap();
        assert_eq!(c.pusher.relay_url.as_deref(), Some("http://from-file"));
        c.apply_env_overrides();
        assert_eq!(
            c.pusher.relay_url.as_deref(),
            Some("https://relay.example.com")
        );
        assert_eq!(c.pusher.relay_token.as_deref(), Some("s3cret"));
        assert_eq!(c.pusher.topic.as_deref(), Some("dev.squelch.ios"));
        assert_eq!(c.pusher.environment.as_deref(), Some("sandbox"));

        // A blank env var is "unset", not "set to empty" — otherwise an exported
        // but empty SQUELCH_RELAY_URL would silently disable a configured relay.
        unsafe {
            std::env::set_var("SQUELCH_RELAY_URL", "   ");
            std::env::remove_var("SQUELCH_RELAY_TOKEN");
            std::env::remove_var("SQUELCH_RELAY_TOPIC");
            std::env::remove_var("SQUELCH_RELAY_APNS_ENV");
        }
        let mut c: Config = toml::from_str("[pusher]\nrelay_url = \"http://from-file\"\n").unwrap();
        c.apply_env_overrides();
        assert_eq!(c.pusher.relay_url.as_deref(), Some("http://from-file"));
        unsafe { std::env::remove_var("SQUELCH_RELAY_URL") }
    }

    #[test]
    fn notify_section_round_trips_through_toml() {
        let cfg: Config =
            toml::from_str("[notify]\nmin_importance = 30\nfreshness_window_secs = 60\n").unwrap();
        assert_eq!(cfg.notify.min_importance, 30);
        assert_eq!(cfg.notify.freshness_window_secs, 60);
        // A partial section keeps the other field's default (#[serde(default)]).
        let cfg: Config = toml::from_str("[notify]\nmin_importance = 30\n").unwrap();
        assert_eq!(cfg.notify.freshness_window_secs, 900);
        // A config that predates the feature has no [notify] table at all.
        let cfg: Config = toml::from_str("squelch_level = 1\n").unwrap();
        assert_eq!(cfg.notify.min_importance, 50);
    }

    #[test]
    fn stage2_cap_sources_track_toml_env_and_default() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_STAGE2_THREAD_DAILY_CAP");
            std::env::remove_var("SQUELCH_STAGE2_SENDER_DAILY_CAP");
            std::env::remove_var("SQUELCH_STAGE2_GLOBAL_DAILY_CAP");
        }
        let dir = std::env::temp_dir().join(format!("squelch-caps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[stage2]\nthread_daily_cap = 7\n").unwrap();

        // thread from TOML => Config; the untouched caps fall to Default.
        let (cfg, sources) = Config::load_from_with_cap_sources(&path);
        assert_eq!(cfg.stage2.thread_daily_cap, 7);
        assert_eq!(sources.thread_daily_cap, CapSource::Config);
        assert_eq!(sources.sender_daily_cap, CapSource::Default);
        assert_eq!(sources.global_daily_cap, CapSource::Default);

        // Env promotes global to Config (and overrides the effective value).
        unsafe {
            std::env::set_var("SQUELCH_STAGE2_GLOBAL_DAILY_CAP", "500");
        }
        let (cfg, sources) = Config::load_from_with_cap_sources(&path);
        assert_eq!(cfg.stage2.global_daily_cap, 500);
        assert_eq!(sources.global_daily_cap, CapSource::Config);
        assert_eq!(sources.thread_daily_cap, CapSource::Config);
        assert_eq!(sources.sender_daily_cap, CapSource::Default);
        unsafe {
            std::env::remove_var("SQUELCH_STAGE2_GLOBAL_DAILY_CAP");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oauth_client_errors_when_missing() {
        let c = Config::default();
        assert!(c.oauth_client().is_err());
    }

    #[test]
    fn credential_backend_default_is_platform_appropriate() {
        let b = CredentialBackend::default();
        if cfg!(target_os = "macos") {
            assert_eq!(b, CredentialBackend::Keyring);
        } else {
            assert_eq!(b, CredentialBackend::File);
        }
    }

    #[test]
    fn credential_backend_parse() {
        assert_eq!(
            CredentialBackend::from_str_lenient("keyring"),
            Some(CredentialBackend::Keyring)
        );
        assert_eq!(
            CredentialBackend::from_str_lenient("  FILE "),
            Some(CredentialBackend::File)
        );
        assert_eq!(CredentialBackend::from_str_lenient("nonsense"), None);
    }

    #[test]
    fn env_selects_credential_backend() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_CRED_BACKEND", "file");
            std::env::set_var("SQUELCH_CREDENTIALS_PATH", "/tmp/squelch-test-creds.json");
        }
        let mut c = Config {
            credential_backend: CredentialBackend::Keyring,
            ..Config::default()
        };
        c.apply_env_overrides();
        assert_eq!(c.credential_backend, CredentialBackend::File);
        assert_eq!(
            c.resolve_credentials_path(),
            PathBuf::from("/tmp/squelch-test-creds.json")
        );
        unsafe {
            std::env::remove_var("SQUELCH_CRED_BACKEND");
            std::env::remove_var("SQUELCH_CREDENTIALS_PATH");
        }
    }

    #[test]
    fn credential_backend_from_toml() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("squelch-cfg-be-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
credential_backend = "file"
credentials_path = "/var/lib/squelch/creds.json"
"#,
        )
        .unwrap();
        // Ensure env doesn't clobber the file value under test.
        unsafe {
            std::env::remove_var("SQUELCH_CRED_BACKEND");
            std::env::remove_var("SQUELCH_CREDENTIALS_PATH");
        }
        let c = Config::load_from(&path);
        assert_eq!(c.credential_backend, CredentialBackend::File);
        assert_eq!(
            c.resolve_credentials_path(),
            PathBuf::from("/var/lib/squelch/creds.json")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
