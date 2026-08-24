//! The one seam between the warden and Kubernetes.
//!
//! Everything this service does to the cluster goes through [`Cluster`]. The
//! real implementation talks to the API server with in-cluster credentials; the
//! test double in [`crate::testing`] records the typed objects it was handed and
//! answers from a map. Nothing above this file knows which one it has, which is
//! how the suite runs with no cluster, no kubeconfig, and no network.
//!
//! The trait is deliberately narrow. It is not a Kubernetes client: it is the
//! few verbs a tenant lifecycle needs, over the six kinds a tenant is made of.
//! Anything wider would be surface the warden's RBAC does not grant anyway.
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
use k8s_openapi::api::apps::v1::{Deployment, DeploymentStatus};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret, Service};
use k8s_openapi::api::networking::v1::{Ingress, NetworkPolicy};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{
    AttachParams, DeleteParams, ListParams, Patch, PatchParams, PostParams, PropagationPolicy,
};
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

    /// The same apply as [`Cluster::apply`], with `dryRun=All`: the API server
    /// merges and defaults the object and answers with what it WOULD have
    /// stored, having stored nothing.
    ///
    /// This exists so a drift report can be honest. Diffing a render against a
    /// live object directly compares a hand-written spec with a defaulted one,
    /// and the answer is dozens of fields nobody set: `terminationMessagePath`,
    /// `dnsPolicy`, a `protocol` on every port, a `creationTimestamp` on the
    /// pod template. Diffing the API server's own answer against the live
    /// object removes every one of them, because both sides went through the
    /// same defaulting, and what is left is exactly what a real apply would
    /// move.
    ///
    /// It says nothing about fields the warden does not declare. Server-side
    /// apply never removes those - they belong to whichever manager wrote them
    /// and they survive on both sides of this diff - so finding them is
    /// [`crate::drift::foreign_managers`]' job, reading the API server's
    /// ownership ledger, and not this one's.
    async fn apply_deployment_dry_run(
        &self,
        deployment: Deployment,
    ) -> Result<Deployment, ClusterError>;

    /// Create, failing with [`ClusterError::AlreadyExists`] if the name is
    /// taken. Used for the identity Secret, where "already there" is a decision
    /// point rather than something to overwrite.
    async fn create(&self, object: Object) -> Result<(), ClusterError>;

    async fn get_secret(&self, name: &str) -> Result<Option<Secret>, ClusterError>;

    /// Every Secret matching `selector`. One caller: the pending sweep, which
    /// has to find tenants nobody is asking about.
    async fn list_secrets(&self, selector: &str) -> Result<Vec<Secret>, ClusterError>;

    async fn get_deployment(&self, name: &str) -> Result<Option<Deployment>, ClusterError>;

    /// The tenant's Service, if one exists.
    ///
    /// ONE caller, and it is a bridge rather than a rule:
    /// [`crate::provision::Warden::torn_down_before_the_marker`], which has to
    /// decide what a workload-less tenant carrying NO cancellation marker is,
    /// on a cluster whose cancelled tenants predate the marker. Intent is read
    /// from [`crate::objects::CANCELLED_AT_ANNOTATION`] everywhere else, and
    /// this read goes away with the last unmarked tenant.
    async fn get_service(&self, name: &str) -> Result<Option<Service>, ClusterError>;

    /// Write or remove ONE annotation on a Secret, leaving every other field on
    /// that object exactly as it is. `None` removes it.
    ///
    /// A merge patch and not [`Cluster::apply`], and the difference is a
    /// tenant's mail. The warden's apply is server-side apply with force under
    /// one field manager, so it declares the whole object: an apply carrying
    /// only metadata would take the identity Secret's `data` with it, and that
    /// Secret holds the age key every credential this tenant ever had was
    /// sealed to. A merge patch touches the one path it names and nothing else.
    ///
    /// A Secret that is not there is SUCCESS. The only marker this writes lives
    /// on the identity Secret ([`crate::objects::CANCELLED_AT_ANNOTATION`]), and
    /// a tenant whose identity Secret is gone is refused by every path that
    /// would read one - it is in no fleet, and it cannot be reconciled or
    /// reopened - so there is no state a missing target could leave behind.
    async fn annotate_secret(
        &self,
        name: &str,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), ClusterError>;

    /// Delete by name. A missing object is success: every caller is either
    /// tearing down or retrying a teardown.
    async fn delete(&self, kind: Kind, name: &str) -> Result<(), ClusterError>;

    /// Block until a pod matching `selector` reports Ready, and return its
    /// NAME. [`ClusterError::Timeout`] after `within`.
    ///
    /// For the one question that needs a pod name rather than a healthy
    /// tenant: [`Cluster::exec`] runs `squelchd pair` inside a specific pod.
    /// "Is this tenant serving the spec we just applied" is a different
    /// question and a stricter one; it is [`Cluster::rollout_complete`]'s.
    async fn ready_pod(&self, selector: &str, within: Duration) -> Result<String, ClusterError>;

    /// Block until the Deployment `name` has finished rolling onto the spec it
    /// currently carries. [`ClusterError::Timeout`] after `within`;
    /// [`ClusterError::NoPod`] if the Deployment is not there at all, which
    /// mid-wait means something deleted the workload while it rolled.
    ///
    /// This is what [`Cluster::ready_pod`] cannot answer. The tenant strategy
    /// is `Recreate`, so an apply that moves the pod template leaves the OLD
    /// pod Ready for as long as it takes to terminate — and it still matches
    /// the tenant's selector while it does. A caller that took the first Ready
    /// pod as proof would be looking at the pod it just replaced and calling a
    /// tenant that is about to be down `active`. For a single signup that is a
    /// cosmetic lie the next status read corrects; for a sweep that walks the
    /// whole fleet on the strength of each answer it is the difference between
    /// one broken tenant and all of them.
    ///
    /// The conditions are exactly `kubectl rollout status`', and each of the
    /// five rules out a different way of being wrong:
    ///
    /// - `status.observedGeneration >= metadata.generation` — the controller
    ///   has SEEN this spec. Until it has, every field below describes the
    ///   PREVIOUS template, and a rollout that has not begun reads as one that
    ///   finished.
    /// - `status.updatedReplicas == spec.replicas` — every replica that exists
    ///   is on the new template. This is the one that is false while the new
    ///   pod is still being created.
    /// - `status.replicas == spec.replicas` — no old replica is left. Under
    ///   `Recreate` that leftover is the terminating pod, and it still holds
    ///   the tenant's `ReadWriteOnce` volume.
    /// - `status.readyReplicas == spec.replicas` — the new replicas are
    ///   serving. Created is not ready: the daemon opens 8848 only after it has
    ///   installed its credential and opened its store, which is precisely the
    ///   step a bad render breaks.
    /// - `status.availableReplicas == spec.replicas` — the new replicas have
    ///   STAYED serving, for `spec.minReadySeconds`. Ready is a snapshot and a
    ///   daemon that comes up and dies passes through it: it reports Ready, the
    ///   condition above is satisfied, and the process is gone before the next
    ///   probe. Available is the count that waits, which makes the soak a
    ///   per-tenant one this loop gets for free and defence in depth behind the
    ///   roller's own pacing. With `minReadySeconds` unset it is Ready plus the
    ///   controller's round trip, so it costs nothing to require either way.
    ///
    /// `spec.replicas` absent is 1, Kubernetes' own default, and any missing
    /// status field is 0: a Deployment the controller has not reported on has
    /// not finished anything.
    ///
    /// A desired count BELOW 1 is never complete, whatever the status says. At
    /// `spec.replicas: 0` all five numbers are 0 and all five conditions hold,
    /// so a tenant somebody scaled to zero would answer this with "finished" and
    /// no pod - the one answer a caller stepping to the next tenant must never
    /// be given, because it is the shape of a mailbox that is off.
    async fn rollout_complete(&self, name: &str, within: Duration) -> Result<(), ClusterError>;

    /// Block until NO pod matches `selector`. [`ClusterError::Timeout`] after
    /// `within`.
    ///
    /// The tenant's data volume is `ReadWriteOnce` and the daemon behind it is
    /// one SQLite file, so exactly one pod may hold it at a time. Inside a
    /// single Deployment the `Recreate` strategy is what guarantees that: the
    /// old pod is gone before the new one is scheduled. Deleting a Deployment
    /// and applying a fresh one steps outside that guarantee, because the two
    /// objects are two rollouts and neither controller waits for the other.
    /// This is the same promise, made by hand across that boundary, and the
    /// [`crate::provision::Warden::reconcile`] path is not allowed to apply
    /// until it holds.
    ///
    /// "Gone" means the list is EMPTY, not that nothing in it is Ready: a pod
    /// in Terminating still has the volume mounted, and a second writer
    /// starting against it is the corruption this waits out.
    async fn pods_gone(&self, selector: &str, within: Duration) -> Result<(), ClusterError>;

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
    guard_meta(namespace, object.metadata())
}

