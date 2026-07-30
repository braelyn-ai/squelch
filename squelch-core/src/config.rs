//! Configuration. Everything tunable lives here, loaded from
//! `~/.config/squelch/config.toml` with env-var overrides. Nothing magic is
//! hardcoded: the Stage-1 triage importance ladder, thresholds, and paths are
//! all fields on [`Config`] with sane defaults so a missing config file still
//! yields a working system.

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
/// Env var listing extra hostnames (comma-separated) the agent door's MCP
/// Streamable HTTP DNS-rebinding guard should accept, additive to the loopback
/// defaults. Needed when a reverse proxy (`tailscale serve`) rewrites `Host`.
pub const ENV_MCP_ALLOWED_HOSTS: &str = "SQUELCH_MCP_ALLOWED_HOSTS";

/// The single, canonical default SQLite path: `~/.local/share/squelch/squelch.db`
/// (XDG data dir). Every binary resolves to THIS when no path is configured, so
/// the MCP server, the TUI, `squelchd`, and the API all agree on one db file.
///
/// Creates the parent directory best-effort. Falls back to a CWD-relative
/// `squelch.db` only when `HOME` is unset (unusual).
pub fn default_db_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let dir = PathBuf::from(home).join(".local/share/squelch");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("squelch.db");
    }
    PathBuf::from("squelch.db")
}

/// Read a canonical env var, falling back to a legacy alias. When only the
/// legacy name is set, emit a one-line deprecation note to stderr (no values are
/// logged) and return its value. Returns `None` if neither is set/non-empty.
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

/// Resolve the SQLite path used by ALL binaries, in one place.
///
/// Precedence: canonical `SQUELCH_DB_PATH` > legacy `SQUELCH_DB` (deprecation
/// note) > [`default_db_path`]. This is the single source of truth; bins call it
/// so they can never drift.
pub fn resolve_db_path() -> PathBuf {
    env_with_legacy(ENV_DB_PATH, ENV_DB_PATH_LEGACY)
        .map(PathBuf::from)
        .unwrap_or_else(default_db_path)
}

/// Resolve the account email used by ALL binaries, in one place.
///
/// Precedence: canonical `SQUELCH_ACCOUNT_EMAIL` > legacy `SQUELCH_ACCOUNT`
/// (deprecation note) > the provided `default_email`.
pub fn resolve_account_email(default_email: &str) -> String {
    env_with_legacy(ENV_ACCOUNT_EMAIL, ENV_ACCOUNT_EMAIL_LEGACY)
        .unwrap_or_else(|| default_email.to_string())
}

/// The MCP agent-door DNS-rebinding allow-list: the loopback defaults rmcp ships
/// with (`localhost`, `127.0.0.1`, `::1`) PLUS any comma-separated hostnames in
/// `SQUELCH_MCP_ALLOWED_HOSTS`. Additive by design — we never drop the loopback
/// entries — so fronting the door with `tailscale serve` (which rewrites `Host`
/// to `*.ts.net`) stops returning 403 without opening the guard entirely.
///
/// Entries may be bare hosts or `host:port` authorities (rmcp matches either).
/// Blank entries are ignored.
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

/// The READ scope. This is all the sync daemon + triage ever request; the read
/// credential is `gmail.readonly` and nothing else. Hard invariant, hence a
/// `const`. See [`WRITE_SCOPES`] for the separate, opt-in action credential.
pub const GMAIL_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";

/// The WRITE scopes, requested ONLY by `squelchd auth --write` and loaded ONLY
/// by human-door action endpoints — never by sync/triage. `gmail.modify` covers
/// label/read-state/archive mutations; `gmail.send` covers sending. Kept as a
/// distinct grep-obvious constant from [`GMAIL_READONLY_SCOPE`] so the two
/// credentials can never be conflated.
pub const GMAIL_MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
pub const GMAIL_SEND_SCOPE: &str = "https://www.googleapis.com/auth/gmail.send";

