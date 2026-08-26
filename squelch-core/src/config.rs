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
/// Account every binary falls back to when neither env var is set.
pub const DEFAULT_ACCOUNT_EMAIL: &str = "me@localhost";
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

/// [`resolve_account_email`] against [`DEFAULT_ACCOUNT_EMAIL`]. The single
/// source of truth for every binary, so they cannot drift onto separate
/// accounts.
pub fn account_email() -> String {
    resolve_account_email(DEFAULT_ACCOUNT_EMAIL)
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
            poll_secs: 5,
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

/// Scheduled re-evaluation tunables. The serde-facing shape of
/// [`crate::triage::revisit::RevisitConfig`], in plain numbers rather than
/// `chrono::Duration`s, so it round-trips through TOML and env vars.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RevisitPassConfig {
    /// Master switch. Off means verdicts are never revisited and mail whose
    /// moment has passed stays where it was filed. Env: `SQUELCH_REVISIT_ENABLED`.
    pub enabled: bool,
    /// Re-evaluations attempted per sync cycle. Env:
    /// `SQUELCH_REVISIT_BATCH_PER_CYCLE`.
    pub batch_per_cycle: usize,
    /// Per-account-per-day cap on re-evaluation calls, counted in the same
    /// `wake_budget` ledger as the stages but on its OWN key — revisit spend is
    /// additional to [`Stage1Config::global_daily_cap`], not inside it, so this
    /// number is its own dollar ceiling at the Stage-1 per-call price.
    /// Env: `SQUELCH_REVISIT_DAILY_CAP`.
    pub daily_cap: u32,
    /// Revisits stored per message per pass. Env: `SQUELCH_REVISIT_MAX_PER_MESSAGE`.
    pub max_per_message: usize,
    /// Total re-evaluations one message may ever receive: the termination
    /// guarantee for a verdict that keeps asking to be looked at again. Env:
    /// `SQUELCH_REVISIT_MAX_LIFETIME`.
    pub max_per_message_lifetime: u32,
    /// Nearest a revisit may be scheduled (hours). Env: `SQUELCH_REVISIT_MIN_LEAD_HOURS`.
    pub min_lead_hours: i64,
    /// Furthest out a revisit may be scheduled (days); beyond this it is dropped
    /// as a hallucinated date. Env: `SQUELCH_REVISIT_MAX_HORIZON_DAYS`.
    pub max_horizon_days: i64,
    /// How long after a deadline to re-evaluate automatically (hours). Env:
    /// `SQUELCH_REVISIT_DEADLINE_GRACE_HOURS`.
    pub deadline_grace_hours: i64,
    /// Revisits closer together than this are one revisit (hours). Env:
    /// `SQUELCH_REVISIT_DEDUPE_HOURS`.
    pub dedupe_window_hours: i64,
    /// Days a row may sit in the standing band, untouched and with nothing
    /// scheduled, before the staleness sweep re-evaluates it anyway. 0 disables
    /// the sweep. Env: `SQUELCH_REVISIT_FYE_STALE_DAYS`.
    pub fye_stale_days: i64,
}

impl Default for RevisitPassConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            batch_per_cycle: 10,
            // ~$2/day at the Stage-1 per-call price. Re-evaluation is a trickle
            // by design — a handful of rows come due on any given day — so a cap
            // this size is a runaway guard, not a working budget.
            daily_cap: 50,
            max_per_message: 4,
            max_per_message_lifetime: 6,
            min_lead_hours: 1,
            max_horizon_days: 400,
            deadline_grace_hours: 24,
            dedupe_window_hours: 12,
            // 14 days: long enough that a real obligation still in play is not
            // churned, short enough that a row nobody has touched in a fortnight
            // gets questioned.
            fye_stale_days: 14,
        }
    }
}

impl RevisitPassConfig {
    /// The planner's view of these settings.
    ///
    /// Every duration goes through the fallible constructor. `Duration::days`
    /// PANICS out of range, and these numbers come from an operator's TOML or
    /// env: `SQUELCH_REVISIT_MAX_HORIZON_DAYS` with one too many digits would
    /// otherwise take down the sync task the first time a verdict scheduled
    /// anything. Every other bound in this module is defensive against a hostile
    /// model; a typo deserves the same treatment, and the default is a better
    /// answer than a crash.
    pub fn planner(&self) -> crate::triage::revisit::RevisitConfig {
        let d = crate::triage::revisit::RevisitConfig::default();
        crate::triage::revisit::RevisitConfig {
            max_per_message: self.max_per_message,
            min_lead: hours_or(self.min_lead_hours, d.min_lead),
            max_horizon: days_or(self.max_horizon_days, d.max_horizon),
            deadline_grace: hours_or(self.deadline_grace_hours, d.deadline_grace),
            dedupe_window: hours_or(self.dedupe_window_hours, d.dedupe_window),
            max_per_message_lifetime: self.max_per_message_lifetime,
            max_why_chars: d.max_why_chars,
        }
    }

    /// The staleness window as a duration, or `None` when the sweep is off.
    /// Clamped like the planner's knobs and for the same reason: the sync task
    /// subtracts this from `now`, and that subtraction panics out of range.
    pub fn fye_stale_window(&self) -> Option<chrono::Duration> {
        if self.fye_stale_days <= 0 {
            return None;
        }
        Some(days_or(
            self.fye_stale_days,
            chrono::Duration::days(Self::default().fye_stale_days),
        ))
    }
}

/// The furthest out any revisit knob may reach, in days. A decade is already
/// past the point where "look again then" means anything, and the ceiling is
/// what keeps `now + horizon` inside the range [`chrono::DateTime`] can hold —
/// that addition panics on overflow, and it runs on the sync task.
const REVISIT_MAX_DAYS: i64 = 3650;

/// `hours` as a [`chrono::Duration`], clamped to [`REVISIT_MAX_DAYS`] and
/// floored at zero; `fallback` if it somehow still does not fit.
fn hours_or(hours: i64, fallback: chrono::Duration) -> chrono::Duration {
    let bounded = hours.clamp(0, REVISIT_MAX_DAYS * 24);
    chrono::Duration::try_hours(bounded).unwrap_or(fallback)
}

