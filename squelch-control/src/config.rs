//! Startup configuration, read once from the environment and validated before
//! the listener binds. Every bad value is a refusal to start, never a default:
//! a control plane that guesses its own redirect URI rejects every consent it
//! will ever see, and one that guesses its base domain hands out tenant URLs
//! that resolve to somebody else.
//!
//! THREE FIELDS HERE ARE SECRETS — the OAuth client secret, the cookie key, and
//! the warden bearer — so [`Config`] has a HAND-WRITTEN `Debug` that redacts
//! them. A derived one would put all three in any `tracing::debug!` that ever
//! formats the config, and that is exactly the line nobody notices adding.

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
            .field("userinfo_url", &self.userinfo_url)
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

        let warden_url = canonical_origin(
            "SQUELCH_CONTROL_WARDEN_URL",
            &require(
                "SQUELCH_CONTROL_WARDEN_URL",
                "the warden's base URL, e.g. https://warden.passband.email",
            )?,
        )?;
        let warden_token = require(
            "SQUELCH_CONTROL_WARDEN_TOKEN",
            "the bearer the warden expects; must match SQUELCH_WARDEN_TOKEN in the cluster",
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
        assert_eq!(
            canonical_domain("Passband.Email").unwrap(),
            "passband.email"
        );
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

    /// The three secrets must not be formattable out of the struct.
    #[test]
    fn debug_redacts_every_secret() {
        let rendered = format!("{:?}", sample());
        assert!(!rendered.contains("TOP-SECRET-VALUE"), "{rendered}");
        assert!(!rendered.contains("WARDEN-BEARER-VALUE"), "{rendered}");
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("<32 bytes>"));
    }

    #[test]
    fn flags_a_plaintext_origin() {
        let mut c = sample();
        assert!(!c.is_insecure());
        c.public_url = "http://127.0.0.1:8852".into();
        assert!(c.is_insecure());
    }
}