/// Convenience: the full set of scopes for the write credential.
pub const WRITE_SCOPES: &[&str] = &[GMAIL_MODIFY_SCOPE, GMAIL_SEND_SCOPE];

/// Which backend persists OAuth tokens.
///
/// `Keyring` uses the OS secret service (macOS Keychain, Linux Secret Service).
/// `File` writes a mode-0600 JSON file — the only viable option on a headless
/// Linux box with no desktop keyring. Default is [`CredentialBackend::default`]:
/// keyring on macOS, file on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialBackend {
    Keyring,
    File,
}

impl Default for CredentialBackend {
    fn default() -> Self {
        // Headless Linux typically has no Secret Service; default to a file.
        // macOS always has Keychain.
        if cfg!(target_os = "macos") {
            CredentialBackend::Keyring
        } else {
            CredentialBackend::File
        }
    }
}

impl CredentialBackend {
    /// Parse from the `credential_backend` config / `SQUELCH_CRED_BACKEND` env
    /// string. Case-insensitive. Unknown values fall back to the platform
    /// default.
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "keyring" => Some(CredentialBackend::Keyring),
            "file" => Some(CredentialBackend::File),
            _ => None,
        }
    }
}

/// Which LLM provider Stage-2 talks to. Selected by KEY PREFIX at resolution
/// time (see [`Stage2Config::resolve_key_and_provider`]) unless forced via the
/// `stage2_provider` config field / `SQUELCH_STAGE2_PROVIDER` env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage2Provider {
    Anthropic,
    OpenAI,
}

impl Stage2Provider {
    /// Parse from the `stage2_provider` config / `SQUELCH_STAGE2_PROVIDER` env
    /// string. Case-insensitive. Unknown values return `None` (caller falls back
    /// to prefix sniffing).
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

    /// Per-provider default cost-ledger prices (USD per MTok input, output).
    /// Anthropic: claude-haiku-4-5 (1.0 / 5.0). OpenAI: gpt-4o-mini (0.15 / 0.60)
    /// — change with the model.
    pub fn default_prices(self) -> (f64, f64) {
        match self {
            Stage2Provider::Anthropic => (1.0, 5.0),
            Stage2Provider::OpenAI => (0.15, 0.60),
        }
    }
}

/// Sync-related tunables. Real config, not constants, so the sync engine can
/// wire them in without a schema change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// How many days of history to backfill on the initial sync.
    pub backfill_days: u32,
    /// How often (seconds) the incremental poll loop wakes to call
    /// `history.list`. A poll batch IS the coalesced batch — polling replaces
    /// the old IDLE wake-coalescing entirely.
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

/// Notification-event tunables. The sync engine writes a row to the `events`
/// table at each triage verdict that is worth interrupting the user for; these
/// two numbers are the whole emission policy that is not structural.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// Importance at or above which a message earns an event on the strength of
    /// its score alone (past_due/deadline tiers and detected deadlines bypass it
    /// entirely, exactly as they bypass the squelch line). Default 50 — the same
    /// number the TUI starts its in-session squelch line at, so "notified" and
    /// "above the line" mean the same thing. Env:
    /// `SQUELCH_NOTIFY_MIN_IMPORTANCE`.
    pub min_importance: u8,
    /// THE STORM GUARD. Mail received longer than this many seconds ago can
    /// never produce an event, whatever its verdict says. Without it, the
    /// Stage-1/Stage-2 passes chewing through a fresh install's backfilled
    /// backlog — or `catch_up()` re-scanning the whole backfill window after an
    /// expired history cursor — would fire hundreds of notifications at once.
    /// This is what implements "never on initial backfill" ROBUSTLY, across
    /// restarts and re-syncs, rather than by trusting a code path to know which
    /// pass it is on. Mail dated in the FUTURE is out of the window too (a
    /// sender-controlled `Date:` header must not be able to buy freshness — see
    /// [`crate::triage::events::is_fresh`]). Default 900 (15 minutes).
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