/// `days` as a [`chrono::Duration`], clamped to [`REVISIT_MAX_DAYS`] and floored
/// at one; `fallback` if it somehow still does not fit.
fn days_or(days: i64, fallback: chrono::Duration) -> chrono::Duration {
    let bounded = days.clamp(1, REVISIT_MAX_DAYS);
    chrono::Duration::try_days(bounded).unwrap_or(fallback)
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

/// The tenant CONSOLE, the browser-facing half of the human door.
///
/// `sso_url` IS THE FEATURE FLAG for the Google sign-in button: absent (the
/// default, and the whole self-host posture), the console renders the
/// pasted-code form alone. It is a LINK TARGET and nothing more, so no trust
/// flows from it; whatever comes back is a pairing code the store adjudicates on
/// its own terms.
///
/// `allow_insecure_cookie` IS AN ESCAPE HATCH and is documented as one where it
/// is read (`squelch-api`'s console): it drops `Secure` from the session cookie
/// on a non-loopback origin, which means the cookie (a live device token) can
/// cross a network in the clear. It exists for the self-host who serves the
/// console over plain http on a LAN and cannot front it with TLS. Default off,
/// the login page says so out loud when it is on, and the daemon warns at
/// startup.
///
/// It is READ AS A STATEMENT ABOUT THE WHOLE ORIGIN, not as a cookie flag: the
/// console also builds its pairing deep link with `http://`, compares `Origin`
/// against that same `http://` origin for CSRF, and stops offering the SSO link.
/// The first two are the point (with the hatch shut, a plain-http LAN console
/// renders a login form and then refuses the POST from it), and the third is the
/// cost of turning it on somewhere that really is https. `Site::origin_is_https`
/// in `squelch-api`'s console is the one place all four are decided.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ConsoleConfig {
    /// The control plane's origin behind the console's "Continue with Google"
    /// link (e.g. `https://signup.passband.email`).
    /// Env: `SQUELCH_CONSOLE_SSO_URL`. `None` => no button.
    pub sso_url: Option<String>,
    /// Declare this console plain-http off loopback: session cookie WITHOUT
    /// `Secure`, `http://` deep link, `http://` CSRF origin, no SSO link.
    /// Env: `SQUELCH_CONSOLE_ALLOW_INSECURE_COOKIE`, spelled exactly `true`.
    /// Anything else (including `1` and `yes`) leaves it off: an operator who
    /// mistypes this one gets the safe answer.
    pub allow_insecure_cookie: bool,
}

/// BYOK carrier-API credentials, plus the cadence the poller keeps.
///
/// CREDENTIALS ARE THE FEATURE FLAG, one carrier at a time: a carrier whose
/// creds are absent — or only half there — is never polled, and when
/// [`CarriersConfig::any_enabled`] is false no carrier API is contacted at all.
/// Nothing in this table turns polling on by itself; the four knobs only pace a
/// poller that credentials have already enabled.
///
/// Every secret can come from the environment instead of the file
/// (`SQUELCH_UPS_CLIENT_ID`, …), and an env PAIR materializes a carrier the TOML
/// never mentions — that is how a container is configured. Secrets are never
/// logged: each cred struct hand-writes a `Debug` that redacts them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CarriersConfig {
    /// Baseline poll cadence (hours) for an in-flight shipment.
    /// Env: `SQUELCH_CARRIERS_POLL_INTERVAL_HOURS`.
    pub poll_interval_hours: u64,
    /// The tighter cadence (minutes) for a shipment that is OUT FOR DELIVERY,
    /// where the next interesting state change is an hour away, not a day.
    /// Env: `SQUELCH_CARRIERS_OFD_POLL_INTERVAL_MINS`.
    pub ofd_poll_interval_mins: u64,
    /// Give up on a shipment this many days after it was first seen: a tracking
    /// number nobody ever delivers must not be polled forever.
    /// Env: `SQUELCH_CARRIERS_MAX_AGE_DAYS`.
    pub max_age_days: u32,
    /// Consecutive per-shipment API failures tolerated before it is dropped.
    /// Env: `SQUELCH_CARRIERS_MAX_FAILURES`.
    pub max_failures: u32,
    /// Hide a shipment from BOTH DOORS' LISTINGS once nothing user-visible has
    /// changed about it for this many days. A LISTING concern, like
    /// [`CarriersConfig::max_failures`], which is why it lives here rather than
    /// in its own table: the same `[carriers]` block already decides when a row
    /// stops being shown.
    ///
    /// `0` DISABLES the filter entirely (nothing is ever hidden for age).
    /// Env: `SQUELCH_CARRIERS_STALE_AFTER_DAYS`.
    pub stale_after_days: u32,
    /// `[carriers.ups]`. `None` (or half a pair) => UPS is never polled.
    pub ups: Option<UpsCarrierConfig>,
    /// `[carriers.fedex]`. `None` (or half a pair) => FedEx is never polled.
    pub fedex: Option<FedexCarrierConfig>,
    /// `[carriers.usps]`. `None` (or half a pair) => USPS is never polled.
    pub usps: Option<UspsCarrierConfig>,
    /// `[carriers.dhl]`. `None` (or a blank key) => DHL is never polled.
    pub dhl: Option<DhlCarrierConfig>,
}

impl Default for CarriersConfig {
    fn default() -> Self {
        Self {
            poll_interval_hours: 6,
            ofd_poll_interval_mins: 60,
            max_age_days: 45,
            max_failures: 5,
            stale_after_days: 7,
            ups: None,
            fedex: None,
            usps: None,
            dhl: None,
        }
    }
}

impl CarriersConfig {
    /// `true` when at least one carrier is FULLY configured. This is the feature
    /// flag for the whole poller: false means no carrier API is ever contacted,
    /// so an operator who configured nothing gets no outbound traffic and no
    /// background task.
    pub fn any_enabled(&self) -> bool {
        self.ups.as_ref().is_some_and(UpsCarrierConfig::enabled)
            || self.fedex.as_ref().is_some_and(FedexCarrierConfig::enabled)
            || self.usps.as_ref().is_some_and(UspsCarrierConfig::enabled)
            || self.dhl.as_ref().is_some_and(DhlCarrierConfig::enabled)
    }

    /// The listing half of this block, as the value both doors carry.
    pub fn list_policy(&self) -> ShipmentListPolicy {
        ShipmentListPolicy {
            suppress_failed_ambiguous_at: self.max_failures,
            stale_after_days: self.stale_after_days,
        }
    }
}

/// Config-derived listing policy for shipments. Both doors hold one so the
/// agent door cannot drift from the human door's view (it did: the agent door
/// used to hardcode the built-in `max_failures` and ignore the operator's).
///
/// EVERY RULE IN HERE IS READ-SIDE. Nothing it hides is deleted, nothing it
/// hides stops being polled, and every hidden row comes back on its own the
/// moment something about it changes — see
/// [`Store::list_shipments`](crate::store::Store::list_shipments).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShipmentListPolicy {
    /// Permanent poll failures after which an AMBIGUOUS-shaped tracking number
    /// is treated as a phantom and hidden. From `[carriers] max_failures`.
    pub suppress_failed_ambiguous_at: u32,
    /// Days without a user-visible change after which a row is hidden as stale.
    /// `0` disables the staleness filter. From `[carriers] stale_after_days`.
    pub stale_after_days: u32,
}

impl Default for ShipmentListPolicy {
    fn default() -> Self {
        CarriersConfig::default().list_policy()
    }
}

impl From<&CarriersConfig> for ShipmentListPolicy {
    fn from(c: &CarriersConfig) -> Self {
        c.list_policy()
    }
}

/// One half of a credential, trimmed, or `None` when absent or blank. BLANK IS
/// ABSENT everywhere here, so a `client_id = ""` left behind in a config cannot
/// half-enable a carrier and send an empty string at someone's auth endpoint.
fn cred_half(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// UPS OAuth client credentials (the client-credentials grant). BOTH halves are
/// required; either one alone leaves UPS off.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpsCarrierConfig {
    /// Env: `SQUELCH_UPS_CLIENT_ID`.
    pub client_id: Option<String>,
    /// Env: `SQUELCH_UPS_CLIENT_SECRET`. Secret material, NEVER logged.
    pub client_secret: Option<String>,
}

