//! Startup configuration, read once from the environment. Every field is
//! validated at load time and the process refuses to start on a bad value.
//!
//! The `.p8` PEM is held in memory for the life of the process: never logged,
//! never echoed in an error, and it leaves only as an ES256 signature.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;

/// Loopback by design: TLS is terminated by a proxy in front of this listener,
/// so binding a public interface would serve the relay in the clear.
pub const DEFAULT_BIND: &str = "127.0.0.1:8850";

/// One entry of the trusted-proxy peer allowlist: an address block, or a single
/// address when written bare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    bits: u8,
}

/// Which peers may speak for someone else. Used when the operator declares hops
/// but no explicit list: loopback plus the RFC1918 and RFC4193 private ranges,
/// which is where a sidecar or private-network proxy sits. No public address is
/// in it, so the header stays unreadable to anything that reaches the listener
/// from the internet.
pub const DEFAULT_TRUSTED_PROXY_CIDRS: &[Cidr] = &[
    Cidr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)), 8),
    Cidr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 128),
    Cidr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)), 8),
    Cidr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 0)), 12),
    Cidr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 0, 0)), 16),
    Cidr::new(IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0)), 7),
];

impl Cidr {
    pub const fn new(addr: IpAddr, bits: u8) -> Self {
        Self { addr, bits }
    }

    /// Parse `10.0.0.0/8`, `fd00::/8`, or a bare address (a host route). A
    /// prefix wider than the family is rejected rather than clamped: it is a
    /// typo, and a clamped block silently trusts more than it says.
    pub fn parse(s: &str) -> Option<Self> {
        let (addr, bits) = match s.split_once('/') {
            None => (s.trim().parse::<IpAddr>().ok()?, None),
            Some((a, b)) => (
                a.trim().parse::<IpAddr>().ok()?,
                Some(b.trim().parse::<u8>().ok()?),
            ),
        };
        let width = if addr.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(width);
        if bits > width {
            return None;
        }
        // Peers are canonicalized before matching, so an IPv4-mapped entry folds
        // to the v4 block it names instead of sitting there matching nothing. A
        // block wider than the mapped range names more than v4 and is refused.
        match addr.to_canonical() {
            IpAddr::V4(v4) if addr.is_ipv6() => Some(Self {
                addr: IpAddr::V4(v4),
                bits: bits.checked_sub(96)?,
            }),
            _ => Some(Self { addr, bits }),
        }
    }

    /// Is `ip` inside this block? A dual-stack listener reports an IPv4 peer as
    /// the IPv4-mapped `::ffff:a.b.c.d`, so the peer is canonicalized first or a
    /// v4 entry would silently never match the proxy it names.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => prefix_eq(&net.octets(), &ip.octets(), self.bits),
            (IpAddr::V6(net), IpAddr::V6(ip)) => prefix_eq(&net.octets(), &ip.octets(), self.bits),
            _ => false,
        }
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.bits)
    }
}

/// Compare the leading `bits` of two same-width addresses. Hand-rolled: a CIDR
/// dependency for one masked byte comparison is not worth the supply chain.
fn prefix_eq(net: &[u8], ip: &[u8], bits: u8) -> bool {
    let whole = bits as usize / 8;
    if net[..whole] != ip[..whole] {
        return false;
    }
    let rest = bits % 8;
    rest == 0 || (net[whole] ^ ip[whole]) & (0xffu8 << (8 - rest)) == 0
}

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

