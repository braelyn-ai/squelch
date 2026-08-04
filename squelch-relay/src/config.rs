//! Startup configuration, read once from the environment. Every field is
//! validated at load time and the process refuses to start on a bad value.
//!
//! The `.p8` PEM is held in memory for the life of the process: never logged,
//! never echoed in an error, and it leaves only as an ES256 signature.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Loopback by design: TLS is terminated by a proxy in front of this listener,
/// so binding a public interface would serve the relay in the clear.
pub const DEFAULT_BIND: &str = "127.0.0.1:8850";

/// Which APNs host to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Production,
    Sandbox,
}

impl Environment {
    /// Parse the wire/env spelling. Accepted values are exactly these two.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "production" => Some(Self::Production),
            "sandbox" => Some(Self::Sandbox),
            _ => None,
        }
    }

    /// The APNs base URL for this environment (no trailing slash).
    pub fn base_url(self) -> &'static str {
        match self {
            Self::Production => "https://api.push.apple.com",
            Self::Sandbox => "https://api.sandbox.push.apple.com",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Sandbox => "sandbox",
        }
    }
}

/// Why the relay refused to start.
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

/// Shortest accepted `SQUELCH_RELAY_AUTH_TOKEN`: a constant-time compare
/// against a four-character token guards nothing.
pub const MIN_AUTH_TOKEN_LEN: usize = 32;

/// Validated startup configuration.
#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// The APNs signing key as a PKCS#8 PEM. SECRET: never log or serialize.
    pub apns_key_pem: String,
    pub apns_key_id: String,
    pub apns_team_id: String,
    /// Allowed `apns-topic` values. Non-empty; the FIRST entry is the default
    /// used when a request omits `topic`.
    pub apns_topics: Vec<String>,
    pub apns_env: Environment,
    /// Bearer token for `POST /v1/push`. `None` serves the route open
    /// (rate-limited only), which `from_env` produces only under an explicit
    /// `SQUELCH_RELAY_ALLOW_ANONYMOUS=1`.
    pub auth_token: Option<String>,
    /// TEST-ONLY base-URL override for the APNs host; wins over `apns_env`'s
    /// host. Production deployments must leave this unset.
    pub apns_url_override: Option<String>,
    /// Where the open buffer lives. `None` keeps it in memory, so anything the
    /// daemon has not drained is lost on restart.
    pub db_path: Option<PathBuf>,
}

/// Hand-written so neither the `.p8` nor the bearer token can reach a log
/// through a stray `{:?}`.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("apns_key_pem", &"<redacted>")
            .field("apns_key_id", &self.apns_key_id)
            .field("apns_team_id", &self.apns_team_id)
            .field("apns_topics", &self.apns_topics)
            .field("apns_env", &self.apns_env)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .field("apns_url_override", &self.apns_url_override)
            .field("db_path", &self.db_path)
            .finish()
    }
}