impl UpsCarrierConfig {
    /// The complete pair, or `None` when either half is missing or blank.
    pub fn credentials(&self) -> Option<(&str, &str)> {
        Some((cred_half(&self.client_id)?, cred_half(&self.client_secret)?))
    }

    /// UPS is enabled iff a complete pair resolves.
    pub fn enabled(&self) -> bool {
        self.credentials().is_some()
    }
}

/// Hand-written so the secret half can never ride out through a stray `{:?}`.
impl std::fmt::Debug for UpsCarrierConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpsCarrierConfig")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// FedEx OAuth client credentials. Both halves required.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct FedexCarrierConfig {
    /// Env: `SQUELCH_FEDEX_CLIENT_ID`.
    pub client_id: Option<String>,
    /// Env: `SQUELCH_FEDEX_CLIENT_SECRET`. Secret material, NEVER logged.
    pub client_secret: Option<String>,
}

impl FedexCarrierConfig {
    /// The complete pair, or `None` when either half is missing or blank.
    pub fn credentials(&self) -> Option<(&str, &str)> {
        Some((cred_half(&self.client_id)?, cred_half(&self.client_secret)?))
    }

    /// FedEx is enabled iff a complete pair resolves.
    pub fn enabled(&self) -> bool {
        self.credentials().is_some()
    }
}

/// Hand-written so the secret half can never ride out through a stray `{:?}`.
impl std::fmt::Debug for FedexCarrierConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FedexCarrierConfig")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// USPS OAuth consumer credentials. Both halves required.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UspsCarrierConfig {
    /// Env: `SQUELCH_USPS_CONSUMER_KEY`.
    pub consumer_key: Option<String>,
    /// Env: `SQUELCH_USPS_CONSUMER_SECRET`. Secret material, NEVER logged.
    pub consumer_secret: Option<String>,
}

impl UspsCarrierConfig {
    /// The complete pair, or `None` when either half is missing or blank.
    pub fn credentials(&self) -> Option<(&str, &str)> {
        Some((
            cred_half(&self.consumer_key)?,
            cred_half(&self.consumer_secret)?,
        ))
    }

    /// USPS is enabled iff a complete pair resolves.
    pub fn enabled(&self) -> bool {
        self.credentials().is_some()
    }
}

/// Hand-written so the secret half can never ride out through a stray `{:?}`.
impl std::fmt::Debug for UspsCarrierConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UspsCarrierConfig")
            .field("consumer_key", &self.consumer_key)
            .field(
                "consumer_secret",
                &self.consumer_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// DHL API key, plus the one carrier-specific budget we keep. DHL's free tier is
/// 250 calls/day and the key is the WHOLE credential, so there is no pair here.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DhlCarrierConfig {
    /// Env: `SQUELCH_DHL_API_KEY`. Secret material, NEVER logged.
    pub api_key: Option<String>,
    /// Calls/day ceiling for DHL. Default 200, deliberately UNDER the 250/day
    /// free tier so a busy day cannot spend an operator into a bill.
    /// Env: `SQUELCH_DHL_DAILY_CAP` (honored only when DHL is already
    /// configured — a cap alone must never conjure a carrier).
    pub daily_cap: u32,
}

impl Default for DhlCarrierConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            daily_cap: 200,
        }
    }
}

impl DhlCarrierConfig {
    /// The key, or `None` when it is missing or blank.
    pub fn api_key(&self) -> Option<&str> {
        cred_half(&self.api_key)
    }

    /// DHL is enabled iff a key resolves.
    pub fn enabled(&self) -> bool {
        self.api_key().is_some()
    }
}

/// Hand-written so the key can never ride out through a stray `{:?}`.
impl std::fmt::Debug for DhlCarrierConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DhlCarrierConfig")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("daily_cap", &self.daily_cap)
            .finish()
    }
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
    /// Characters of `subject + body` fed to the embedder per message. Default
    /// 1000. A pair with `max_tokens`: keep this near tokens x 4 so we neither
    /// pad past what the model reads nor truncate twice at different places.
    pub max_chars: usize,
    /// Tokens the model reads per text (fastembed `max_length`). Default 256,
    /// not the model's 512 ceiling: attention scratch is quadratic in sequence
    /// length and a batch pads to its longest member, so on the fp32 model a
    /// batch-8 pass of 512-token texts adds +324 MB against +123 MB at 256
    /// (batch-1: +44 MB against +13 MB). The subject and first ~1000 characters
    /// are where recall lives; long newsletters lose their tails.
    pub max_tokens: usize,
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
            max_tokens: crate::embed::DEFAULT_EMBED_MAX_TOKENS,
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
            max_tokens: self.max_tokens,
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

    // ---- Stage-1 LLM pass (the model run on every non-sealed email) --------
    // The heuristic fields above are its WITNESS and its offline fallback, never
    // an alternative verdict: see [`crate::triage::router`]. Key, provider, and
    // endpoint come from [`Stage2Config::resolve_llm`]; only the fields below
    // are Stage-1's own.
    /// The Stage-1 model id string. Default `claude-opus-5`: triage is the
    /// product, so the pass that sees every email runs the best model there is,
    /// and the two stages differ by `effort` and context rather than by model.
    /// Env: `SQUELCH_STAGE1_MODEL`.
    pub model: String,
    /// Reasoning depth for Stage-1 (`low`/`medium`/`high`/`xhigh`/`max`), sent
    /// as `output_config.effort`. `low` is deliberate: Stage-1 reads a compact
    /// row and the router, not the model's own thinking, decides what deserves
    /// a harder look. Set to `None` when pointing `model` at one that rejects
    /// the field (Haiku 4.5, Sonnet 4.5). Env: `SQUELCH_STAGE1_EFFORT`.
    pub effort: Option<String>,
    /// How close to the surface threshold counts as a boundary row for
    /// [`crate::triage::router::EscalationReason::Boundary`]. Widening this
    /// escalates more mail near the line, which is where a scoring error flips
    /// visibility. Env: `SQUELCH_STAGE1_BOUNDARY_MARGIN`.
    pub escalation_boundary_margin: u8,
    /// Cap on the flattened email body (chars) fed into the UNTRUSTED block.
    /// Env: `SQUELCH_STAGE1_MAX_BODY_CHARS`.
    pub max_body_chars: usize,
    /// How many queued rows to refine per sync cycle. Env:
    /// `SQUELCH_STAGE1_BATCH_PER_CYCLE`.
    pub batch_per_cycle: usize,
    /// Global per-account-per-day call cap — the only cap Stage-1 has, since it
    /// must see every email, and SHARED with the specialist extractors.
    ///
    /// THIS IS A DOLLAR CEILING WEARING A COUNT. At the defaults below (opus-5 at
    /// $5/$25 per MTok, a 6000-char body, and thinking billing against
    /// [`crate::triage::llm::MAX_TOKENS`]) a Stage-1 call runs on the order of
    /// four cents, so this number times four cents is the most one account can
    /// spend here in a day. It was 1000 when the pass ran a small model with a
    /// 400-token ceiling and a 1500-char body; the model swap changed the price
    /// per call on four axes at once, and the cap had to come down with it.
    /// Re-derive it whenever `model`, `max_body_chars`, or the prices move.
    /// Env: `SQUELCH_STAGE1_GLOBAL_DAILY_CAP`.
    pub global_daily_cap: u32,
    /// Per-million-input-token price (USD) for the Stage-1 model. Default 5.0
    /// (claude-opus-5). Env: `SQUELCH_STAGE1_PRICE_IN_PER_MTOK`.
    pub price_in_per_mtok: f64,
    /// Per-million-output-token price (USD) for the Stage-1 model. Default 25.0
    /// (claude-opus-5). Env: `SQUELCH_STAGE1_PRICE_OUT_PER_MTOK`.
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
            model: "claude-opus-5".to_string(),
            effort: Some("low".to_string()),
            escalation_boundary_margin: 10,
            // 6000, not 1500: the old cap was sized for a 200K-context small
            // model and routinely cut the body before the part that decides the
            // verdict (the amount, the date, the ask). The model reading this
            // now has a 1M window.
            max_body_chars: 6000,
            batch_per_cycle: 10,
            // ~$20/day worst case at ~4c a call. Comfortably above what even a
            // heavy mailbox spends (Stage-1 sees every inbound message, and the
            // extractors share this counter), and low enough that a runaway
            // cannot quietly bill an order of magnitude more than the pass costs.
            global_daily_cap: 500,
            price_in_per_mtok: 5.0,
            price_out_per_mtok: 25.0,
        }
    }
}