/// APNs PUSHER config: where the blind relay lives and how to authenticate to
/// it. See [`crate::push`] for the task itself.
///
/// `relay_url` IS THE FEATURE FLAG. Absent (the default), the daemon never spawns
/// the pusher and never opens a socket toward anyone — iOS push is strictly
/// opt-in, and a squelch install that has not been told about a relay is
/// structurally incapable of talking to one.
///
/// PRIVACY: the relay is BLIND on purpose. Nothing here configures content,
/// because no content is ever sent — the push carries an event id and a collapse
/// id and nothing else. `topic`/`environment` are pass-throughs for operators
/// running a relay with more than one bundle id or a sandbox build.
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

/// The default embedding-weights cache dir: `~/.local/share/squelch/models`
/// (a sibling of the sqlite db under the XDG data dir). Falls back to a
/// CWD-relative `squelch-models` only when `HOME` is unset.
pub fn default_embed_cache_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local/share/squelch")
            .join("models");
    }
    PathBuf::from("squelch-models")
}

/// On-box semantic-recall (v1) tunables. Embeddings run locally via fastembed
/// (ONNX, CPU); weights download ONCE to `cache_dir` on first run. `model` and
/// `dims` MUST agree with each other and with the `message_vecs` vec0 table's
/// `float[N]` declaration in `store/schema.sql` — the store asserts this at open
/// time. Schema applies fresh; changing `dims` means resetting the dev db.
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
    /// Importance for a bill-shaped message from an UNKNOWN sender. Deliberately
    /// moderate: it should surface for a Stage-2 look, not scream. See bug #3
    /// (scam "past-due" from an unknown sender must never land CONFIDENT PastDue).
    pub bill_unknown_sender_importance: u8,
    /// Sanity dampener: an extracted bill amount strictly greater than this
    /// (dollars) is treated as absurd and shaves confidence (never raises tier).
    /// Default $50,000 — a real household bill essentially never exceeds this.
    pub bill_absurd_amount_threshold: f64,

    // ---- Stage-1 LLM pass (the SMALL model run on every non-rule email) ----
    //
    // The heuristic fields above are the SEED / FALLBACK; these tune the LLM
    // refine pass. The Stage-1 pass reuses Stage-2's key/provider resolution
    // ([`Stage2Config::resolve_key_and_provider`]) — only the model, prices,
    // batch size, and (global-only) daily cap are Stage-1-specific.
    /// The Stage-1 model id string. Default `claude-haiku-4-5` (a small, cheap
    /// model — it sees nearly every email). Env: `SQUELCH_STAGE1_MODEL`.
    pub model: String,
    /// Cap on the flattened email body (chars) fed into the UNTRUSTED block.
    /// Env: `SQUELCH_STAGE1_MAX_BODY_CHARS`.
    pub max_body_chars: usize,
    /// How many queued rows to refine per sync cycle. Env:
    /// `SQUELCH_STAGE1_BATCH_PER_CYCLE`.
    pub batch_per_cycle: usize,
    /// GLOBAL-per-account-per-day Stage-1 API-call cap. Stage-1 needs ONLY a
    /// global cap — it must see every email, so per-thread/sender caps make no
    /// sense here. Default 1000. Env: `SQUELCH_STAGE1_GLOBAL_DAILY_CAP`.
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