/// Read a var, treating whitespace-only as unset.
fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Config {
    /// Load and validate from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_raw = var("SQUELCH_RELAY_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind: SocketAddr = bind_raw.parse().map_err(|e| {
            ConfigError::invalid(format!("invalid SQUELCH_RELAY_BIND `{bind_raw}`: {e}"))
        })?;

        // Exactly one key source: two leave "which one is live?" ambiguous.
        let key_path = var("SQUELCH_RELAY_APNS_KEY_PATH").map(PathBuf::from);
        let key_inline = var("SQUELCH_RELAY_APNS_KEY");
        let apns_key_pem = match (key_path, key_inline) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::invalid(
                    "set exactly one of SQUELCH_RELAY_APNS_KEY_PATH or SQUELCH_RELAY_APNS_KEY, not both",
                ));
            }
            (None, None) => {
                return Err(ConfigError::invalid(
                    "set SQUELCH_RELAY_APNS_KEY_PATH (path to the .p8) or SQUELCH_RELAY_APNS_KEY (inline PEM)",
                ));
            }
            (Some(p), None) => std::fs::read_to_string(&p).map_err(|e| {
                ConfigError::invalid(format!(
                    "cannot read SQUELCH_RELAY_APNS_KEY_PATH `{}`: {e}",
                    p.display()
                ))
            })?,
            // Secret managers flatten a pasted PEM onto one line with literal
            // `\n` sequences. A real PEM has no backslash, so with no actual
            // newlines these are unambiguously mangled framing — undo it.
            (None, Some(pem)) if !pem.contains('\n') && pem.contains("\\n") => {
                pem.replace("\\n", "\n")
            }
            (None, Some(pem)) => pem,
        };

        let apns_key_id = var("SQUELCH_RELAY_APNS_KEY_ID")
            .ok_or_else(|| ConfigError::invalid("SQUELCH_RELAY_APNS_KEY_ID is required"))?;
        let apns_team_id = var("SQUELCH_RELAY_APNS_TEAM_ID")
            .ok_or_else(|| ConfigError::invalid("SQUELCH_RELAY_APNS_TEAM_ID is required"))?;

        let topics_raw = var("SQUELCH_RELAY_APNS_TOPICS").ok_or_else(|| {
            ConfigError::invalid(
                "SQUELCH_RELAY_APNS_TOPICS is required (comma-separated; the first is the default)",
            )
        })?;
        let apns_topics = parse_topics(&topics_raw)?;

        let apns_env = match var("SQUELCH_RELAY_APNS_ENV") {
            None => Environment::Production,
            Some(s) => Environment::parse(&s).ok_or_else(|| {
                ConfigError::invalid(format!(
                    "invalid SQUELCH_RELAY_APNS_ENV `{s}`: expected `production` or `sandbox`"
                ))
            })?,
        };

        // Serving the push route open is legitimate but must be a TYPED choice:
        // a forgotten variable must never silently expose the fan-out to the
        // internet behind nothing but one shared per-IP bucket.
        let allow_anonymous = match var("SQUELCH_RELAY_ALLOW_ANONYMOUS") {
            None => false,
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" => true,
                "0" | "false" | "no" => false,
                other => {
                    return Err(ConfigError::invalid(format!(
                        "invalid SQUELCH_RELAY_ALLOW_ANONYMOUS `{other}`: expected `1` or `0`"
                    )));
                }
            },
        };
        let auth_token = match (var("SQUELCH_RELAY_AUTH_TOKEN"), allow_anonymous) {
            (Some(_), true) => {
                return Err(ConfigError::invalid(
                    "SQUELCH_RELAY_AUTH_TOKEN is set alongside SQUELCH_RELAY_ALLOW_ANONYMOUS=1; pick one",
                ));
            }
            (None, false) => {
                return Err(ConfigError::invalid(
                    "SQUELCH_RELAY_AUTH_TOKEN is required (set SQUELCH_RELAY_ALLOW_ANONYMOUS=1 to deliberately serve /v1/push open; /v1/opens is then not served at all)",
                ));
            }
            (None, true) => None,
            (Some(t), false) => {
                if t.len() < MIN_AUTH_TOKEN_LEN {
                    return Err(ConfigError::invalid(format!(
                        "SQUELCH_RELAY_AUTH_TOKEN must be at least {MIN_AUTH_TOKEN_LEN} characters"
                    )));
                }
                // A token carrying whitespace or a control character can never
                // be presented: it cannot go in an HTTP header. `var()` trims
                // the ends, so what is left is interior — a secret pasted across
                // two lines, which is exactly the shape a wrapped copy produces.
                // Booting anyway would serve a relay that 401s every client
                // forever, with a bare 401 and nothing in the logs to explain it.
                if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(ConfigError::invalid(
                        "SQUELCH_RELAY_AUTH_TOKEN contains whitespace or a control character, so it can never be sent as a bearer header; re-set it as one unbroken line",
                    ));
                }
                Some(t)
            }
        };

        let apns_url_override = match var("SQUELCH_RELAY_APNS_URL_OVERRIDE") {
            None => None,
            Some(u) => {
                if !(u.starts_with("http://") || u.starts_with("https://")) {
                    return Err(ConfigError::invalid(format!(
                        "invalid SQUELCH_RELAY_APNS_URL_OVERRIDE `{u}`: expected an http(s) base URL"
                    )));
                }
                Some(u.trim_end_matches('/').to_string())
            }
        };

        let db_path = var("SQUELCH_RELAY_DB_PATH").map(PathBuf::from);

        Ok(Self {
            bind,
            apns_key_pem,
            apns_key_id,
            apns_team_id,
            apns_topics,
            apns_env,
            auth_token,
            apns_url_override,
            db_path,
        })
    }

    /// Resolve a requested topic against the allowlist: `None` yields the first
    /// configured topic, an unlisted topic is rejected so a caller can never
    /// push to a bundle id this deployment was not configured for. `first()`
    /// (not indexing) so a hand-built empty list is a rejected request, not a
    /// panic in the push handler.
    pub fn resolve_topic<'a>(&'a self, requested: Option<&str>) -> Option<&'a str> {
        match requested {
            None => self.apns_topics.first().map(String::as_str),
            Some(t) => self
                .apns_topics
                .iter()
                .find(|allowed| allowed.as_str() == t)
                .map(|s| s.as_str()),
        }
    }

    /// The APNs base URL in force, honouring the test-only override.
    pub fn apns_base_url(&self) -> &str {
        self.apns_url_override
            .as_deref()
            .unwrap_or_else(|| self.apns_env.base_url())
    }
}