/// Prompt-cache multipliers on the per-MTok INPUT price, for costing the
/// ledger's cache-token columns at read time. These are Anthropic's standard
/// 5-minute-ephemeral-cache rates (the TTL our `cache_control` blocks use):
/// cache writes bill at 1.25x the input price, cache reads at 0.1x.
pub const CACHE_WRITE_INPUT_MULT: f64 = 1.25;
pub const CACHE_READ_INPUT_MULT: f64 = 0.1;

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
    /// `model_used` on applied rows. Defaults to the SAME model as Stage-1:
    /// escalation buys more context and more thinking, not a bigger brain.
    pub model: String,
    /// Reasoning depth for Stage-2, sent as `output_config.effort`. `xhigh`
    /// against Stage-1's `low` is one of the two things escalation actually
    /// buys (the other is [`Stage2Queued`](crate::store::Stage2Queued)'s
    /// thread/sender/neighbour context). Env: `SQUELCH_STAGE2_EFFORT`.
    pub effort: Option<String>,
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
    /// `wake_budget`. Like Stage-1's, this is a dollar ceiling in disguise: an
    /// escalated call carries a 12000-char body plus thread and sender context
    /// and thinks at `xhigh`, so it runs around a dime. See
    /// [`Stage1Config::global_daily_cap`].
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
            // Stage-2 is the ESCALATION pass: same model as Stage-1, more
            // context and more thinking.
            model: "claude-opus-5".to_string(),
            effort: Some("xhigh".to_string()),
            // Roomier than Stage-1's: an escalated row is one where the detail
            // that settles it may be deep in the body.
            max_body_chars: 12_000,
            batch_per_cycle: 10,
            thread_daily_cap: 3,
            // ~$12/day worst case at ~10c a call; see the field doc.
            global_daily_cap: 120,
            sender_daily_cap: 5,
            max_age_days: 7,
            // claude-opus-5 per-MTok (input / output).
            price_in_per_mtok: 5.0,
            price_out_per_mtok: 25.0,
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

/// The hosted assistant relay's credential + endpoint, resolved by
/// [`Stage2Config::resolve_assistant`]. Like [`ResolvedLlm`], no `Debug` on
/// purpose: `api_key` is key material and must never reach a log line.
pub struct ResolvedAssistant {
    pub api_key: String,
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
    ///
    /// HOSTED LEGACY NOTE: a tenant pod rendered before the shared-key bridge
    /// was removed still carries a raw `ANTHROPIC_API_KEY` alongside the
    /// gateway base URL, so this resolves the raw key and every call 401s
    /// against the gateway (which accepts only virtual keys) until
    /// `squelch-control llm mint` re-applies the Deployment. The sync passes
    /// treat those 401s as config-level — rows stay queued — so the backlog
    /// survives until the re-apply; see deploy/hosted/PRODUCTION.md, "History".
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