/// [`guard`] for a caller that holds a typed object rather than an [`Object`].
/// The check is the metadata's, and wrapping a value in an enum only to unwrap
/// it again would say otherwise.
fn guard_meta(namespace: &str, metadata: &ObjectMeta) -> Result<(), ClusterError> {
    let name = metadata.name.as_deref().unwrap_or_default();
    let target = metadata.namespace.clone().unwrap_or_default();
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

    async fn apply_deployment_dry_run(
        &self,
        deployment: Deployment,
    ) -> Result<Deployment, ClusterError> {
        guard_meta(&self.namespace, &deployment.metadata)?;
        let name = deployment.metadata.name.clone().unwrap_or_default();
        // The same manager and the same force as the real apply, so the answer
        // is the answer to "what would `apply` do", and `dryRun=All`, so it
        // does none of it.
        let params = PatchParams::apply(FIELD_MANAGER).force().dry_run();
        self.api::<Deployment>()
            .patch(&name, &params, &Patch::Apply(&deployment))
            .await
            .map_err(|source| ClusterError::Api {
                op: "dry run apply",
                source: Box::new(source),
            })
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

    async fn get_service(&self, name: &str) -> Result<Option<Service>, ClusterError> {
        optional(self.api::<Service>().get(name).await, "get service")
    }

    async fn annotate_secret(
        &self,
        name: &str,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), ClusterError> {
        // A JSON merge patch: the named path is set, a null REMOVES the key,
        // and every other field of the object is left alone. Under the warden's
        // own field manager, the one that already owns this Secret.
        let patch = serde_json::json!({ "metadata": { "annotations": { key: value } } });
        let params = PatchParams {
            field_manager: Some(FIELD_MANAGER.to_string()),
            ..Default::default()
        };
        optional(
            self.api::<Secret>()
                .patch(name, &params, &Patch::Merge(&patch))
                .await,
            "annotate secret",
        )
        .map(|_| ())
    }

    async fn delete(&self, kind: Kind, name: &str) -> Result<(), ClusterError> {
        // BACKGROUND, explicitly, and the Deployment is why.
        //
        // With no `propagationPolicy` on the wire the API server falls back to
        // the resource's own default, and `apps/v1` Deployment's default is
        // foreground: the DELETE returns at once, but the object STAYS, wearing
        // a `deletionTimestamp` and a `foregroundDeletion` finalizer, until the
        // ReplicaSet and the pods behind it have been collected. An update to
        // an object in that state is still accepted, so
        // [`crate::Warden::reconcile`] could re-apply onto the corpse, watch
        // the collector finish a moment later, and be left with no Deployment
        // at all and a purge that purged nothing.
        //
        // Background deletion removes the object immediately and collects the
        // dependents behind it, so a name that answered a DELETE is a name the
        // next apply creates fresh, with an ownership ledger that starts empty.
        // That is the entire point of the recreate path.
        let params = DeleteParams {
            propagation_policy: Some(PropagationPolicy::Background),
            ..Default::default()
        };
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

    async fn rollout_complete(&self, name: &str, within: Duration) -> Result<(), ClusterError> {
        // Polled on the same 2s cadence as `ready_pod`, and for the same
        // reasons: this runs once per tenant, it is bounded by the deadline the
        // caller chose, and the question is four integers on one object.
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let Some(deployment) = self.get_deployment(name).await? else {
                // Waiting out the deadline for an object that no longer exists
                // would only spend the whole timeout to say what this read
                // already said.
                return Err(ClusterError::NoPod);
            };
            if rolled_out(&deployment) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ClusterError::Timeout(within));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn pods_gone(&self, selector: &str, within: Duration) -> Result<(), ClusterError> {
        let pods: Api<Pod> = self.api();
        let params = ListParams::default().labels(selector);
        // Polled on the same cadence as `ready_pod`, and for the same reason:
        // this runs once, bounded, and the question is a list length.
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let list = pods
                .list(&params)
                .await
                .map_err(|source| ClusterError::Api {
                    op: "list pods",
                    source: Box::new(source),
                })?;
            if list.items.is_empty() {
                return Ok(());
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

/// Whether a Deployment has finished rolling onto the spec it carries.
///
/// The five conditions, and why each one is load bearing, are documented on
/// [`Cluster::rollout_complete`]. Pure, so both the real poll loop and the test
/// double answer from the same rule rather than from two readings of it.
pub(crate) fn rolled_out(deployment: &Deployment) -> bool {
    let desired = deployment
        .spec
        .as_ref()
        .and_then(|spec| spec.replicas)
        .unwrap_or(1);
    // A tenant scaled to zero has no pod, and every count on it is 0 - which
    // satisfies all five conditions below and reads as a finished rollout. It
    // is the opposite: a desired count under 1 is a mailbox that is OFF, and a
    // caller waiting for this tenant to be serving before it touches the next
    // one is entitled to be told no.
    if desired < 1 {
        return false;
    }
    let generation = deployment.metadata.generation.unwrap_or(0);
    let status = deployment.status.as_ref();
    // A field the controller has not written is 0, never "assume it is fine":
    // the absent case is a Deployment nothing has acted on yet.
    let count = |pick: fn(&DeploymentStatus) -> Option<i32>| status.and_then(pick).unwrap_or(0);

    status.and_then(|s| s.observed_generation).unwrap_or(0) >= generation
        && count(|s| s.updated_replicas) == desired
        && count(|s| s.replicas) == desired
        && count(|s| s.ready_replicas) == desired
        && count(|s| s.available_replicas) == desired
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
            Object::Deployment(Box::new(objects::deployment(
                &config, &name, "hash", None, None,
            ))),
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

    /// The five numbers `kubectl rollout status` waits on, one broken at a
    /// time. Each of these is a real moment in a `Recreate` roll, and calling
    /// any of them "done" is what would let a fleet sweep march past a tenant
    /// it had just taken down.
    #[test]
    fn a_rollout_is_complete_only_when_all_five_numbers_agree() {
        use k8s_openapi::api::apps::v1::DeploymentSpec;

        let deployment = |generation: i64, status: DeploymentStatus| Deployment {
            metadata: ObjectMeta {
                generation: Some(generation),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(1),
                ..Default::default()
            }),
            status: Some(status),
        };
        let settled = DeploymentStatus {
            observed_generation: Some(3),
            replicas: Some(1),
            updated_replicas: Some(1),
            ready_replicas: Some(1),
            available_replicas: Some(1),
            ..Default::default()
        };
        assert!(rolled_out(&deployment(3, settled.clone())));

        // The apply landed and the controller has not looked yet: every number
        // below is about the template this roll replaced.
        assert!(!rolled_out(&deployment(
            4,
            DeploymentStatus {
                observed_generation: Some(3),
                ..settled.clone()
            }
        )));
        // The new pod is not created yet.
        assert!(!rolled_out(&deployment(
            3,
            DeploymentStatus {
                updated_replicas: Some(0),
                ready_replicas: Some(0),
                available_replicas: Some(0),
                ..settled.clone()
            }
        )));
        // The old pod is still terminating, and still on the volume.
        assert!(!rolled_out(&deployment(
            3,
            DeploymentStatus {
                replicas: Some(2),
                ..settled.clone()
            }
        )));
        // Running is not serving: the daemon opens 8848 last.
        assert!(!rolled_out(&deployment(
            3,
            DeploymentStatus {
                ready_replicas: Some(0),
                available_replicas: Some(0),
                ..settled.clone()
            }
        )));
        // Ready is not STAYING ready. This is the shape of a daemon that came
        // up, answered one probe and died, and of one still inside its
        // `minReadySeconds` soak: the roller must wait through both rather than
        // step to the next tenant on a green that is seconds old.
        assert!(!rolled_out(&deployment(
            3,
            DeploymentStatus {
                available_replicas: Some(0),
                ..settled.clone()
            }
        )));

        // A Deployment nothing has reported on. `spec.replicas` absent is 1,
        // so an empty status is one replica short of everything.
        assert!(!rolled_out(&Deployment::default()));
        assert!(!rolled_out(&Deployment {
            status: Some(DeploymentStatus::default()),
            ..Default::default()
        }));
        // And the same object once the controller has caught up with it, with
        // the replica count left to the default on both sides.
        assert!(rolled_out(&Deployment {
            status: Some(DeploymentStatus {
                observed_generation: Some(0),
                replicas: Some(1),
                updated_replicas: Some(1),
                ready_replicas: Some(1),
                available_replicas: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        }));
    }

    /// The zero that satisfies every rule and means the opposite of finished.
    ///
    /// At `spec.replicas: 0` all five numbers agree at 0, so a predicate that
    /// only compared them would call a tenant with NO POD a completed rollout -
    /// and the caller that acts on the answer would step to the next tenant on
    /// the strength of a mailbox that is off. A desired count under 1 is never
    /// a finished rollout, whatever the status says.
    #[test]
    fn a_tenant_scaled_to_zero_has_not_finished_rolling_out() {
        use k8s_openapi::api::apps::v1::DeploymentSpec;

        let off = |replicas: i32| Deployment {
            metadata: ObjectMeta {
                generation: Some(2),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(replicas),
                ..Default::default()
            }),
            status: Some(DeploymentStatus {
                observed_generation: Some(2),
                replicas: Some(0),
                updated_replicas: Some(0),
                ready_replicas: Some(0),
                available_replicas: Some(0),
                ..Default::default()
            }),
        };
        assert!(!rolled_out(&off(0)));
        // The same object with the count it is meant to have is the ordinary
        // "not ready yet", which proves the assertion above is the zero and
        // not the shape.
        assert!(!rolled_out(&off(1)));
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
        let object = Object::Deployment(Box::new(objects::deployment(
            &config, &name, "hash", None, None,
        )));
        assert_eq!(object.kind(), Kind::Deployment);
        assert_eq!(object.name(), "alice");
    }
}
