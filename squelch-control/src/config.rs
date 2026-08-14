//! Startup configuration, read once from the environment and validated before
//! the listener binds. Every bad value is a refusal to start, never a default:
//! a control plane that guesses its own redirect URI rejects every consent it
//! will ever see, and one that guesses its base domain hands out tenant URLs
//! that resolve to somebody else.
//!
//! SIX FIELDS HERE ARE SECRETS: the OAuth client secret, the cookie key, the
//! warden bearer, the Bifrost admin token, the admin-page token, and the Resend
//! API key. So [`Config`] (and [`BifrostConfig`] and [`WaitlistConfig`] inside
//! it) has a HAND-WRITTEN `Debug` that redacts them. A derived one would put
//! all six in any `tracing::debug!` that ever formats the config, and that is
//! exactly the line nobody notices adding.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;

/// Loopback by design: TLS is terminated by the platform edge (Railway) or a
/// proxy, so binding a public interface would serve signup in the clear. The
/// container entrypoint widens this to `0.0.0.0:$PORT` for platforms that
/// inject a port, which is an explicit act rather than a default.
pub const DEFAULT_BIND: &str = "127.0.0.1:8852";

/// Where the control store lives when nothing says otherwise. Matches the
/// mount point the image documents.
pub const DEFAULT_DB_PATH: &str = "/data/control.sqlite3";

/// Google's token endpoint. Pinned as a constant and deliberately NOT readable
/// from the environment: this request carries the confidential client secret,
/// so "which host do we send the secret to" must not be a deploy-time typo or
/// an injected variable. Tests point [`Config::token_url`] at a mock by
/// constructing the struct directly.
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Google's authorization endpoint, pinned for the same reason: it is the URL a
/// user's browser is redirected to, and an environment-supplied one is a
/// phishing page with our domain in front of it.
pub const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// The one Gmail call that names the mailbox behind an access token.
/// `gmail.readonly` permits it.
pub const GMAIL_PROFILE_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me/profile";

/// OpenID Connect's userinfo endpoint: what names the mailbox behind an
/// `openid email` token, which is all a console login ever holds. Pinned for the
/// same reason as the rest: this answer decides WHO somebody is, so the host it
/// comes from must not be an environment variable.
pub const GOOGLE_USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";

/// Ceiling on `SQUELCH_CONTROL_TRUSTED_PROXY_HOPS`. Same reasoning as the
/// broker's: nobody stacks eight proxies, a larger number is a typo, and a typo
/// degrades silently into one shared rate-limit bucket for the whole internet.
pub const MAX_TRUSTED_PROXY_HOPS: usize = 8;

/// Minimum decoded cookie-key length. HMAC-SHA256's block is 64 bytes and its
/// output 32; below 32 bytes of real entropy the signature stops being the
/// thing that decides whether a signup session is ours.
pub const MIN_COOKIE_KEY_BYTES: usize = 32;

/// Budget for every outbound call this service makes: the token exchange, the
/// profile lookup, and each warden request. All are one small round trip, and
/// an unbounded one would pin a request task for as long as the far end cared
/// to hold it.
pub const OUTBOUND_TIMEOUT: Duration = Duration::from_secs(30);

/// Minimum Bifrost admin-credential length (the whole `username:password`),
/// the same bar the warden holds its own bearer to: below this the credential
/// stops being the thing that decides who can mint unbounded LLM spend.
pub const MIN_BIFROST_TOKEN_LEN: usize = 32;

/// The monthly spend a tenant's virtual key is minted with when
/// `SQUELCH_CONTROL_LLM_BUDGET_USD` says nothing.
pub const DEFAULT_LLM_BUDGET_USD: f64 = 5.00;

/// Ceiling on the configured budget. Same reasoning as the proxy-hop cap: no
/// tenant's triage costs four figures a month, a larger number is a typo, and
/// this typo degrades into real money.
pub const MAX_LLM_BUDGET_USD: f64 = 1_000.0;

/// The models allowed on every minted virtual key when
/// `SQUELCH_CONTROL_LLM_MODELS` says nothing. NEVER empty: on the live
/// gateway an empty `allowed_models` is deny-all, and wildcards are
/// unreliable, so "no list" must mean "the product's list", not "nothing".
pub const DEFAULT_LLM_MODELS: &str = "claude-haiku-4-5,claude-sonnet-5";

/// The monthly spend a tenant's ASSISTANT key is minted with when
/// `SQUELCH_CONTROL_ASSISTANT_BUDGET_USD` says nothing. Higher than triage's:
/// the assistant answers a person's own questions on demand, so its ceiling is
/// theirs to spend, not the product's background hum.
pub const DEFAULT_ASSISTANT_BUDGET_USD: f64 = 10.00;

/// The models allowed on every minted ASSISTANT key when
/// `SQUELCH_CONTROL_ASSISTANT_MODELS` says nothing. Never empty, for the same
/// deny-all reason as the triage list; a different set because the assistant
/// wants a frontier model where triage wants a cheap one.
pub const DEFAULT_ASSISTANT_MODELS: &str = "claude-haiku-4-5,claude-opus-4-8";

/// Ceiling on one configured model name. Real ids are ~30 characters; a
/// larger one is a paste accident.
pub const MAX_LLM_MODEL_LEN: usize = 128;

/// Minimum admin-token length, the same bar the warden bearer and the Bifrost
/// credential are held to. This one token is the entire authentication of the
/// admin page on a PUBLIC service, and the page mints invites, so anything a
/// human could think up is not it: `openssl rand -base64 32`.
pub const MIN_ADMIN_TOKEN_LEN: usize = 32;