    /// Resolve the hosted assistant relay's key + endpoint. Some ONLY when BOTH
    /// `SQUELCH_ASSISTANT_API_KEY` and a valid `anthropic_base_url` are present:
    /// an assistant virtual key only works at the gateway, and without a gateway
    /// there is nothing to relay to — self-host BYOK lives in the app, not here.
    /// The URL is `<base>/v1/messages`, exactly as [`Stage2Config::resolve_llm`]
    /// builds it, and the env var is read lazily at call time like every other
    /// key source. Empty strings count as absent, and key material is never
    /// logged.
    pub fn resolve_assistant(&self) -> Option<ResolvedAssistant> {
        let base = self.validated_base_url()?;
        let api_key = env_nonempty("SQUELCH_ASSISTANT_API_KEY")?;
        Some(ResolvedAssistant {
            api_key,
            url: format!("{}/v1/messages", base.trim_end_matches('/')),
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

/// [`env_override_opt`] for the reasoning-effort slots, where CLEARING the value
/// has to be expressible: an operator who repoints a stage at a model with no
/// effort support (Haiku 4.5, Sonnet 4.5) must be able to drop the field, since
/// sending it to such a model is a 400 on every call. `none`/`off` clear it;
/// anything else sets it verbatim (the API, not this parser, is the authority on
/// which level names are valid).
fn env_override_effort(name: &str, slot: &mut Option<String>) {
    if let Ok(v) = std::env::var(name) {
        let v = v.trim();
        if v.is_empty() {
            return;
        }
        *slot = match v.to_ascii_lowercase().as_str() {
            "none" | "off" => None,
            _ => Some(v.to_string()),
        };
    }
}

/// [`env_override_opt`] for a secret that lives INSIDE an optional credential
/// struct: a usable value materializes that struct — a container gets its
/// carriers entirely from the environment, with no `[carriers.ups]` table on
/// disk to attach to — and then writes the named half.
///
/// Trimmed, and blank is "unset", which is the relay block's rule: an
/// exported-but-empty var neither clobbers a configured secret nor conjures a
/// carrier out of nothing. Because a carrier needs BOTH halves to count, a lone
/// id still leaves it disabled.
fn env_override_cred<T: Default>(
    name: &str,
    holder: &mut Option<T>,
    half: impl FnOnce(&mut T) -> &mut Option<String>,
) {
    if let Ok(v) = std::env::var(name) {
        let v = v.trim();
        if !v.is_empty() {
            *half(holder.get_or_insert_default()) = Some(v.to_string());
        }
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
    /// Scheduled re-evaluation: how often triage looks at its own past verdicts.
    pub revisit: RevisitPassConfig,
    /// APNs pusher: the blind relay's URL + bearer. Absent `relay_url` means the
    /// task is never spawned.
    pub pusher: PusherConfig,
    /// Outbound read tracking. Absent `base_url` means no send is ever tracked.
    pub tracking: TrackingConfig,
    /// Prometheus scrape endpoint. Absent `bind` means the listener is never
    /// opened.
    pub metrics: MetricsConfig,
    /// The tenant console. Absent `sso_url` means no Google sign-in button,
    /// which is the self-host posture.
    pub console: ConsoleConfig,
    /// BYOK carrier APIs. No credentials anywhere means no carrier is ever
    /// polled, which is the default.
    pub carriers: CarriersConfig,
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
            revisit: RevisitPassConfig::default(),
            pusher: PusherConfig::default(),
            tracking: TrackingConfig::default(),
            metrics: MetricsConfig::default(),
            console: ConsoleConfig::default(),
            carriers: CarriersConfig::default(),
        }
    }
}

impl Config {
    /// The escalation router's tunables, assembled HERE and nowhere else so the
    /// surface threshold cannot fork: the router's idea of "the line" is the
    /// same [`NotifyConfig::min_importance`] that decides whether a row earns a
    /// notification, because a row on the boundary of being seen at all is
    /// exactly the row worth a second look.
    pub fn router(&self) -> crate::triage::router::RouterConfig {
        crate::triage::router::RouterConfig {
            surface_threshold: self.notify.min_importance,
            boundary_margin: self.stage1.escalation_boundary_margin,
        }
    }

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
            // ---- The tenant console ----------------------------------------
            // Same rule again: an exported-but-empty var leaves the Google
            // button off the page rather than rendering one that points at "".
            ("SQUELCH_CONSOLE_SSO_URL", &mut self.console.sso_url),
        ] {
            if let Ok(v) = std::env::var(name) {
                let v = v.trim();
                if !v.is_empty() {
                    *slot = Some(v.to_string());
                }
            }
        }
        // The console's plain-http escape hatch. A BOOL, so it takes the same
        // lenient parse the other flags do and an unreadable value leaves the
        // setting where it was, which is off.
        env_override(
            "SQUELCH_CONSOLE_ALLOW_INSECURE_COOKIE",
            &mut self.console.allow_insecure_cookie,
        );

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
        env_override_effort("SQUELCH_STAGE2_EFFORT", &mut self.stage2.effort);
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
        // ---- Revisit overrides ----------------------------------------------
        env_override("SQUELCH_REVISIT_ENABLED", &mut self.revisit.enabled);
        env_override(
            "SQUELCH_REVISIT_BATCH_PER_CYCLE",
            &mut self.revisit.batch_per_cycle,
        );
        env_override("SQUELCH_REVISIT_DAILY_CAP", &mut self.revisit.daily_cap);
        env_override(
            "SQUELCH_REVISIT_MAX_PER_MESSAGE",
            &mut self.revisit.max_per_message,
        );
        env_override(
            "SQUELCH_REVISIT_MAX_LIFETIME",
            &mut self.revisit.max_per_message_lifetime,
        );
        env_override(
            "SQUELCH_REVISIT_MIN_LEAD_HOURS",
            &mut self.revisit.min_lead_hours,
        );
        env_override(
            "SQUELCH_REVISIT_MAX_HORIZON_DAYS",
            &mut self.revisit.max_horizon_days,
        );
        env_override(
            "SQUELCH_REVISIT_DEADLINE_GRACE_HOURS",
            &mut self.revisit.deadline_grace_hours,
        );
        env_override(
            "SQUELCH_REVISIT_DEDUPE_HOURS",
            &mut self.revisit.dedupe_window_hours,
        );
        env_override(
            "SQUELCH_REVISIT_FYE_STALE_DAYS",
            &mut self.revisit.fye_stale_days,
        );

        env_override("SQUELCH_STAGE1_MODEL", &mut self.stage1.model);
        env_override_effort("SQUELCH_STAGE1_EFFORT", &mut self.stage1.effort);
        env_override(
            "SQUELCH_STAGE1_BOUNDARY_MARGIN",
            &mut self.stage1.escalation_boundary_margin,
        );
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

        // ---- BYOK carrier APIs ---------------------------------------------
        // Secrets, handled the relay block's way: trimmed, blank is "unset",
        // never echoed anywhere (each cred struct's `Debug` redacts them). The
        // one difference is that a value MATERIALIZES its carrier when the TOML
        // never mentioned one, because that is how a container is configured —
        // and since a carrier needs both halves, a lone id leaves it off.
        env_override_cred("SQUELCH_UPS_CLIENT_ID", &mut self.carriers.ups, |c| {
            &mut c.client_id
        });
        env_override_cred("SQUELCH_UPS_CLIENT_SECRET", &mut self.carriers.ups, |c| {
            &mut c.client_secret
        });
        env_override_cred("SQUELCH_FEDEX_CLIENT_ID", &mut self.carriers.fedex, |c| {
            &mut c.client_id
        });
        env_override_cred(
            "SQUELCH_FEDEX_CLIENT_SECRET",
            &mut self.carriers.fedex,
            |c| &mut c.client_secret,
        );
        env_override_cred("SQUELCH_USPS_CONSUMER_KEY", &mut self.carriers.usps, |c| {
            &mut c.consumer_key
        });
        env_override_cred(
            "SQUELCH_USPS_CONSUMER_SECRET",
            &mut self.carriers.usps,
            |c| &mut c.consumer_secret,
        );
        env_override_cred("SQUELCH_DHL_API_KEY", &mut self.carriers.dhl, |c| {
            &mut c.api_key
        });
        env_override(
            "SQUELCH_CARRIERS_POLL_INTERVAL_HOURS",
            &mut self.carriers.poll_interval_hours,
        );
        env_override(
            "SQUELCH_CARRIERS_OFD_POLL_INTERVAL_MINS",
            &mut self.carriers.ofd_poll_interval_mins,
        );
        env_override(
            "SQUELCH_CARRIERS_MAX_AGE_DAYS",
            &mut self.carriers.max_age_days,
        );
        env_override(
            "SQUELCH_CARRIERS_MAX_FAILURES",
            &mut self.carriers.max_failures,
        );
        env_override(
            "SQUELCH_CARRIERS_STALE_AFTER_DAYS",
            &mut self.carriers.stale_after_days,
        );
        // Only when DHL already exists: a budget must never conjure a carrier
        // (the api_key override above is the only thing that can).
        if let Some(dhl) = self.carriers.dhl.as_mut() {
            env_override("SQUELCH_DHL_DAILY_CAP", &mut dhl.daily_cap);
        }

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

        // Same idea for the carrier cadences: a ZERO interval is a spin loop
        // against somebody else's rate-limited API, so it gets a floor of 1
        // rather than a config that quietly hammers UPS.
        for (name, interval) in [
            (
                "carriers.poll_interval_hours",
                &mut self.carriers.poll_interval_hours,
            ),
            (
                "carriers.ofd_poll_interval_mins",
                &mut self.carriers.ofd_poll_interval_mins,
            ),
        ] {
            if *interval == 0 {
                eprintln!("squelch: config {name}=0 would poll without pause; clamping to 1");
                *interval = 1;
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
        assert_eq!(c.sync.poll_secs, 5);
        assert!(c.client_id.is_none());
    }

    #[test]
    fn embed_defaults_are_sane() {
        // Only the two lengths this test owns. The model pin and the backfill
        // batch size have their own tests where they are decided.
        let c = EmbedConfig::default();
        assert_eq!(c.dims, 384);
        assert_eq!(c.max_chars, 1000);
        assert_eq!(c.max_tokens, 256);
    }

    /// `settings()` carries the token budget through to the embedder, and the
    /// two truncation lengths are independently configurable.
    #[test]
    fn embed_settings_carry_max_tokens() {
        assert_eq!(EmbedConfig::default().settings().max_tokens, 256);

        let cfg: Config = toml::from_str("[embed]\nmax_tokens = 128\nmax_chars = 700\n").unwrap();
        assert_eq!(cfg.embed.max_tokens, 128);
        assert_eq!(cfg.embed.max_chars, 700);
        let s = cfg.embed.settings();
        assert_eq!(s.max_tokens, 128);
        assert_eq!(s.model_name, cfg.embed.model);
        assert_eq!(s.dims, 384);

        // A config written before the field existed still parses to the default.
        let cfg: Config = toml::from_str("[embed]\nmax_chars = 2000\n").unwrap();
        assert_eq!(cfg.embed.max_chars, 2000);
        assert_eq!(cfg.embed.max_tokens, 256);
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
        assert_eq!(c.sync.poll_secs, 5);
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

    /// The console block is config-file-first with an env override, the same
    /// shape as tracking and metrics, and both of its settings FAIL CLOSED on
    /// anything unusable: a blank URL leaves the Google button off, and a knob
    /// that is not exactly `true` leaves the cookie `Secure`.
    #[test]
    fn console_env_overrides_match_the_tracking_block() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("SQUELCH_CONSOLE_SSO_URL");
            std::env::remove_var("SQUELCH_CONSOLE_ALLOW_INSECURE_COOKIE");
        }
        let mut c = Config::default();
        c.apply_env_overrides();
        assert_eq!(c.console.sso_url, None, "unset => no sign-in button");
        assert!(!c.console.allow_insecure_cookie, "the hatch is shut");

        unsafe {
            std::env::set_var("SQUELCH_CONSOLE_SSO_URL", "  ");
            std::env::set_var("SQUELCH_CONSOLE_ALLOW_INSECURE_COOKIE", "yes please");
        }
        let mut c = Config::default();
        c.console.sso_url = Some("https://from-file.example".to_string());
        c.apply_env_overrides();
        assert_eq!(
            c.console.sso_url.as_deref(),
            Some("https://from-file.example"),
            "a blank env value does not clobber the file"
        );
        assert!(
            !c.console.allow_insecure_cookie,
            "an unparseable knob is not an opt-in"
        );

        unsafe {
            std::env::set_var("SQUELCH_CONSOLE_SSO_URL", " https://signup.example ");
            std::env::set_var("SQUELCH_CONSOLE_ALLOW_INSECURE_COOKIE", "true");
        }
        let mut c = Config::default();
        c.console.sso_url = Some("https://from-file.example".to_string());
        c.apply_env_overrides();
        assert_eq!(c.console.sso_url.as_deref(), Some("https://signup.example"));
        assert!(c.console.allow_insecure_cookie);

        unsafe {
            std::env::remove_var("SQUELCH_CONSOLE_SSO_URL");
            std::env::remove_var("SQUELCH_CONSOLE_ALLOW_INSECURE_COOKIE");
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
        assert_eq!(c.model, "claude-opus-5");
        assert_eq!(c.effort.as_deref(), Some("xhigh"));
        assert_eq!(c.max_body_chars, 12_000);
        assert_eq!(c.batch_per_cycle, 10);
        assert_eq!(c.thread_daily_cap, 3);
        assert_eq!(c.global_daily_cap, 120);
        assert_eq!(c.sender_daily_cap, 5);
        assert_eq!(c.max_age_days, 7);
        assert_eq!(c.price_in_per_mtok, 5.0);
        assert_eq!(c.price_out_per_mtok, 25.0);
    }

    #[test]
    fn stage1_llm_defaults_are_sane() {
        let c = Stage1Config::default();
        assert_eq!(c.model, "claude-opus-5");
        assert_eq!(c.effort.as_deref(), Some("low"));
        assert_eq!(c.global_daily_cap, 500);
        assert_eq!(c.batch_per_cycle, 10);
        assert_eq!(c.max_body_chars, 6000);
        assert_eq!(c.price_in_per_mtok, 5.0);
        assert_eq!(c.price_out_per_mtok, 25.0);
    }

    /// An operator typo in a revisit knob must not be able to panic the sync
    /// task: `Duration::days` and `now + duration` both blow up out of range.
    #[test]
    fn absurd_revisit_knobs_clamp_instead_of_panicking() {
        let cfg = RevisitPassConfig {
            min_lead_hours: i64::MIN,
            max_horizon_days: i64::MAX,
            deadline_grace_hours: i64::MAX,
            dedupe_window_hours: -5,
            ..RevisitPassConfig::default()
        };
        let planner = cfg.planner();
        assert_eq!(planner.min_lead, chrono::Duration::zero());
        assert_eq!(planner.dedupe_window, chrono::Duration::zero());
        assert_eq!(
            planner.max_horizon,
            chrono::Duration::days(REVISIT_MAX_DAYS)
        );
        assert_eq!(
            planner.deadline_grace,
            chrono::Duration::days(REVISIT_MAX_DAYS)
        );
        // The addition the planner actually performs, which is what panics.
        let now = chrono::Utc::now();
        let _ = now + planner.max_horizon;
    }

    /// Both stages run the SAME model on purpose: escalation buys context and
    /// reasoning depth, not a bigger model. If these two ever diverge by
    /// default, the escalation story has quietly changed and the prompts and
    /// docs that describe it need to change with it.
    #[test]
    fn both_stages_default_to_one_model_differing_only_in_effort() {
        let s1 = Stage1Config::default();
        let s2 = Stage2Config::default();
        assert_eq!(s1.model, s2.model);
        assert_eq!(s1.price_in_per_mtok, s2.price_in_per_mtok);
        assert_eq!(s1.price_out_per_mtok, s2.price_out_per_mtok);
        assert_ne!(s1.effort, s2.effort);
    }

    /// An operator repointing a stage at a model with no effort support must be
    /// able to DROP the field: sending it to such a model is a 400 on every
    /// call, so "unset" has to be reachable from the environment.
    #[test]
    fn effort_can_be_cleared_from_the_environment() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut slot = Some("xhigh".to_string());
        // SAFETY: guarded by ENV_LOCK.
        unsafe { std::env::set_var("SQUELCH_TEST_EFFORT", "none") };
        env_override_effort("SQUELCH_TEST_EFFORT", &mut slot);
        assert_eq!(slot, None);

        // SAFETY: guarded by ENV_LOCK.
        unsafe { std::env::set_var("SQUELCH_TEST_EFFORT", "medium") };
        env_override_effort("SQUELCH_TEST_EFFORT", &mut slot);
        assert_eq!(slot.as_deref(), Some("medium"));

        // Blank means "unset", which must not clobber a configured value.
        // SAFETY: guarded by ENV_LOCK.
        unsafe { std::env::set_var("SQUELCH_TEST_EFFORT", "") };
        env_override_effort("SQUELCH_TEST_EFFORT", &mut slot);
        assert_eq!(slot.as_deref(), Some("medium"));

        // SAFETY: guarded by ENV_LOCK.
        unsafe { std::env::remove_var("SQUELCH_TEST_EFFORT") };
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
            std::env::remove_var("SQUELCH_ASSISTANT_API_KEY");
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
    fn assistant_requires_key_and_gateway_together() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_stage2_env();
        let mut c = Stage2Config::default();

        // Neither => None.
        assert!(c.resolve_assistant().is_none());

        // Key alone => None: without a gateway there is nothing to relay to.
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_ASSISTANT_API_KEY", "sk-bf-assistant");
        }
        assert!(c.resolve_assistant().is_none());

        // Base URL alone => None: no credential to relay with.
        unsafe {
            std::env::remove_var("SQUELCH_ASSISTANT_API_KEY");
        }
        c.anthropic_base_url = Some("https://gw.example.com".into());
        assert!(c.resolve_assistant().is_none());

        // Both => Some, with the gateway messages endpoint joined exactly as
        // resolve_llm builds it (trailing slash folds).
        unsafe {
            std::env::set_var("SQUELCH_ASSISTANT_API_KEY", "sk-bf-assistant");
        }
        let r = c.resolve_assistant().expect("key + gateway => Some");
        assert_eq!(r.api_key, "sk-bf-assistant");
        assert_eq!(r.url, "https://gw.example.com/v1/messages");
        c.anthropic_base_url = Some("https://gw.example.com/".into());
        assert_eq!(
            c.resolve_assistant().unwrap().url,
            "https://gw.example.com/v1/messages"
        );

        // An empty key counts as absent, like every other key source.
        unsafe {
            std::env::set_var("SQUELCH_ASSISTANT_API_KEY", "");
        }
        assert!(c.resolve_assistant().is_none());

        // A base URL that fails the transport check is absent everywhere, so it
        // cannot half-configure the relay.
        unsafe {
            std::env::set_var("SQUELCH_ASSISTANT_API_KEY", "sk-bf-assistant");
        }
        c.anthropic_base_url = Some("http://gw.example.com".into());
        assert!(c.resolve_assistant().is_none());

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
            with_base("https://gw.example.com/")
                .resolve_llm()
                .unwrap()
                .url,
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

    /// Clear every carrier credential/knob var. Caller must hold `ENV_LOCK`.
    fn clear_carrier_env() {
        // SAFETY: caller holds ENV_LOCK.
        unsafe {
            for name in [
                "SQUELCH_UPS_CLIENT_ID",
                "SQUELCH_UPS_CLIENT_SECRET",
                "SQUELCH_FEDEX_CLIENT_ID",
                "SQUELCH_FEDEX_CLIENT_SECRET",
                "SQUELCH_USPS_CONSUMER_KEY",
                "SQUELCH_USPS_CONSUMER_SECRET",
                "SQUELCH_DHL_API_KEY",
                "SQUELCH_DHL_DAILY_CAP",
                "SQUELCH_CARRIERS_POLL_INTERVAL_HOURS",
                "SQUELCH_CARRIERS_OFD_POLL_INTERVAL_MINS",
                "SQUELCH_CARRIERS_MAX_AGE_DAYS",
                "SQUELCH_CARRIERS_MAX_FAILURES",
                "SQUELCH_CARRIERS_STALE_AFTER_DAYS",
            ] {
                std::env::remove_var(name);
            }
        }
    }

    /// The feature is OFF out of the box, and a config written before it existed
    /// (no `[carriers]` table at all) still parses.
    #[test]
    fn carriers_default_is_off_and_legacy_configs_still_parse() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_carrier_env();
        let mut c = Config::default();
        c.apply_env_overrides();
        assert!(!c.carriers.any_enabled(), "no creds => no carrier polling");
        assert!(c.carriers.ups.is_none());
        assert!(c.carriers.fedex.is_none());
        assert!(c.carriers.usps.is_none());
        assert!(c.carriers.dhl.is_none());
        assert_eq!(c.carriers.poll_interval_hours, 6);
        assert_eq!(c.carriers.ofd_poll_interval_mins, 60);
        assert_eq!(c.carriers.max_age_days, 45);
        assert_eq!(c.carriers.max_failures, 5);
        assert_eq!(c.carriers.stale_after_days, 7);

        // A config predating the feature has no [carriers] table whatsoever.
        let cfg: Config = toml::from_str("squelch_level = 1\n").unwrap();
        assert_eq!(cfg.carriers, CarriersConfig::default());
        assert!(!cfg.carriers.any_enabled());
    }

    /// The listing policy both doors carry is derived from `[carriers]`, and its
    /// `Default` is the config default — so a hand-built `ApiState` or
    /// `SquelchServer` filters the way an unconfigured daemon does.
    #[test]
    fn the_listing_policy_tracks_the_carriers_block() {
        assert_eq!(
            ShipmentListPolicy::default(),
            CarriersConfig::default().list_policy()
        );
        assert_eq!(ShipmentListPolicy::default().stale_after_days, 7);
        assert_eq!(
            ShipmentListPolicy::default().suppress_failed_ambiguous_at,
            5
        );

        let carriers = CarriersConfig {
            max_failures: 2,
            stale_after_days: 0,
            ..CarriersConfig::default()
        };
        let policy = ShipmentListPolicy::from(&carriers);
        assert_eq!(policy.suppress_failed_ambiguous_at, 2);
        assert_eq!(
            policy.stale_after_days, 0,
            "0 is a real value (the filter off), never a fallback to the default"
        );
    }

    /// `stale_after_days` is configurable both ways, like every other knob in the
    /// block — the env form is how a container sets it.
    #[test]
    fn stale_after_days_comes_from_toml_or_the_environment() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_carrier_env();

        let cfg: Config = toml::from_str("[carriers]\nstale_after_days = 21\n").unwrap();
        assert_eq!(cfg.carriers.stale_after_days, 21);

        let mut c = Config::default();
        // SAFETY: we hold ENV_LOCK.
        unsafe { std::env::set_var("SQUELCH_CARRIERS_STALE_AFTER_DAYS", "0") };
        c.apply_env_overrides();
        assert_eq!(c.carriers.stale_after_days, 0, "0 disables the filter");
        clear_carrier_env();
    }