/// Stage-2 LLM triage tunables. The Anthropic API pass runs ONLY over rows
/// Stage-1 refined but left non-confident. The queue predicate is the four
/// clauses: `stage1_model_used IS NOT NULL AND needs_stage2=1 AND model_used IS
/// NULL AND sensitivity='normal'` (Stage-1 has looked, escalation is flagged,
/// Stage-2 hasn't processed it yet, and it is non-sealed). Runs under a strict
/// per-thread + per-sender + per-account daily budget.
///
/// Stage-2 is ENABLED BY KEY PRESENCE: it turns on only when an API key is
/// resolvable ([`Stage2Config::api_key`] / `ANTHROPIC_API_KEY`). The `model`,
/// caps, and budgets are all config so an operator can retune without a
/// recompile. Env overrides follow the existing naming (`SQUELCH_MODEL`,
/// `SQUELCH_STAGE2_*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Stage2Config {
    /// Anthropic API key. Resolved from the config file's `anthropic_api_key`
    /// or the standard `ANTHROPIC_API_KEY` env var (env wins). When absent,
    /// Stage-2 is DISABLED gracefully (one stderr notice; rows stay queued).
    /// Never logged.
    pub anthropic_api_key: Option<String>,
    /// Force the Stage-2 provider, overriding key-prefix sniffing. `anthropic`
    /// or `openai`. When `None` (default), the provider is inferred from which
    /// key is resolved and its prefix. Env: `SQUELCH_STAGE2_PROVIDER`.
    pub stage2_provider: Option<Stage2Provider>,
    /// The model id string. Default `claude-haiku-4-5` (Anthropic). For OpenAI,
    /// set this to an OpenAI model such as `gpt-4o-mini` (config-driven so the
    /// provider can change without code). Written verbatim into the `model`
    /// request field and stored as `model_used` on applied rows.
    pub model: String,
    /// Cap on the flattened email body (chars) fed into the UNTRUSTED block.
    /// The body is truncated to this and the truncation is noted in-band.
    pub max_body_chars: usize,
    /// How many queued rows to process per sync cycle (fetch cap).
    pub batch_per_cycle: usize,
    /// Per-thread-per-day API-call cap (the circuit breaker). Incremented
    /// BEFORE the call so retry storms can't exceed it.
    pub thread_daily_cap: u32,
    /// NEW global-per-account-per-day API-call cap. Same increment-before
    /// discipline, counted via a `thread_id='__global__'` sentinel row in
    /// `wake_budget`.
    pub global_daily_cap: u32,
    /// Per-SENDER-per-day API-call cap. Same increment-before discipline as the
    /// thread/global caps, counted via a `thread_id='sender:<addr>'` sentinel
    /// row in `wake_budget` (no real Gmail thread id starts with `sender:`).
    /// Stops one chatty sender fanning many DIFFERENT threads from burning the
    /// budget. Env: `SQUELCH_STAGE2_SENDER_DAILY_CAP`.
    pub sender_daily_cap: u32,
    /// Skip (don't spend a model call on) any queued row whose message
    /// `received_at` is older than this many days: it is marked processed with
    /// `model_used='stale-skip'`, keeping its Stage-1 values, so it neither
    /// consumes budget nor sits queued forever. Env: `SQUELCH_STAGE2_MAX_AGE_DAYS`.
    pub max_age_days: u32,
    /// Per-million-input-token price (USD) for the configured model, used only to
    /// compute the `est_cost_usd_today` figure surfaced by `/client/stats`.
    /// Default 1.0 matches claude-haiku-4-5 (Anthropic); the OpenAI default is
    /// 0.15 (gpt-4o-mini). NOTE: change-with-model — if you change `model` or
    /// provider, update this and `price_out_per_mtok` to that model's pricing.
    /// Env: `SQUELCH_STAGE2_PRICE_IN_PER_MTOK`.
    pub price_in_per_mtok: f64,
    /// Per-million-output-token price (USD) for the configured model. Default 5.0
    /// matches claude-haiku-4-5 (Anthropic); the OpenAI default is 0.60
    /// (gpt-4o-mini). Change-with-model. See [`Stage2Config::price_in_per_mtok`].
    /// Env: `SQUELCH_STAGE2_PRICE_OUT_PER_MTOK`.
    pub price_out_per_mtok: f64,
}

