//! Startup configuration, read once from the environment and validated hard
//! enough that a running warden is a warden whose paths and names all make
//! sense.
//!
//! Everything here except the bearer token is a path, a port, or a hostname.
//! The token is the one secret this process holds, so [`Config`] has a manual
//! `Debug` that redacts it: a derived one would print it the first time anyone
//! adds a `dbg!` to a handler.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Loopback by design. Caddy fronts this listener at `warden.<base>` with TLS
/// and the control plane reaches it over the public internet; binding a public
/// interface here would serve the provisioning API in the clear.
pub const DEFAULT_BIND: &str = "127.0.0.1:8852";

/// Where per-tenant state dirs live: `<tenants_dir>/<label>/`.
pub const DEFAULT_TENANTS_DIR: &str = "/var/lib/squelch/tenants";

/// Where the warden's own state file lives.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/squelch/warden";

/// Where the per-tenant systemd `EnvironmentFile`s are rendered.
pub const DEFAULT_ENV_DIR: &str = "/etc/squelch/tenants";

/// Where the per-tenant Caddy site files are rendered. The main Caddyfile
/// imports this directory with a glob; see `deploy/hosted/Caddyfile`.
pub const DEFAULT_CADDY_DIR: &str = "/etc/caddy/tenants";

/// The daemon binary the template unit runs and the pair exec invokes.
pub const DEFAULT_SQUELCHD_BIN: &str = "/usr/local/bin/squelchd";

/// util-linux `setpriv`, used to drop to the tenant user for the pair exec.
/// The same tool the docker entrypoints use, for the same reason.
pub const DEFAULT_SETPRIV_BIN: &str = "/usr/bin/setpriv";

/// `systemctl`, absolute so a PATH the unit did not set cannot pick another.
pub const DEFAULT_SYSTEMCTL_BIN: &str = "/usr/bin/systemctl";

/// `chown`, likewise absolute.
pub const DEFAULT_CHOWN_BIN: &str = "/usr/bin/chown";

/// The unprivileged account every tenant daemon runs as.
pub const DEFAULT_TENANT_USER: &str = "squelch";

/// The systemd unit that fronts the tenants, reloaded when a site file
/// changes.
pub const DEFAULT_CADDY_UNIT: &str = "caddy";

/// First loopback port handed to a tenant.
pub const DEFAULT_PORT_BASE: u16 = 9100;

/// How many ports past the base the allocator will consider. One box holds
/// tens of tenants, not thousands; a warden that has burned through 900 ports
/// has a leak, and should say so rather than wander into the ephemeral range.
pub const PORT_RANGE: u16 = 900;

/// Shortest bearer token that may be configured.
///
/// This is the credential for a service that can create a systemd unit, so a
/// short one is a boot failure rather than a warning. 32 characters is what
/// `openssl rand -base64 24` prints, and the SETUP doc tells the operator to
/// use 32 bytes.
pub const MIN_TOKEN_LEN: usize = 32;

/// Why the warden refused to start.
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
    /// The shared secret the control plane presents. Never logged, never in a
    /// response body, compared only through [`squelch_httpauth::ct_eq`].
    pub token: String,
    /// `passband.email`. Tenant hostnames are `<label>.<base_domain>`; nothing
    /// in this crate ever hardcodes the domain.
    pub base_domain: String,
    pub state_dir: PathBuf,
    pub tenants_dir: PathBuf,
    pub env_dir: PathBuf,
    pub caddy_dir: PathBuf,
    pub squelchd_bin: PathBuf,
    pub setpriv_bin: PathBuf,
    pub systemctl_bin: PathBuf,
    pub chown_bin: PathBuf,
    /// Path to the box's age identity file. The warden never reads it — it only
    /// names it in each tenant's env file so systemd can hand it to the daemon,
    /// which is the only process that decrypts. Required, because a tenant
    /// whose env file lacks it would start a daemon that cannot read its own
    /// credentials and would write the next one in plaintext.
    pub age_identity: PathBuf,
    pub tenant_user: String,
    pub caddy_unit: String,
    pub port_base: u16,
}

