//! Startup configuration, read once from the environment. Every field is
//! validated at load time and the process refuses to start on a bad value.
//!
//! There is nothing to redact here, and that is the design rather than an
//! oversight: the broker holds no OAuth client credentials, no signing key, and
//! no bearer token. A derived `Debug` on this struct can never leak a secret
//! because the process never has one.

use std::net::SocketAddr;

/// Loopback by design: TLS is terminated by a proxy in front of this listener,
/// so binding a public interface would serve the broker in the clear.
pub const DEFAULT_BIND: &str = "127.0.0.1:8851";

/// Why the broker refused to start.
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
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// The externally visible base URL, canonicalized with NO trailing slash
    /// (`https://auth.passband.email`). Two things depend on it: the
    /// `redirect_uri` every registration is checked against, and the `/link`
    /// URL a daemon prints. It is not derivable from `bind`, which is loopback.
    pub public_url: String,
    /// `public_url` + `/callback`, precomputed because every registration
    /// compares `redirect_uri` against it byte for byte.
    pub callback_url: String,
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
        let bind_raw = var("SQUELCH_BROKER_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind: SocketAddr = bind_raw.parse().map_err(|e| {
            ConfigError::invalid(format!("invalid SQUELCH_BROKER_BIND `{bind_raw}`: {e}"))
        })?;

        // Required, with no default: guessing it would mean guessing the
        // `redirect_uri` every daemon must match, and a wrong guess rejects
        // every registration the deployment will ever see.
        let public_raw = var("SQUELCH_BROKER_PUBLIC_URL").ok_or_else(|| {
            ConfigError::invalid(
                "SQUELCH_BROKER_PUBLIC_URL is required (the externally visible base URL, e.g. https://auth.passband.email)",
            )
        })?;
        let public_url = canonical_public_url(&public_raw)?;
        let callback_url = format!("{public_url}/callback");

        Ok(Self {
            bind,
            public_url,
            callback_url,
        })
    }

    /// The `/link` URL for a session, which is what the daemon prints and the
    /// human opens. Built here so the one place that knows the public origin is
    /// the one place that formats it.
    pub fn link_url(&self, session_id: &str) -> String {
        format!("{}/link?s={}", self.public_url, session_id)
    }

    /// Whether the public origin is plain HTTP. Legal for a local dev run and
    /// nowhere else: the auth code Google delivers to `/callback` would cross
    /// the network in the clear.
    pub fn is_insecure(&self) -> bool {
        self.public_url.starts_with("http://")
    }
}

/// Parse, validate, and canonicalize the public base URL.
///
/// A path, a query, a fragment, or userinfo would all survive into the
/// `redirect_uri` we hand daemons and into the `/link` URL we print, so each is
/// a refusal to boot rather than something quietly stripped. The canonical form
/// comes from the parser (lowercased host, default port dropped) so that a
/// registration comparing `redirect_uri` as a string is comparing against a
/// normalized value.
fn canonical_public_url(raw: &str) -> Result<String, ConfigError> {
    let url = url::Url::parse(raw).map_err(|e| {
        ConfigError::invalid(format!("invalid SQUELCH_BROKER_PUBLIC_URL `{raw}`: {e}"))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::invalid(format!(
            "invalid SQUELCH_BROKER_PUBLIC_URL `{raw}`: expected an http(s) URL"
        )));
    }
    if url.host_str().is_none() {
        return Err(ConfigError::invalid(format!(
            "invalid SQUELCH_BROKER_PUBLIC_URL `{raw}`: no host"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::invalid(
            "invalid SQUELCH_BROKER_PUBLIC_URL: userinfo is not allowed in the public base URL",
        ));
    }
    // The router mounts `/callback` and `/link` at the root, so a base URL with
    // a path prefix would advertise addresses this process does not serve.
    if !matches!(url.path(), "" | "/") {
        return Err(ConfigError::invalid(format!(
            "invalid SQUELCH_BROKER_PUBLIC_URL `{raw}`: expected an origin with no path"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ConfigError::invalid(format!(
            "invalid SQUELCH_BROKER_PUBLIC_URL `{raw}`: expected no query or fragment"
        )));
    }
    // `as_str()` is the parser's normalized serialization; the path is `/` or
    // empty by the check above, so trimming it cannot eat a real segment.
    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_a_public_origin() {
        for (raw, want) in [
            ("https://auth.passband.email", "https://auth.passband.email"),
            (
                "https://auth.passband.email/",
                "https://auth.passband.email",
            ),
            (
                "https://AUTH.Passband.Email/",
                "https://auth.passband.email",
            ),
            // The default port is not part of the canonical origin, so a
            // daemon spelling it either way still matches.
            (
                "https://auth.passband.email:443",
                "https://auth.passband.email",
            ),
            ("http://127.0.0.1:8851", "http://127.0.0.1:8851"),
        ] {
            assert_eq!(canonical_public_url(raw).unwrap(), want, "{raw}");
        }
    }

    #[test]
    fn refuses_a_public_url_that_is_not_a_plain_origin() {
        for bad in [
            "",
            "auth.passband.email",
            "ftp://auth.passband.email",
            "https://",
            "https://auth.passband.email/broker",
            "https://auth.passband.email/?x=1",
            "https://auth.passband.email/#frag",
            "https://user:pw@auth.passband.email",
        ] {
            assert!(
                canonical_public_url(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn derives_the_callback_and_link_urls() {
        let c = Config {
            bind: DEFAULT_BIND.parse().unwrap(),
            public_url: canonical_public_url("https://auth.passband.email/").unwrap(),
            callback_url: String::new(),
        };
        assert_eq!(
            format!("{}/callback", c.public_url),
            "https://auth.passband.email/callback"
        );
        assert_eq!(c.link_url("abc"), "https://auth.passband.email/link?s=abc");
        assert!(!c.is_insecure());
    }

    #[test]
    fn flags_a_plaintext_origin() {
        let c = Config {
            bind: DEFAULT_BIND.parse().unwrap(),
            public_url: "http://127.0.0.1:8851".into(),
            callback_url: "http://127.0.0.1:8851/callback".into(),
        };
        assert!(c.is_insecure());
    }
}
