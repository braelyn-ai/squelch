//! squelch-warden: the cluster-side tenant provisioner for hosted Passband.
//!
//! One single-node k3s cluster runs one squelchd pod per tenant, each with its
//! own volume, its own age identity, its own NetworkPolicy, and its own
//! subdomain. The warden is the small in-cluster service that makes that
//! happen: the control plane (`squelch-control`, on Railway) calls the JSON
//! routes in [`handlers`], and the warden does the two things kube cannot do
//! for itself, which are minting a tenant's key and rendering a tenant's
//! manifests.
//!
//! ## What the warden is trusted with, and what it is not
//!
//! It IS trusted with the tenant namespace. Its RBAC is a Role bound to exactly
//! that namespace: no ClusterRole, no cluster-admin, no reach into
//! `kube-system` or into its own namespace. Its bearer token is that namespace.
//!
//! It is NOT trusted with anyone's mail. It mints each tenant's age identity in
//! memory, writes it straight into that tenant's Secret, and forgets it; the
//! control plane learns only the public recipient and seals with that. So the
//! warden cannot read a credential it stored a second after it stored it, and
//! the control plane could never read one at all. Recovery from a lost Secret
//! is re-consent, not an escrowed key.
//!
//! [`validate::validate_ciphertext`] is the last checkpoint on that path: a
//! credential body that is not age-armored is refused before it reaches the API
//! server, because an unarmored one would mean the control plane had handed
//! over a plaintext refresh token.
//!
//! It also never links `squelch-core`. It has no store, no mail parser, and no
//! OAuth client.
//!
//! ## Layout
//!
//! - [`config`] - env-driven startup config, validated hard, token redacted.
//! - [`validate`] - everything the control plane may send, plus [`validate::TenantName`],
//!   the type that carries a checked label into a manifest.
//! - [`identity`] - per-tenant age key minting.
//! - [`objects`] - every tenant object, as typed `k8s-openapi` structs. **No
//!   YAML is rendered anywhere in this crate.**
//! - [`cluster`] - the one trait every Kubernetes call goes through, and the
//!   kube-rs implementation of it.
//! - [`provision`] - the two-phase sequence, the reconcile that repairs a
//!   tenant somebody edited, the pending sweep, and the fleet roll that walks
//!   every tenant onto today's render one at a time.
//! - [`drift`] - reading a live tenant back: who else owns part of it, and what
//!   an apply of today's render would change.
//! - [`pair`] - reading `squelchd pair`'s output back.
//! - [`devices`] - reading `squelchd token first-paired`'s output back: one
//!   timestamp, which is the whole of the activation signal.
//! - [`auth`] / [`ratelimit`] / [`handlers`] - the wire.
//!
//! The static infrastructure that installs the warden itself, and the runbook
//! for a fresh box, live in `deploy/hosted/`.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post, put},
};

pub mod auth;
pub mod cluster;
pub mod config;
pub mod devices;
pub mod drift;
pub mod handlers;
pub mod identity;
pub mod objects;
pub mod pair;
pub mod provision;
pub mod ratelimit;
#[cfg(test)]
pub mod testing;
pub mod validate;

pub use cluster::{Cluster, KubeCluster};
pub use config::{Config, ConfigError};
pub use drift::{DriftReport, FieldChange, ForeignManager};
pub use pair::Pairing;
pub use provision::{Created, Reconciled, Rolled, TenantStatus, Warden, WardenError};

/// State threaded through the router. Cheap to clone (one `Arc`).
#[derive(Clone)]
pub struct WardenState {
    inner: Arc<StateInner>,
}

struct StateInner {
    warden: Arc<Warden>,
    /// One bucket set for the whole gated surface; see [`ratelimit`].
    limiter: Mutex<ratelimit::RateLimiter>,
    /// Refused requests since this process started. A COUNT, never material.
    auth_failures: AtomicU64,
}

impl WardenState {
    pub fn new(warden: Arc<Warden>) -> Self {
        Self {
            inner: Arc::new(StateInner {
                warden,
                limiter: Mutex::new(ratelimit::RateLimiter::per_minute(
                    ratelimit::CONTROL_REQUESTS_PER_MINUTE,
                )),
                auth_failures: AtomicU64::new(0),
            }),
        }
    }

