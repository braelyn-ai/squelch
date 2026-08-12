//! The test double for [`crate::cluster::Cluster`], plus the fixtures every
//! test module builds on.
//!
//! Compiled only under `cfg(test)`: a provisioner that shipped a fake cluster
//! in its release binary would be one refactor away from using it. Nothing in
//! this crate's suite needs a kube-apiserver, a kubeconfig, a container
//! runtime, or a network, and this file is why.
//!
//! [`MockCluster`] imitates the two API-server behaviours the warden actually
//! depends on, rather than pretending to be Kubernetes:
//!
//! - `stringData` on a Secret is stored as `data`. A mock that echoed
//!   `stringData` back would let a reader that looks at the wrong field pass
//!   here and find nothing on a real cluster.
//! - a Deployment that has been applied reports a ready replica, unless the
//!   test says otherwise. That is what makes `pending` / `active` / `failed`
//!   testable without a scheduler.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::ByteString;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentStatus};
use k8s_openapi::api::core::v1::Secret;

use crate::cluster::{Cluster, ClusterError, ExecOutput, Kind, Object};
use crate::config::{
    Config, DEFAULT_BIND, DEFAULT_CPU_LIMIT, DEFAULT_CPU_REQUEST, DEFAULT_EPHEMERAL_LIMIT,
    DEFAULT_EPHEMERAL_REQUEST, DEFAULT_INGRESS_CLASS, DEFAULT_INGRESS_NAMESPACE,
    DEFAULT_MEMORY_LIMIT, DEFAULT_MEMORY_REQUEST, DEFAULT_OAUTH_SECRET_NAME,
    DEFAULT_PENDING_TTL_SECS, DEFAULT_RUN_AS, DEFAULT_STORAGE_CLASS, DEFAULT_STORAGE_SIZE,
    DEFAULT_TENANT_NAMESPACE, DEFAULT_TLS_SECRET, DEFAULT_TMP_SIZE, Resources,
};
use crate::provision::Warden;

/// The bearer every test presents.
pub const TEST_TOKEN: &str = "warden-test-token-0123456789abcdef";

/// A config with the production defaults and no environment read.
pub fn test_config() -> Config {
    Config {
        bind: DEFAULT_BIND.parse().unwrap(),
        token: TEST_TOKEN.to_string(),
        base_domain: "passband.email".to_string(),
        image: "ghcr.io/braelyn-ai/squelchd:v0.4.0".to_string(),
        namespace: DEFAULT_TENANT_NAMESPACE.to_string(),
        ingress_namespace: DEFAULT_INGRESS_NAMESPACE.to_string(),
        ingress_pod_label: ("app.kubernetes.io/name".to_string(), "traefik".to_string()),
        ingress_class: DEFAULT_INGRESS_CLASS.to_string(),
        tls_secret: DEFAULT_TLS_SECRET.to_string(),
        oauth_secret_name: DEFAULT_OAUTH_SECRET_NAME.to_string(),
        storage_class: DEFAULT_STORAGE_CLASS.to_string(),
        storage_size: DEFAULT_STORAGE_SIZE.to_string(),
        daemon_resources: Resources {
            cpu_request: DEFAULT_CPU_REQUEST.to_string(),
            cpu_limit: DEFAULT_CPU_LIMIT.to_string(),
            memory_request: DEFAULT_MEMORY_REQUEST.to_string(),
            memory_limit: DEFAULT_MEMORY_LIMIT.to_string(),
            ephemeral_request: DEFAULT_EPHEMERAL_REQUEST.to_string(),
            ephemeral_limit: DEFAULT_EPHEMERAL_LIMIT.to_string(),
        },
        tmp_size: DEFAULT_TMP_SIZE.to_string(),
        pull_secret: None,
        user_namespaces: true,
        model_pvc: None,
        node_cidr: None,
        run_as: DEFAULT_RUN_AS,
        ready_timeout: Duration::from_secs(1),
        pending_ttl: Duration::from_secs(DEFAULT_PENDING_TTL_SECS),
        trusted_proxy_hops: 0,
        llm_base_url: None,
        llm_stage1_model: None,
        llm_stage2_model: None,
        llm_stage1_daily_cap: None,
        llm_stage2_daily_cap: None,
    }
}

/// [`test_config`] with the LLM gateway feature switched on, for the llm-key
/// paths: `set_llm_key` refuses outright when `llm_base_url` is absent.
pub fn llm_test_config() -> Config {
    Config {
        llm_base_url: Some("https://bifrost.railway.internal".to_string()),
        ..test_config()
    }
}

