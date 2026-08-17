//! The one seam between the warden and Kubernetes.
//!
//! Everything this service does to the cluster goes through [`Cluster`]. The
//! real implementation talks to the API server with in-cluster credentials; the
//! test double in [`crate::testing`] records the typed objects it was handed and
//! answers from a map. Nothing above this file knows which one it has, which is
//! how the suite runs with no cluster, no kubeconfig, and no network.
//!
//! The trait is deliberately narrow. It is not a Kubernetes client: it is the
//! eight verbs a tenant lifecycle needs, over the six kinds a tenant is made
//! of. Anything wider would be surface the warden's RBAC does not grant anyway.
//!
//! ## The second gate
//!
//! [`KubeCluster::guard`] re-checks every object's name and namespace
//! immediately before it goes on the wire. The label was already validated at
//! the API boundary and again by [`crate::validate::TenantName`], so this is
//! the third check of the same thing, and it is here because the failure it
//! prevents is a write into a namespace the warden was never meant to touch.
//! Three cheap checks beat one clever one.

use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret, Service};
use k8s_openapi::api::networking::v1::{Ingress, NetworkPolicy};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{AttachParams, DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::client::UpgradeConnectionError;
use kube::{Api, Client, Resource};
use tokio::io::AsyncReadExt;

/// Field manager for server-side apply. Every field the warden owns is stamped
/// with this, so a human who edits a tenant object by hand gets a conflict
/// rather than having their edit silently reverted on the next apply.
pub const FIELD_MANAGER: &str = "squelch-warden";

/// One tenant object, typed.
///
/// An enum rather than a generic method per kind, so the trait stays small and
/// a test can assert an exact ordered list of what a provision applied.
#[derive(Debug, Clone, PartialEq)]
pub enum Object {
    Secret(Box<Secret>),
    Pvc(Box<PersistentVolumeClaim>),
    NetworkPolicy(Box<NetworkPolicy>),
    Service(Box<Service>),
    Deployment(Box<Deployment>),
    Ingress(Box<Ingress>),
}

impl Object {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Secret(_) => Kind::Secret,
            Self::Pvc(_) => Kind::Pvc,
            Self::NetworkPolicy(_) => Kind::NetworkPolicy,
            Self::Service(_) => Kind::Service,
            Self::Deployment(_) => Kind::Deployment,
            Self::Ingress(_) => Kind::Ingress,
        }
    }

    pub fn metadata(&self) -> &ObjectMeta {
        match self {
            Self::Secret(o) => &o.metadata,
            Self::Pvc(o) => &o.metadata,
            Self::NetworkPolicy(o) => &o.metadata,
            Self::Service(o) => &o.metadata,
            Self::Deployment(o) => &o.metadata,
            Self::Ingress(o) => &o.metadata,
        }
    }

    /// The object's name, or `""` when it somehow has none. Only for logs and
    /// for the guard, which turns the empty case into a refusal.
    pub fn name(&self) -> &str {
        self.metadata().name.as_deref().unwrap_or_default()
    }
}

/// The kinds the warden deletes by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Secret,
    Pvc,
    NetworkPolicy,
    Service,
    Deployment,
    Ingress,
}

/// What a `squelchd pair` exec produced.
///
/// PRIVACY: `stdout` holds a LIVE pairing code. Nothing logs this struct, and
/// it has no `Debug`.
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    /// Whether the command exited zero.
    pub ok: bool,
}

/// Why a cluster operation failed.
///
/// These carry API detail and are for the warden's own logs. The wire never
/// sees one: [`crate::provision`] turns each into a terse machine reason.
#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("{op} failed: {source}")]
    Api {
        op: &'static str,
        /// Boxed: `kube::Error` is a large enum, and this type is the `Err`
        /// half of every result in the module. Boxing keeps the happy path's
        /// `Result` small.
        #[source]
        source: Box<kube::Error>,
    },
    #[error("object already exists")]
    AlreadyExists,
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("refused to act on an object named `{name}` in namespace `{namespace}`")]
    Refused { name: String, namespace: String },
    #[error("no pod is ready")]
    NoPod,
    #[error("exec did not report a status")]
    NoExecStatus,
}

