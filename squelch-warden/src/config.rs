//! Startup configuration, read once from the environment and validated hard
//! enough that a running warden is a warden whose names, images, bounds and
//! timeouts all make sense.
//!
//! Everything here except the bearer token is a name, a hostname, a quantity, a
//! duration or a flag. The token is the one secret this process holds, so
//! [`Config`] has a manual `Debug` that redacts it: a derived one would print it
//! the first time anyone adds a `dbg!` to a handler.
//!
//! The v1 warden had a config full of filesystem paths and binary locations
//! because it *was* the orchestrator. This one has none: kube owns the
//! lifecycle, so what is left is the handful of cluster facts a manifest needs.
//!
//! Reading is parameterised over a lookup rather than hardwired to
//! `std::env::var`, which is what lets the boot-refusal table below run in
//! parallel against maps instead of mutating the process environment.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// All interfaces by design. The warden runs as a pod behind a ClusterIP
/// Service and its own Ingress; binding loopback inside a pod would make the
/// Service unreachable. The pod has its own network namespace and a
/// NetworkPolicy in front, which is what "not exposed" means here.
pub const DEFAULT_BIND: &str = "0.0.0.0:8852";

/// The namespace every tenant's objects live in. The warden's RBAC is scoped to
/// exactly this namespace and nothing else.
pub const DEFAULT_TENANT_NAMESPACE: &str = "tenants";

/// The namespace the ingress controller runs in. k3s ships Traefik in
/// `kube-system`.
pub const DEFAULT_INGRESS_NAMESPACE: &str = "kube-system";

/// Pod label the ingress controller carries, as `key=value`. Used by the
/// per-tenant NetworkPolicy to name the one peer allowed to reach a tenant.
pub const DEFAULT_INGRESS_POD_LABEL: &str = "app.kubernetes.io/name=traefik";

/// `ingressClassName` on every tenant Ingress.
pub const DEFAULT_INGRESS_CLASS: &str = "traefik";

/// Secret holding the wildcard certificate for `*.<base domain>`, in the tenant
/// namespace, issued by cert-manager via DNS-01.
pub const DEFAULT_TLS_SECRET: &str = "passband-wildcard-tls";

/// Secret holding the Google OAuth client every tenant daemon refreshes with.
///
/// One Secret, shared by every tenant, in the tenant namespace. A tenant's
/// refresh token was minted by the control plane's CONFIDENTIAL WEB client, and
/// a refresh token only works for the client that minted it, so a tenant daemon
/// that does not hold that client's id and secret cannot refresh and does not
/// boot. See `deploy/hosted/SETUP.md`, "The Google OAuth client".
pub const DEFAULT_OAUTH_SECRET_NAME: &str = "google-oauth-client";

/// k3s's built-in dynamic provisioner.
pub const DEFAULT_STORAGE_CLASS: &str = "local-path";

/// Per-tenant data volume size. The SQLite index plus the embedding cache.
pub const DEFAULT_STORAGE_SIZE: &str = "10Gi";

/// A tenant daemon's CPU floor. An idle mailbox is a poll loop; this is enough
/// to keep one responsive without reserving a core it will not use.
pub const DEFAULT_CPU_REQUEST: &str = "100m";

/// A tenant daemon's CPU ceiling. One core: an embedding backfill is the only
/// thing that ever wants more, and a tenant that wants more than a core is a
/// tenant taking the box from everyone else.
pub const DEFAULT_CPU_LIMIT: &str = "1000m";

/// A tenant daemon's memory floor: SQLite plus the ONNX embedder's working set.
pub const DEFAULT_MEMORY_REQUEST: &str = "256Mi";

/// A tenant daemon's memory ceiling. Past this the container is OOM-killed and
/// restarted, which is one tenant down rather than the node down.
pub const DEFAULT_MEMORY_LIMIT: &str = "1Gi";

/// Ephemeral-storage floor for a tenant daemon: `/tmp` plus its own logs. The
/// mailbox lives on a PersistentVolume and does not count against this.
pub const DEFAULT_EPHEMERAL_REQUEST: &str = "256Mi";

/// Ephemeral-storage ceiling. Without one, a tenant filling `/tmp` fills the
/// NODE's root filesystem and takes every other tenant with it.
pub const DEFAULT_EPHEMERAL_LIMIT: &str = "1Gi";

/// `sizeLimit` on the `/tmp` emptyDir. An emptyDir with no size limit is a
/// tmpfs-or-disk hole with the node's free space at the bottom of it.
pub const DEFAULT_TMP_SIZE: &str = "512Mi";