    #[test]
    fn carriers_section_round_trips_through_toml() {
        let cfg: Config = toml::from_str(
            r#"
[carriers]
poll_interval_hours = 3
max_failures = 9

[carriers.ups]
client_id = "ups-id"
client_secret = "ups-sekret"

[carriers.dhl]
api_key = "dhl-key"
daily_cap = 240
"#,
        )
        .unwrap();
        assert_eq!(cfg.carriers.poll_interval_hours, 3);
        assert_eq!(cfg.carriers.max_failures, 9);
        // Unspecified knobs keep their defaults (#[serde(default)]).
        assert_eq!(cfg.carriers.ofd_poll_interval_mins, 60);
        assert_eq!(cfg.carriers.max_age_days, 45);
        assert_eq!(
            cfg.carriers.ups.as_ref().unwrap().credentials(),
            Some(("ups-id", "ups-sekret"))
        );
        assert_eq!(
            cfg.carriers.dhl.as_ref().unwrap().api_key(),
            Some("dhl-key")
        );
        assert_eq!(cfg.carriers.dhl.as_ref().unwrap().daily_cap, 240);
        // The carriers nobody configured stay absent.
        assert!(cfg.carriers.fedex.is_none());
        assert!(cfg.carriers.usps.is_none());
        assert!(cfg.carriers.any_enabled());

        // And it survives a serialize/parse lap unchanged.
        let rendered = toml::to_string(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(reparsed.carriers, cfg.carriers);

        // An omitted daily_cap lands under DHL's 250/day free tier.
        let cfg: Config = toml::from_str("[carriers.dhl]\napi_key = \"k\"\n").unwrap();
        assert_eq!(cfg.carriers.dhl.unwrap().daily_cap, 200);
    }

    /// The container case: nothing on disk, everything in the environment. A
    /// full env PAIR has to materialize a carrier the TOML never mentioned.
    #[test]
    fn carrier_env_materializes_a_carrier_absent_from_the_toml() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_carrier_env();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_FEDEX_CLIENT_ID", "  fedex-id  ");
            std::env::set_var("SQUELCH_FEDEX_CLIENT_SECRET", "fedex-sekret");
            std::env::set_var("SQUELCH_USPS_CONSUMER_KEY", "usps-key");
            std::env::set_var("SQUELCH_USPS_CONSUMER_SECRET", "usps-sekret");
            std::env::set_var("SQUELCH_DHL_API_KEY", "dhl-key");
            std::env::set_var("SQUELCH_DHL_DAILY_CAP", "150");
            std::env::set_var("SQUELCH_CARRIERS_POLL_INTERVAL_HOURS", "2");
            std::env::set_var("SQUELCH_CARRIERS_MAX_AGE_DAYS", "10");
        }
        let mut c = Config::default();
        c.apply_env_overrides();
        assert_eq!(
            c.carriers.fedex.as_ref().unwrap().credentials(),
            // Trimmed, exactly like the relay block.
            Some(("fedex-id", "fedex-sekret"))
        );
        assert_eq!(
            c.carriers.usps.as_ref().unwrap().credentials(),
            Some(("usps-key", "usps-sekret"))
        );
        assert_eq!(c.carriers.dhl.as_ref().unwrap().api_key(), Some("dhl-key"));
        assert_eq!(c.carriers.dhl.as_ref().unwrap().daily_cap, 150);
        assert_eq!(c.carriers.poll_interval_hours, 2);
        assert_eq!(c.carriers.max_age_days, 10);
        assert!(c.carriers.any_enabled());
        // Untouched carriers are still absent, not empty-but-present.
        assert!(c.carriers.ups.is_none());