impl ClusterError {
    /// A log-safe one-liner: what failed, at which operation, and for an API
    /// error the HTTP status. Never the source's `Display`.
    ///
    /// `kube::Error::Api` carries the API server's own message, and the API
    /// server quotes the offending REQUEST back in some of them. A tenant's
    /// request body is somebody's sealed credential, so the log line gets the
    /// SHAPE of the failure and the operator gets the rest from
    /// `kubectl -n tenants get events`.
    pub fn summary(&self) -> String {
        match self {
            Self::Api { op, source } => match source.as_ref() {
                kube::Error::Api(response) => format!("api({op}): http {}", response.code),
                // The exec path only. `pods/exec` is a WebSocket, and
                // `Client::connect` does NOT turn a non-101 answer into
                // `Error::Api` the way an ordinary request does — it reports
                // the missing upgrade instead. Flattening that into a bare
                // `transport` threw away the one number that names the cause:
                // 403 for a `pods/exec` grant the Role does not carry, 404 for
                // a container name that is not in the pod, and anything at all
                // if the connection ever ends up on HTTP/2, which cannot carry
                // an upgrade at all. A status code is a number the branch
                // above already logs, not request content, so this stays as
                // log-safe as it was.
                kube::Error::UpgradeConnection(UpgradeConnectionError::ProtocolSwitch(status)) => {
                    format!("api({op}): transport(upgrade) http {}", status.as_u16())
                }
                // The remaining upgrade failures are handshake mismatches: the
                // fact that the handshake is what broke is the whole message,
                // and their payloads (a hyper error, a key) are not for logs.
                kube::Error::UpgradeConnection(_) => format!("api({op}): transport(upgrade)"),
                // Not a response: a transport, TLS or decode failure, whose
                // variant name says everything a log line should.
                _ => format!("api({op}): transport"),
            },
            Self::AlreadyExists => "already_exists".to_string(),
            Self::Timeout(within) => format!("timeout after {within:?}"),
            Self::Refused { name, namespace } => format!("refused {namespace}/{name}"),
            Self::NoPod => "no_ready_pod".to_string(),
            Self::NoExecStatus => "no_exec_status".to_string(),
        }
    }
}

/// Everything the warden does to a cluster.
#[async_trait]
pub trait Cluster: Send + Sync {
    /// Server-side apply: create or update, idempotently.
    async fn apply(&self, object: Object) -> Result<(), ClusterError>;

    /// Create, failing with [`ClusterError::AlreadyExists`] if the name is
    /// taken. Used for the identity Secret, where "already there" is a decision
    /// point rather than something to overwrite.
    async fn create(&self, object: Object) -> Result<(), ClusterError>;

    async fn get_secret(&self, name: &str) -> Result<Option<Secret>, ClusterError>;

    /// Every Secret matching `selector`. One caller: the pending sweep, which
    /// has to find tenants nobody is asking about.
    async fn list_secrets(&self, selector: &str) -> Result<Vec<Secret>, ClusterError>;

    async fn get_deployment(&self, name: &str) -> Result<Option<Deployment>, ClusterError>;

    /// Delete by name. A missing object is success: every caller is either
    /// tearing down or retrying a teardown.
    async fn delete(&self, kind: Kind, name: &str) -> Result<(), ClusterError>;

    /// Block until a pod matching `selector` reports Ready, and return its
    /// name. [`ClusterError::Timeout`] after `within`.
    async fn ready_pod(&self, selector: &str, within: Duration) -> Result<String, ClusterError>;

    /// Run `argv` inside `pod` and collect both streams.
    async fn exec(&self, pod: &str, argv: &[String]) -> Result<ExecOutput, ClusterError>;
}

/// The real one: kube-rs against the in-cluster API server.
pub struct KubeCluster {
    client: Client,
    namespace: String,
}