/// An age-armored body of the shape the control plane sends. Not real
/// ciphertext: the warden never decrypts, so nothing in these tests needs it to
/// be.
pub fn armored(marker: &str) -> String {
    format!(
        "-----BEGIN AGE ENCRYPTED FILE-----\nYWdlLWVuY3J5cHRpb24ub3JnL3Yx{marker}\n-----END AGE ENCRYPTED FILE-----\n"
    )
}

/// Exactly what `squelchd pair --url ...` prints today, captured from
/// `squelchd/src/bin/squelchd.rs::cmd_pair`. The parser is held to this string,
/// so when the daemon's output changes this is the thing to change first and
/// watch fail.
pub fn pair_stdout(code: &str, url: &str) -> String {
    format!(
        "Pairing code: {code}\n\
         Valid for 10 minutes, until 2026-08-07T12:00:00Z.\n\
         \n\
         On the device you are pairing, open this link:\n\
         \n\
         \x20 passband://pair?url={}&code={code}\n\
         \n\
         Or type the code into Passband, pointed at {url}\n",
        url.replace(':', "%3A").replace('/', "%2F"),
    )
}

#[derive(Default)]
struct MockInner {
    /// Everything that currently exists, keyed the way the API server keys it.
    objects: BTreeMap<(Kind, String), Object>,
    /// Every apply and create, in order.
    applied: Vec<(Kind, String)>,
    /// Every delete, in order.
    deleted: Vec<(Kind, String)>,
    execs: Vec<(String, Vec<String>)>,
    exec_stdout: String,
    exec_ok: bool,
    /// Whether an applied Deployment comes up.
    ready: bool,
    /// Whether reads answer at all.
    reads_ok: bool,
    /// Whether the next `create` loses a race with a concurrent one.
    create_loses_race: bool,
}

/// A cluster that records instead of connecting.
pub struct MockCluster {
    inner: Mutex<MockInner>,
}

impl Default for MockCluster {
    fn default() -> Self {
        Self::new()
    }
}

impl MockCluster {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockInner {
                exec_stdout: pair_stdout("ABCD-1234", "https://alice.passband.email"),
                exec_ok: true,
                ready: true,
                reads_ok: true,
                ..Default::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pods stop coming up. A Deployment already stored keeps whatever status
    /// it was last applied with, exactly as a real one would: a Deployment's
    /// status changes when the cluster changes it, not when a flag flips.
    pub fn never_ready(&self) {
        self.lock().ready = false;
    }

    /// The cluster recovers. The next apply of a Deployment records a ready
    /// replica; one applied while it was broken still reads as not ready until
    /// something applies it again, which is what makes the retry path testable.
    pub fn becomes_ready(&self) {
        self.lock().ready = true;
    }

    /// Every read fails, as a partitioned API server would.
    pub fn break_reads(&self) {
        self.lock().reads_ok = false;
    }

    /// The next `create` loses a race, the way a second signup for one label
    /// does: the read said the name was free and the write says it is not.
    /// The only way to reach that path, since the mock is otherwise serialized.
    pub fn create_loses_race(&self) {
        self.lock().create_loses_race = true;
    }

    /// What the next `squelchd pair` prints.
    pub fn exec_prints(&self, stdout: &str) {
        let mut inner = self.lock();
        inner.exec_stdout = stdout.to_string();
        inner.exec_ok = true;
    }

    /// The next exec exits non-zero.
    pub fn exec_fails(&self) {
        self.lock().exec_ok = false;
    }

    /// Every apply and create, in order, as `(kind, name)`.
    pub fn applied(&self) -> Vec<(Kind, String)> {
        self.lock().applied.clone()
    }

    /// Just the names, for the phase-one assertions.
    pub fn applied_names(&self) -> Vec<String> {
        self.lock()
            .applied
            .iter()
            .map(|(_, name)| name.clone())
            .collect()
    }

    /// Every delete, in order.
    pub fn deleted(&self) -> Vec<(Kind, String)> {
        self.lock().deleted.clone()
    }

    /// The typed object currently stored under `(kind, name)`.
    pub fn object(&self, kind: Kind, name: &str) -> Option<Object> {
        self.lock().objects.get(&(kind, name.to_string())).cloned()
    }

    pub fn exists(&self, kind: Kind, name: &str) -> bool {
        self.object(kind, name).is_some()
    }

    /// The stored Secret, with `data` populated the way the API server would.
    pub fn secret(&self, name: &str) -> Option<Secret> {
        match self.object(Kind::Secret, name) {
            Some(Object::Secret(secret)) => Some(*secret),
            _ => None,
        }
    }

    /// The last `(pod, argv)` an exec ran with.
    pub fn last_exec(&self) -> Option<(String, Vec<String>)> {
        self.lock().execs.last().cloned()
    }