/// Resend's API origin. Pinned as a constant and deliberately NOT readable from
/// the environment, for the same reason Google's token endpoint is: this
/// request carries the sending API key, so "which host do we send the key to"
/// must not be a deploy-time typo. Tests point [`WaitlistConfig::resend_url`]
/// at a mock by constructing the struct directly.
pub const RESEND_URL: &str = "https://api.resend.com";

/// The origin the waitlist form is served from, and so the only one the CORS
/// answer names. A default rather than a required variable because the product
/// has exactly one marketing site; an operator running their own points this at
/// it.
pub const DEFAULT_WAITLIST_ORIGIN: &str = "https://passband.app";

/// Ceiling on the Resend API key. Real ones are ~35 characters.
const MAX_RESEND_API_KEY_LEN: usize = 256;

/// Ceiling on the `From:` the invite is sent as. RFC 5321's address limit, with
/// the display name riding in front of it.
const MAX_INVITE_FROM_LEN: usize = 320;

/// Why the control plane refused to start.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
}

impl ConfigError {
    fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }
}

/// Validated startup configuration.
#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// The externally visible base URL of THIS service, canonicalized with no
    /// trailing slash (`https://signup.passband.email`). The `redirect_uri`
    /// Google is given is derived from it, so it is not derivable from `bind`.
    pub public_url: String,
    /// `public_url` + `/oauth/callback`, precomputed because it is sent twice
    /// (on the consent URL and again on the exchange) and Google compares them.
    pub redirect_uri: String,
    /// The hosted base domain (`passband.email`). Tenant URLs are
    /// `https://<label>.<base_domain>`; NEVER hardcoded anywhere in this crate.
    pub base_domain: String,
    /// The confidential web client. `client_secret` is redacted from `Debug`.
    pub client_id: String,
    pub client_secret: String,
    /// HMAC key for the signup cookie. Redacted from `Debug`.
    pub cookie_key: Vec<u8>,
    /// Base URL of the warden (`https://warden.passband.email`).
    pub warden_url: String,
    /// Bearer presented to the warden on every request. Redacted from `Debug`.
    pub warden_token: String,
    pub db_path: PathBuf,
    /// How many proxies the operator asserts sit in front of this listener. `0`
    /// trusts nothing and meters the TCP peer. See [`crate::ratelimit`].
    pub trusted_proxy_hops: usize,
    /// Google's token endpoint. A field rather than a constant use-site so the
    /// signup flow can be tested end to end against a mock; `from_env` always
    /// pins [`GOOGLE_TOKEN_URL`].
    pub token_url: String,
    /// Google's authorization endpoint, pinned by `from_env` the same way.
    pub auth_url: String,
    /// Gmail's profile endpoint, pinned by `from_env` the same way.
    pub profile_url: String,
    /// OIDC's userinfo endpoint, pinned by `from_env` the same way. The console
    /// login reads it; the signup flow never touches it.
    pub userinfo_url: String,
    /// The Bifrost LLM gateway, when this deployment has one. `None` means the
    /// feature is OFF and signup provisions tenants with no LLM key at all;
    /// a partial configuration is a refusal to boot, never a silent off.
    pub bifrost: Option<BifrostConfig>,
    /// The waitlist and its admin page, when this deployment has them. `None`
    /// means those routes are NOT MOUNTED at all (a 404, not a 403), because a
    /// deployment with no admin token must not answer at an admin URL.
    pub waitlist: Option<WaitlistConfig>,
}

/// The Bifrost governance settings: where the gateway is, the admin
/// credential that mints virtual keys, the monthly budget each tenant's key
/// is minted with, and the models it is allowed to call. `admin_token` is a
/// secret; the hand-written `Debug` redacts it.
#[derive(Clone)]
pub struct BifrostConfig {
    /// Base URL of the gateway's governance API, canonical https origin.
    pub url: String,
    /// The gateway admin's `username:password`, presented as HTTP Basic on
    /// every governance call (session bearers expire after 30 days; Basic
    /// works statically on `/api/*`). Redacted from `Debug`.
    pub admin_token: String,
    /// Monthly budget, USD, stamped on every minted TRIAGE virtual key.
    pub budget_usd: f64,
    /// `allowed_models` for every minted triage virtual key. Never empty.
    pub models: Vec<String>,
    /// Monthly budget, USD, stamped on every minted ASSISTANT virtual key.
    pub assistant_budget_usd: f64,
    /// `allowed_models` for every minted assistant virtual key. Never empty.
    pub assistant_models: Vec<String>,
}

impl std::fmt::Debug for BifrostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BifrostConfig")
            .field("url", &self.url)
            .field("admin_token", &"<redacted>")
            .field("budget_usd", &self.budget_usd)
            .field("models", &self.models)
            .field("assistant_budget_usd", &self.assistant_budget_usd)
            .field("assistant_models", &self.assistant_models)
            .finish()
    }
}

/// The waitlist settings: the token that opens the admin page, the credential
/// and the sender the invite email goes out with, and the one browser origin
/// the public form may be posted from. `admin_token` and `resend_api_key` are
/// secrets; the hand-written `Debug` redacts both.
#[derive(Clone)]
pub struct WaitlistConfig {
    /// The operator's password for the admin page, compared with
    /// [`squelch_httpauth::ct_eq`]. Redacted from `Debug`.
    pub admin_token: String,
    /// Bearer presented to Resend on the one call this feature makes.
    /// Redacted from `Debug`.
    pub resend_api_key: String,
    /// The `From:` every invite is sent as, e.g. `Passband
    /// <invites@passband.app>`. Must be an address on a domain verified at
    /// Resend, or every send is refused.
    pub invite_from: String,
    /// The origin the waitlist form is served from, echoed as
    /// `Access-Control-Allow-Origin` on that route and nowhere else. A canonical
    /// origin: no path, no trailing slash.
    pub allowed_origin: String,
    /// Resend's API origin. A field rather than a constant use-site so the send
    /// can be tested against a mock; `from_env` always pins [`RESEND_URL`].
    pub resend_url: String,
}