/// UID/GID every tenant daemon runs as. Fixed and high, so it collides with no
/// system account on the node, and so `runAsNonRoot` has something to enforce.
/// With `hostUsers: false` this is a namespaced id anyway.
pub const DEFAULT_RUN_AS: i64 = 10001;

/// How long provisioning waits for a tenant pod to report Ready before giving
/// up. First boot pulls an image and opens a fresh SQLite file; three minutes
/// is generous for that and short enough that a signup does not hang forever.
pub const DEFAULT_READY_TIMEOUT_SECS: u64 = 180;

/// How long a tenant may sit `pending` before the sweep collects it. A signup
/// that reached phase one and never came back has parked an identity Secret
/// that nothing will ever use and that holds the subdomain against everyone
/// else. A day is far longer than any real signup and short enough that a
/// squatted label frees itself.
pub const DEFAULT_PENDING_TTL_SECS: u64 = 24 * 60 * 60;

/// Floor on the pending TTL. Anything shorter would race a signup that is still
/// in the operator's hands.
pub const MIN_PENDING_TTL_SECS: u64 = 600;

/// Ceiling on `SQUELCH_WARDEN_TRUSTED_PROXY_HOPS`. No deployment stacks eight
/// proxies; a larger number is a typo, and a typo here degrades silently to one
/// shared bucket for every caller, so it is a refusal to boot instead.
pub const MAX_TRUSTED_PROXY_HOPS: usize = 8;

/// Shortest bearer token that may be configured.
///
/// This is the credential for a service that can create workloads and read
/// Secrets in the tenant namespace, so a short one is a boot failure rather
/// than a warning. 32 characters is what `openssl rand -base64 24` prints.
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

/// Compute bounds for one container, as Kubernetes quantities.
///
/// Requests are what the scheduler reserves; limits are what the kubelet
/// enforces. Both halves are here because a pod with requests and no limits
/// starves its neighbours under load, and a pod with limits and no requests
/// gets scheduled onto a box that has nothing left to give it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resources {
    pub cpu_request: String,
    pub cpu_limit: String,
    pub memory_request: String,
    pub memory_limit: String,
    pub ephemeral_request: String,
    pub ephemeral_limit: String,
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
    /// The squelchd image every tenant Deployment runs, e.g.
    /// `ghcr.io/braelyn-ai/squelchd:v0.4.0`. Pinned by the operator: a tenant
    /// that silently moved to a new daemon on a restart would be an upgrade
    /// nobody scheduled.
    pub image: String,
    /// Namespace every tenant object is created in.
    pub namespace: String,
    /// Namespace the ingress controller runs in.
    pub ingress_namespace: String,
    /// The ingress controller's pod label, split.
    pub ingress_pod_label: (String, String),
    pub ingress_class: String,
    /// Wildcard TLS Secret name, in [`Config::namespace`].
    pub tls_secret: String,
    /// Secret holding the Google OAuth client id and secret every tenant daemon
    /// refreshes with, in [`Config::namespace`]. See
    /// [`DEFAULT_OAUTH_SECRET_NAME`].
    pub oauth_secret_name: String,
    pub storage_class: String,
    pub storage_size: String,
    /// Bounds on the tenant daemon container.
    pub daemon_resources: Resources,
    /// `sizeLimit` on the tenant's `/tmp` emptyDir.
    pub tmp_size: String,
    /// `imagePullSecrets` entry for GHCR, when the image is not public.
    pub pull_secret: Option<String>,
    /// Whether to ask for per-pod user namespaces (`hostUsers: false`).
    ///
    /// ON by default, because it is the strongest per-tenant boundary this
    /// design has: root inside a tenant's pod maps to an unprivileged id on the
    /// node. It is a flag because the field needs a kernel with idmapped mounts
    /// and a cluster with the `UserNamespacesSupport` gate on; on a cluster
    /// without it the field is ignored (older API servers prune it), and on a
    /// node without it pods fail to start. See `deploy/hosted/SETUP.md`.
    pub user_namespaces: bool,
    /// Optional PVC holding a pre-seeded embedding-weights cache. When set,
    /// every tenant's init container copies from it instead of each tenant
    /// downloading ~130 MB from Hugging Face on first boot.
    pub model_pvc: Option<String>,
    /// The node network, when the CNI drops kubelet probe traffic without one.
    ///
    /// A readiness probe originates from the NODE, not from a pod, so it
    /// matches no peer in a tenant's NetworkPolicy. Some CNIs exempt
    /// host-originated traffic and some do not; on one that does not, every
    /// provision times out waiting for a pod that is healthy. Setting this adds
    /// one ingress rule allowing that CIDR to the daemon port. See
    /// `deploy/hosted/SETUP.md`, "Verify the policy".
    pub node_cidr: Option<String>,
    /// UID/GID tenant daemons run as.
    pub run_as: i64,
    pub ready_timeout: Duration,
    /// How long a `pending` tenant survives the sweep.
    pub pending_ttl: Duration,
    /// How many proxies the operator runs in front of this listener. Zero means
    /// `X-Forwarded-For` is never read, which is the safe default: the header is
    /// caller-supplied and would otherwise mint unlimited rate-limit
    /// identities.
    pub trusted_proxy_hops: usize,
}