    fn store(&self, object: Object) {
        let mut inner = self.lock();
        let key = (object.kind(), object.name().to_string());
        let ready = inner.ready;
        inner.applied.push(key.clone());
        inner.objects.insert(key, persist(object, ready));
    }
}

/// Imitate the API server: `stringData` is folded into `data`, and an applied
/// Deployment gets the status a running one would have.
fn persist(object: Object, ready: bool) -> Object {
    match object {
        Object::Secret(mut secret) => {
            if let Some(string_data) = secret.string_data.take() {
                let data = secret.data.get_or_insert_with(BTreeMap::new);
                for (key, value) in string_data {
                    data.insert(key, ByteString(value.into_bytes()));
                }
            }
            Object::Secret(secret)
        }
        Object::Deployment(mut deployment) => {
            deployment.status = Some(DeploymentStatus {
                ready_replicas: Some(i32::from(ready)),
                ..Default::default()
            });
            Object::Deployment(deployment)
        }
        other => other,
    }
}

#[async_trait]
impl Cluster for MockCluster {
    async fn apply(&self, object: Object) -> Result<(), ClusterError> {
        self.store(object);
        Ok(())
    }

    async fn create(&self, object: Object) -> Result<(), ClusterError> {
        if self.lock().create_loses_race || self.exists(object.kind(), object.name()) {
            return Err(ClusterError::AlreadyExists);
        }
        self.store(object);
        Ok(())
    }

    async fn get_secret(&self, name: &str) -> Result<Option<Secret>, ClusterError> {
        if !self.lock().reads_ok {
            return Err(ClusterError::NoPod);
        }
        Ok(self.secret(name))
    }

    async fn list_secrets(&self, selector: &str) -> Result<Vec<Secret>, ClusterError> {
        if !self.lock().reads_ok {
            return Err(ClusterError::NoPod);
        }
        // The API server matches labels; so does this, on the one selector the
        // warden ever sends.
        let (key, value) = selector.split_once('=').ok_or(ClusterError::NoPod)?;
        Ok(self
            .lock()
            .objects
            .values()
            .filter_map(|object| match object {
                Object::Secret(secret) => Some(secret.as_ref().clone()),
                _ => None,
            })
            .filter(|secret| {
                secret
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(key))
                    .is_some_and(|v| v == value)
            })
            .collect())
    }

    async fn get_deployment(&self, name: &str) -> Result<Option<Deployment>, ClusterError> {
        if !self.lock().reads_ok {
            return Err(ClusterError::NoPod);
        }
        Ok(match self.object(Kind::Deployment, name) {
            Some(Object::Deployment(deployment)) => Some(*deployment),
            _ => None,
        })
    }

    async fn delete(&self, kind: Kind, name: &str) -> Result<(), ClusterError> {
        let mut inner = self.lock();
        inner.deleted.push((kind, name.to_string()));
        inner.objects.remove(&(kind, name.to_string()));
        Ok(())
    }

    async fn ready_pod(&self, selector: &str, within: Duration) -> Result<String, ClusterError> {
        if !self.lock().ready {
            return Err(ClusterError::Timeout(within));
        }
        // The name a ReplicaSet would have given it, derived from the selector
        // so a test can tell one tenant's pod from another's.
        let instance = selector
            .split(',')
            .find_map(|part| part.strip_prefix("app.kubernetes.io/instance="))
            .ok_or(ClusterError::NoPod)?;
        Ok(format!("{instance}-abc123"))
    }

    async fn exec(&self, pod: &str, argv: &[String]) -> Result<ExecOutput, ClusterError> {
        let mut inner = self.lock();
        inner.execs.push((pod.to_string(), argv.to_vec()));
        Ok(ExecOutput {
            stdout: inner.exec_stdout.clone(),
            stderr: String::new(),
            ok: inner.exec_ok,
        })
    }
}

/// A warden wired to a mock cluster, with the mock kept for assertions.
pub struct Harness {
    pub warden: Arc<Warden>,
    pub cluster: Arc<MockCluster>,
    pub config: Arc<Config>,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_config(test_config())
    }

    pub fn with_config(config: Config) -> Self {
        let cluster = Arc::new(MockCluster::new());
        let config = Arc::new(config);
        let warden = Arc::new(Warden::new(
            config.clone(),
            cluster.clone() as Arc<dyn Cluster>,
        ));
        Self {
            warden,
            cluster,
            config,
        }
    }

    /// Router state over this harness's warden, for the wire tests.
    pub fn state(&self) -> crate::WardenState {
        crate::WardenState::new(self.warden.clone())
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}
