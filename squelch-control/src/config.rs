//! Startup configuration, read once from the environment and validated before
//! the listener binds. Every bad value is a refusal to start, never a default:
//! a control plane that guesses its own redirect URI rejects every consent it
//! will ever see, and one that guesses its base domain hands out tenant URLs
//! that resolve to somebody else.
//!
//! FOUR FIELDS HERE ARE SECRETS — the OAuth client secret, the cookie key, the
//! warden bearer, and the Bifrost admin token — so [`Config`] (and
//! [`BifrostConfig`] inside it) has a HAND-WRITTEN `Debug` that redacts them. A
//! derived one would put all four in any `tracing::debug!` that ever formats
//! the config, and that is exactly the line nobody notices adding.

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

/// Minimum Bifrost admin-token length, the same bar the warden holds its own
/// bearer to: below this the token stops being the thing that decides who can
/// mint unbounded LLM spend.
pub const MIN_BIFROST_TOKEN_LEN: usize = 32;

/// The monthly spend a tenant's virtual key is minted with when
/// `SQUELCH_CONTROL_LLM_BUDGET_USD` says nothing.
pub const DEFAULT_LLM_BUDGET_USD: f64 = 5.00;

/// Ceiling on the configured budget. Same reasoning as the proxy-hop cap: no
/// tenant's triage costs four figures a month, a larger number is a typo, and
/// this typo degrades into real money.
pub const MAX_LLM_BUDGET_USD: f64 = 1_000.0;

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
    /// The Bifrost LLM gateway, when this deployment has one. `None` means the
    /// feature is OFF and signup provisions tenants with no LLM key at all;
    /// a partial trio is a refusal to boot, never a silent off.
    pub bifrost: Option<BifrostConfig>,
}

/// The Bifrost governance trio: where the gateway is, the admin bearer that
/// mints virtual keys, and the monthly budget each tenant's key is minted
/// with. `admin_token` is a secret; the hand-written `Debug` redacts it.
#[derive(Clone)]
pub struct BifrostConfig {
    /// Base URL of the gateway's governance API, canonical https origin.
    pub url: String,
    /// Admin bearer presented on every governance call. Redacted from `Debug`.
    pub admin_token: String,
    /// Monthly budget, USD, stamped on every minted virtual key.
    pub budget_usd: f64,
}

impl std::fmt::Debug for BifrostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BifrostConfig")
            .field("url", &self.url)
            .field("admin_token", &"<redacted>")
            .field("budget_usd", &self.budget_usd)
            .finish()
    }
}

impl std::fmt::Debug for Config {
    /// Hand-written so the three secrets can never ride out in a formatted
    /// config. The cookie key shows its LENGTH, which is the only property
    /// anyone debugging it needs.
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
            // BifrostConfig's own Debug redacts the admin token.
            .field("bifrost", &self.bifrost)
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
            bifrost,
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
    /// The Bifrost trio from the environment: `Ok(None)` when the feature is
    /// off, an error when it is half-configured. For the `llm` operator
    /// commands, which need this and the warden pair and nothing else.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        bifrost_from(
            var("SQUELCH_CONTROL_BIFROST_URL"),
            var("SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN"),
            var("SQUELCH_CONTROL_LLM_BUDGET_USD"),
        )
    }
}

