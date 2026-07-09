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
        }
    }
}

/// Stage-2 LLM triage tunables. The Anthropic API pass runs ONLY over rows
/// Stage-1 left non-confident (`model_used IS NULL AND sensitivity='normal'`),
/// under a strict per-thread + per-account daily budget.
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
            model: "claude-haiku-4-5".to_string(),
            max_body_chars: 1500,
            batch_per_cycle: 10,
            thread_daily_cap: 3,
            global_daily_cap: 200,
            sender_daily_cap: 5,
            max_age_days: 7,
            price_in_per_mtok: 1.0,
            price_out_per_mtok: 5.0,
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
        assert_eq!(c.model, "claude-haiku-4-5");
        assert_eq!(c.max_body_chars, 1500);
        assert_eq!(c.batch_per_cycle, 10);
        assert_eq!(c.thread_daily_cap, 3);
        assert_eq!(c.global_daily_cap, 200);
        assert_eq!(c.sender_daily_cap, 5);
        assert_eq!(c.max_age_days, 7);
        assert_eq!(c.price_in_per_mtok, 1.0);
        assert_eq!(c.price_out_per_mtok, 5.0);
    }

    #[test]
    fn stage2_enabled_by_key_presence() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
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