impl std::fmt::Debug for WaitlistConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaitlistConfig")
            .field("admin_token", &"<redacted>")
            .field("resend_api_key", &"<redacted>")
            .field("invite_from", &self.invite_from)
            .field("allowed_origin", &self.allowed_origin)
            .field("resend_url", &self.resend_url)
            .finish()
    }
}

impl std::fmt::Debug for Config {
    /// Hand-written so the secrets can never ride out in a formatted config.
    /// The cookie key shows its LENGTH, which is the only property anyone
    /// debugging it needs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("public_url", &self.public_url)
            .field("redirect_uri", &self.redirect_uri)
            .field("base_domain", &self.base_domain)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("cookie_key", &format!("<{} bytes>", self.cookie_key.len()))
            .field("warden_url", &self.warden_url)
            .field("warden_token", &"<redacted>")
            .field("db_path", &self.db_path)
            .field("trusted_proxy_hops", &self.trusted_proxy_hops)
            .field("token_url", &self.token_url)
            .field("auth_url", &self.auth_url)
            .field("profile_url", &self.profile_url)
            .field("userinfo_url", &self.userinfo_url)
            // BifrostConfig's own Debug redacts the admin token.
            .field("bifrost", &self.bifrost)
            // WaitlistConfig's own Debug redacts the admin token and the
            // Resend key.
            .field("waitlist", &self.waitlist)
            .finish()
    }
}

/// Read a var, treating whitespace-only as unset.
pub fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn require(name: &str, what: &str) -> Result<String, ConfigError> {
    var(name).ok_or_else(|| ConfigError::invalid(format!("{name} is required ({what})")))
}

/// The control store path, for the CLI subcommands that touch only the store
/// and must not need an OAuth client or a warden to mint an invite.
pub fn db_path_from_env() -> PathBuf {
    var("SQUELCH_CONTROL_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DB_PATH))
}

impl Config {
    /// Load and validate from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_raw = var("SQUELCH_CONTROL_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind: SocketAddr = bind_raw.parse().map_err(|e| {
            ConfigError::invalid(format!("invalid SQUELCH_CONTROL_BIND `{bind_raw}`: {e}"))
        })?;

        let public_url = canonical_origin(
            "SQUELCH_CONTROL_PUBLIC_URL",
            &require(
                "SQUELCH_CONTROL_PUBLIC_URL",
                "this service's externally visible origin, e.g. https://signup.passband.email",
            )?,
        )?;
        let redirect_uri = format!("{public_url}/oauth/callback");

        let base_domain = canonical_domain(&require(
            "SQUELCH_CONTROL_BASE_DOMAIN",
            "the hosted base domain tenants live under, e.g. passband.email",
        )?)?;

        let client_id = require(
            "SQUELCH_CONTROL_CLIENT_ID",
            "the confidential WEB OAuth client id, not the desktop client",
        )?;
        let client_secret = require(
            "SQUELCH_CONTROL_CLIENT_SECRET",
            "the confidential web OAuth client secret",
        )?;

        let cookie_key = decode_cookie_key(&require(
            "SQUELCH_CONTROL_COOKIE_KEY",
            "base64 or hex, at least 32 bytes decoded; generate with `openssl rand -base64 48`",
        )?)?;

        // NO AGE RECIPIENT HERE, and its absence is the v2 design: every tenant
        // gets its own identity, minted by the warden and never seen by this
        // process, so the recipient to seal to arrives in the 201 of the first
        // provisioning call instead of sitting in this deployment's environment.
        // One static recipient would have meant one key opening every mailbox.

        let (warden_url, warden_token) = warden_from_env()?;

        let bifrost = bifrost_from(
            var("SQUELCH_CONTROL_BIFROST_URL"),
            var("SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN"),
            var("SQUELCH_CONTROL_LLM_BUDGET_USD"),
            var("SQUELCH_CONTROL_LLM_MODELS"),
            var("SQUELCH_CONTROL_ASSISTANT_BUDGET_USD"),
            var("SQUELCH_CONTROL_ASSISTANT_MODELS"),
        )?;

        let waitlist = waitlist_from(
            var("SQUELCH_CONTROL_ADMIN_TOKEN"),
            var("SQUELCH_CONTROL_RESEND_API_KEY"),
            var("SQUELCH_CONTROL_INVITE_FROM"),
            var("SQUELCH_CONTROL_WAITLIST_ORIGIN"),
        )?;

        let trusted_proxy_hops = match var("SQUELCH_CONTROL_TRUSTED_PROXY_HOPS") {
            None => 0,
            Some(v) => {
                let hops: usize = v.parse().map_err(|e| {
                    ConfigError::invalid(format!(
                        "invalid SQUELCH_CONTROL_TRUSTED_PROXY_HOPS `{v}`: {e}"
                    ))
                })?;
                if hops > MAX_TRUSTED_PROXY_HOPS {
                    return Err(ConfigError::invalid(format!(
                        "SQUELCH_CONTROL_TRUSTED_PROXY_HOPS `{hops}` exceeds {MAX_TRUSTED_PROXY_HOPS}; set it to the number of proxies in front of this listener (1 behind Railway's edge)"
                    )));
                }
                hops
            }
        };

        Ok(Self {
            bind,
            public_url,
            redirect_uri,
            base_domain,
            client_id,
            client_secret,
            cookie_key,
            warden_url,
            warden_token,
            db_path: db_path_from_env(),
            trusted_proxy_hops,
            token_url: GOOGLE_TOKEN_URL.to_string(),
            auth_url: GOOGLE_AUTH_URL.to_string(),
            profile_url: GMAIL_PROFILE_URL.to_string(),
            userinfo_url: GOOGLE_USERINFO_URL.to_string(),
            bifrost,
            waitlist,
        })
    }

    /// The tenant's daemon URL. The ONE place the hosted URL shape is spelled,
    /// so the base domain arrives from config on every path that renders one.
    pub fn tenant_url(&self, label: &str) -> String {
        format!("https://{label}.{}", self.base_domain)
    }

    /// Whether this service's own origin is plain HTTP. Legal for a local dev
    /// run and nowhere else: the signup cookie could not carry `Secure`, and
    /// Google would deliver an authorization code in the clear.
    pub fn is_insecure(&self) -> bool {
        self.public_url.starts_with("http://")
    }
}