impl std::fmt::Debug for Config {
    /// Manual, and the reason is the one field: `token`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("token", &"<redacted>")
            .field("base_domain", &self.base_domain)
            .field("state_dir", &self.state_dir)
            .field("tenants_dir", &self.tenants_dir)
            .field("env_dir", &self.env_dir)
            .field("caddy_dir", &self.caddy_dir)
            .field("squelchd_bin", &self.squelchd_bin)
            .field("setpriv_bin", &self.setpriv_bin)
            .field("systemctl_bin", &self.systemctl_bin)
            .field("chown_bin", &self.chown_bin)
            .field("age_identity", &self.age_identity)
            .field("tenant_user", &self.tenant_user)
            .field("caddy_unit", &self.caddy_unit)
            .field("port_base", &self.port_base)
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

/// A path var with a default, refused unless absolute: every one of these is
/// used from a process whose working directory is systemd's, so a relative
/// path would resolve somewhere nobody meant.
fn path_var(name: &str, default: &str) -> Result<PathBuf, ConfigError> {
    let raw = var(name).unwrap_or_else(|| default.to_string());
    let path = PathBuf::from(&raw);
    if !path.is_absolute() {
        return Err(ConfigError::invalid(format!(
            "{name} must be an absolute path; got `{raw}`"
        )));
    }
    Ok(path)
}

impl Config {
    /// Load and validate from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_raw = var("SQUELCH_WARDEN_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind: SocketAddr = bind_raw.parse().map_err(|e| {
            ConfigError::invalid(format!("invalid SQUELCH_WARDEN_BIND `{bind_raw}`: {e}"))
        })?;

        // No default and no "generate one for you": a warden that invented its
        // own token would be a warden the control plane cannot reach, and a
        // warden with a guessable one is root on the box for whoever guesses.
        let token = var("SQUELCH_WARDEN_TOKEN").ok_or_else(|| {
            ConfigError::invalid(
                "SQUELCH_WARDEN_TOKEN is required (32+ characters of OS entropy: `openssl rand -base64 32`)",
            )
        })?;
        if token.len() < MIN_TOKEN_LEN {
            // The LENGTH is named, never the value.
            return Err(ConfigError::invalid(format!(
                "SQUELCH_WARDEN_TOKEN is {} characters; at least {MIN_TOKEN_LEN} are required",
                token.len()
            )));
        }

        let base_domain = var("SQUELCH_WARDEN_BASE_DOMAIN").ok_or_else(|| {
            ConfigError::invalid(
                "SQUELCH_WARDEN_BASE_DOMAIN is required (the domain tenants are subdomains of, e.g. passband.email)",
            )
        })?;
        let base_domain = canonical_base_domain(&base_domain)?;

        let port_base = match var("SQUELCH_WARDEN_PORT_BASE") {
            None => DEFAULT_PORT_BASE,
            Some(v) => {
                let port: u16 = v.parse().map_err(|e| {
                    ConfigError::invalid(format!("invalid SQUELCH_WARDEN_PORT_BASE `{v}`: {e}"))
                })?;
                // Above the well-known range and far enough below the
                // ephemeral floor (32768 on Linux) that the whole range sits
                // outside it: a tenant bound to an ephemeral port would lose a
                // race with an outbound connection after a reboot.
                if port < 1024 || port.checked_add(PORT_RANGE).is_none_or(|end| end > 32000) {
                    return Err(ConfigError::invalid(format!(
                        "SQUELCH_WARDEN_PORT_BASE `{port}` must be between 1024 and {}",
                        32000 - PORT_RANGE
                    )));
                }
                port
            }
        };

        let tenant_user =
            var("SQUELCH_WARDEN_TENANT_USER").unwrap_or_else(|| DEFAULT_TENANT_USER.to_string());
        if !tenant_user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Err(ConfigError::invalid(
                "SQUELCH_WARDEN_TENANT_USER must be a plain user name (letters, digits, _ and -)",
            ));
        }
        let caddy_unit =
            var("SQUELCH_WARDEN_CADDY_UNIT").unwrap_or_else(|| DEFAULT_CADDY_UNIT.to_string());
        if !caddy_unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
        {
            return Err(ConfigError::invalid(
                "SQUELCH_WARDEN_CADDY_UNIT must be a systemd unit name",
            ));
        }

        Ok(Self {
            bind,
            token,
            base_domain,
            state_dir: path_var("SQUELCH_WARDEN_STATE_DIR", DEFAULT_STATE_DIR)?,
            tenants_dir: path_var("SQUELCH_WARDEN_TENANTS_DIR", DEFAULT_TENANTS_DIR)?,
            env_dir: path_var("SQUELCH_WARDEN_ENV_DIR", DEFAULT_ENV_DIR)?,
            caddy_dir: path_var("SQUELCH_WARDEN_CADDY_DIR", DEFAULT_CADDY_DIR)?,
            squelchd_bin: path_var("SQUELCH_WARDEN_SQUELCHD_BIN", DEFAULT_SQUELCHD_BIN)?,
            setpriv_bin: path_var("SQUELCH_WARDEN_SETPRIV_BIN", DEFAULT_SETPRIV_BIN)?,
            systemctl_bin: path_var("SQUELCH_WARDEN_SYSTEMCTL_BIN", DEFAULT_SYSTEMCTL_BIN)?,
            chown_bin: path_var("SQUELCH_WARDEN_CHOWN_BIN", DEFAULT_CHOWN_BIN)?,
            age_identity: {
                let raw = var("SQUELCH_WARDEN_AGE_IDENTITY").ok_or_else(|| {
                    ConfigError::invalid(
                        "SQUELCH_WARDEN_AGE_IDENTITY is required (path to the box's age identity file, which only the daemons read)",
                    )
                })?;
                let path = PathBuf::from(&raw);
                if !path.is_absolute() {
                    return Err(ConfigError::invalid(format!(
                        "SQUELCH_WARDEN_AGE_IDENTITY must be an absolute path; got `{raw}`"
                    )));
                }
                path
            },
            tenant_user,
            caddy_unit,
            port_base,
        })
    }

    /// `<label>.<base_domain>`.
    pub fn hostname(&self, label: &str) -> String {
        format!("{label}.{}", self.base_domain)
    }

    /// The URL a paired device talks to. Always https: the pairing code and the
    /// device token it buys cross the public internet.
    pub fn tenant_url(&self, label: &str) -> String {
        format!("https://{}", self.hostname(label))
    }

    /// The systemd instance for a tenant, template unit and all.
    pub fn unit_name(&self, label: &str) -> String {
        format!("squelchd@{label}.service")
    }

    /// `<tenants_dir>/<label>` — the tenant's whole world: sqlite db,
    /// credentials, HOME.
    pub fn tenant_dir(&self, label: &str) -> PathBuf {
        self.tenants_dir.join(label)
    }

    pub fn credentials_path(&self, label: &str) -> PathBuf {
        self.tenant_dir(label).join("credentials.json")
    }

    pub fn db_path(&self, label: &str) -> PathBuf {
        self.tenant_dir(label).join("squelch.db")
    }

    pub fn env_file(&self, label: &str) -> PathBuf {
        self.env_dir.join(format!("{label}.env"))
    }

    pub fn caddy_file(&self, label: &str) -> PathBuf {
        self.caddy_dir.join(format!("{label}.caddy"))
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    /// `user:group` for the tenant's files. One name for both, which is what
    /// `useradd --system squelch` creates.
    pub fn tenant_owner(&self) -> String {
        format!("{}:{}", self.tenant_user, self.tenant_user)
    }
}