/// Validate the Bifrost trio, all-or-nothing.
///
/// None of the three set means the feature is OFF, which is a legal deployment
/// (self-host, or hosted before the gateway exists). Some-but-not-all is a
/// REFUSAL TO BOOT: a control plane that quietly ran without the gateway would
/// provision every tenant keyless and nobody would notice until the first
/// triage call failed, weeks of signups later. The budget alone is the one
/// field with a default, because "the trio is on" is decided by the url and
/// the token, and a budget without either is still a half-configured feature.
fn bifrost_from(
    url: Option<String>,
    admin_token: Option<String>,
    budget: Option<String>,
) -> Result<Option<BifrostConfig>, ConfigError> {
    if url.is_none() && admin_token.is_none() && budget.is_none() {
        return Ok(None);
    }
    let (Some(url), Some(admin_token)) = (url, admin_token) else {
        return Err(ConfigError::invalid(
            "SQUELCH_CONTROL_BIFROST_URL and SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN must be set \
             together (SQUELCH_CONTROL_LLM_BUDGET_USD is optional); set both or neither",
        ));
    };
    let url = canonical_origin("SQUELCH_CONTROL_BIFROST_URL", &url)?;
    // https only, no local-dev exception: what rides this connection is an
    // admin bearer that mints spend and, coming back, a live per-tenant key.
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
    let budget_usd = match budget {
        None => DEFAULT_LLM_BUDGET_USD,
        Some(v) => {
            let usd: f64 = v.parse().map_err(|e| {
                ConfigError::invalid(format!("invalid SQUELCH_CONTROL_LLM_BUDGET_USD `{v}`: {e}"))
            })?;
            if !usd.is_finite() || usd <= 0.0 || usd > MAX_LLM_BUDGET_USD {
                return Err(ConfigError::invalid(format!(
                    "SQUELCH_CONTROL_LLM_BUDGET_USD `{v}` must be a positive amount no larger than {MAX_LLM_BUDGET_USD}"
                )));
            }
            usd
        }
    };
    Ok(Some(BifrostConfig {
        url,
        admin_token,
        budget_usd,
    }))
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
            bifrost: Some(BifrostConfig {
                url: "https://bifrost.passband.email".into(),
                admin_token: "BIFROST-ADMIN-VALUE".into(),
                budget_usd: DEFAULT_LLM_BUDGET_USD,
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

    /// The four secrets must not be formattable out of the struct.
    #[test]
    fn debug_redacts_every_secret() {
        let rendered = format!("{:?}", sample());
        assert!(!rendered.contains("TOP-SECRET-VALUE"), "{rendered}");
        assert!(!rendered.contains("WARDEN-BEARER-VALUE"), "{rendered}");
        assert!(!rendered.contains("BIFROST-ADMIN-VALUE"), "{rendered}");
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("<32 bytes>"));
    }

    /// The Bifrost trio is a unit: all set is the feature on, none is off, and
    /// anything in between is a refusal to boot rather than a silent off.
    #[test]
    fn the_bifrost_trio_is_all_or_nothing() {
        let some = |s: &str| Some(s.to_string());
        let token = "b".repeat(MIN_BIFROST_TOKEN_LEN);

        assert!(bifrost_from(None, None, None).unwrap().is_none(), "off");

        let on = bifrost_from(some("https://bifrost.example"), some(&token), None)
            .unwrap()
            .expect("url + token switches the feature on");
        assert_eq!(on.url, "https://bifrost.example");
        assert_eq!(on.budget_usd, DEFAULT_LLM_BUDGET_USD);

        let on = bifrost_from(some("https://bifrost.example"), some(&token), some("12.5"))
            .unwrap()
            .unwrap();
        assert_eq!(on.budget_usd, 12.5);

        // Every partial combination refuses.
        assert!(bifrost_from(some("https://bifrost.example"), None, None).is_err());
        assert!(bifrost_from(None, some(&token), None).is_err());
        assert!(bifrost_from(None, None, some("5")).is_err());
        assert!(bifrost_from(some("https://bifrost.example"), None, some("5")).is_err());
        assert!(bifrost_from(None, some(&token), some("5")).is_err());
    }

    /// The values themselves are held to the same bar the rest of the config
    /// is: https only, a real token, a budget that is money and not a typo.
    #[test]
    fn the_bifrost_trio_is_validated() {
        let some = |s: &str| Some(s.to_string());
        let token = "b".repeat(MIN_BIFROST_TOKEN_LEN);

        assert!(bifrost_from(some("http://bifrost.example"), some(&token), None).is_err());
        assert!(bifrost_from(some("not a url"), some(&token), None).is_err());
        let short = "b".repeat(MIN_BIFROST_TOKEN_LEN - 1);
        assert!(bifrost_from(some("https://bifrost.example"), some(&short), None).is_err());
        for bad in ["nonsense", "0", "-5", "NaN", "inf", "1000000"] {
            assert!(
                bifrost_from(some("https://bifrost.example"), some(&token), some(bad)).is_err(),
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