/// Split and validate the topic allowlist. Topics become an `apns-topic`
/// header value, so they must be printable ASCII with no spaces.
fn parse_topics(raw: &str) -> Result<Vec<String>, ConfigError> {
    let topics: Vec<String> = raw
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if topics.is_empty() {
        return Err(ConfigError::invalid(
            "SQUELCH_RELAY_APNS_TOPICS is empty after parsing; expected at least one bundle id",
        ));
    }
    for t in &topics {
        if !t.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(ConfigError::invalid(format!(
                "invalid topic `{t}` in SQUELCH_RELAY_APNS_TOPICS: expected printable ASCII with no spaces"
            )));
        }
    }
    Ok(topics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(topics: &[&str]) -> Config {
        Config {
            bind: DEFAULT_BIND.parse().unwrap(),
            apns_key_pem: String::new(),
            apns_key_id: "K".into(),
            apns_team_id: "T".into(),
            apns_topics: topics.iter().map(|s| s.to_string()).collect(),
            apns_env: Environment::Production,
            auth_token: None,
            apns_url_override: None,
            db_path: None,
        }
    }

    /// A derived `Debug` would hand the `.p8` to the first
    /// `tracing::debug!(?config)` anyone ever writes.
    #[test]
    fn debug_redacts_the_key_and_the_bearer() {
        let mut c = cfg(&["dev.squelch.ios"]);
        c.apns_key_pem = "-----BEGIN PRIVATE KEY-----\nSUPERSECRETKEYBODY\n".into();
        c.auth_token = Some("supersecretbearertokenvalue00000".into());
        let s = format!("{c:?}");
        assert!(!s.contains("SUPERSECRETKEYBODY"), "PEM body leaked: {s}");
        assert!(
            !s.contains("supersecretbearertokenvalue"),
            "token leaked: {s}"
        );
        assert_eq!(s.matches("<redacted>").count(), 2);
        // The non-secret fields still print, or the impl is useless.
        assert!(s.contains("dev.squelch.ios"));
        assert!(s.contains("KEYID") || s.contains("\"K\""));
    }

    #[test]
    fn parses_topic_lists() {
        assert_eq!(parse_topics("a.b, c.d ,").unwrap(), vec!["a.b", "c.d"]);
        assert!(parse_topics("  ").is_err());
        assert!(parse_topics(",,").is_err());
        assert!(parse_topics("has space").is_err());
    }

    #[test]
    fn resolves_topic_against_allowlist() {
        let c = cfg(&["dev.squelch.ios", "dev.squelch.ios.beta"]);
        assert_eq!(c.resolve_topic(None), Some("dev.squelch.ios"));
        assert_eq!(
            c.resolve_topic(Some("dev.squelch.ios.beta")),
            Some("dev.squelch.ios.beta")
        );
        assert_eq!(c.resolve_topic(Some("com.evil.app")), None);
    }

    #[test]
    fn environment_round_trips() {
        assert_eq!(
            Environment::parse("production"),
            Some(Environment::Production)
        );
        assert_eq!(Environment::parse("sandbox"), Some(Environment::Sandbox));
        assert_eq!(Environment::parse("Production"), None);
        assert_eq!(Environment::parse(""), None);
        assert_eq!(
            Environment::Sandbox.base_url(),
            "https://api.sandbox.push.apple.com"
        );
    }

    #[test]
    fn override_wins_over_environment_host() {
        let mut c = cfg(&["dev.squelch.ios"]);
        assert_eq!(c.apns_base_url(), "https://api.push.apple.com");
        c.apns_url_override = Some("http://127.0.0.1:9".into());
        assert_eq!(c.apns_base_url(), "http://127.0.0.1:9");
    }
}