impl KubeCluster {
    /// Connect using the pod's ServiceAccount. Fails at startup if the warden
    /// is not running in a cluster, which is the only place it is meant to run.
    ///
    /// ## This client has to speak HTTP/1.1, and it is one line for a reason
    ///
    /// [`Cluster::exec`] is `pods/exec`, and `pods/exec` is a WebSocket: the
    /// request goes out with `Connection: Upgrade` and the API server answers
    /// `101 Switching Protocols`. HTTP/2 has no 101 — a stream there is
    /// multiplexed, never handed over — so an exec dispatched on an h2
    /// connection dies at the transport in under a millisecond while every
    /// plain request sharing that same pool keeps working. Nothing but pairing
    /// breaks, which is what makes the failure so easy to misread.
    ///
    /// [`Client::try_default`] is the safe construction, and it is safe on
    /// purpose rather than by luck: kube-rs builds its rustls connector with
    /// `enable_http1()` and nothing else, and hyper-rustls leaves
    /// `alpn_protocols` EMPTY in exactly that state. An empty ALPN list is
    /// never offered, so the API server cannot select `h2`, so hyper's pool
    /// takes its HTTP/1.1 path — the one that carries upgrades. The warden is
    /// a handful of calls per signup; multiplexing has nothing to win here.
    ///
    /// So do not hand-roll a connector for this. `enable_all_versions()`,
    /// `enable_http2()` and any `HttpsConnector::from((http, tls_config))`
    /// carrying an ALPN list of your own all put `h2` on the wire, and all of
    /// them break pairing and only pairing. Today the image is built with
    /// `cargo build -p squelch-warden`, which resolves hyper-rustls without its
    /// `http2` feature, so the first two would not even compile — treat that as
    /// a second lock rather than as the reason this comment is unnecessary.
    pub async fn connect(namespace: String) -> Result<Self, kube::Error> {
        Ok(Self {
            client: Client::try_default().await?,
            namespace,
        })
    }

    pub fn new(client: Client, namespace: String) -> Self {
        Self { client, namespace }
    }

    fn api<K>(&self) -> Api<K>
    where
        K: Resource<Scope = k8s_openapi::NamespaceResourceScope>
            + Clone
            + serde::de::DeserializeOwned
            + std::fmt::Debug,
        K::DynamicType: Default,
    {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn guard(&self, object: &Object) -> Result<(), ClusterError> {
        guard(&self.namespace, object)
    }
}

/// The last check before an object crosses the wire.
///
/// A name that is not a plain DNS-1123 label, or a namespace that is not the
/// one this warden was scoped to, is a bug somewhere above here, and the right
/// response to a bug at this depth is to refuse rather than to find out what
/// the API server makes of it.
///
/// A free function rather than a method so it can be tested without a client:
/// what it checks is the object, and the client has nothing to do with it.
fn guard(namespace: &str, object: &Object) -> Result<(), ClusterError> {
    let name = object.name();
    let target = object.metadata().namespace.clone().unwrap_or_default();
    let name_ok = !name.is_empty() && name.len() <= 63 && crate::config::is_dns_label(name);
    if name_ok && target == namespace {
        return Ok(());
    }
    Err(ClusterError::Refused {
        name: name.to_string(),
        namespace: target,
    })
}

/// Map a kube 404 onto `None`.
fn optional<T>(
    result: Result<T, kube::Error>,
    op: &'static str,
) -> Result<Option<T>, ClusterError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(None),
        Err(source) => Err(ClusterError::Api {
            op,
            source: Box::new(source),
        }),
    }
}