impl std::fmt::Debug for Config {
    /// Manual, and the reason is the one field: `token`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("token", &"<redacted>")
            .field("base_domain", &self.base_domain)
            .field("image", &self.image)
            .field("namespace", &self.namespace)
            .field("ingress_namespace", &self.ingress_namespace)
            .field("ingress_pod_label", &self.ingress_pod_label)
            .field("ingress_class", &self.ingress_class)
            .field("tls_secret", &self.tls_secret)
            .field("oauth_secret_name", &self.oauth_secret_name)
            .field("storage_class", &self.storage_class)
            .field("storage_size", &self.storage_size)
            .field("daemon_resources", &self.daemon_resources)
            .field("tmp_size", &self.tmp_size)
            .field("pull_secret", &self.pull_secret)
            .field("user_namespaces", &self.user_namespaces)
            .field("model_pvc", &self.model_pvc)
            .field("node_cidr", &self.node_cidr)
            .field("run_as", &self.run_as)
            .field("ready_timeout", &self.ready_timeout)
            .field("pending_ttl", &self.pending_ttl)
            .field("trusted_proxy_hops", &self.trusted_proxy_hops)
            .finish()
    }
}

/// Where one variable comes from. The process environment in production, a map
/// in the tests.
type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Read a var, treating whitespace-only as unset.
fn var(get: Lookup, name: &str) -> Option<String> {
    get(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// A var that must be a DNS-1123 label (a k8s object or namespace name).
///
/// Every one of these ends up as `metadata.name` or as a `namespaceSelector`
/// value. The API server would refuse a bad one, but it would refuse it on the
/// first signup rather than at boot, and the operator would be reading a kube
/// error instead of a sentence.
fn name_var(get: Lookup, name: &str, default: &str) -> Result<String, ConfigError> {
    let value = var(get, name).unwrap_or_else(|| default.to_string());
    if value.len() > 63 || !is_dns_label(&value) {
        return Err(ConfigError::invalid(format!(
            "{name} must be a Kubernetes name: 1 to 63 characters of a-z, 0-9 and hyphen, not starting or ending with a hyphen"
        )));
    }
    Ok(value)
}

/// A var that must be a Kubernetes quantity: digits, an optional decimal point,
/// and a suffix like `Gi`, `Mi` or `m`. Refused here rather than at the API
/// server so the operator hears it at boot instead of on the first signup.
fn quantity_var(get: Lookup, name: &str, default: &str) -> Result<String, ConfigError> {
    let value = var(get, name).unwrap_or_else(|| default.to_string());
    let shaped = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.')
        && value.starts_with(|c: char| c.is_ascii_digit());
    if !shaped {
        return Err(ConfigError::invalid(format!(
            "{name} must be a Kubernetes quantity (e.g. 10Gi, 256Mi, 100m)"
        )));
    }
    Ok(value)
}

/// A var that must be an optional Kubernetes object name.
fn optional_name_var(get: Lookup, name: &str) -> Result<Option<String>, ConfigError> {
    match var(get, name) {
        None => Ok(None),
        Some(raw) => {
            if !is_dns_label(&raw) || raw.len() > 63 {
                return Err(ConfigError::invalid(format!(
                    "{name} must be a Kubernetes name"
                )));
            }
            Ok(Some(raw))
        }
    }
}

/// `^[a-z0-9]([a-z0-9-]*[a-z0-9])?$`, hand-rolled.
pub(crate) fn is_dns_label(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}

impl Config {
    /// Load and validate from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(&|name| std::env::var(name).ok())
    }

    /// Load and validate from an arbitrary lookup.
    pub fn from_source(get: Lookup) -> Result<Self, ConfigError> {
        let bind_raw = var(get, "SQUELCH_WARDEN_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind: SocketAddr = bind_raw.parse().map_err(|e| {
            ConfigError::invalid(format!("invalid SQUELCH_WARDEN_BIND `{bind_raw}`: {e}"))
        })?;

        // No default and no "generate one for you": a warden that invented its
        // own token would be a warden the control plane cannot reach, and a
        // warden with a guessable one can create workloads in the namespace
        // that holds every tenant's mail.
        let token = var(get, "SQUELCH_WARDEN_TOKEN").ok_or_else(|| {
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

        let base_domain = var(get, "SQUELCH_WARDEN_BASE_DOMAIN").ok_or_else(|| {
            ConfigError::invalid(
                "SQUELCH_WARDEN_BASE_DOMAIN is required (the domain tenants are subdomains of, e.g. passband.email)",
            )
        })?;
        let base_domain = canonical_base_domain(&base_domain)?;

        let image = var(get, "SQUELCH_WARDEN_IMAGE").ok_or_else(|| {
            ConfigError::invalid(
                "SQUELCH_WARDEN_IMAGE is required (the squelchd image tenants run, e.g. ghcr.io/braelyn-ai/squelchd:v0.4.0)",
            )
        })?;
        let image = canonical_image(&image)?;

        let ingress_pod_label = {
            let raw = var(get, "SQUELCH_WARDEN_INGRESS_POD_LABEL")
                .unwrap_or_else(|| DEFAULT_INGRESS_POD_LABEL.to_string());
            let (key, value) = raw.split_once('=').ok_or_else(|| {
                ConfigError::invalid(
                    "SQUELCH_WARDEN_INGRESS_POD_LABEL must be `key=value` (e.g. app.kubernetes.io/name=traefik)",
                )
            })?;
            if key.is_empty() || value.is_empty() || raw.contains(char::is_whitespace) {
                return Err(ConfigError::invalid(
                    "SQUELCH_WARDEN_INGRESS_POD_LABEL must be `key=value` with no spaces",
                ));
            }
            (key.to_string(), value.to_string())
        };

        let user_namespaces = match var(get, "SQUELCH_WARDEN_USER_NAMESPACES") {
            None => true,
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => true,
                "0" | "false" | "off" | "no" => false,
                _ => {
                    return Err(ConfigError::invalid(
                        "SQUELCH_WARDEN_USER_NAMESPACES must be on or off",
                    ));
                }
            },
        };

        let run_as = match var(get, "SQUELCH_WARDEN_RUN_AS_UID") {
            None => DEFAULT_RUN_AS,
            Some(v) => {
                let uid: i64 = v.parse().map_err(|e| {
                    ConfigError::invalid(format!("invalid SQUELCH_WARDEN_RUN_AS_UID `{v}`: {e}"))
                })?;
                // Zero is the whole point of runAsNonRoot; the upper bound is
                // the conventional ceiling for a container id.
                if !(1000..=65533).contains(&uid) {
                    return Err(ConfigError::invalid(
                        "SQUELCH_WARDEN_RUN_AS_UID must be between 1000 and 65533 (never 0)",
                    ));
                }
                uid
            }
        };

        let ready_timeout = match var(get, "SQUELCH_WARDEN_READY_TIMEOUT_SECS") {
            None => DEFAULT_READY_TIMEOUT_SECS,
            Some(v) => {
                let secs: u64 = v.parse().map_err(|e| {
                    ConfigError::invalid(format!(
                        "invalid SQUELCH_WARDEN_READY_TIMEOUT_SECS `{v}`: {e}"
                    ))
                })?;
                if !(10..=900).contains(&secs) {
                    return Err(ConfigError::invalid(
                        "SQUELCH_WARDEN_READY_TIMEOUT_SECS must be between 10 and 900",
                    ));
                }
                secs
            }
        };

        let pending_ttl = match var(get, "SQUELCH_WARDEN_PENDING_TTL_SECS") {
            None => DEFAULT_PENDING_TTL_SECS,
            Some(v) => {
                let secs: u64 = v.parse().map_err(|e| {
                    ConfigError::invalid(format!(
                        "invalid SQUELCH_WARDEN_PENDING_TTL_SECS `{v}`: {e}"
                    ))
                })?;
                if secs < MIN_PENDING_TTL_SECS {
                    return Err(ConfigError::invalid(format!(
                        "SQUELCH_WARDEN_PENDING_TTL_SECS must be at least {MIN_PENDING_TTL_SECS}; a shorter one would collect a signup that is still in progress"
                    )));
                }
                secs
            }
        };

        // Trusting a forwarded header is a claim about the deployment's own
        // topology that only the operator can make, so it is opt-in and the
        // default trusts nothing.
        let trusted_proxy_hops = match var(get, "SQUELCH_WARDEN_TRUSTED_PROXY_HOPS") {
            None => 0,
            Some(v) => {
                let hops: usize = v.parse().map_err(|e| {
                    ConfigError::invalid(format!(
                        "invalid SQUELCH_WARDEN_TRUSTED_PROXY_HOPS `{v}`: {e}"
                    ))
                })?;
                if hops > MAX_TRUSTED_PROXY_HOPS {
                    return Err(ConfigError::invalid(format!(
                        "SQUELCH_WARDEN_TRUSTED_PROXY_HOPS `{hops}` exceeds {MAX_TRUSTED_PROXY_HOPS}; set it to the number of proxies you run in front of this listener (1 for a single ingress controller)"
                    )));
                }
                hops
            }
        };

        let node_cidr = match var(get, "SQUELCH_WARDEN_NODE_CIDR") {
            None => None,
            Some(raw) => Some(canonical_cidr(&raw)?),
        };

        Ok(Self {
            bind,
            token,
            base_domain,
            image,
            namespace: name_var(
                get,
                "SQUELCH_WARDEN_TENANT_NAMESPACE",
                DEFAULT_TENANT_NAMESPACE,
            )?,
            ingress_namespace: name_var(
                get,
                "SQUELCH_WARDEN_INGRESS_NAMESPACE",
                DEFAULT_INGRESS_NAMESPACE,
            )?,
            ingress_pod_label,
            ingress_class: name_var(get, "SQUELCH_WARDEN_INGRESS_CLASS", DEFAULT_INGRESS_CLASS)?,
            tls_secret: name_var(get, "SQUELCH_WARDEN_TLS_SECRET", DEFAULT_TLS_SECRET)?,
            oauth_secret_name: name_var(
                get,
                "SQUELCH_WARDEN_OAUTH_SECRET_NAME",
                DEFAULT_OAUTH_SECRET_NAME,
            )?,
            storage_class: name_var(get, "SQUELCH_WARDEN_STORAGE_CLASS", DEFAULT_STORAGE_CLASS)?,
            storage_size: quantity_var(get, "SQUELCH_WARDEN_STORAGE_SIZE", DEFAULT_STORAGE_SIZE)?,
            daemon_resources: Resources {
                cpu_request: quantity_var(get, "SQUELCH_WARDEN_CPU_REQUEST", DEFAULT_CPU_REQUEST)?,
                cpu_limit: quantity_var(get, "SQUELCH_WARDEN_CPU_LIMIT", DEFAULT_CPU_LIMIT)?,
                memory_request: quantity_var(
                    get,
                    "SQUELCH_WARDEN_MEMORY_REQUEST",
                    DEFAULT_MEMORY_REQUEST,
                )?,
                memory_limit: quantity_var(
                    get,
                    "SQUELCH_WARDEN_MEMORY_LIMIT",
                    DEFAULT_MEMORY_LIMIT,
                )?,
                ephemeral_request: quantity_var(
                    get,
                    "SQUELCH_WARDEN_EPHEMERAL_REQUEST",
                    DEFAULT_EPHEMERAL_REQUEST,
                )?,
                ephemeral_limit: quantity_var(
                    get,
                    "SQUELCH_WARDEN_EPHEMERAL_LIMIT",
                    DEFAULT_EPHEMERAL_LIMIT,
                )?,
            },
            tmp_size: quantity_var(get, "SQUELCH_WARDEN_TMP_SIZE", DEFAULT_TMP_SIZE)?,
            pull_secret: optional_name_var(get, "SQUELCH_WARDEN_IMAGE_PULL_SECRET")?,
            user_namespaces,
            model_pvc: optional_name_var(get, "SQUELCH_WARDEN_MODEL_PVC")?,
            node_cidr,
            run_as,
            ready_timeout: Duration::from_secs(ready_timeout),
            pending_ttl: Duration::from_secs(pending_ttl),
            trusted_proxy_hops,
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
}

/// Validate the base domain: a bare hostname, nothing else.
///
/// A scheme, a path, or a port would each survive into an Ingress `host` and
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

/// Validate the tenant image reference.
///
/// It lands in a typed `image` field, so nothing here is stopping an injection.
/// What it stops is a value with a space or a newline in it, which the API
/// server accepts and the kubelet then fails to pull with an error the operator
/// reads three layers down.
fn canonical_image(raw: &str) -> Result<String, ConfigError> {
    let image = raw.trim();
    if image.len() > 512 {
        return Err(ConfigError::invalid(
            "SQUELCH_WARDEN_IMAGE is longer than 512 characters",
        ));
    }
    // Everything an OCI reference may contain and nothing else.
    if !image
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'/' | b':' | b'@'))
    {
        return Err(ConfigError::invalid(
            "SQUELCH_WARDEN_IMAGE is not an image reference (letters, digits and . - _ / : @ only)",
        ));
    }
    // An untagged image is `:latest`, which means a tenant's daemon version is
    // whatever the registry happened to hold the last time the pod restarted.
    if !(image.contains(':') || image.contains('@')) {
        return Err(ConfigError::invalid(
            "SQUELCH_WARDEN_IMAGE must carry a tag or a digest; an untagged image means a tenant silently upgrades on restart",
        ));
    }
    Ok(image.to_string())
}

/// Validate the node CIDR: an address and a prefix length, both parsed.
///
/// It becomes an `ipBlock` that OPENS a hole in every tenant's ingress policy,
/// so a typo here is a policy that allows something nobody meant to allow.
/// `0.0.0.0/0` is refused by name for the same reason.
fn canonical_cidr(raw: &str) -> Result<String, ConfigError> {
    let value = raw.trim();
    let (addr, prefix) = value.split_once('/').ok_or_else(|| {
        ConfigError::invalid(
            "SQUELCH_WARDEN_NODE_CIDR must be a CIDR block with a prefix length (e.g. 10.0.0.5/32)",
        )
    })?;
    let addr: IpAddr = addr.parse().map_err(|_| {
        ConfigError::invalid("SQUELCH_WARDEN_NODE_CIDR does not start with an IP address")
    })?;
    let bits: u8 = prefix.parse().map_err(|_| {
        ConfigError::invalid("SQUELCH_WARDEN_NODE_CIDR has a prefix length that is not a number")
    })?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    if bits > max {
        return Err(ConfigError::invalid(format!(
            "SQUELCH_WARDEN_NODE_CIDR prefix length must be between 0 and {max}"
        )));
    }
    if bits == 0 {
        return Err(ConfigError::invalid(
            "SQUELCH_WARDEN_NODE_CIDR must name the node network, not every address; a /0 would let anything reach every tenant",
        ));
    }
    Ok(format!("{addr}/{bits}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::testing::test_config;

    /// The minimum environment a warden boots from: everything else has a
    /// default, and these three have none on purpose.
    fn required() -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "SQUELCH_WARDEN_TOKEN".to_string(),
                "0123456789abcdef0123456789abcdef".to_string(),
            ),
            (
                "SQUELCH_WARDEN_BASE_DOMAIN".to_string(),
                "passband.email".to_string(),
            ),
            (
                "SQUELCH_WARDEN_IMAGE".to_string(),
                "ghcr.io/braelyn-ai/squelchd:v0.4.0".to_string(),
            ),
        ])
    }

    /// Load from a map instead of the process environment, so the table below
    /// runs in parallel and mutates nothing global.
    fn load(vars: &BTreeMap<String, String>) -> Result<Config, ConfigError> {
        Config::from_source(&|name| vars.get(name).cloned())
    }

    fn with(key: &str, value: &str) -> Result<Config, ConfigError> {
        let mut vars = required();
        vars.insert(key.to_string(), value.to_string());
        load(&vars)
    }

    fn without(key: &str) -> Result<Config, ConfigError> {
        let mut vars = required();
        vars.remove(key);
        load(&vars)
    }

    #[test]
    fn the_defaults_are_what_the_manifests_expect() {
        let c = load(&required()).unwrap();
        assert_eq!(c.bind.to_string(), DEFAULT_BIND);
        assert_eq!(c.namespace, "tenants");
        assert_eq!(c.ingress_namespace, "kube-system");
        assert_eq!(c.ingress_class, "traefik");
        assert_eq!(c.tls_secret, "passband-wildcard-tls");
        assert_eq!(c.oauth_secret_name, "google-oauth-client");
        assert_eq!(c.storage_class, "local-path");
        assert_eq!(c.storage_size, "10Gi");
        assert_eq!(c.daemon_resources.cpu_request, "100m");
        assert_eq!(c.daemon_resources.cpu_limit, "1000m");
        assert_eq!(c.daemon_resources.memory_request, "256Mi");
        assert_eq!(c.daemon_resources.memory_limit, "1Gi");
        assert_eq!(c.daemon_resources.ephemeral_request, "256Mi");
        assert_eq!(c.daemon_resources.ephemeral_limit, "1Gi");
        assert_eq!(c.tmp_size, "512Mi");
        assert_eq!(c.run_as, DEFAULT_RUN_AS);
        assert_eq!(c.ready_timeout, Duration::from_secs(180));
        assert_eq!(c.pending_ttl, Duration::from_secs(86_400));
        assert_eq!(c.trusted_proxy_hops, 0);
        assert!(c.user_namespaces);
        assert!(c.node_cidr.is_none());
        assert!(c.model_pvc.is_none());
        assert!(c.pull_secret.is_none());
    }

    /// The boot-refusal table. Every one of these is a value that would
    /// otherwise fail on the first signup, in a kube error, hours later.
    #[test]
    fn refuses_to_boot_on_a_bad_value() {
        // The three with no default.
        assert!(without("SQUELCH_WARDEN_TOKEN").is_err());
        assert!(without("SQUELCH_WARDEN_BASE_DOMAIN").is_err());
        assert!(without("SQUELCH_WARDEN_IMAGE").is_err());
        // Whitespace-only is unset, not a value.
        assert!(with("SQUELCH_WARDEN_TOKEN", "   ").is_err());

        let table: &[(&str, &str)] = &[
            // A token the length of a password somebody typed.
            ("SQUELCH_WARDEN_TOKEN", "hunter2"),
            ("SQUELCH_WARDEN_TOKEN", &"a".repeat(MIN_TOKEN_LEN - 1)),
            ("SQUELCH_WARDEN_BASE_DOMAIN", "https://passband.email"),
            ("SQUELCH_WARDEN_BASE_DOMAIN", "passband"),
            ("SQUELCH_WARDEN_IMAGE", "ghcr.io/braelyn-ai/squelchd"),
            ("SQUELCH_WARDEN_BIND", "not-an-address"),
            // Namespaces and object names, which become metadata.name.
            ("SQUELCH_WARDEN_TENANT_NAMESPACE", "Tenants"),
            ("SQUELCH_WARDEN_TENANT_NAMESPACE", "-tenants"),
            ("SQUELCH_WARDEN_TENANT_NAMESPACE", "tenants/x"),
            ("SQUELCH_WARDEN_TENANT_NAMESPACE", &"a".repeat(64)),
            ("SQUELCH_WARDEN_INGRESS_NAMESPACE", "kube system"),
            ("SQUELCH_WARDEN_TLS_SECRET", "Wildcard"),
            ("SQUELCH_WARDEN_OAUTH_SECRET_NAME", "oauth_client"),
            ("SQUELCH_WARDEN_MODEL_PVC", "Models"),
            ("SQUELCH_WARDEN_IMAGE_PULL_SECRET", "GHCR"),
            // The ingress peer, which is a label rather than a name.
            ("SQUELCH_WARDEN_INGRESS_POD_LABEL", "traefik"),
            (
                "SQUELCH_WARDEN_INGRESS_POD_LABEL",
                "app.kubernetes.io/name=",
            ),
            ("SQUELCH_WARDEN_INGRESS_POD_LABEL", "a = b"),
            // Quantities.
            ("SQUELCH_WARDEN_STORAGE_SIZE", "ten gigs"),
            ("SQUELCH_WARDEN_STORAGE_SIZE", "-10Gi"),
            ("SQUELCH_WARDEN_CPU_REQUEST", "half a core"),
            ("SQUELCH_WARDEN_MEMORY_LIMIT", "1 Gi"),
            ("SQUELCH_WARDEN_TMP_SIZE", "unlimited"),
            ("SQUELCH_WARDEN_EPHEMERAL_LIMIT", "Mi512"),
            // Durations, at both ends.
            ("SQUELCH_WARDEN_READY_TIMEOUT_SECS", "9"),
            ("SQUELCH_WARDEN_READY_TIMEOUT_SECS", "901"),
            ("SQUELCH_WARDEN_READY_TIMEOUT_SECS", "forever"),
            ("SQUELCH_WARDEN_PENDING_TTL_SECS", "60"),
            ("SQUELCH_WARDEN_PENDING_TTL_SECS", "a day"),
            // Identity.
            ("SQUELCH_WARDEN_RUN_AS_UID", "0"),
            ("SQUELCH_WARDEN_RUN_AS_UID", "999"),
            ("SQUELCH_WARDEN_RUN_AS_UID", "65534"),
            ("SQUELCH_WARDEN_RUN_AS_UID", "root"),
            // Flags and counts.
            ("SQUELCH_WARDEN_USER_NAMESPACES", "maybe"),
            ("SQUELCH_WARDEN_TRUSTED_PROXY_HOPS", "9"),
            ("SQUELCH_WARDEN_TRUSTED_PROXY_HOPS", "-1"),
            // The one that opens a hole in every tenant's ingress policy.
            ("SQUELCH_WARDEN_NODE_CIDR", "10.0.0.5"),
            ("SQUELCH_WARDEN_NODE_CIDR", "10.0.0.5/33"),
            ("SQUELCH_WARDEN_NODE_CIDR", "0.0.0.0/0"),
            ("SQUELCH_WARDEN_NODE_CIDR", "the node"),
        ];
        for (key, value) in table {
            assert!(with(key, value).is_err(), "{key}=`{value}` was accepted");
        }
    }

    #[test]
    fn accepts_the_values_an_operator_actually_sets() {
        let c = with("SQUELCH_WARDEN_NODE_CIDR", " 10.0.0.5/32 ").unwrap();
        assert_eq!(c.node_cidr.as_deref(), Some("10.0.0.5/32"));
        let c = with("SQUELCH_WARDEN_NODE_CIDR", "fd00::1/128").unwrap();
        assert_eq!(c.node_cidr.as_deref(), Some("fd00::1/128"));

        assert!(
            !with("SQUELCH_WARDEN_USER_NAMESPACES", "off")
                .unwrap()
                .user_namespaces
        );
        assert_eq!(
            with("SQUELCH_WARDEN_TRUSTED_PROXY_HOPS", "1")
                .unwrap()
                .trusted_proxy_hops,
            1
        );
        assert_eq!(
            with("SQUELCH_WARDEN_PENDING_TTL_SECS", "3600")
                .unwrap()
                .pending_ttl,
            Duration::from_secs(3600)
        );
        assert_eq!(
            with("SQUELCH_WARDEN_MEMORY_LIMIT", "2Gi")
                .unwrap()
                .daemon_resources
                .memory_limit,
            "2Gi"
        );
        assert_eq!(
            with("SQUELCH_WARDEN_OAUTH_SECRET_NAME", "passband-oauth")
                .unwrap()
                .oauth_secret_name,
            "passband-oauth"
        );
    }

    /// The token is the one value that must never appear in a diagnostic, and
    /// `from_source` is where an operator's bad one is described back to them.
    #[test]
    fn a_refusal_never_quotes_the_token() {
        let rendered = with("SQUELCH_WARDEN_TOKEN", "hunter2")
            .unwrap_err()
            .to_string();
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("7 characters"), "{rendered}");
    }

    #[test]
    fn derives_tenant_hostnames_from_the_label() {
        let c = test_config();
        assert_eq!(c.hostname("alice"), "alice.passband.email");
        assert_eq!(c.tenant_url("alice"), "https://alice.passband.email");
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

    #[test]
    fn refuses_an_image_without_a_tag_or_a_digest() {
        assert!(canonical_image("ghcr.io/braelyn-ai/squelchd:v1").is_ok());
        assert!(
            canonical_image(
                "ghcr.io/braelyn-ai/squelchd@sha256:0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_ok()
        );
        // The one that would silently upgrade every tenant on a restart.
        assert!(canonical_image("ghcr.io/braelyn-ai/squelchd").is_err());
        assert!(canonical_image("ghcr.io/x/y:v1 && rm -rf /").is_err());
        assert!(canonical_image("ghcr.io/x/y:v1\nSOMETHING=else").is_err());
    }

    #[test]
    fn accepts_only_kubernetes_names() {
        assert!(is_dns_label("tenants"));
        assert!(is_dns_label("kube-system"));
        assert!(!is_dns_label("-tenants"));
        assert!(!is_dns_label("tenants-"));
        assert!(!is_dns_label("Tenants"));
        assert!(!is_dns_label("tenants/x"));
        assert!(!is_dns_label(""));
    }
}