    /// The configured bearer. Reached only by [`auth::require_bearer`], and
    /// only as bytes for a constant-time compare.
    pub(crate) fn token(&self) -> &str {
        &self.inner.warden.config().token
    }

    pub(crate) fn warden(&self) -> Arc<Warden> {
        self.inner.warden.clone()
    }

    pub(crate) fn trusted_proxy_hops(&self) -> usize {
        self.inner.warden.config().trusted_proxy_hops
    }

    /// Charge one request against `ip`'s bucket. False means "over the limit".
    ///
    /// Poisoning is RECOVERED, not propagated: the guarded value is a token
    /// bucket with no invariant a panic could corrupt, and `.expect()` would
    /// brick every future request while `/healthz` kept answering 200.
    pub(crate) fn check_rate(&self, ip: IpAddr) -> bool {
        self.inner
            .limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(ip, Instant::now())
    }

    /// Count one refusal and return the running total, for the one log line
    /// [`auth::require_bearer`] writes.
    pub(crate) fn record_auth_failure(&self) -> u64 {
        self.inner.auth_failures.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Build the warden router.
///
/// Every `/v1` route sits behind a rate limit, the bearer gate and a body
/// limit, in that order from the outside in: refusing a flood should not cost a
/// constant-time compare, and parsing a body should not happen for a request
/// that has no business being here. `/healthz` sits outside all three: it is
/// the kubelet's liveness and readiness probe, it takes no input, and it says
/// nothing but `ok`. Putting a credential in front of that would only mean the
/// kubelet has to hold one.
pub fn router(state: WardenState) -> Router {
    let v1 = Router::new()
        .route("/v1/tenants", post(handlers::create_tenant))
        .route(
            "/v1/tenants/{label}",
            get(handlers::get_tenant).delete(handlers::delete_tenant),
        )
        .route(
            "/v1/tenants/{label}/credentials",
            put(handlers::set_credentials),
        )
        .route("/v1/tenants/{label}/llm-key", put(handlers::set_llm_key))
        .route("/v1/tenants/{label}/drift", get(handlers::get_drift))
        .route("/v1/tenants/{label}/devices", get(handlers::get_devices))
        .route(
            "/v1/tenants/{label}/reconcile",
            post(handlers::reconcile_tenant),
        )
        .route("/v1/tenants/{label}/pair", post(handlers::repair_tenant))
        .layer(DefaultBodyLimit::max(handlers::MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit_control,
        ))
        .with_state(state);

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(v1)
}

#[cfg(test)]
mod hard_rule {
    /// The crate's hard rule, enforced against the crate's own source.
    ///
    /// Every tenant object is a typed `k8s-openapi` struct. There is no
    /// template engine, no YAML serializer, and no string that becomes a
    /// manifest, because a tenant label interpolated into text is exactly the
    /// bug class this design exists to remove. `deploy/hosted/` is YAML and is
    /// meant to be: nothing there is per-tenant.
    ///
    /// A grep rather than a type, because what it is checking is the ABSENCE of
    /// a thing, and absence has no type.
    ///
    /// The needles are assembled from halves at runtime rather than written as
    /// literals. Otherwise this file would contain every string it is looking
    /// for and would be the first thing it failed on.
    #[test]
    fn no_yaml_is_rendered_anywhere_in_this_crate() {
        let banned: Vec<String> = [
            ("serde", "_yaml"),
            ("serde", "_yml"),
            ("serde", "_norway"),
            // The key every hand-written manifest starts with, in both its
            // YAML and its JSON spelling. If either shows up in a Rust string
            // here, somebody is building a manifest where a struct belongs.
            // (`kind` is not checked: Rust spells `kind: Kind` all over this
            // crate, and a needle that matches an ordinary parameter is a
            // needle nobody keeps.)
            ("api", "Version:"),
            ("\"api", "Version\""),
        ]
        .iter()
        .map(|(a, b)| format!("{a}{b}"))
        .collect();

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0usize;
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                checked += 1;
                for needle in &banned {
                    assert!(
                        !text.contains(needle),
                        "{} contains `{needle}`; tenant objects are typed, not templated",
                        path.display()
                    );
                }
            }
        }
        assert!(checked >= 9, "only {checked} source files were checked");
    }
}