#[async_trait]
impl Cluster for KubeCluster {
    async fn apply(&self, object: Object) -> Result<(), ClusterError> {
        self.guard(&object)?;
        let name = object.name().to_string();
        // Force, because the warden is the only writer that should own these
        // fields and a half-finished apply from a previous attempt must not
        // wedge the next one.
        let params = PatchParams::apply(FIELD_MANAGER).force();
        let op = "apply";
        macro_rules! apply {
            ($api:expr, $value:expr) => {{
                $api.patch(&name, &params, &Patch::Apply($value.as_ref()))
                    .await
                    .map(|_| ())
                    .map_err(|source| ClusterError::Api {
                        op,
                        source: Box::new(source),
                    })
            }};
        }
        match object {
            Object::Secret(o) => apply!(self.api::<Secret>(), o),
            Object::Pvc(o) => apply!(self.api::<PersistentVolumeClaim>(), o),
            Object::NetworkPolicy(o) => apply!(self.api::<NetworkPolicy>(), o),
            Object::Service(o) => apply!(self.api::<Service>(), o),
            Object::Deployment(o) => apply!(self.api::<Deployment>(), o),
            Object::Ingress(o) => apply!(self.api::<Ingress>(), o),
        }
    }

    async fn create(&self, object: Object) -> Result<(), ClusterError> {
        self.guard(&object)?;
        let params = PostParams {
            field_manager: Some(FIELD_MANAGER.to_string()),
            ..Default::default()
        };
        let op = "create";
        macro_rules! create {
            ($api:expr, $value:expr) => {{
                match $api.create(&params, $value.as_ref()).await {
                    Ok(_) => Ok(()),
                    Err(kube::Error::Api(e)) if e.code == 409 => Err(ClusterError::AlreadyExists),
                    Err(source) => Err(ClusterError::Api {
                        op,
                        source: Box::new(source),
                    }),
                }
            }};
        }
        match object {
            Object::Secret(o) => create!(self.api::<Secret>(), o),
            Object::Pvc(o) => create!(self.api::<PersistentVolumeClaim>(), o),
            Object::NetworkPolicy(o) => create!(self.api::<NetworkPolicy>(), o),
            Object::Service(o) => create!(self.api::<Service>(), o),
            Object::Deployment(o) => create!(self.api::<Deployment>(), o),
            Object::Ingress(o) => create!(self.api::<Ingress>(), o),
        }
    }

    async fn get_secret(&self, name: &str) -> Result<Option<Secret>, ClusterError> {
        optional(self.api::<Secret>().get(name).await, "get secret")
    }

    async fn list_secrets(&self, selector: &str) -> Result<Vec<Secret>, ClusterError> {
        let params = ListParams::default().labels(selector);
        self.api::<Secret>()
            .list(&params)
            .await
            .map(|list| list.items)
            .map_err(|source| ClusterError::Api {
                op: "list secrets",
                source: Box::new(source),
            })
    }

    async fn get_deployment(&self, name: &str) -> Result<Option<Deployment>, ClusterError> {
        optional(self.api::<Deployment>().get(name).await, "get deployment")
    }

    async fn delete(&self, kind: Kind, name: &str) -> Result<(), ClusterError> {
        let params = DeleteParams::default();
        let op = "delete";
        macro_rules! delete {
            ($api:expr) => {{ optional($api.delete(name, &params).await, op).map(|_| ()) }};
        }
        match kind {
            Kind::Secret => delete!(self.api::<Secret>()),
            Kind::Pvc => delete!(self.api::<PersistentVolumeClaim>()),
            Kind::NetworkPolicy => delete!(self.api::<NetworkPolicy>()),
            Kind::Service => delete!(self.api::<Service>()),
            Kind::Deployment => delete!(self.api::<Deployment>()),
            Kind::Ingress => delete!(self.api::<Ingress>()),
        }
    }