/// Validate the base domain: a bare hostname, nothing else.
///
/// A scheme, a path, or a port would each survive into a Caddy site address and
/// into the URL baked into a pairing deep link, where the failure shows up as a
/// device that cannot connect and no explanation.
fn canonical_base_domain(raw: &str) -> Result<String, ConfigError> {
    let domain = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return Err(ConfigError::invalid("SQUELCH_WARDEN_BASE_DOMAIN is empty"));
    }
    if domain.contains("://") || domain.contains('/') || domain.contains(':') {
        return Err(ConfigError::invalid(
            "SQUELCH_WARDEN_BASE_DOMAIN must be a bare domain with no scheme, port or path (e.g. passband.email)",
        ));
    }
    if !domain.contains('.') {
        return Err(ConfigError::invalid(
            "SQUELCH_WARDEN_BASE_DOMAIN must be a domain with at least one dot (e.g. passband.email)",
        ));
    }
    if domain.starts_with('.')
        || domain.contains("..")
        || !domain
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'))
    {
        return Err(ConfigError::invalid(
            "SQUELCH_WARDEN_BASE_DOMAIN is not a valid domain name",
        ));
    }
    Ok(domain)
}

/// A path as a string for a command line or an env file.
///
/// Non-UTF-8 paths are theoretically possible on unix and are not a thing any
/// of these paths can be: they all come from config, which came from an env
/// var, which was already a `String`. `to_string_lossy` is the honest answer to
/// an impossible case.
pub fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_config;

    #[test]
    fn derives_every_tenant_path_from_the_label() {
        let c = test_config();
        assert_eq!(c.hostname("alice"), "alice.passband.email");
        assert_eq!(c.tenant_url("alice"), "https://alice.passband.email");
        assert_eq!(c.unit_name("alice"), "squelchd@alice.service");
        assert_eq!(
            c.tenant_dir("alice"),
            PathBuf::from("/var/lib/squelch/tenants/alice")
        );
        assert_eq!(
            c.credentials_path("alice"),
            PathBuf::from("/var/lib/squelch/tenants/alice/credentials.json")
        );
        assert_eq!(
            c.env_file("alice"),
            PathBuf::from("/etc/squelch/tenants/alice.env")
        );
        assert_eq!(
            c.caddy_file("alice"),
            PathBuf::from("/etc/caddy/tenants/alice.caddy")
        );
        assert_eq!(
            c.state_file(),
            PathBuf::from("/var/lib/squelch/warden/state.json")
        );
        assert_eq!(c.tenant_owner(), "squelch:squelch");
    }

    #[test]
    fn debug_redacts_the_token() {
        let c = test_config();
        let rendered = format!("{c:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(&c.token));
    }

    #[test]
    fn canonicalizes_the_base_domain() {
        assert_eq!(
            canonical_base_domain(" Passband.Email. ").unwrap(),
            "passband.email"
        );
        for bad in [
            "https://passband.email",
            "passband.email/",
            "passband.email:443",
            "passband",
            ".passband.email",
            "pass..band.email",
            "pass band.email",
            "",
        ] {
            assert!(canonical_base_domain(bad).is_err(), "{bad} was accepted");
        }
    }
}