impl Default for Stage2Config {
    fn default() -> Self {
        Self {
            anthropic_api_key: None,
            stage2_provider: None,
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

impl Stage2Config {
    /// Resolve the Stage-2 API key AND its provider.
    ///
    /// Resolution order (first match wins):
    ///   1. `SQUELCH_STAGE2_API_KEY` — explicit, provider SNIFFED from the key
    ///      prefix: `sk-ant-` => Anthropic, otherwise OpenAI.
    ///   2. `ANTHROPIC_API_KEY` — provider = Anthropic.
    ///   3. `OPENAI_API_KEY` — provider = OpenAI.
    ///   4. config-file `anthropic_api_key` — provider = Anthropic.
    ///
    /// The `stage2_provider` config field / `SQUELCH_STAGE2_PROVIDER` env var
    /// (already folded into `stage2_provider` by `apply_env_overrides`) FORCE-
    /// OVERRIDES the inferred provider when set. Empty strings are treated as
    /// absent. Key material is never logged by callers.
    pub fn resolve_key_and_provider(&self) -> Option<(String, Stage2Provider)> {
        let (key, inferred) = if let Some(key) = env_nonempty("SQUELCH_STAGE2_API_KEY") {
            // Explicit var: sniff the provider from the prefix.
            let provider = if key.starts_with("sk-ant-") {
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
        Some((key, provider))
    }

    /// Resolve just the API key (provider-agnostic). Retained for callers that
    /// only need presence/the key string. See [`resolve_key_and_provider`].
    pub fn resolve_api_key(&self) -> Option<String> {
        self.resolve_key_and_provider().map(|(k, _)| k)
    }

    /// Stage-2 is enabled iff an API key is resolvable.
    pub fn enabled(&self) -> bool {
        self.resolve_key_and_provider().is_some()
    }
}

/// Read an env var, returning `None` when unset or empty.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

// ---- Stage-2 daily-cap runtime-override plumbing ---------------------------
//
// The three Stage-2 daily caps are configurable at THREE layers, highest wins:
//   1. runtime OVERRIDE — an `app_settings` row (key below), set by the human
//      door's POST /client/triage-config. Applied without a restart.
//   2. config/env — the TOML `[stage2]` key OR its `SQUELCH_STAGE2_*` env var.
//   3. built-in default — [`Stage2Config::default`].
// These constants are the shared `app_settings.key` names so the store (writer),
// the sync pass (reader), and the API (reader/writer) never drift.

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

/// Which layer supplied a Stage-2 daily cap, reported by the human door's
/// triage-config endpoint. `Config` covers BOTH the TOML file and env overrides
/// (indistinguishable to the client and both mean "operator-set"); `Default`
/// means the built-in default was used. The runtime `app_settings` OVERRIDE
/// layer is reported separately by the API (as "override" when a row exists), so
/// it is not represented here.
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

/// The config/env-layer source of each Stage-2 daily cap. Computed at config
/// load and threaded to the human door so it can report "default" vs "config"
/// (the "override" case is decided at read time from `app_settings`).
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
    let in_env = std::env::var(env_var).map(|v| !v.is_empty()).unwrap_or(false);
    if in_toml || in_env {
        CapSource::Config
    } else {
        CapSource::Default
    }
}

/// Compute the config/env-layer [`Stage2CapSources`] for a (possibly absent)
/// config file path, consulting both the TOML `[stage2]` keys and the
/// `SQUELCH_STAGE2_*` env vars. A missing/unparseable file contributes no TOML
/// keys (env may still promote a cap to `Config`).
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            account_email: None,
            // The single canonical default, shared with every other binary (see
            // `default_db_path`). NOT a CWD-relative "squelch.db".
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

    /// Like [`Config::load`], but ALSO returns the config/env-layer
    /// [`Stage2CapSources`] (whether each Stage-2 daily cap came from the
    /// default or from config/env). Wire the sources into the human door so it
    /// can report "default" vs "config" on `/client/triage-config`.
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

    /// Like [`Config::load_from`], but ALSO returns the config/env-layer
    /// [`Stage2CapSources`]. See [`Config::load_with_cap_sources`].
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
        // Canonical SQUELCH_DB_PATH, with legacy SQUELCH_DB accepted (deprecated).
        if let Some(p) = env_with_legacy(ENV_DB_PATH, ENV_DB_PATH_LEGACY) {
            self.db_path = PathBuf::from(p);
        }
        if let Ok(v) = std::env::var("SQUELCH_MIN_IMPORTANCE")
            && let Ok(n) = v.parse::<u8>()
        {
            self.default_min_importance = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_CLIENT_ID")
            && !v.is_empty()
        {
            self.client_id = Some(v);
        }
        if let Ok(v) = std::env::var("SQUELCH_CLIENT_SECRET")
            && !v.is_empty()
        {
            self.client_secret = Some(v);
        }
        // Canonical SQUELCH_ACCOUNT_EMAIL, with legacy SQUELCH_ACCOUNT accepted
        // (deprecated).
        if let Some(v) = env_with_legacy(ENV_ACCOUNT_EMAIL, ENV_ACCOUNT_EMAIL_LEGACY) {
            self.account_email = Some(v);
        }
        if let Ok(v) = std::env::var("SQUELCH_BACKFILL_DAYS")
            && let Ok(n) = v.parse::<u32>()
        {
            self.sync.backfill_days = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_POLL_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            self.sync.poll_secs = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_SQUELCH_LEVEL")
            && let Ok(n) = v.parse::<u8>()
        {
            self.squelch_level = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_NOTIFY_MIN_IMPORTANCE")
            && let Ok(n) = v.parse::<u8>()
        {
            self.notify.min_importance = n;
        }
        // ---- APNs pusher (blind relay) -------------------------------------
        // `relay_url` is the on/off switch for the whole feature, so it is read
        // exactly like every other override and nothing derives from its absence
        // beyond "do not spawn the task". The TOKEN is never echoed anywhere.
        for (name, slot) in [
            ("SQUELCH_RELAY_URL", &mut self.pusher.relay_url),
            ("SQUELCH_RELAY_TOKEN", &mut self.pusher.relay_token),
            ("SQUELCH_RELAY_TOPIC", &mut self.pusher.topic),
            ("SQUELCH_RELAY_APNS_ENV", &mut self.pusher.environment),
        ] {
            if let Ok(v) = std::env::var(name) {
                let v = v.trim();
                if !v.is_empty() {
                    *slot = Some(v.to_string());
                }
            }
        }

        if let Ok(v) = std::env::var("SQUELCH_CRED_BACKEND")
            && let Some(b) = CredentialBackend::from_str_lenient(&v)
        {
            self.credential_backend = b;
        }
        if let Some(p) = std::env::var_os("SQUELCH_CREDENTIALS_PATH") {
            self.credentials_path = Some(PathBuf::from(p));
        }

        // ---- Stage-2 overrides ---------------------------------------------
        // The API key itself is resolved lazily via env in
        // `Stage2Config::resolve_key_and_provider`; no need to copy it here.
        if let Ok(v) = std::env::var("SQUELCH_STAGE2_PROVIDER")
            && let Some(p) = Stage2Provider::from_str_lenient(&v)
        {
            self.stage2.stage2_provider = Some(p);
        }
        if let Ok(v) = std::env::var("SQUELCH_MODEL")
            && !v.is_empty()
        {
            self.stage2.model = v;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE2_MAX_BODY_CHARS")
            && let Ok(n) = v.parse::<usize>()
        {
            self.stage2.max_body_chars = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE2_BATCH_PER_CYCLE")
            && let Ok(n) = v.parse::<usize>()
        {
            self.stage2.batch_per_cycle = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE2_THREAD_DAILY_CAP")
            && let Ok(n) = v.parse::<u32>()
        {
            self.stage2.thread_daily_cap = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE2_GLOBAL_DAILY_CAP")
            && let Ok(n) = v.parse::<u32>()
        {
            self.stage2.global_daily_cap = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE2_SENDER_DAILY_CAP")
            && let Ok(n) = v.parse::<u32>()
        {
            self.stage2.sender_daily_cap = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE2_MAX_AGE_DAYS")
            && let Ok(n) = v.parse::<u32>()
        {
            self.stage2.max_age_days = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE2_PRICE_IN_PER_MTOK")
            && let Ok(n) = v.parse::<f64>()
        {
            self.stage2.price_in_per_mtok = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE2_PRICE_OUT_PER_MTOK")
            && let Ok(n) = v.parse::<f64>()
        {
            self.stage2.price_out_per_mtok = n;
        }

        // ---- Stage-1 LLM overrides -----------------------------------------
        if let Ok(v) = std::env::var("SQUELCH_STAGE1_MODEL")
            && !v.is_empty()
        {
            self.stage1.model = v;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE1_MAX_BODY_CHARS")
            && let Ok(n) = v.parse::<usize>()
        {
            self.stage1.max_body_chars = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE1_BATCH_PER_CYCLE")
            && let Ok(n) = v.parse::<usize>()
        {
            self.stage1.batch_per_cycle = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE1_GLOBAL_DAILY_CAP")
            && let Ok(n) = v.parse::<u32>()
        {
            self.stage1.global_daily_cap = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE1_PRICE_IN_PER_MTOK")
            && let Ok(n) = v.parse::<f64>()
        {
            self.stage1.price_in_per_mtok = n;
        }
        if let Ok(v) = std::env::var("SQUELCH_STAGE1_PRICE_OUT_PER_MTOK")
            && let Ok(n) = v.parse::<f64>()
        {
            self.stage1.price_out_per_mtok = n;
        }

        // RANGE-GUARD the daily caps from config/env, matching the POST
        // /client/triage-config validation (1..=100000). Without this, a cap of
        // 0 from toml/env silently blocks EVERY stage-2 row each cycle (used >=
        // cap is always true at 0) with only a once-daily notice to show for
        // it. Out-of-range values clamp with a startup warning rather than
        // erroring — a misconfigured cap shouldn't take the daemon down. Runs
        // here (the tail of env application, which itself runs after TOML
        // parse) so it guards BOTH layers.
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
            .map(|h| {
                h.join(".config")
                    .join("squelch")
                    .join("credentials.json")
            })
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

/// Mirror config-representable keys from parsed `.env` pairs into the TOML
/// config file at `path`, merging with any existing content.
///
/// A repo-root `.env` only works when a binary is launched from that CWD;
/// mirroring it into `~/.config/squelch/config.toml` makes the same
/// account/paths visible to every binary (`squelch-tui`, `squelch-mcp`,
/// standalone `squelch-api`) regardless of CWD. Only keys that are actual
/// [`Config`] fields are written — env-only secrets (`SQUELCH_API_TOKEN`,
/// `ANTHROPIC_API_KEY`, …) never land on disk here. Existing unrelated keys in
/// the file are preserved; keys the `.env` defines win. Refuses to touch a file
/// it cannot parse (never clobbers a broken-but-hand-written config).
///
/// Returns `Ok(true)` if the file was (re)written, `Ok(false)` if there was
/// nothing to change.
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
    if let Some(b) = get("SQUELCH_CRED_BACKEND").and_then(|v| CredentialBackend::from_str_lenient(&v)) {
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
    // Write-then-rename so a crash mid-write can't leave a half-written config
    // (which we would then refuse to touch). The tmp file is CREATED 0600 —
    // client_secret lives in here, so it must never exist world-readable even
    // for the instant before a chmod. Any failure removes the tmp file.
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
        assert_eq!(c.client_id.as_deref(), Some("abc.apps.googleusercontent.com"));
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
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "this is [not toml"
        );
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

    /// Clear every Stage-2 key/provider env var. Caller must hold `ENV_LOCK`.
    fn clear_stage2_env() {
        // SAFETY: caller holds ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_STAGE2_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("SQUELCH_STAGE2_PROVIDER");
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
        assert_eq!(c.pusher.relay_url.as_deref(), Some("https://relay.example.com"));
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