/// Ceiling on `SQUELCH_RELAY_TRUSTED_PROXY_HOPS`. No deployment stacks eight
/// proxies; a larger number is a typo, and a typo here degrades silently to the
/// shared bucket, so it is a refusal to boot instead.
pub const MAX_TRUSTED_PROXY_HOPS: usize = 8;

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
    /// How many proxies the operator asserts are in front of this listener. `0`
    /// trusts nothing and meters the TCP peer; `N` meters the Nth-from-the-right
    /// `X-Forwarded-For` entry, but only for a peer in
    /// [`Config::proxy_allowlist`]. See [`crate::ratelimit::client_ip`].
    pub trusted_proxy_hops: usize,
    /// Peers allowed to speak for someone else through `X-Forwarded-For`.
    /// `None` is [`DEFAULT_TRUSTED_PROXY_CIDRS`].
    pub trusted_proxy_cidrs: Option<Vec<Cidr>>,
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
            .field("trusted_proxy_hops", &self.trusted_proxy_hops)
            .field("trusted_proxy_cidrs", &self.trusted_proxy_cidrs)
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

        // Trusting a forwarded header is a claim about the deployment's own
        // topology that only the operator can make, so it is opt-in and the
        // default trusts nothing.
        let trusted_proxy_hops = match var("SQUELCH_RELAY_TRUSTED_PROXY_HOPS") {
            None => 0,
            Some(v) => {
                let hops: usize = v.parse().map_err(|e| {
                    ConfigError::invalid(format!(
                        "invalid SQUELCH_RELAY_TRUSTED_PROXY_HOPS `{v}`: {e}"
                    ))
                })?;
                if hops > MAX_TRUSTED_PROXY_HOPS {
                    return Err(ConfigError::invalid(format!(
                        "SQUELCH_RELAY_TRUSTED_PROXY_HOPS `{hops}` exceeds {MAX_TRUSTED_PROXY_HOPS}; set it to the number of proxies you run in front of this listener (1 for a single TLS proxy)"
                    )));
                }
                hops
            }
        };

        // The hop count says how much of the header the operator's own proxies
        // wrote; this says which peers those proxies are. Without it the count
        // alone would trust whoever opened the socket, so a listener reachable
        // past the proxy hands every caller a header they choose.
        let trusted_proxy_cidrs = match var("SQUELCH_RELAY_TRUSTED_PROXY_CIDRS") {
            None => None,
            Some(raw) => {
                if trusted_proxy_hops == 0 {
                    return Err(ConfigError::invalid(
                        "SQUELCH_RELAY_TRUSTED_PROXY_CIDRS is set but SQUELCH_RELAY_TRUSTED_PROXY_HOPS is 0, so no forwarded header is ever read; set the hop count as well or unset the allowlist",
                    ));
                }
                Some(parse_cidrs(&raw)?)
            }
        };

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
            trusted_proxy_hops,
            trusted_proxy_cidrs,
        })
    }

    /// The peer allowlist in force. `trusted_proxy_hops` is a claim about
    /// traffic arriving THROUGH the operator's proxies, so it applies only to
    /// peers that could be one; every other peer is metered by its own address.
    pub fn proxy_allowlist(&self) -> &[Cidr] {
        self.trusted_proxy_cidrs
            .as_deref()
            .unwrap_or(DEFAULT_TRUSTED_PROXY_CIDRS)
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

/// Split and validate the trusted-peer allowlist. An unparseable entry is a
/// refusal to boot: dropping it would leave a relay that reads the header from
/// fewer peers than the operator wrote, which shows up only as mis-metering.
fn parse_cidrs(raw: &str) -> Result<Vec<Cidr>, ConfigError> {
    let mut cidrs = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let parsed = Cidr::parse(entry).ok_or_else(|| {
            ConfigError::invalid(format!(
                "invalid entry `{entry}` in SQUELCH_RELAY_TRUSTED_PROXY_CIDRS: expected an address or a CIDR block (`10.0.0.0/8`, `fd00::/8`, `203.0.113.7`)"
            ))
        })?;
        cidrs.push(parsed);
    }
    if cidrs.is_empty() {
        return Err(ConfigError::invalid(
            "SQUELCH_RELAY_TRUSTED_PROXY_CIDRS is empty after parsing; leave it unset for the loopback-and-private-ranges default",
        ));
    }
    Ok(cidrs)
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
            trusted_proxy_hops: 0,
            trusted_proxy_cidrs: None,
        }
    }

    fn addr(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn trusted(ip: &str) -> bool {
        DEFAULT_TRUSTED_PROXY_CIDRS
            .iter()
            .any(|c| c.contains(addr(ip)))
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
    fn parses_blocks_and_bare_addresses() {
        let block = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(block.contains(addr("10.255.1.2")));
        assert!(!block.contains(addr("11.0.0.1")));
        // A bare address is a host route, not a wildcard.
        let host = Cidr::parse("203.0.113.7").unwrap();
        assert!(host.contains(addr("203.0.113.7")));
        assert!(!host.contains(addr("203.0.113.8")));
        assert_eq!(host.to_string(), "203.0.113.7/32");

        // The boundary lands mid-byte, which is where a hand-rolled mask is
        // worth testing.
        let twelve = Cidr::parse("172.16.0.0/12").unwrap();
        assert!(twelve.contains(addr("172.16.0.0")));
        assert!(twelve.contains(addr("172.31.255.255")));
        assert!(!twelve.contains(addr("172.15.255.255")));
        assert!(!twelve.contains(addr("172.32.0.0")));

        let seven = Cidr::parse("fc00::/7").unwrap();
        assert!(seven.contains(addr("fd00::1")));
        assert!(seven.contains(addr("fc00::1")));
        assert!(!seven.contains(addr("fe00::1")));
        assert!(!seven.contains(addr("2001:db8::1")));

        // Families never cross.
        assert!(!block.contains(addr("::1")));
        assert!(!seven.contains(addr("10.0.0.1")));
        // A v4 peer arriving on a dual-stack socket still matches a v4 entry.
        assert!(block.contains(addr("::ffff:10.1.2.3")));

        // An IPv4-mapped entry names a v4 block, and one wider than the mapped
        // range names more than v4.
        let mapped = Cidr::parse("::ffff:10.0.0.0/104").unwrap();
        assert_eq!(mapped.to_string(), "10.0.0.0/8");
        assert!(mapped.contains(addr("10.1.2.3")));
        assert!(Cidr::parse("::ffff:0:0/64").is_none());

        // `/0` is a real (if reckless) block; wider than the family is a typo.
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains(addr("8.8.8.8")));
        assert!(Cidr::parse("10.0.0.0/33").is_none());
        assert!(Cidr::parse("fc00::/129").is_none());
        assert!(Cidr::parse("10.0.0.0/eight").is_none());
        assert!(Cidr::parse("not-an-address").is_none());
        assert!(Cidr::parse("").is_none());
    }

    /// The default has to cover the sidecar and private-network proxy cases and
    /// nothing public, or trusting it would be the bug it replaces.
    #[test]
    fn the_default_allowlist_is_loopback_and_private_ranges() {
        for ip in [
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "fd12:3456::1",
        ] {
            assert!(trusted(ip), "{ip} should be a trusted peer");
        }
        for ip in [
            "203.0.113.7",
            "198.51.100.7",
            "8.8.8.8",
            "172.32.0.1",
            "192.169.0.1",
            "2001:db8::1",
            "0.0.0.0",
        ] {
            assert!(!trusted(ip), "{ip} must not be a trusted peer");
        }
    }

    #[test]
    fn parses_cidr_lists() {
        let list = parse_cidrs(" 10.0.0.0/8 , 203.0.113.7 ,").unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].contains(addr("10.0.0.1")));
        assert!(list[1].contains(addr("203.0.113.7")));
        assert!(parse_cidrs("  ").is_err());
        assert!(parse_cidrs("10.0.0.0/8, nonsense").is_err());
    }

    #[test]
    fn the_allowlist_defaults_only_when_unset() {
        let mut c = cfg(&["dev.squelch.ios"]);
        assert_eq!(c.proxy_allowlist(), DEFAULT_TRUSTED_PROXY_CIDRS);
        c.trusted_proxy_cidrs = Some(vec![Cidr::parse("203.0.113.7").unwrap()]);
        assert_eq!(c.proxy_allowlist().len(), 1);
        assert!(!c.proxy_allowlist()[0].contains(addr("127.0.0.1")));
    }

    #[test]
    fn override_wins_over_environment_host() {
        let mut c = cfg(&["dev.squelch.ios"]);
        assert_eq!(c.apns_base_url(), "https://api.push.apple.com");
        c.apns_url_override = Some("http://127.0.0.1:9".into());
        assert_eq!(c.apns_base_url(), "http://127.0.0.1:9");
    }
}