        // Env beats the file, same as everywhere else.
        unsafe {
            std::env::set_var("SQUELCH_UPS_CLIENT_ID", "env-id");
        }
        let mut c: Config = toml::from_str(
            "[carriers.ups]\nclient_id = \"file-id\"\nclient_secret = \"file-sekret\"\n",
        )
        .unwrap();
        c.apply_env_overrides();
        assert_eq!(
            c.carriers.ups.as_ref().unwrap().credentials(),
            Some(("env-id", "file-sekret"))
        );
        clear_carrier_env();
    }

    /// A blank carrier var is "unset", never "set to empty": it must not clobber
    /// a configured secret, and it must not conjure a credential-shaped table
    /// that would make the poller think a carrier is available.
    #[test]
    fn blank_carrier_env_is_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_carrier_env();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_UPS_CLIENT_ID", "   ");
            std::env::set_var("SQUELCH_UPS_CLIENT_SECRET", "");
            std::env::set_var("SQUELCH_DHL_API_KEY", "  ");
            std::env::set_var("SQUELCH_CARRIERS_POLL_INTERVAL_HOURS", "");
        }
        let mut c: Config = toml::from_str(
            "[carriers]\npoll_interval_hours = 4\n\n\
             [carriers.ups]\nclient_id = \"file-id\"\nclient_secret = \"file-sekret\"\n",
        )
        .unwrap();
        c.apply_env_overrides();
        assert_eq!(
            c.carriers.ups.as_ref().unwrap().credentials(),
            Some(("file-id", "file-sekret")),
            "a blank env value does not clobber the file"
        );
        assert_eq!(c.carriers.poll_interval_hours, 4);
        assert!(
            c.carriers.dhl.is_none(),
            "a blank secret never materializes a carrier"
        );
        clear_carrier_env();
    }

    /// Half a pair is not a credential. A `client_id` with no secret leaves that
    /// carrier disabled, whether it came from the file or from one lone env var
    /// — and a blank half in the file counts as absent too.
    #[test]
    fn half_a_credential_pair_is_not_enabled() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_carrier_env();

        let cfg: Config = toml::from_str("[carriers.ups]\nclient_id = \"only-the-id\"\n").unwrap();
        let ups = cfg.carriers.ups.as_ref().unwrap();
        assert!(!ups.enabled(), "an id with no secret is not a credential");
        assert_eq!(ups.credentials(), None);
        assert!(!cfg.carriers.any_enabled());

        // A blank half in the file is absent, not "configured with nothing".
        let cfg: Config =
            toml::from_str("[carriers.usps]\nconsumer_key = \"k\"\nconsumer_secret = \"   \"\n")
                .unwrap();
        assert!(!cfg.carriers.usps.as_ref().unwrap().enabled());
        assert!(!cfg.carriers.any_enabled());

        // One lone env var materializes the struct but not the feature.
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("SQUELCH_FEDEX_CLIENT_ID", "lonely-id");
        }
        let mut c = Config::default();
        c.apply_env_overrides();
        assert!(c.carriers.fedex.is_some());
        assert!(!c.carriers.fedex.as_ref().unwrap().enabled());
        assert!(
            !c.carriers.any_enabled(),
            "half a pair leaves the feature off"
        );
        clear_carrier_env();
    }

    /// Carrier secrets are redacted from `Debug`, the way every other
    /// secret-bearing config struct in the workspace does it — a `{:?}` of the
    /// whole `Config` must not put an API key in a log.
    #[test]
    fn carrier_debug_redacts_the_secrets() {
        let cfg: Config = toml::from_str(
            r#"
[carriers.ups]
client_id = "ups-id"
client_secret = "ups-sekret"

[carriers.fedex]
client_id = "fedex-id"
client_secret = "fedex-sekret"

[carriers.usps]
consumer_key = "usps-key"
consumer_secret = "usps-sekret"

[carriers.dhl]
api_key = "dhl-key"
"#,
        )
        .unwrap();
        let rendered = format!("{:?}", cfg.carriers);
        for secret in ["ups-sekret", "fedex-sekret", "usps-sekret", "dhl-key"] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
        assert_eq!(rendered.matches("<redacted>").count(), 4);
        // The public halves stay visible — they are what makes a Debug useful.
        assert!(rendered.contains("ups-id"));
        assert!(rendered.contains("usps-key"));
        // And the same holds through the whole Config's derived Debug.
        assert!(!format!("{cfg:?}").contains("ups-sekret"));
    }

    /// A zero cadence would spin against a carrier's API; it floors at 1.
    #[test]
    fn zero_carrier_interval_is_floored() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_carrier_env();
        let mut c: Config =
            toml::from_str("[carriers]\npoll_interval_hours = 0\nofd_poll_interval_mins = 0\n")
                .unwrap();
        c.apply_env_overrides();
        assert_eq!(c.carriers.poll_interval_hours, 1);
        assert_eq!(c.carriers.ofd_poll_interval_mins, 1);
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