/// The warden pair from the environment, validated. Shared by `serve` (via
/// [`Config::from_env`]) and the `llm` operator commands, which need a warden
/// client but must not need an OAuth client or a cookie key to rotate a key.
pub fn warden_from_env() -> Result<(String, String), ConfigError> {
    let url = canonical_origin(
        "SQUELCH_CONTROL_WARDEN_URL",
        &require(
            "SQUELCH_CONTROL_WARDEN_URL",
            "the warden's base URL, e.g. https://warden.passband.email",
        )?,
    )?;
    let token = require(
        "SQUELCH_CONTROL_WARDEN_TOKEN",
        "the bearer the warden expects; must match SQUELCH_WARDEN_TOKEN in the cluster",
    )?;
    Ok((url, token))
}

impl BifrostConfig {
    /// The Bifrost settings from the environment: `Ok(None)` when the feature
    /// is off, an error when it is half-configured. For the `llm` operator
    /// commands, which need this and the warden pair and nothing else.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        bifrost_from(
            var("SQUELCH_CONTROL_BIFROST_URL"),
            var("SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN"),
            var("SQUELCH_CONTROL_LLM_BUDGET_USD"),
            var("SQUELCH_CONTROL_LLM_MODELS"),
            var("SQUELCH_CONTROL_ASSISTANT_BUDGET_USD"),
            var("SQUELCH_CONTROL_ASSISTANT_MODELS"),
        )
    }
}

/// Validate the Bifrost settings, all-or-nothing.
///
/// None of the six set means the feature is OFF, which is a legal deployment
/// (self-host, or hosted before the gateway exists). Some-but-not-all is a
/// REFUSAL TO BOOT: a control plane that quietly ran without the gateway would
/// provision every tenant keyless and nobody would notice until the first
/// triage call failed, weeks of signups later. The budgets and the model lists
/// are the four fields with defaults, because "the feature is on" is decided
/// by the url and the credential, and any of the others without both of
/// those is still a half-configured feature.
fn bifrost_from(
    url: Option<String>,
    admin_token: Option<String>,
    budget: Option<String>,
    models: Option<String>,
    assistant_budget: Option<String>,
    assistant_models: Option<String>,
) -> Result<Option<BifrostConfig>, ConfigError> {
    if url.is_none()
        && admin_token.is_none()
        && budget.is_none()
        && models.is_none()
        && assistant_budget.is_none()
        && assistant_models.is_none()
    {
        return Ok(None);
    }
    let (Some(url), Some(admin_token)) = (url, admin_token) else {
        return Err(ConfigError::invalid(
            "SQUELCH_CONTROL_BIFROST_URL and SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN must be set \
             together (the SQUELCH_CONTROL_LLM_* and SQUELCH_CONTROL_ASSISTANT_* budget and \
             model variables are optional); set both or neither",
        ));
    };
    let url = canonical_origin("SQUELCH_CONTROL_BIFROST_URL", &url)?;
    // https only, no local-dev exception: what rides this connection is an
    // admin credential that mints spend and, coming back, a live per-tenant
    // key.
    if !url.starts_with("https://") {
        return Err(ConfigError::invalid(format!(
            "invalid SQUELCH_CONTROL_BIFROST_URL `{url}`: the governance API must be https"
        )));
    }
    if admin_token.len() < MIN_BIFROST_TOKEN_LEN {
        return Err(ConfigError::invalid(format!(
            "SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN is {} characters; at least {MIN_BIFROST_TOKEN_LEN} are required",
            admin_token.len()
        )));
    }
    // The credential is the gateway admin's `username:password`, sent as HTTP
    // Basic (session bearers expire after 30 days; Basic works statically on
    // `/api/*`). Exactly one `:` splitting two nonempty halves, checked here
    // so a pasted session bearer fails at boot instead of as a 401 on the
    // first signup weeks later.
    let colons = admin_token.matches(':').count();
    if colons != 1 || admin_token.starts_with(':') || admin_token.ends_with(':') {
        return Err(ConfigError::invalid(
            "SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN must be the gateway admin's `username:password` \
             (exactly one `:` between two nonempty halves), sent as HTTP Basic; a session bearer \
             expires and does not belong here",
        ));
    }
    let budget_usd = parse_budget("SQUELCH_CONTROL_LLM_BUDGET_USD", budget, DEFAULT_LLM_BUDGET_USD)?;
    let models = parse_models(
        "SQUELCH_CONTROL_LLM_MODELS",
        &models.unwrap_or_else(|| DEFAULT_LLM_MODELS.to_string()),
    )?;
    let assistant_budget_usd = parse_budget(
        "SQUELCH_CONTROL_ASSISTANT_BUDGET_USD",
        assistant_budget,
        DEFAULT_ASSISTANT_BUDGET_USD,
    )?;
    let assistant_models = parse_models(
        "SQUELCH_CONTROL_ASSISTANT_MODELS",
        &assistant_models.unwrap_or_else(|| DEFAULT_ASSISTANT_MODELS.to_string()),
    )?;
    Ok(Some(BifrostConfig {
        url,
        admin_token,
        budget_usd,
        models,
        assistant_budget_usd,
        assistant_models,
    }))
}