    async fn ready_pod(&self, selector: &str, within: Duration) -> Result<String, ClusterError> {
        let pods: Api<Pod> = self.api();
        let params = ListParams::default().labels(selector);
        // Polled rather than watched on purpose: this runs once per signup,
        // for at most a couple of minutes, and a poll has no stream to
        // reconnect and no bookmark to lose. The pod is Ready or it is not.
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let list = pods
                .list(&params)
                .await
                .map_err(|source| ClusterError::Api {
                    op: "list pods",
                    source: Box::new(source),
                })?;
            if let Some(name) = list
                .items
                .iter()
                .find(|p| is_ready(p))
                .and_then(|p| p.metadata.name.clone())
            {
                return Ok(name);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ClusterError::Timeout(within));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn exec(&self, pod: &str, argv: &[String]) -> Result<ExecOutput, ClusterError> {
        let pods: Api<Pod> = self.api();
        let params = AttachParams::default()
            .container("squelchd")
            .stdin(false)
            .stdout(true)
            .stderr(true);
        let mut process = pods
            .exec(pod, argv.iter().map(String::as_str), &params)
            .await
            .map_err(|source| ClusterError::Api {
                op: "exec",
                source: Box::new(source),
            })?;

        // Taken before the streams are drained: `join` consumes the process,
        // and the status channel has to be claimed while it still exists.
        let status = process.take_status().ok_or(ClusterError::NoExecStatus)?;
        let stdout = drain(process.stdout()).await;
        let stderr = drain(process.stderr()).await;
        let status = status.await;
        let _ = process.join().await;

        Ok(ExecOutput {
            stdout,
            stderr,
            ok: status.and_then(|s| s.status).as_deref() == Some("Success"),
        })
    }
}

/// Read an exec stream to the end, lossily. A stream that errors mid-way gives
/// back what it managed, and the parser decides whether that is enough.
async fn drain(stream: Option<impl AsyncReadExt + Unpin>) -> String {
    let Some(mut stream) = stream else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

/// Whether a pod reports `Ready=True`. Kubernetes says a pod is ready when
/// every container's readiness probe passes, which for a tenant means the
/// daemon accepted a connection on 8848.
fn is_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|conditions| {
            conditions
                .iter()
                .any(|c| c.type_ == "Ready" && c.status == "True")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects;
    use crate::testing::test_config;
    use crate::validate::TenantName;
    use k8s_openapi::api::core::v1::{PodCondition, PodStatus};

    #[test]
    fn the_guard_accepts_what_the_object_builders_produce() {
        let config = test_config();
        let name = TenantName::parse("alice").unwrap();
        for object in [
            Object::Secret(Box::new(objects::credential_secret(&config, &name, "ct"))),
            Object::Service(Box::new(objects::service(&config, &name))),
            Object::Ingress(Box::new(objects::ingress(&config, &name))),
            Object::Deployment(Box::new(objects::deployment(&config, &name, "hash", None))),
            Object::NetworkPolicy(Box::new(objects::network_policy(&config, &name))),
            Object::Pvc(Box::new(objects::data_pvc(&config, &name))),
        ] {
            assert!(guard("tenants", &object).is_ok(), "{:?}", object.kind());
        }
    }

    /// The gate that would catch a bug above it: an object aimed at another
    /// namespace, or named something that is not a plain label, never leaves
    /// this process.
    #[test]
    fn the_guard_refuses_a_stray_namespace_or_a_strange_name() {
        let config = test_config();
        let name = TenantName::parse("alice").unwrap();

        let mut elsewhere = objects::service(&config, &name);
        elsewhere.metadata.namespace = Some("kube-system".to_string());
        assert!(matches!(
            guard("tenants", &Object::Service(Box::new(elsewhere))).unwrap_err(),
            ClusterError::Refused { .. }
        ));

        for bad in ["", "Alice", "alice/../etc", "-alice", &"a".repeat(64)] {
            let mut object = objects::service(&config, &name);
            object.metadata.name = Some(bad.to_string());
            assert!(
                matches!(
                    guard("tenants", &Object::Service(Box::new(object))).unwrap_err(),
                    ClusterError::Refused { .. }
                ),
                "`{bad}` got through"
            );
        }
    }

    #[test]
    fn reads_the_ready_condition_the_way_kubernetes_writes_it() {
        let condition = |type_: &str, status: &str| PodCondition {
            type_: type_.to_string(),
            status: status.to_string(),
            ..Default::default()
        };
        let with = |conditions: Vec<PodCondition>| Pod {
            status: Some(PodStatus {
                conditions: Some(conditions),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(is_ready(&with(vec![condition("Ready", "True")])));
        assert!(is_ready(&with(vec![
            condition("Initialized", "True"),
            condition("Ready", "True"),
        ])));
        assert!(!is_ready(&with(vec![condition("Ready", "False")])));
        assert!(!is_ready(&with(vec![condition("Initialized", "True")])));
        assert!(!is_ready(&with(vec![])));
        assert!(!is_ready(&Pod::default()));
    }

    /// What reaches a log line. The API server's message never does: it can
    /// quote the request back, and a tenant's request is a sealed credential.
    #[test]
    fn an_error_summary_is_the_shape_of_the_failure_and_nothing_else() {
        // What an API server hands back when it dislikes a write: its own
        // message, quoting the request.
        let status = kube::core::Status {
            message:
                "Secret \"alice-credential\" is invalid: data: -----BEGIN AGE ENCRYPTED FILE-----"
                    .to_string(),
            reason: "Invalid".to_string(),
            code: 422,
            ..Default::default()
        };
        let api = ClusterError::Api {
            op: "apply",
            source: Box::new(kube::Error::Api(Box::new(status))),
        };
        let summary = api.summary();
        assert_eq!(summary, "api(apply): http 422");
        assert!(!summary.contains("AGE"));
        assert!(!summary.contains("alice"));

        assert_eq!(ClusterError::AlreadyExists.summary(), "already_exists");
        assert_eq!(ClusterError::NoPod.summary(), "no_ready_pod");
        assert_eq!(
            ClusterError::Refused {
                name: "alice".to_string(),
                namespace: "tenants".to_string(),
            }
            .summary(),
            "refused tenants/alice"
        );
    }

    /// The one transport failure that gets a word, because the word is the
    /// diagnosis. `Client::connect` does not raise a non-101 answer as
    /// `Error::Api`, so a refused or misaddressed exec used to land in the same
    /// bare `transport` bucket as a TLS failure — and an exec that reached an
    /// HTTP/2 connection, which can never carry an upgrade, landed there too.
    /// The status is a number, so this costs nothing in privacy.
    #[test]
    fn an_exec_that_never_reached_101_says_which_kind_of_never() {
        // Same `http` crate kube parses responses with: axum re-exports it and
        // the workspace resolves exactly one 1.x.
        use axum::http::StatusCode;

        let upgrade = |e: UpgradeConnectionError| ClusterError::Api {
            op: "exec",
            source: Box::new(kube::Error::UpgradeConnection(e)),
        };

        // What a Role without `pods/exec` produces, and what an h2 connection
        // or a stray reverse proxy would produce: an answer that was not 101.
        assert_eq!(
            upgrade(UpgradeConnectionError::ProtocolSwitch(
                StatusCode::FORBIDDEN
            ))
            .summary(),
            "api(exec): transport(upgrade) http 403"
        );
        assert_eq!(
            upgrade(UpgradeConnectionError::ProtocolSwitch(
                StatusCode::NOT_FOUND
            ))
            .summary(),
            "api(exec): transport(upgrade) http 404"
        );

        // A handshake that got its 101 and then disagreed. No payload printed.
        for mismatch in [
            UpgradeConnectionError::MissingUpgradeWebSocketHeader,
            UpgradeConnectionError::MissingConnectionUpgradeHeader,
            UpgradeConnectionError::SecWebSocketAcceptKeyMismatch,
            UpgradeConnectionError::SecWebSocketProtocolMismatch,
        ] {
            assert_eq!(upgrade(mismatch).summary(), "api(exec): transport(upgrade)");
        }

        // Everything that is not an upgrade failure stays exactly as terse as
        // it was: the hint is additive, not a loosening.
        assert_eq!(
            ClusterError::Api {
                op: "exec",
                source: Box::new(kube::Error::LinesCodecMaxLineLengthExceeded),
            }
            .summary(),
            "api(exec): transport"
        );
    }

    #[test]
    fn an_object_knows_its_own_kind_and_name() {
        let config = test_config();
        let name = TenantName::parse("alice").unwrap();
        let object =
            Object::Deployment(Box::new(objects::deployment(&config, &name, "hash", None)));
        assert_eq!(object.kind(), Kind::Deployment);
        assert_eq!(object.name(), "alice");
    }
}