/// Validate the waitlist settings, all-or-nothing.
///
/// None of the four set means the feature is OFF, which is a legal deployment
/// (self-host, or hosted before there is a marketing site to collect from) and
/// means the waitlist and admin routes are NOT MOUNTED. Some-but-not-all is a
/// REFUSAL TO BOOT, and the stakes are higher here than for the gateway: a
/// deployment that quietly came up with an admin token and no way to send would
/// approve people into silence, and one that came up with a sender and no token
/// would have no admin page to approve from. The origin is the one field with a
/// default, because "the feature is on" is decided by the token, the key, and
/// the sender, and an origin without those three is still half a feature.
fn waitlist_from(
    admin_token: Option<String>,
    resend_api_key: Option<String>,
    invite_from: Option<String>,
    allowed_origin: Option<String>,
) -> Result<Option<WaitlistConfig>, ConfigError> {
    if admin_token.is_none()
        && resend_api_key.is_none()
        && invite_from.is_none()
        && allowed_origin.is_none()
    {
        return Ok(None);
    }
    let (Some(admin_token), Some(resend_api_key), Some(invite_from)) =
        (admin_token, resend_api_key, invite_from)
    else {
        return Err(ConfigError::invalid(
            "SQUELCH_CONTROL_ADMIN_TOKEN, SQUELCH_CONTROL_RESEND_API_KEY and \
             SQUELCH_CONTROL_INVITE_FROM must be set together \
             (SQUELCH_CONTROL_WAITLIST_ORIGIN is optional); set all three or none",
        ));
    };
    if admin_token.len() < MIN_ADMIN_TOKEN_LEN {
        return Err(ConfigError::invalid(format!(
            "SQUELCH_CONTROL_ADMIN_TOKEN is {} characters; at least {MIN_ADMIN_TOKEN_LEN} are required. Generate one with `openssl rand -base64 32`",
            admin_token.len()
        )));
    }
    // The key becomes an Authorization header. Held to printable ASCII here so
    // a pasted key with a stray newline in it fails at boot rather than as an
    // unsendable request on the first approval.
    if !(1..=MAX_RESEND_API_KEY_LEN).contains(&resend_api_key.len())
        || !resend_api_key.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(ConfigError::invalid(
            "invalid SQUELCH_CONTROL_RESEND_API_KEY: expected a Resend API key (printable ASCII, \
             no spaces), e.g. re_...",
        ));
    }
    // The sender lands in a mail header at Resend, so a control character or a
    // line break is refused rather than passed on, and it must at least be
    // shaped like an address: `invites@passband.app` or
    // `Passband <invites@passband.app>`.
    let from_ok = (3..=MAX_INVITE_FROM_LEN).contains(&invite_from.len())
        && invite_from.contains('@')
        && invite_from
            .bytes()
            .all(|b| b == b' ' || b.is_ascii_graphic());
    if !from_ok {
        return Err(ConfigError::invalid(format!(
            "invalid SQUELCH_CONTROL_INVITE_FROM `{invite_from}`: expected an address on a domain \
             verified at Resend, e.g. `Passband <invites@passband.app>`"
        )));
    }
    let allowed_origin = match allowed_origin {
        None => DEFAULT_WAITLIST_ORIGIN.to_string(),
        Some(raw) => canonical_origin("SQUELCH_CONTROL_WAITLIST_ORIGIN", &raw)?,
    };
    Ok(Some(WaitlistConfig {
        admin_token,
        resend_api_key,
        invite_from,
        allowed_origin,
        resend_url: RESEND_URL.to_string(),
    }))
}

/// Parse one monthly budget: money, positive, and no bigger than the typo
/// ceiling. One function for both keys' budgets so the bar cannot drift.
fn parse_budget(name: &str, raw: Option<String>, default: f64) -> Result<f64, ConfigError> {
    match raw {
        None => Ok(default),
        Some(v) => {
            let usd: f64 = v
                .parse()
                .map_err(|e| ConfigError::invalid(format!("invalid {name} `{v}`: {e}")))?;
            if !usd.is_finite() || usd <= 0.0 || usd > MAX_LLM_BUDGET_USD {
                return Err(ConfigError::invalid(format!(
                    "{name} `{v}` must be a positive amount no larger than {MAX_LLM_BUDGET_USD}"
                )));
            }
            Ok(usd)
        }
    }
}

/// Parse a comma-separated model allow-list. Empty segments (a trailing
/// comma) are tolerated; what remains must be at least one name, each held to
/// the same allowlist bar as every other value that lands in an outbound
/// request: model ids are made of letters, digits, `-`, `_`, `.` and nothing
/// that could restructure JSON or a log line. An EMPTY list is refused rather
/// than passed through, because on the live gateway empty `allowed_models` is
/// deny-all.
fn parse_models(name: &str, raw: &str) -> Result<Vec<String>, ConfigError> {
    let ok = |m: &str| {
        m.len() <= MAX_LLM_MODEL_LEN
            && m.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    };
    let models: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect();
    if models.is_empty() {
        return Err(ConfigError::invalid(format!(
            "invalid {name} `{raw}`: at least one model is required (an empty \
             allow-list would deny every call)"
        )));
    }
    if let Some(bad) = models.iter().find(|m| !ok(m)) {
        return Err(ConfigError::invalid(format!(
            "invalid {name} entry `{bad}`: expected a model id made of \
             letters, digits, `-`, `_`, `.`"
        )));
    }
    Ok(models)
}

/// Parse, validate, and canonicalize an origin (`https://host[:port]`).
///
/// A path, query, fragment, or userinfo would each survive into a redirect URI
/// or an outbound request, so every one is a refusal to boot rather than
/// something quietly stripped. The canonical form comes from the parser
/// (lowercased host, default port dropped).
fn canonical_origin(name: &str, raw: &str) -> Result<String, ConfigError> {
    let url = url::Url::parse(raw)
        .map_err(|e| ConfigError::invalid(format!("invalid {name} `{raw}`: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::invalid(format!(
            "invalid {name} `{raw}`: expected an http(s) URL"
        )));
    }
    if url.host_str().is_none() {
        return Err(ConfigError::invalid(format!(
            "invalid {name} `{raw}`: no host"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::invalid(format!(
            "invalid {name}: userinfo is not allowed in a base URL"
        )));
    }
    if !matches!(url.path(), "" | "/") {
        return Err(ConfigError::invalid(format!(
            "invalid {name} `{raw}`: expected an origin with no path"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ConfigError::invalid(format!(
            "invalid {name} `{raw}`: expected no query or fragment"
        )));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// Validate the hosted base domain. It is interpolated into every tenant URL
/// and every deep link, so anything that is not a plain DNS name — a scheme, a
/// port, a path, a leading dot, an underscore — is refused at boot.
fn canonical_domain(raw: &str) -> Result<String, ConfigError> {
    let d = raw.trim().trim_end_matches('.').to_lowercase();
    let ok = d.len() <= 253
        && d.split('.').count() >= 2
        && d.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    if !ok {
        return Err(ConfigError::invalid(format!(
            "invalid SQUELCH_CONTROL_BASE_DOMAIN `{raw}`: expected a bare DNS name like passband.email"
        )));
    }
    Ok(d)
}

/// Decode the cookie key from base64 (standard or url-safe, padded or not) or
/// hex, and hold it to [`MIN_COOKIE_KEY_BYTES`].
///
/// Several encodings are accepted because the operator generates this with
/// whichever tool is on the box, and a key that "works" only because it was
/// read as ASCII bytes would silently have a quarter of the entropy it looks
/// like. The refusal names the fix.
fn decode_cookie_key(raw: &str) -> Result<Vec<u8>, ConfigError> {
    let bytes = decode_key_material(raw).ok_or_else(|| {
        ConfigError::invalid(
            "invalid SQUELCH_CONTROL_COOKIE_KEY: expected base64 or hex; generate one with `openssl rand -base64 48`",
        )
    })?;
    if bytes.len() < MIN_COOKIE_KEY_BYTES {
        return Err(ConfigError::invalid(format!(
            "SQUELCH_CONTROL_COOKIE_KEY decodes to {} bytes; at least {MIN_COOKIE_KEY_BYTES} are required. Generate one with `openssl rand -base64 48`",
            bytes.len()
        )));
    }
    Ok(bytes)
}

fn decode_key_material(raw: &str) -> Option<Vec<u8>> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    // Hex first, and only when it is unambiguous: an even-length all-hex string
    // is never meant as base64 in practice, and reading it as base64 would
    // halve the entropy the operator thinks they configured.
    if s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        let mut out = Vec::with_capacity(s.len() / 2);
        for chunk in s.as_bytes().chunks(2) {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
        }
        return Some(out);
    }
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(s))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            bind: DEFAULT_BIND.parse().unwrap(),
            public_url: "https://signup.passband.email".into(),
            redirect_uri: "https://signup.passband.email/oauth/callback".into(),
            base_domain: "passband.email".into(),
            client_id: "client-id".into(),
            client_secret: "TOP-SECRET-VALUE".into(),
            cookie_key: vec![7; 32],
            warden_url: "https://warden.passband.email".into(),
            warden_token: "WARDEN-BEARER-VALUE".into(),
            db_path: PathBuf::from(":memory:"),
            trusted_proxy_hops: 0,
            token_url: GOOGLE_TOKEN_URL.into(),
            auth_url: GOOGLE_AUTH_URL.into(),
            profile_url: GMAIL_PROFILE_URL.into(),
            userinfo_url: GOOGLE_USERINFO_URL.into(),
            bifrost: Some(BifrostConfig {
                url: "https://bifrost.passband.email".into(),
                admin_token: "admin:BIFROST-ADMIN-VALUE".into(),
                budget_usd: DEFAULT_LLM_BUDGET_USD,
                models: vec!["claude-haiku-4-5".into()],
                assistant_budget_usd: DEFAULT_ASSISTANT_BUDGET_USD,
                assistant_models: vec!["claude-opus-4-8".into()],
            }),
            waitlist: Some(WaitlistConfig {
                admin_token: "ADMIN-PAGE-TOKEN-VALUE".into(),
                resend_api_key: "RESEND-API-KEY-VALUE".into(),
                invite_from: "Passband <invites@passband.app>".into(),
                allowed_origin: DEFAULT_WAITLIST_ORIGIN.into(),
                resend_url: RESEND_URL.into(),
            }),
        }
    }

    #[test]
    fn canonicalizes_an_origin() {
        for (raw, want) in [
            (
                "https://signup.passband.email",
                "https://signup.passband.email",
            ),
            (
                "https://SIGNUP.Passband.Email/",
                "https://signup.passband.email",
            ),
            (
                "https://signup.passband.email:443",
                "https://signup.passband.email",
            ),
            ("http://127.0.0.1:8852", "http://127.0.0.1:8852"),
        ] {
            assert_eq!(canonical_origin("X", raw).unwrap(), want, "{raw}");
        }
    }

    #[test]
    fn refuses_an_origin_that_is_not_a_plain_origin() {
        for bad in [
            "",
            "signup.passband.email",
            "ftp://signup.passband.email",
            "https://",
            "https://signup.passband.email/signup",
            "https://signup.passband.email/?x=1",
            "https://signup.passband.email/#f",
            "https://user:pw@signup.passband.email",
        ] {
            assert!(canonical_origin("X", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn validates_the_base_domain() {
        assert_eq!(canonical_domain("Passband.Email").unwrap(), "passband.email");
        assert_eq!(
            canonical_domain("passband.email.").unwrap(),
            "passband.email"
        );
        for bad in [
            "passband",
            "https://passband.email",
            "passband.email:443",
            ".passband.email",
            "passband..email",
            "-passband.email",
            "passband.email/x",
            "pass_band.email",
        ] {
            assert!(canonical_domain(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn tenant_urls_come_from_the_configured_base_domain() {
        let mut c = sample();
        assert_eq!(c.tenant_url("ada"), "https://ada.passband.email");
        c.base_domain = "example.test".into();
        assert_eq!(c.tenant_url("ada"), "https://ada.example.test");
    }

    #[test]
    fn decodes_cookie_keys_in_the_encodings_operators_actually_produce() {
        let hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        assert_eq!(decode_cookie_key(hex).unwrap().len(), 32);
        let b64 = base64::engine::general_purpose::STANDARD.encode([9u8; 48]);
        assert_eq!(decode_cookie_key(&b64).unwrap().len(), 48);
        let b64url = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([9u8; 32]);
        assert_eq!(decode_cookie_key(&b64url).unwrap().len(), 32);
    }

    /// A short key is a refusal to serve, not a warning: it is the only thing
    /// standing between a forged cookie and a signup session.
    #[test]
    fn refuses_a_short_or_unparseable_cookie_key() {
        for bad in ["", "   ", "not base64 !!!", "abcd"] {
            assert!(decode_cookie_key(bad).is_err(), "{bad:?}");
        }
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        assert!(decode_cookie_key(&short).is_err());
    }

    /// Every secret must not be formattable out of the struct.
    #[test]
    fn debug_redacts_every_secret() {
        let rendered = format!("{:?}", sample());
        assert!(!rendered.contains("TOP-SECRET-VALUE"), "{rendered}");
        assert!(!rendered.contains("WARDEN-BEARER-VALUE"), "{rendered}");
        assert!(!rendered.contains("BIFROST-ADMIN-VALUE"), "{rendered}");
        assert!(!rendered.contains("ADMIN-PAGE-TOKEN-VALUE"), "{rendered}");
        assert!(!rendered.contains("RESEND-API-KEY-VALUE"), "{rendered}");
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("<32 bytes>"));
    }

    /// The Bifrost settings are a unit: url + credential is the feature on,
    /// none set is off, and anything in between is a refusal to boot rather
    /// than a silent off. The budgets and the model lists are the four fields
    /// with defaults.
    #[test]
    fn the_bifrost_settings_are_all_or_nothing() {
        let some = |s: &str| Some(s.to_string());
        let token = format!("admin:{}", "b".repeat(MIN_BIFROST_TOKEN_LEN));

        assert!(
            bifrost_from(None, None, None, None, None, None).unwrap().is_none(),
            "off"
        );

        let on = bifrost_from(some("https://bifrost.example"), some(&token), None, None, None, None)
            .unwrap()
            .expect("url + credential switches the feature on");
        assert_eq!(on.url, "https://bifrost.example");
        assert_eq!(on.budget_usd, DEFAULT_LLM_BUDGET_USD);
        // The default model lists are the product's, and never empty.
        assert_eq!(on.models, vec!["claude-haiku-4-5", "claude-sonnet-5"]);
        // The assistant key gets its own defaults: a bigger budget and a
        // frontier model where triage runs a cheap one.
        assert_eq!(on.assistant_budget_usd, DEFAULT_ASSISTANT_BUDGET_USD);
        assert_eq!(on.assistant_models, vec!["claude-haiku-4-5", "claude-opus-4-8"]);

        let on = bifrost_from(
            some("https://bifrost.example"),
            some(&token),
            some("12.5"),
            some("claude-opus-4-1, claude-haiku-4-5,"),
            some("25"),
            some("claude-opus-4-8,"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(on.budget_usd, 12.5);
        assert_eq!(on.models, vec!["claude-opus-4-1", "claude-haiku-4-5"]);
        assert_eq!(on.assistant_budget_usd, 25.0);
        assert_eq!(on.assistant_models, vec!["claude-opus-4-8"]);

        // Every partial combination refuses.
        assert!(bifrost_from(some("https://bifrost.example"), None, None, None, None, None).is_err());
        assert!(bifrost_from(None, some(&token), None, None, None, None).is_err());
        assert!(bifrost_from(None, None, some("5"), None, None, None).is_err());
        assert!(bifrost_from(None, None, None, some("claude-haiku-4-5"), None, None).is_err());
        assert!(bifrost_from(some("https://bifrost.example"), None, some("5"), None, None, None).is_err());
        assert!(bifrost_from(None, some(&token), some("5"), None, None, None).is_err());
        assert!(
            bifrost_from(None, some(&token), None, some("claude-haiku-4-5"), None, None).is_err(),
            "models without the url is still a half-configured feature"
        );
        // The assistant knobs are held to the same unit: alone, each is a
        // half-configured feature, not a silent off.
        assert!(bifrost_from(None, None, None, None, some("10"), None).is_err());
        assert!(bifrost_from(None, None, None, None, None, some("claude-opus-4-8")).is_err());
    }

    /// The values themselves are held to the same bar the rest of the config
    /// is: https only, a real `username:password`, a budget that is money and
    /// not a typo, a model list that could not restructure a request.
    #[test]
    fn the_bifrost_settings_are_validated() {
        let some = |s: &str| Some(s.to_string());
        let token = format!("admin:{}", "b".repeat(MIN_BIFROST_TOKEN_LEN));

        assert!(bifrost_from(some("http://bifrost.example"), some(&token), None, None, None, None).is_err());
        assert!(bifrost_from(some("not a url"), some(&token), None, None, None, None).is_err());
        let short = format!("a:{}", "b".repeat(MIN_BIFROST_TOKEN_LEN - 3));
        assert!(bifrost_from(some("https://bifrost.example"), some(&short), None, None, None, None).is_err());
        // The credential is Basic material: `username:password`, exactly one
        // colon, both halves nonempty. A pasted session bearer (no colon)
        // must fail at boot, not as a 401 weeks later.
        for bad in [
            "b".repeat(MIN_BIFROST_TOKEN_LEN),                    // no colon: a bearer
            format!(":{}", "b".repeat(MIN_BIFROST_TOKEN_LEN)),    // empty username
            format!("{}:", "b".repeat(MIN_BIFROST_TOKEN_LEN)),    // empty password
            format!("a:b:{}", "c".repeat(MIN_BIFROST_TOKEN_LEN)), // two colons
        ] {
            assert!(
                bifrost_from(some("https://bifrost.example"), some(&bad), None, None, None, None)
                    .is_err(),
                "{bad:?}"
            );
        }
        // Both budgets are held to the same money-not-a-typo bar.
        for bad in ["nonsense", "0", "-5", "NaN", "inf", "1000000"] {
            assert!(
                bifrost_from(some("https://bifrost.example"), some(&token), some(bad), None, None, None)
                    .is_err(),
                "{bad:?}"
            );
            assert!(
                bifrost_from(some("https://bifrost.example"), some(&token), None, None, some(bad), None)
                    .is_err(),
                "assistant: {bad:?}"
            );
        }
        // A model list that is empty once parsed, or carries a name that
        // could restructure a request, refuses to boot — either list.
        for bad in ["", " , ,", "claude haiku", "claude/../x", "mod\"el"] {
            assert!(
                bifrost_from(some("https://bifrost.example"), some(&token), None, some(bad), None, None)
                    .is_err(),
                "{bad:?}"
            );
            assert!(
                bifrost_from(some("https://bifrost.example"), some(&token), None, None, None, some(bad))
                    .is_err(),
                "assistant: {bad:?}"
            );
        }
    }

    /// The waitlist settings are a unit: token + key + sender is the feature
    /// on, none set is off (the routes are not mounted at all), and anything in
    /// between is a refusal to boot. The origin is the one field with a
    /// default.
    #[test]
    fn the_waitlist_settings_are_all_or_nothing() {
        let some = |s: &str| Some(s.to_string());
        let token = "t".repeat(MIN_ADMIN_TOKEN_LEN);
        let key = "re_the_sending_key";
        let from = "Passband <invites@passband.app>";

        assert!(
            waitlist_from(None, None, None, None).unwrap().is_none(),
            "off"
        );

        let on = waitlist_from(some(&token), some(key), some(from), None)
            .unwrap()
            .expect("token + key + sender switches the feature on");
        assert_eq!(on.admin_token, token);
        assert_eq!(on.invite_from, from);
        assert_eq!(on.allowed_origin, DEFAULT_WAITLIST_ORIGIN);
        // Pinned, never read from the environment.
        assert_eq!(on.resend_url, RESEND_URL);

        let on = waitlist_from(
            some(&token),
            some(key),
            some(from),
            some("https://Staging.Passband.App/"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(on.allowed_origin, "https://staging.passband.app");

        // Every partial combination refuses.
        assert!(waitlist_from(some(&token), None, None, None).is_err());
        assert!(waitlist_from(None, some(key), None, None).is_err());
        assert!(waitlist_from(None, None, some(from), None).is_err());
        assert!(waitlist_from(None, None, None, some("https://passband.app")).is_err());
        assert!(waitlist_from(some(&token), some(key), None, None).is_err());
        assert!(waitlist_from(some(&token), None, some(from), None).is_err());
        assert!(waitlist_from(None, some(key), some(from), None).is_err());
        assert!(
            waitlist_from(None, None, None, some("https://passband.app")).is_err(),
            "an origin on its own is still a half-configured feature"
        );
    }

    /// The values are held to the same bar as the rest of the config: a token
    /// long enough to be the whole authentication of a public admin page, a key
    /// that can be a header, a sender that can be a mail header, and an origin
    /// that is an origin.
    #[test]
    fn the_waitlist_settings_are_validated() {
        let some = |s: &str| Some(s.to_string());
        let token = "t".repeat(MIN_ADMIN_TOKEN_LEN);
        let key = "re_the_sending_key";
        let from = "Passband <invites@passband.app>";

        let short = "t".repeat(MIN_ADMIN_TOKEN_LEN - 1);
        assert!(waitlist_from(some(&short), some(key), some(from), None).is_err());

        // A key that could not be an Authorization header.
        for bad in ["re_key with spaces", "re_key\nX-Evil: 1", &"k".repeat(300)] {
            assert!(
                waitlist_from(some(&token), some(bad), some(from), None).is_err(),
                "{bad:?}"
            );
        }
        // A sender that is not an address, or that carries a line break into a
        // mail header.
        for bad in [
            "Passband",
            "a@",
            "Passband <invites@passband.app>\r\nBcc: someone@example.com",
        ] {
            assert!(
                waitlist_from(some(&token), some(key), some(bad), None).is_err(),
                "{bad:?}"
            );
        }
        // A bare address with no display name is fine.
        assert!(waitlist_from(some(&token), some(key), some("invites@passband.app"), None).is_ok());
        // The origin goes through the same canonicalization as every other
        // origin here: no path, no query, no userinfo.
        for bad in [
            "passband.app",
            "https://passband.app/waitlist",
            "https://passband.app/?x=1",
        ] {
            assert!(
                waitlist_from(some(&token), some(key), some(from), some(bad)).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn flags_a_plaintext_origin() {
        let mut c = sample();
        assert!(!c.is_insecure());
        c.public_url = "http://127.0.0.1:8852".into();
        assert!(c.is_insecure());
    }
}
