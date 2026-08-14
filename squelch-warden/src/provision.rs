//! Provisioning: two phases, seven objects, and no state of our own.
//!
//! The v1 warden was an orchestrator. It allocated ports, wrote files, drove
//! systemd, kept a JSON state file, and unwound every one of those on any
//! failure. All of that is gone. Kubernetes has a control loop; this service
//! mints identities, renders typed manifests, and asks. What is left here is
//! ordering and refusals.
//!
//! ## Why two phases
//!
//! The control plane cannot seal a credential until it knows the tenant's
//! recipient, and the tenant's recipient does not exist until somebody mints
//! it. So:
//!
//! 1. `POST /v1/tenants` mints the identity, writes it into the tenant's Secret,
//!    and returns the RECIPIENT. Nothing else exists yet. The tenant is
//!    `pending`.
//! 2. `PUT /v1/tenants/{label}/credentials` takes the sealed blob, applies the
//!    workload, waits for the pod, and execs `squelchd pair`.
//!
//! A signup that dies between the two leaves a pending tenant, and phase one is
//! idempotent for exactly that case: re-posting the same label with the same
//! address returns the same recipient rather than minting a second key that
//! nothing was sealed to. A pending tenant that nobody ever comes back for is
//! collected by [`Warden::sweep_pending`] once it is older than the configured
//! TTL; recovery is signing up again.
//!
//! ## Why there is no unwind
//!
//! Every apply is a server-side apply, so a retry of phase two converges rather
//! than duplicating. Nothing is allocated that could leak (the daemon port is
//! the same 8848 in every pod's own network namespace) and nothing is recorded
//! that could go stale (the cluster is the record). A failed phase two leaves
//! objects that the next attempt overwrites, which is a better answer than a
//! best-effort teardown running on a cluster that is already misbehaving.
//!
//! ## What "converges" means for a credential, exactly
//!
//! Storing a new sealed blob is not enough on its own to change what a daemon
//! is using: the daemon reads a COPY on its own volume, because a Secret mount
//! is read-only and the file is rewritten on every token refresh. So phase two
//! stamps the ciphertext's hash onto the pod template
//! ([`objects::CREDENTIAL_HASH_ANNOTATION`]), which makes a new blob a new pod
//! spec, which is a roll, and the init container reinstalls precisely when the
//! mounted Secret differs from the marker it left last time.
//!
//! Which leaves one rule worth stating plainly: **a PUT converges for a tenant
//! that is pending, failed or stopped, and a tenant that is ACTIVE is a 409.**
//! Re-consent for a running tenant is therefore DELETE (which keeps the mail)
//! and then PUT, not PUT alone.
//!
//! ## Privacy, and these are not suggestions
//!
//! - The minted identity is never logged, never returned, and never persisted
//!   anywhere but the tenant's Secret.
//! - The credential ciphertext is never logged and never inspected beyond the
//!   armor check in [`crate::validate::validate_ciphertext`].
//! - `squelchd pair` prints a LIVE pairing code. No exec output is logged at
//!   any level, which is the only rule that keeps that true.
//! - [`Warden::drift`] and [`Warden::reconcile`] recover the stored ciphertext
//!   and the stored LLM key to re-render a Deployment, and both leave those
//!   functions only as a SHA-256 in a pod-template annotation. What a drift
//!   report quotes is a Deployment spec: field names, images, mount points, and
//!   Secret references by name.
//! - The tenant's mailbox address never reaches a log line or a response body.
//!   The label does: it is a public subdomain and it is in the ingress
//!   controller's access log already, and an operator with no identifier cannot
//!   debug a failed signup.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use k8s_openapi::api::core::v1::Secret;

use crate::cluster::{Cluster, ClusterError, Kind, Object};
use crate::config::Config;
use crate::drift::{self, DriftReport};
use crate::identity::TenantIdentity;
use crate::objects;
use crate::pair::{self, Pairing};
use crate::validate::{self, ApiKeyError, CiphertextError, EmailError, LabelError, TenantName};

/// What a tenant looks like from outside. The four words the wire has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantStatus {
    /// Phase one done, phase two not started: an identity exists and nothing
    /// is running. A signup that was interrupted looks exactly like this, which
    /// is why phase one is idempotent.
    Pending,
    /// A Deployment with a ready replica.
    Active,
    /// A Deployment with no ready replica. Not a diagnosis, a statement: this
    /// tenant is not serving right now.
    Failed,
    /// Deleted: the workload is gone and the data, the identity and the sealed
    /// credential are all still here.
    Stopped,
}

impl TenantStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// The answer to phase one.
#[derive(Debug, Clone)]
pub struct Created {
    /// `age1...`. The control plane seals this tenant's credentials to it and
    /// can never open them again.
    pub recipient: String,
}

/// The answer to a reconcile: what it took to put the tenant back on today's
/// render.
///
/// Two words, both fixed vocabulary, and nothing about the tenant. The report
/// of what was WRONG is [`DriftReport`]'s job; this says what was done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Reconciled {
    /// `created`, `converged` or `recreated`. The last one means the Deployment
    /// was deleted and applied fresh because another field manager owned part
    /// of it; see [`Warden::reconcile`] for why nothing gentler works.
    pub deployment: &'static str,
    /// Always `active`, and it is the stronger sense of the word: a reconcile
    /// that returns at all has waited for the Deployment's ROLLOUT to finish,
    /// so every replica is on the render this call applied and serving. The
    /// status route says `active` for one ready replica of any generation,
    /// which is the right answer to a different question.
    pub status: &'static str,
}

impl Reconciled {
    fn new(deployment: &'static str) -> Self {
        Self {
            deployment,
            status: TenantStatus::Active.as_str(),
        }
    }
}

/// What a fleet roll looked at, and what it did about it.
///
/// Counts and LABELS, which are public subdomains and are in the ingress
/// controller's access log already. An operator reading this has to be able to
/// say which tenants moved, which ones need a person, and where a halted run
/// stopped; nothing else about a tenant belongs in it. See [`Warden::roll`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Rolled {
    /// How many tenants this run examined. On a halt that is the fleet up to
    /// and INCLUDING the one it stopped at, so the four buckets below always
    /// add up to this - and the gap between it and the fleet's size is how many
    /// tenants the run never reached.
    pub checked: usize,
    /// Converged onto today's render, in the order it happened, each with a
    /// finished rollout before the next was touched. In a dry run, the tenants
    /// that WOULD have been.
    pub rolled: Vec<String>,
    /// Already matched the render. A re-run of a finished roll is all of these
    /// and no writes at all.
    pub current: usize,
    /// Skipped because another field manager owns part of the Deployment.
    /// Repairing one costs it its pod, so a person decides; see
    /// [`Warden::roll`].
    pub skipped_foreign: Vec<String>,
    /// Skipped because there is no workload to converge: pending is a signup to
    /// finish, stopped is an account to reopen.
    pub skipped_inactive: Vec<String>,
    /// The label the run stopped at, if it stopped. Everything after it in
    /// [`Warden::fleet`] order was left exactly as it was.
    pub halted_on: Option<String>,
}

/// What one tenant's turn in a [`Warden::roll`] came to. Internal: what leaves
/// the roll is [`Rolled`], which is the same four outcomes counted up.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Converged, or in a dry run, would have been.
    Rolled,
    Current,
    Foreign,
    Inactive,
}

/// Why an operation failed.
///
/// The `Cluster` variant carries a MACHINE REASON and nothing else: it goes on
/// the wire to the control plane, which logs it, and an API error string there
/// would be this cluster's internals in someone else's log aggregator. The
/// detail is logged here instead, where it belongs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WardenError {
    #[error("invalid label: {0}")]
    InvalidLabel(#[from] LabelError),
    #[error("invalid account_email: {0}")]
    InvalidEmail(#[from] EmailError),
    #[error("invalid cred_read_ciphertext: {0}")]
    InvalidCiphertext(#[from] CiphertextError),
    #[error("invalid api_key: {0}")]
    InvalidApiKey(#[from] ApiKeyError),
    /// The llm-key body named neither slot. An absent slot means "leave it as
    /// it is", so a request with both absent would install nothing and roll
    /// nothing; that is a caller bug worth naming, not a no-op to swallow.
    #[error("no keys in the request")]
    NoKeys,
    #[error("label already exists")]
    Conflict,
    #[error("no such tenant")]
    NotFound,
    /// The LLM gateway feature is off on this warden
    /// ([`crate::config::Config::llm_base_url`] is absent), so a stored key
    /// would never reach any pod. Refusing beats storing it and rolling the
    /// fleet for nothing.
    #[error("llm gateway not configured")]
    LlmNotConfigured,
    /// There is no workload to converge: the tenant is pending or stopped, and
    /// both of those transitions belong to somebody else. See
    /// [`Warden::reconcile`].
    #[error("nothing to reconcile")]
    NotReconcilable,
    #[error("{reason}")]
    Cluster { reason: &'static str },
}

impl WardenError {
    pub(crate) fn cluster(reason: &'static str) -> Self {
        Self::Cluster { reason }
    }
}

/// Log the shape of the failure here and return the machine reason for the
/// wire.
///
/// [`ClusterError::summary`] rather than the error's `Display`: a kube API
/// error carries the API server's message, and the API server quotes the
/// offending request back in some of them. The offending request, on this
/// service, is somebody's sealed credential.
fn fail(label: &str, reason: &'static str, error: &ClusterError) -> WardenError {
    tracing::error!(
        tenant = label,
        reason,
        error = %error.summary(),
        "warden step failed"
    );
    WardenError::cluster(reason)
}

/// Unix seconds. A clock set before 1970 gives 0, which stamps a record as
/// ancient and makes the sweep believe nothing has aged at all; both halves of
/// that are the harmless direction.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// When phase one ran, from [`objects::CREATED_AT_ANNOTATION`].
///
/// `None` for a Secret without one, and the sweep treats that as "not
/// collectable": a record this warden cannot date is a record it does not
/// destroy.
fn created_at(secret: &Secret) -> Option<u64> {
    secret
        .metadata
        .annotations
        .as_ref()?
        .get(objects::CREATED_AT_ANNOTATION)?
        .parse()
        .ok()
}

/// Read one key out of a Secret as the API server hands it back.
///
/// The warden WRITES `stringData` and the API server stores `data`, so a reader
/// that looked at `stringData` would work against a mock and find nothing
/// against a cluster.
fn secret_value(secret: &Secret, key: &str) -> Option<String> {
    secret
        .data
        .as_ref()
        .and_then(|data| data.get(key))
        .and_then(|bytes| String::from_utf8(bytes.0.clone()).ok())
}

/// The provisioner.
///
/// No lock and no state file. Two signups landing on the same label at the same
/// moment are settled by the API server: whichever `create` of the identity
/// Secret loses gets `AlreadyExists`, which becomes the same 409 a serialized
/// pair of requests would have produced.
pub struct Warden {
    config: Arc<Config>,
    cluster: Arc<dyn Cluster>,
}

impl Warden {
    pub fn new(config: Arc<Config>, cluster: Arc<dyn Cluster>) -> Self {
        Self { config, cluster }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Phase one: mint this tenant's age identity, store it, hand back the
    /// public recipient.
    ///
    /// Idempotent while the tenant is pending. A second call with the SAME
    /// address returns the SAME recipient, because the control plane may have
    /// lost its answer to a timeout and a fresh key would be a key nothing was
    /// ever sealed to. A second call with a DIFFERENT address is a 409: that is
    /// not a retry, it is two people asking for one subdomain.
    pub async fn create_tenant(
        &self,
        raw_label: &str,
        raw_email: &str,
    ) -> Result<Created, WardenError> {
        // Validation first, before anything is touched: a bad request must cost
        // the cluster nothing.
        let name = TenantName::parse(raw_label)?;
        let account_email = validate::validate_account_email(raw_email)?;

        if let Some(existing) = self.identity(&name).await? {
            let recipient = self.reuse(&name, &existing, &account_email).await?;
            tracing::info!(tenant = %name, "re-issued the recipient for a pending tenant");
            return Ok(Created { recipient });
        }

        // Minted here and alive only until the apply below. It is not returned,
        // not logged, and not kept: the tenant's Secret is the only copy in
        // existence once this function returns. (The bytes are not zeroized on
        // the way out; they pass through serde and TLS buffers this crate does
        // not own, so a claim of scrubbed memory would be a claim we cannot
        // keep. What we can promise is that nothing writes them anywhere else.)
        let identity = TenantIdentity::mint();
        let recipient = identity.recipient().to_string();
        let secret =
            objects::identity_secret(&self.config, &name, &identity, &account_email, now_secs());

        match self.cluster.create(Object::Secret(Box::new(secret))).await {
            Ok(()) => {}
            // Lost a race with a concurrent signup for the same label. The
            // other one won; this is the same answer a serialized pair would
            // have produced.
            Err(ClusterError::AlreadyExists) => return Err(WardenError::Conflict),
            Err(e) => return Err(fail(name.as_str(), "identity_write_failed", &e)),
        }

        tracing::info!(tenant = %name, "tenant pending: identity minted");
        Ok(Created { recipient })
    }

    /// Phase two: store the sealed credential, apply the workload, wait for the
    /// pod, and mint the first pairing.
    ///
    /// Also the re-consent path for a tenant that is not currently serving: the
    /// ciphertext's hash rides on the pod template, so a new blob rolls the
    /// Deployment and the init container reinstalls it. See the module docs.
    pub async fn set_credentials(
        &self,
        raw_label: &str,
        raw_ciphertext: &str,
    ) -> Result<Pairing, WardenError> {
        let name = TenantName::parse(raw_label)?;
        let ciphertext = validate::validate_ciphertext(raw_ciphertext)?;

        // An unknown label here means phase one never ran, or ran against a
        // different warden. Either way there is no key to have sealed to.
        if self.identity(&name).await?.is_none() {
            return Err(WardenError::NotFound);
        }
        // "Already provisioned" means SERVING, not merely "some objects exist".
        // A phase two that died waiting for a pod left objects behind, and the
        // control plane's retry has to be able to converge on them rather than
        // bounce off a 409 forever.
        if self.status_of(&name).await? == TenantStatus::Active {
            return Err(WardenError::Conflict);
        }

        // Verbatim. The warden does not parse, re-serialize or pretty-print
        // this: it is somebody else's ciphertext and the only correct thing to
        // do with it is put it where the daemon will look.
        self.apply(
            &name,
            Object::Secret(Box::new(objects::credential_secret(
                &self.config,
                &name,
                &ciphertext,
            ))),
            "credential_write_failed",
        )
        .await?;

        // Order is the contract. The volume exists before anything wants it;
        // the NetworkPolicy exists before the pod it polices, so a tenant is
        // never briefly reachable; the Ingress is last, so the hostname starts
        // answering only once there is something behind it.
        self.apply(
            &name,
            Object::Pvc(Box::new(objects::data_pvc(&self.config, &name))),
            "volume_failed",
        )
        .await?;
        self.apply(
            &name,
            Object::NetworkPolicy(Box::new(objects::network_policy(&self.config, &name))),
            "network_policy_failed",
        )
        .await?;
        self.apply(
            &name,
            Object::Service(Box::new(objects::service(&self.config, &name))),
            "service_failed",
        )
        .await?;
        // A virtual key stored before provisioning must reach the pod being
        // born: "PUT llm-key then PUT credentials" is a legal order, and the
        // pod that comes up here has to carry the key's hash or the first
        // rotation would find nothing to differ from. The Secret can only
        // exist if `llm_base_url` was configured when `set_llm_key` accepted
        // it, so this pickup cannot stamp the annotation with the feature off.
        let llm_hash = self.llm_hash(&name).await?;
        // The hash of what was just stored, not of what is running: this is the
        // whole mechanism by which a re-consent reaches the daemon.
        self.apply(
            &name,
            Object::Deployment(Box::new(objects::deployment(
                &self.config,
                &name,
                &objects::credential_hash(&ciphertext),
                llm_hash.as_deref(),
            ))),
            "workload_failed",
        )
        .await?;
        self.apply(
            &name,
            Object::Ingress(Box::new(objects::ingress(&self.config, &name))),
            "ingress_failed",
        )
        .await?;

        let pod = self
            .cluster
            .ready_pod(&objects::pod_selector(&name), self.config.ready_timeout)
            .await
            .map_err(|e| fail(name.as_str(), "not_ready", &e))?;

        let pairing = self.mint_pairing(&name, &pod).await?;
        tracing::info!(tenant = %name, "tenant provisioned");
        Ok(pairing)
    }

    /// Store or rotate the tenant's LLM gateway virtual keys: whichever of the
    /// triage and assistant relay slots the control plane sent, and at least
    /// one of them.
    ///
    /// `None` means "leave that slot as it is", and that promise has to be
    /// kept HERE: the apply below is a server-side apply with force under one
    /// field manager ([`crate::cluster`]), so a data key absent from the
    /// applied Secret is REMOVED, and a triage-only rotation that rendered
    /// only the triage key would silently clear an installed assistant key.
    /// So the stored Secret is read first and every slot the request did not
    /// provide is carried forward: what gets applied is always the union, and
    /// the roll hash is computed over that same union.
    ///
    /// Legal at any point after phase one, deliberately: the control plane
    /// mints the keys alongside the signup, so "PUT llm-key then PUT
    /// credentials" must birth a keyed pod, and a rotation against a running
    /// tenant must reach the daemon. The second half works exactly like a
    /// re-consent does — the combined hash of BOTH keys rides on the pod
    /// template ([`objects::LLM_KEY_HASH_ANNOTATION`] via
    /// [`objects::llm_keys_hash`], so rotating either one is a changed pod
    /// spec), and when a Deployment exists it is rebuilt and re-applied here
    /// and the changed annotation rolls the pod. With no Deployment yet,
    /// storing the Secret is the whole job; [`Warden::set_credentials`] picks
    /// the keys up when the workload is applied.
    ///
    /// The keys are live credentials: never logged, never returned, stored
    /// verbatim in the tenant's Secret and read back only as a hash.
    pub async fn set_llm_key(
        &self,
        raw_label: &str,
        raw_api_key: Option<&str>,
        raw_assistant_api_key: Option<&str>,
    ) -> Result<(), WardenError> {
        // Feature gate first: with no gateway URL configured, the env these
        // keys would feed is never rendered ([`objects::daemon_env`]), so
        // storing them would only roll pods onto values they cannot use.
        if self.config.llm_base_url.is_none() {
            return Err(WardenError::LlmNotConfigured);
        }
        let name = TenantName::parse(raw_label)?;
        if raw_api_key.is_none() && raw_assistant_api_key.is_none() {
            return Err(WardenError::NoKeys);
        }
        // Both slots held to the same constraints: both become env values
        // through the same secretKeyRef mechanism.
        let api_key = raw_api_key
            .map(validate::validate_llm_api_key)
            .transpose()?;
        let assistant_api_key = raw_assistant_api_key
            .map(validate::validate_llm_api_key)
            .transpose()?;
        if self.identity(&name).await?.is_none() {
            return Err(WardenError::NotFound);
        }

        // The union with what is already stored: the apply below force-owns
        // the whole Secret, so a slot missing from it would be DELETED, and a
        // one-slot rotation must not clear the other slot. A stored value was
        // validated on its way in, so it is carried forward verbatim.
        let existing = self
            .cluster
            .get_secret(&name.llm_secret())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?;
        let api_key = api_key.or_else(|| {
            existing
                .as_ref()
                .and_then(|s| secret_value(s, objects::LLM_API_KEY_KEY))
        });
        let assistant_api_key = assistant_api_key.or_else(|| {
            existing
                .as_ref()
                .and_then(|s| secret_value(s, objects::ASSISTANT_API_KEY_KEY))
        });

        self.apply(
            &name,
            Object::Secret(Box::new(objects::llm_secret(
                &self.config,
                &name,
                api_key.as_deref(),
                assistant_api_key.as_deref(),
            ))),
            "llm_key_write_failed",
        )
        .await?;

        let deployment = self
            .cluster
            .get_deployment(name.as_str())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?;
        if deployment.is_some() {
            // The rebuild needs the credential-ciphertext hash the running pod
            // was rolled for, and the stored Secret is the byte-exact source
            // of it: set_credentials wrote the validated ciphertext verbatim.
            let ciphertext = self
                .cluster
                .get_secret(&name.credential_secret())
                .await
                .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?
                .as_ref()
                .and_then(|secret| secret_value(secret, objects::CREDENTIAL_KEY));
            let Some(ciphertext) = ciphertext else {
                // A Deployment with no sealed credential behind it is a state
                // this warden never writes; refusing beats rolling a pod onto
                // a hash of nothing.
                tracing::error!(
                    tenant = %name,
                    reason = "credential_missing",
                    "a workload exists but its credential Secret does not"
                );
                return Err(WardenError::cluster("credential_missing"));
            };
            self.apply(
                &name,
                Object::Deployment(Box::new(objects::deployment(
                    &self.config,
                    &name,
                    &objects::credential_hash(&ciphertext),
                    // Over the UNION just applied, not the request: the pod
                    // must be rolled for what the Secret now holds.
                    Some(&objects::llm_keys_hash(
                        api_key.as_deref(),
                        assistant_api_key.as_deref(),
                    )),
                ))),
                "workload_failed",
            )
            .await?;
        }

        tracing::info!(tenant = %name, "llm key stored");
        Ok(())
    }

    /// Re-mint a pairing code for a later device. Nothing else about the tenant
    /// is touched, and the previous code is superseded, which is the daemon's
    /// documented behaviour: one live pairing code per account.
    pub async fn repair(&self, raw_label: &str) -> Result<Pairing, WardenError> {
        let name = TenantName::parse(raw_label)?;
        if self.identity(&name).await?.is_none() {
            return Err(WardenError::NotFound);
        }
        let pod = self
            .cluster
            .ready_pod(&objects::pod_selector(&name), self.config.ready_timeout)
            .await
            // A known tenant with nothing running is not a 404 (the tenant
            // exists) and not a 409 (nothing conflicts); it is this cluster
            // being unable to do the thing right now.
            .map_err(|e| fail(name.as_str(), "no_ready_pod", &e))?;
        self.mint_pairing(&name, &pod).await
    }

    /// What the cluster says about this tenant.
    pub async fn status(&self, raw_label: &str) -> Result<TenantStatus, WardenError> {
        let name = TenantName::parse(raw_label)?;
        if self.identity(&name).await?.is_none() {
            return Err(WardenError::NotFound);
        }
        self.status_of(&name).await
    }

    /// What is on this tenant's Deployment that the warden did not put there,
    /// and what an apply of today's render would change.
    ///
    /// Read-only. The one write it makes is a `dryRun=All` apply, which the API
    /// server merges, defaults and then discards; nothing is stored, nothing is
    /// rolled, and a tenant is exactly as it was when this returns. See
    /// [`crate::drift`] for why the two halves of the answer are two separate
    /// questions.
    ///
    /// A tenant with no Deployment - pending, or stopped - reports its status
    /// with both arrays empty. There is no object to have drifted, and
    /// rendering one to diff against would be inventing a finding.
    ///
    /// The render has to be the render this tenant would GET, which means the
    /// same two hashes phase two stamped on it: the credential ciphertext's,
    /// recovered byte-exact from the stored Secret, and the LLM key's when one
    /// exists. A render with either one wrong would report a pod-template
    /// annotation as drift on every single tenant.
    pub async fn drift(&self, raw_label: &str) -> Result<DriftReport, WardenError> {
        let name = TenantName::parse(raw_label)?;
        if self.identity(&name).await?.is_none() {
            return Err(WardenError::NotFound);
        }
        // Two reads of the same object rather than one shared one:
        // `status_of` is the single definition of what the four words mean,
        // and a diagnostic route can afford the second GET.
        let status = self.status_of(&name).await?;
        let live = self
            .cluster
            .get_deployment(name.as_str())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?;
        let Some(live) = live else {
            return Ok(DriftReport {
                status: status.as_str(),
                deployment_present: false,
                foreign: Vec::new(),
                changes: Vec::new(),
            });
        };

        let ciphertext = self
            .cluster
            .get_secret(&name.credential_secret())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?
            .as_ref()
            .and_then(|secret| secret_value(secret, objects::CREDENTIAL_KEY));
        let Some(ciphertext) = ciphertext else {
            // The same state `set_llm_key` refuses to render against: a
            // workload whose sealed credential is gone. There is no honest
            // render to compare the live object with.
            tracing::error!(
                tenant = %name,
                reason = "credential_missing",
                "a workload exists but its credential Secret does not"
            );
            return Err(WardenError::cluster("credential_missing"));
        };
        let llm_hash = self.llm_hash(&name).await?;

        let rendered = objects::deployment(
            &self.config,
            &name,
            &objects::credential_hash(&ciphertext),
            llm_hash.as_deref(),
        );
        let merged = self
            .cluster
            .apply_deployment_dry_run(rendered)
            .await
            // A dry run the API server REFUSES is a finding, not an outage: it
            // means today's render can no longer be applied to this tenant at
            // all - an immutable field the render moved, an admission webhook
            // that rejects it - and "the cluster is unavailable" would send the
            // operator looking at the wrong thing entirely. A 4xx is the API
            // server answering; anything else is it failing to.
            .map_err(|e| {
                let reason = match &e {
                    ClusterError::Api { source, .. } => match source.as_ref() {
                        kube::Error::Api(response) if (400..500).contains(&response.code) => {
                            "render_rejected"
                        }
                        _ => "cluster_unavailable",
                    },
                    _ => "cluster_unavailable",
                };
                fail(name.as_str(), reason, &e)
            })?;

        let foreign = drift::foreign_managers(&live);
        let changes = drift::diff_spec(
            &serde_json::to_value(&live.spec).unwrap_or(serde_json::Value::Null),
            &serde_json::to_value(&merged.spec).unwrap_or(serde_json::Value::Null),
        );
        Ok(DriftReport {
            status: status.as_str(),
            deployment_present: true,
            foreign,
            changes,
        })
    }

    /// Put a running tenant back onto today's render, and purge anything
    /// another field manager has taken ownership of.
    ///
    /// [`Warden::drift`] answers "what is wrong with this tenant"; this is the
    /// fix, and it is the only path in the service that repairs an object
    /// rather than converging one.
    ///
    /// ## Why a re-apply is not enough, and a delete is
    ///
    /// Server-side apply owns FIELDS. Every apply the warden makes declares the
    /// fields in [`objects::deployment`] and forces them; a field the warden
    /// does NOT declare belongs to whichever manager wrote it, and an apply
    /// neither reports it nor removes it, no matter how many times it runs.
    /// The incident this route exists for is exactly that shape: a
    /// `kubectl set env` stamped a Secret reference onto the seed container,
    /// the warden's applies converged around it for weeks, and it detonated as
    /// `Init:CreateContainerConfigError` the day the referenced Secret went
    /// away. There is no forced apply that takes that field back. Deleting the
    /// Deployment and applying a fresh one is the only honest purge: the new
    /// object's ownership ledger starts empty and carries exactly what the
    /// warden declares.
    ///
    /// So the delete happens only when [`drift::foreign_managers`] finds
    /// somebody, because a delete costs the tenant its pod and an ordinary
    /// re-apply does not.
    ///
    /// ## Why the wait between them
    ///
    /// The data volume is `ReadWriteOnce` and the daemon is one SQLite file.
    /// Within one Deployment the `Recreate` strategy guarantees the old pod is
    /// gone before the new one starts; across a delete and a re-create there is
    /// no controller holding that promise, so [`Cluster::pods_gone`] holds it
    /// here. Nothing is applied until the old pod is off the volume, and a
    /// timeout there is a refusal rather than a second writer.
    ///
    /// That refusal has a cost, and [`Warden::interrupted`] is what keeps it
    /// from becoming a trap: between the delete and the apply there is no
    /// Deployment, so a reconcile that dies in the window leaves a tenant
    /// reading [`TenantStatus::Stopped`], and a route that refused every
    /// stopped tenant would refuse to finish the job its own failure started.
    ///
    /// ## What it will and will not act on
    ///
    /// [`TenantStatus::Active`] and [`TenantStatus::Failed`] both proceed.
    /// Failed is precisely the incident state - a pod stuck on a foreign secret
    /// reference has no ready replica - and refusing it would make this route
    /// useless in the one case it was built for.
    ///
    /// [`TenantStatus::Pending`] is [`WardenError::NotReconcilable`]: it has
    /// never had a workload, and bringing one up is a signup to finish with
    /// [`Warden::set_credentials`], not a shape to converge.
    ///
    /// [`TenantStatus::Stopped`] depends on WHY it stopped, which the status
    /// word cannot say and [`Warden::interrupted`] can. A cancelled account and
    /// an interrupted reconcile look identical from the Deployment alone; the
    /// Service tells them apart, and starting a cancelled tenant back up would
    /// be a resurrection nobody asked for.
    ///
    /// Secrets are never rewritten. This converges SHAPE; identities,
    /// credentials and keys are what they were when it started, and the two
    /// hashes on the pod template are recovered from the stored Secrets so the
    /// render is the one this tenant is entitled to rather than a new one.
    pub async fn reconcile(&self, raw_label: &str) -> Result<Reconciled, WardenError> {
        let name = TenantName::parse(raw_label)?;
        if self.identity(&name).await?.is_none() {
            return Err(WardenError::NotFound);
        }
        match self.status_of(&name).await? {
            TenantStatus::Active | TenantStatus::Failed => {}
            // Stopped by an interrupted reconcile, not by a cancellation: the
            // objects around the missing Deployment are still standing, and
            // finishing is the whole point of the route.
            TenantStatus::Stopped if self.interrupted(&name).await? => {
                tracing::info!(
                    tenant = %name,
                    "resuming a reconcile that did not finish; the workload is still routed"
                );
            }
            TenantStatus::Pending | TenantStatus::Stopped => {
                return Err(WardenError::NotReconcilable);
            }
        }

        // Recovered before anything is written, so a tenant this warden cannot
        // render honestly costs the cluster nothing but reads.
        let ciphertext = self
            .cluster
            .get_secret(&name.credential_secret())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?
            .as_ref()
            .and_then(|secret| secret_value(secret, objects::CREDENTIAL_KEY));
        let Some(ciphertext) = ciphertext else {
            // The same state `set_llm_key` and `drift` refuse: a workload whose
            // sealed credential is gone. Rendering against a hash of nothing
            // would roll the pod onto a credential that never existed.
            tracing::error!(
                tenant = %name,
                reason = "credential_missing",
                "a workload exists but its credential Secret does not"
            );
            return Err(WardenError::cluster("credential_missing"));
        };
        let llm_hash = self.llm_hash(&name).await?;

        // The same order phase two applies in, for the same reasons: the volume
        // exists before anything wants it, the NetworkPolicy before the pod it
        // polices, and the Ingress last so the hostname answers only once there
        // is something behind it. That ordering matters more here than there,
        // because the pod in the middle of it may be about to be deleted.
        self.apply(
            &name,
            Object::Pvc(Box::new(objects::data_pvc(&self.config, &name))),
            "volume_failed",
        )
        .await?;
        self.apply(
            &name,
            Object::NetworkPolicy(Box::new(objects::network_policy(&self.config, &name))),
            "network_policy_failed",
        )
        .await?;
        self.apply(
            &name,
            Object::Service(Box::new(objects::service(&self.config, &name))),
            "service_failed",
        )
        .await?;

        let rendered = objects::deployment(
            &self.config,
            &name,
            &objects::credential_hash(&ciphertext),
            llm_hash.as_deref(),
        );
        let live = self
            .cluster
            .get_deployment(name.as_str())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?;
        let outcome = match live.as_ref().map(drift::foreign_managers) {
            // The Deployment was there when the status was read and is gone
            // now, so something deleted it while this ran - and the only thing
            // that deletes a live tenant's workload is a cancellation. Re-check
            // before rebuilding it: `delete` takes the Service first, so a
            // Service that has also gone means this is a teardown in progress
            // and applying would race a cancellation back onto the internet.
            None => {
                if !self.interrupted(&name).await? {
                    tracing::warn!(
                        tenant = %name,
                        "the workload was deleted while reconciling; refusing to rebuild it"
                    );
                    return Err(WardenError::NotReconcilable);
                }
                "created"
            }
            Some(foreign) if !foreign.is_empty() => {
                // COUNT, not names. A `fieldManager` is chosen by whoever wrote
                // the field, it may carry a newline, and this formatter writes
                // one line per event - so a name here would let the owner of
                // the drift forge log lines about it. The names are in the
                // drift report, which is structured and escaped.
                tracing::warn!(
                    tenant = %name,
                    managers = foreign.len(),
                    "recreating a Deployment other field managers own fields on"
                );
                self.cluster
                    .delete(Kind::Deployment, name.as_str())
                    .await
                    .map_err(|e| fail(name.as_str(), "workload_delete_failed", &e))?;
                // Nothing is applied until the old pod is off the volume.
                self.cluster
                    .pods_gone(&objects::pod_selector(&name), self.config.ready_timeout)
                    .await
                    .map_err(|e| fail(name.as_str(), "pods_not_gone", &e))?;
                // The delete is Background, so the name is free the moment the
                // API server answers. A Deployment still standing here is one
                // somebody else's finalizer is holding, and applying onto an
                // object mid-deletion would write a spec the collector is about
                // to throw away - leaving the tenant with no workload and this
                // route reporting success.
                if self
                    .cluster
                    .get_deployment(name.as_str())
                    .await
                    .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?
                    .is_some()
                {
                    tracing::error!(
                        tenant = %name,
                        reason = "workload_still_deleting",
                        "the Deployment outlived its own delete; a finalizer is holding it"
                    );
                    return Err(WardenError::cluster("workload_still_deleting"));
                }
                "recreated"
            }
            Some(_) => "converged",
        };
        self.apply(
            &name,
            Object::Deployment(Box::new(rendered)),
            "workload_failed",
        )
        .await?;
        self.apply(
            &name,
            Object::Ingress(Box::new(objects::ingress(&self.config, &name))),
            "ingress_failed",
        )
        .await?;

        // Everything is applied by the time this runs, so a timeout here says
        // the pod did not come back rather than that the reconcile did not
        // happen - the same thing a failed phase two says, and the operator
        // reads it the same way.
        //
        // The ROLLOUT rather than a ready pod, and the difference is the whole
        // value of the answer. Under `Recreate` the pod the render replaced is
        // Ready and matching this tenant's selector for as long as it takes to
        // terminate, so "some pod is ready" can be true of a tenant that is
        // about to be down. [`Cluster::rollout_complete`] is true only once the
        // controller has observed this spec and every replica on it is
        // serving, which is what a caller acting on the answer - the fleet
        // roll in [`Warden::roll`], stepping to the next tenant - is entitled
        // to assume it means.
        self.cluster
            .rollout_complete(name.as_str(), self.config.ready_timeout)
            .await
            .map_err(|e| fail(name.as_str(), "not_ready", &e))?;

        tracing::info!(tenant = %name, deployment = outcome, "tenant reconciled");
        Ok(Reconciled::new(outcome))
    }

    /// Stop a tenant: the workload, the route and the policy go; the DATA
    /// STAYS.
    ///
    /// The PersistentVolumeClaim, the identity Secret and the credential Secret
    /// are all left in place. This is the "cancel my account" path, not the
    /// "destroy my mail" path, and the second one is a later flag with a lot
    /// more ceremony behind it. The LLM Secret is the exception — it holds a
    /// gateway credential, not data, and goes with the workload; see below.
    ///
    /// Idempotent: an unknown label is success. The control plane calls this on
    /// its own unwind paths, where a retry after a partial failure must not
    /// turn into a 404 it has to special-case.
    pub async fn delete(&self, raw_label: &str) -> Result<(), WardenError> {
        let name = TenantName::parse(raw_label)?;
        // Ingress first: no new requests get routed at a pod that is about to
        // go. NetworkPolicy last: the pod stays policed for the whole of its
        // termination.
        //
        // The Service goes BEFORE the Deployment, and that order is load
        // bearing rather than cosmetic. This loop bails on its first error, so
        // whichever object is deleted first may be the only one deleted at all,
        // and [`Warden::interrupted`] reads a surviving Service as proof that a
        // reconcile died mid-purge and should be resumed. Taking the Deployment
        // first would let a half-finished cancellation - Deployment gone,
        // Service left behind by a failed call - look exactly like that, and
        // the next reconcile would put a cancelled mailbox back on the
        // internet. With this order, no state that has lost its Deployment can
        // still have its Service, so the signal cannot be forged by a failure
        // here. It also drains the endpoint before the pod goes, which is the
        // same direction the Ingress rule above is reasoning in.
        for (kind, reason) in [
            (Kind::Ingress, "ingress_delete_failed"),
            (Kind::Service, "service_delete_failed"),
            (Kind::Deployment, "workload_delete_failed"),
            (Kind::NetworkPolicy, "network_policy_delete_failed"),
        ] {
            self.cluster
                .delete(kind, name.as_str())
                .await
                .map_err(|e| fail(name.as_str(), reason, &e))?;
        }
        // The LLM key goes too, and it is the one Secret this path deletes: it
        // is a live credential to a shared gateway, not tenant data. The
        // control plane revokes the key when it cancels the account, so a kept
        // Secret would hold a dead token at best; a re-opened account gets a
        // freshly minted one through `set_llm_key`.
        self.cluster
            .delete(Kind::Secret, &name.llm_secret())
            .await
            .map_err(|e| fail(name.as_str(), "llm_key_delete_failed", &e))?;
        tracing::info!(tenant = %name, "tenant stopped; volume, identity and credential kept");
        Ok(())
    }

    /// Collect pending tenants older than the configured TTL, and return how
    /// many were collected.
    ///
    /// A signup that reached phase one and never came back parks an identity
    /// Secret nothing will ever open, holding a public subdomain against
    /// everyone else, and the control plane has no way to see it: the warden's
    /// wire is per-label and a label nobody remembers is a label nobody asks
    /// about. So this is the one janitor, on a timer.
    ///
    /// Three guards, and they are the reason this is safe to run unattended:
    ///
    /// - only a Secret named `<label>-identity` whose label still parses;
    /// - only one carrying [`objects::CREATED_AT_ANNOTATION`], so a record this
    ///   warden cannot date is a record it will not destroy;
    /// - only while [`TenantStatus::Pending`] holds, re-checked immediately
    ///   before the delete, which means no Deployment and no sealed credential
    ///   exist. A tenant that ever got as far as a credential is never
    ///   collectable by this path at any age.
    ///
    /// A tenant collected here loses nothing but a reservation: no mail exists,
    /// no credential was ever sealed, and the recovery is signing up again.
    ///
    /// One failure does not abort the sweep. The next tenant's Secret has
    /// nothing to do with this one's, and a janitor that gives up on the first
    /// error is a janitor that stops working the first time a label is odd.
    pub async fn sweep_pending(&self) -> Result<usize, WardenError> {
        let now = now_secs();
        let ttl = self.config.pending_ttl.as_secs();
        let secrets = self
            .cluster
            .list_secrets(objects::MANAGED_SELECTOR)
            .await
            // No tenant to name: this failure is the sweep's, not anyone's.
            .map_err(|e| fail("-", "sweep_list_failed", &e))?;

        let mut collected = 0usize;
        for secret in secrets {
            let Some(name) = secret
                .metadata
                .name
                .as_deref()
                .and_then(TenantName::from_identity_secret)
            else {
                continue;
            };
            let Some(created) = created_at(&secret) else {
                continue;
            };
            if now.saturating_sub(created) < ttl {
                continue;
            }
            match self.status_of(&name).await {
                Ok(TenantStatus::Pending) => {}
                Ok(_) => continue,
                Err(_) => continue,
            }
            match self
                .cluster
                .delete(Kind::Secret, &name.identity_secret())
                .await
            {
                Ok(()) => {
                    collected += 1;
                    tracing::info!(tenant = %name, "collected an abandoned pending tenant");
                }
                // Logged and dropped: nothing is waiting on this answer, and
                // the next pass tries again.
                Err(e) => {
                    let _ = fail(name.as_str(), "sweep_delete_failed", &e);
                }
            }
        }
        Ok(collected)
    }

    /// Every tenant this warden has provisioned, sorted.
    ///
    /// Read from the CLUSTER, by the same enumeration [`Warden::sweep_pending`]
    /// walks: the identity Secrets carrying [`objects::MANAGED_SELECTOR`], with
    /// any name that does not parse back into a label skipped rather than
    /// guessed at. The cluster is the record; there is no list of tenants
    /// anywhere in this service to fall out of date with it.
    ///
    /// Deliberately not the control plane's tenant table, and the difference is
    /// the whole point of doing it here. A tenant can exist in this cluster
    /// with no row over there - `squelch-control` logs that as PROVISIONED BUT
    /// NOT RECORDED, and it is what a signup that died between the warden's
    /// answer and the control plane's write leaves behind - and a tenant in
    /// that state is the one most likely to have been finished by hand, which
    /// is to say the one most likely to be shaped like nothing else in the
    /// fleet. A sweep that cannot see it is a sweep with a blind spot exactly
    /// where the drift is.
    ///
    /// Sorted so a run is deterministic and two runs are comparable: the same
    /// fleet is walked in the same order every time, and two summaries read
    /// side by side are two readings of one list.
    pub async fn fleet(&self) -> Result<Vec<TenantName>, WardenError> {
        let secrets = self
            .cluster
            .list_secrets(objects::MANAGED_SELECTOR)
            .await
            // No tenant to name: this failure is the enumeration's, not
            // anyone's.
            .map_err(|e| fail("-", "fleet_list_failed", &e))?;
        let mut fleet: Vec<TenantName> = secrets
            .iter()
            .filter_map(|secret| {
                secret
                    .metadata
                    .name
                    .as_deref()
                    .and_then(TenantName::from_identity_secret)
            })
            .collect();
        fleet.sort();
        Ok(fleet)
    }

    /// Walk the whole fleet and put every tenant that has fallen behind today's
    /// render back onto it, one at a time, stopping at the first failure.
    ///
    /// The warden writes a tenant's objects once, at provision time, and never
    /// revisits them. So changing [`crate::config::Config::image`] - or
    /// anything else [`objects::deployment`] renders - changes what the NEXT
    /// signup gets and nothing at all about the tenants already running. This
    /// is the pass that closes that gap, and it is the only thing in the
    /// service that touches more than one tenant's workload.
    ///
    /// ## Sequential, and verified between steps
    ///
    /// Zero downtime for a single tenant is not on offer: the strategy is
    /// `Recreate`, the volume is `ReadWriteOnce`, and the store is one SQLite
    /// file, so a tenant's daemon goes away for as long as its replacement
    /// takes to come up. What IS on offer is that the FLEET is never down, and
    /// that is bought by doing one tenant at a time and waiting for the roll to
    /// finish before starting the next. [`Warden::reconcile`] does that waiting
    /// through [`Cluster::rollout_complete`], which is why it and not a bare
    /// ready-pod check is what this loop stands on.
    ///
    /// ## Halting is the safety property, not a limitation
    ///
    /// A render that cannot come up is the failure this design fears, because
    /// a warden that kept going would apply it to every tenant it has. So the
    /// first tenant that does not converge ends the run: [`Rolled::halted_on`]
    /// names it, the tenants after it are not touched, and the answer is the
    /// summary of what happened rather than an error. A bad render costs
    /// exactly one tenant, which is the price that makes running this
    /// unattended defensible at all.
    ///
    /// That covers a read that fails as well as a reconcile that fails. A
    /// tenant this warden could not even inspect is not a tenant it may skip
    /// past - the next one would be rolled on the strength of a cluster that
    /// has just stopped answering - and throwing the summary away with an `Err`
    /// would discard the record of the tenants it already changed.
    ///
    /// ## What it refuses to touch
    ///
    /// **A Deployment another field manager owns fields on is SKIPPED**, and
    /// this is the single most important rule in here. `reconcile` purges a
    /// foreign-owned field the only way server-side apply allows: by DELETING
    /// the Deployment and applying a fresh one. That is a defensible decision
    /// for an operator looking at one drift report and a completely
    /// indefensible one for a timer walking a fleet, because it takes somebody's
    /// live mailbox down to remove a field that a human put there on purpose.
    /// Skipping also keeps this loop to plain server-side applies, so it can
    /// never race a concurrent signup into a workload it had deleted out from
    /// under it. Foreign drift is a page for a human, not a job for a timer.
    ///
    /// A tenant that is [`TenantStatus::Pending`] or [`TenantStatus::Stopped`]
    /// is skipped too, for the reason `reconcile` refuses them: pending is a
    /// signup to finish and stopped is an account to reopen, and neither is a
    /// shape to converge. A tenant an interrupted reconcile left mid-repair
    /// reads `stopped` as well, and this loop does not finish it either - it
    /// lands in [`Rolled::skipped_inactive`], where a person can see it and
    /// call [`Warden::reconcile`] on that one label.
    ///
    /// Which leaves [`TenantStatus::Active`] and [`TenantStatus::Failed`], the
    /// two that have a workload for today's render to apply to. Failed is
    /// deliberately in that list: a tenant a previous render broke is exactly
    /// the one a new render is likely to fix.
    ///
    /// `dry_run` does every read and no write: it reports what it WOULD have
    /// rolled, which is what an operator runs before the bump rather than
    /// after.
    ///
    /// PRIVACY: this walks every tenant in the fleet and logs as it goes. What
    /// goes in a line is a count, a status word or a LABEL, never a mailbox
    /// address and never an API error string; the per-tenant failure detail
    /// goes through [`fail`] like every other cluster error in this file.
    pub async fn roll(&self, dry_run: bool) -> Result<Rolled, WardenError> {
        let fleet = self.fleet().await?;
        tracing::info!(
            fleet = fleet.len(),
            dry_run,
            "fleet roll starting; one tenant at a time, stopping at the first failure"
        );

        let mut summary = Rolled::default();
        for name in &fleet {
            summary.checked += 1;
            let label = name.as_str().to_string();
            match self.roll_one(name, dry_run).await {
                Ok(Step::Rolled) => summary.rolled.push(label),
                Ok(Step::Current) => summary.current += 1,
                Ok(Step::Foreign) => summary.skipped_foreign.push(label),
                Ok(Step::Inactive) => summary.skipped_inactive.push(label),
                // The step already logged what failed and why, through `fail`.
                // What this adds is that the rest of the fleet is not going to
                // happen, which is the part an operator has to see.
                Err(_) => {
                    tracing::error!(
                        tenant = %name,
                        remaining = fleet.len() - summary.checked,
                        "fleet roll halted; the tenants after this one were not touched"
                    );
                    summary.halted_on = Some(label);
                    break;
                }
            }
        }

        tracing::info!(
            checked = summary.checked,
            rolled = summary.rolled.len(),
            current = summary.current,
            skipped_foreign = summary.skipped_foreign.len(),
            skipped_inactive = summary.skipped_inactive.len(),
            halted = summary.halted_on.is_some(),
            dry_run,
            "fleet roll finished"
        );
        Ok(summary)
    }

    /// One tenant's turn in a [`Warden::roll`]. The decisions and their reasons
    /// are documented there; this is the order they are asked in, which is
    /// cheapest-and-safest first: the status word, then the drift report, then
    /// the only call that writes anything.
    async fn roll_one(&self, name: &TenantName, dry_run: bool) -> Result<Step, WardenError> {
        match self.status_of(name).await? {
            TenantStatus::Active | TenantStatus::Failed => {}
            TenantStatus::Pending | TenantStatus::Stopped => return Ok(Step::Inactive),
        }

        let report = self.drift(name.as_str()).await?;
        if !report.foreign.is_empty() {
            // COUNT, not names, for the reason `reconcile` gives: a
            // `fieldManager` is chosen by whoever wrote the field and this
            // formatter writes one line per event. The names are in the drift
            // report, which is structured and escaped.
            tracing::warn!(
                tenant = %name,
                managers = report.foreign.len(),
                "skipping a tenant another field manager owns fields on; repairing it \
                 means deleting its workload, which is a decision for a person"
            );
            return Ok(Step::Foreign);
        }
        if report.changes.is_empty() {
            return Ok(Step::Current);
        }

        if dry_run {
            tracing::info!(
                tenant = %name,
                changes = report.changes.len(),
                "would roll this tenant onto today's render"
            );
            return Ok(Step::Rolled);
        }
        // No foreign owner, so this takes `reconcile`'s converged path: plain
        // applies, no delete, and a wait for the rollout to actually finish
        // before the next tenant is touched.
        let reconciled = self.reconcile(name.as_str()).await?;
        tracing::info!(
            tenant = %name,
            deployment = reconciled.deployment,
            changes = report.changes.len(),
            "rolled a tenant onto today's render"
        );
        Ok(Step::Rolled)
    }

    /// The tenant's identity Secret, if phase one ever ran.
    async fn identity(&self, name: &TenantName) -> Result<Option<Secret>, WardenError> {
        self.cluster
            .get_secret(&name.identity_secret())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))
    }

    /// The idempotent-retry path of phase one.
    async fn reuse(
        &self,
        name: &TenantName,
        existing: &Secret,
        account_email: &str,
    ) -> Result<String, WardenError> {
        // Only a PENDING tenant may be re-posted. Once a tenant is serving, a
        // second POST is somebody claiming a taken subdomain.
        if self.status_of(name).await? != TenantStatus::Pending {
            return Err(WardenError::Conflict);
        }
        let stored_email = secret_value(existing, objects::ACCOUNT_EMAIL_KEY);
        let recipient = secret_value(existing, objects::RECIPIENT_KEY);
        match (stored_email, recipient) {
            (Some(stored), Some(recipient)) if stored == account_email => Ok(recipient),
            // A pending tenant whose Secret has no recipient in it is a Secret
            // this warden did not write, or wrote and then had edited. Refusing
            // is the only safe answer: minting a second key would orphan
            // whatever the first one sealed.
            _ => Err(WardenError::Conflict),
        }
    }

    /// The pod-template hash for whatever LLM keys this tenant currently holds,
    /// or `None` for a tenant holding neither.
    ///
    /// Every path that RE-RENDERS an existing tenant's Deployment has to derive
    /// this the same way [`Warden::set_llm_key`] stamps it, and "the same way"
    /// means over BOTH slots through [`objects::llm_keys_hash`]. A caller that
    /// hashed only the triage key would produce a value no rotation ever
    /// writes, which makes every keyed tenant read as permanently drifted and
    /// makes a reconcile roll the pod onto a hash the next rotation disagrees
    /// with. Either slot alone is enough to have a hash: a half-failed mint
    /// leaves a Secret holding one of the two.
    ///
    /// It lives here rather than inline because it is the third caller that
    /// made the first two disagree.
    async fn llm_hash(&self, name: &TenantName) -> Result<Option<String>, WardenError> {
        Ok(self
            .cluster
            .get_secret(&name.llm_secret())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?
            .as_ref()
            .and_then(|secret| {
                let api_key = secret_value(secret, objects::LLM_API_KEY_KEY);
                let assistant = secret_value(secret, objects::ASSISTANT_API_KEY_KEY);
                (api_key.is_some() || assistant.is_some())
                    .then(|| objects::llm_keys_hash(api_key.as_deref(), assistant.as_deref()))
            }))
    }

    /// Whether a [`TenantStatus::Stopped`] tenant was stopped by an
    /// INTERRUPTED RECONCILE rather than by a cancellation.
    ///
    /// Both states are "no Deployment, credential still sealed", so the status
    /// word cannot tell them apart and the surviving objects have to. The two
    /// paths that reach here leave different wreckage:
    ///
    /// - [`Warden::delete`] takes the Ingress, the Deployment, the Service and
    ///   the NetworkPolicy down together. A cancelled tenant has no Service.
    /// - [`Warden::reconcile`] applies the Service BEFORE it touches the
    ///   Deployment, so a reconcile that died in the delete-recreate window
    ///   left the Service exactly where it was.
    ///
    /// So a surviving Service means the workload is still routed and only the
    /// Deployment is missing, which is a job to finish rather than an account
    /// to reopen. The check is deliberately the Service and not the Ingress:
    /// the Ingress is the object an operator is most likely to have applied by
    /// hand during the era this route replaces, and a hand-applied Ingress must
    /// not read as consent to restart a cancelled mailbox.
    async fn interrupted(&self, name: &TenantName) -> Result<bool, WardenError> {
        Ok(self
            .cluster
            .get_service(name.as_str())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?
            .is_some())
    }

    /// Derive the status from what exists. See [`TenantStatus`].
    async fn status_of(&self, name: &TenantName) -> Result<TenantStatus, WardenError> {
        let deployment = self
            .cluster
            .get_deployment(name.as_str())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?;

        if let Some(deployment) = deployment {
            let ready = deployment
                .status
                .and_then(|s| s.ready_replicas)
                .unwrap_or(0);
            return Ok(if ready >= 1 {
                TenantStatus::Active
            } else {
                TenantStatus::Failed
            });
        }
        // No workload. Whether that is "never started" or "deleted" is the
        // difference between a signup to finish and an account to reopen, and
        // the sealed credential is what tells them apart.
        let sealed = self
            .cluster
            .get_secret(&name.credential_secret())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?;
        Ok(if sealed.is_some() {
            TenantStatus::Stopped
        } else {
            TenantStatus::Pending
        })
    }

    async fn apply(
        &self,
        name: &TenantName,
        object: Object,
        reason: &'static str,
    ) -> Result<(), WardenError> {
        self.cluster
            .apply(object)
            .await
            .map_err(|e| fail(name.as_str(), reason, &e))
    }

    /// Run `squelchd pair` inside the tenant's own pod and read the handoff out
    /// of what it printed.
    ///
    /// Inside the pod because that is where the tenant's store is, and because
    /// the exec session inherits the container's environment, so the store
    /// path, the account address and `HOME` are already correct. The URL is the
    /// one thing the daemon cannot know about itself.
    async fn mint_pairing(&self, name: &TenantName, pod: &str) -> Result<Pairing, WardenError> {
        let url = self.config.tenant_url(name.as_str());
        let argv = objects::pair_argv(&self.config, name);
        let output = self
            .cluster
            .exec(pod, &argv)
            .await
            .map_err(|e| fail(name.as_str(), "pair_failed", &e))?;

        // From here to the end of this function the output is a live
        // credential. It is parsed, moved into the response, and dropped.
        // Nothing logs it, on either stream, at any level.
        if !output.ok {
            tracing::error!(
                tenant = %name,
                reason = "pair_failed",
                "squelchd pair exited non-zero"
            );
            return Err(WardenError::cluster("pair_failed"));
        }
        let combined = format!("{}\n{}", output.stdout, output.stderr);
        pair::parse(&combined, &url).ok_or_else(|| {
            tracing::error!(
                tenant = %name,
                reason = "pair_output_unparsed",
                "squelchd pair succeeded but its output did not contain a code and a link"
            );
            WardenError::cluster("pair_output_unparsed")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Harness, armored, llm_test_config};

    /// The whole two-phase happy path, asserted as an exact ordered list of
    /// applies. If provisioning grows a step, or reorders one, this test is the
    /// thing that notices.
    #[tokio::test]
    async fn provisions_a_tenant_in_two_phases() {
        let h = Harness::new();

        let created = h
            .warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        assert!(created.recipient.starts_with("age1"));

        // Phase one wrote the identity BEFORE the recipient came back: a
        // recipient the control plane holds and the cluster does not is a blob
        // nothing can ever open.
        assert_eq!(h.cluster.applied_names(), vec!["alice-identity"]);
        let stored = h.cluster.secret("alice-identity").unwrap();
        assert_eq!(
            crate::provision::secret_value(&stored, objects::RECIPIENT_KEY).unwrap(),
            created.recipient
        );
        assert_eq!(
            crate::provision::secret_value(&stored, objects::ACCOUNT_EMAIL_KEY).unwrap(),
            "alice@example.com"
        );
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Pending
        );

        let pairing = h
            .warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        assert_eq!(pairing.pair_code, "ABCD-1234");
        assert_eq!(pairing.pair_url, "https://alice.passband.email");
        assert!(pairing.deep_link.starts_with("passband://pair?url="));

        assert_eq!(
            h.cluster.applied(),
            vec![
                (Kind::Secret, "alice-identity".to_string()),
                (Kind::Secret, "alice-credential".to_string()),
                (Kind::Pvc, "alice-data".to_string()),
                (Kind::NetworkPolicy, "alice".to_string()),
                (Kind::Service, "alice".to_string()),
                (Kind::Deployment, "alice".to_string()),
                (Kind::Ingress, "alice".to_string()),
            ]
        );
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );

        // The pairing ran inside the tenant's own pod, with the tenant's URL.
        let (pod, argv) = h.cluster.last_exec().unwrap();
        assert_eq!(pod, "alice-abc123");
        assert_eq!(argv.last().unwrap(), "https://alice.passband.email");
    }

    /// The retry story the control plane depends on: a signup that died between
    /// the two calls comes back to the same key.
    #[tokio::test]
    async fn re_posting_a_pending_label_returns_the_same_recipient() {
        let h = Harness::new();
        let first = h
            .warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        let second = h
            .warden
            .create_tenant("ALICE", " alice@example.com ")
            .await
            .unwrap();
        assert_eq!(first.recipient, second.recipient);
        // No second identity was written.
        assert_eq!(h.cluster.applied_names(), vec!["alice-identity"]);
    }

    /// Two signups for one label landing at the same instant. Both read a free
    /// name; the API server settles it, and the loser of `create` gets the same
    /// 409 a serialized pair would have produced. The only path to
    /// `ClusterError::AlreadyExists` that is not already covered by the
    /// re-post case.
    #[tokio::test]
    async fn losing_a_create_race_is_the_same_409() {
        let h = Harness::new();
        h.cluster.create_loses_race();
        assert_eq!(
            h.warden
                .create_tenant("alice", "alice@example.com")
                .await
                .unwrap_err(),
            WardenError::Conflict
        );
        // The identity that lost was never stored, and nothing else was
        // touched: the winner's Secret is the only one in existence.
        assert!(h.cluster.applied().is_empty());
    }

    #[tokio::test]
    async fn a_pending_label_claimed_by_a_different_address_is_a_conflict() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        assert_eq!(
            h.warden
                .create_tenant("alice", "someone-else@example.com")
                .await
                .unwrap_err(),
            WardenError::Conflict
        );
    }

    #[tokio::test]
    async fn a_provisioned_label_is_a_conflict_even_for_its_owner() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();

        assert_eq!(
            h.warden
                .create_tenant("alice", "alice@example.com")
                .await
                .unwrap_err(),
            WardenError::Conflict
        );
        assert_eq!(
            h.warden
                .set_credentials("alice", &armored("alice"))
                .await
                .unwrap_err(),
            WardenError::Conflict
        );
    }

    /// Phase two on a label nobody minted a key for. There is nothing the
    /// ciphertext could have been sealed to.
    #[tokio::test]
    async fn credentials_for_an_unknown_label_are_a_404() {
        let h = Harness::new();
        assert_eq!(
            h.warden
                .set_credentials("nobody", &armored("x"))
                .await
                .unwrap_err(),
            WardenError::NotFound
        );
        assert!(h.cluster.applied().is_empty());
    }

    #[tokio::test]
    async fn invalid_input_never_reaches_the_cluster() {
        let h = Harness::new();

        assert!(matches!(
            h.warden
                .create_tenant("-nope-", "alice@example.com")
                .await
                .unwrap_err(),
            WardenError::InvalidLabel(_)
        ));
        assert_eq!(
            h.warden
                .create_tenant("mcp", "alice@example.com")
                .await
                .unwrap_err(),
            WardenError::InvalidLabel(LabelError::Reserved)
        );
        assert!(matches!(
            h.warden
                .create_tenant("alice", "alice@example.com\nSQUELCH_API_TOKEN=hunter2")
                .await
                .unwrap_err(),
            WardenError::InvalidEmail(_)
        ));

        // The one that matters most: a plaintext credentials file must never
        // land in a Secret.
        h.warden
            .create_tenant("bob", "bob@example.com")
            .await
            .unwrap();
        assert!(matches!(
            h.warden
                .set_credentials("bob", r#"{"slots":{"read:bob@x.com":{}}}"#)
                .await
                .unwrap_err(),
            WardenError::InvalidCiphertext(_)
        ));
        assert_eq!(h.cluster.applied_names(), vec!["bob-identity"]);
    }

    /// A phase two that died waiting for a pod must be retryable. This is the
    /// difference between "the signup is stuck forever" and "press it again".
    #[tokio::test]
    async fn a_failed_phase_two_can_be_retried() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.cluster.never_ready();

        assert_eq!(
            h.warden
                .set_credentials("alice", &armored("alice"))
                .await
                .unwrap_err(),
            WardenError::cluster("not_ready")
        );
        // Not `pending` any more, and not `active`: honestly failed.
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Failed
        );

        h.cluster.becomes_ready();
        let pairing = h
            .warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        assert_eq!(pairing.pair_code, "ABCD-1234");
    }

    /// Re-consent, end to end: a stopped tenant takes a NEW sealed blob, and
    /// the pod template changes with it. Without the hash annotation the
    /// Deployment would be byte-identical, nothing would roll, and the daemon
    /// would keep using the credential it already had.
    #[tokio::test]
    async fn a_stopped_tenant_can_be_re_credentialed() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("first"))
            .await
            .unwrap();
        let before = pod_annotation(&h);

        h.warden.delete("alice").await.unwrap();
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Stopped
        );

        h.warden
            .set_credentials("alice", &armored("second"))
            .await
            .unwrap();
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );
        // The stored blob is the new one, verbatim...
        let stored = h.cluster.secret("alice-credential").unwrap();
        assert_eq!(
            crate::provision::secret_value(&stored, objects::CREDENTIAL_KEY).unwrap(),
            armored("second")
        );
        // ...and the pod it belongs to is a different pod.
        assert_ne!(before, pod_annotation(&h));
    }

    /// The annotation the running pod carries, as the mock stored it.
    fn pod_annotation(h: &Harness) -> String {
        pod_annotations(h)[objects::CREDENTIAL_HASH_ANNOTATION].clone()
    }

    /// The LLM key hash on the running pod's template, when one is stamped.
    fn llm_annotation(h: &Harness) -> Option<String> {
        pod_annotations(h)
            .get(objects::LLM_KEY_HASH_ANNOTATION)
            .cloned()
    }

    fn pod_annotations(h: &Harness) -> std::collections::BTreeMap<String, String> {
        let Some(crate::cluster::Object::Deployment(deployment)) =
            h.cluster.object(Kind::Deployment, "alice")
        else {
            panic!("no deployment");
        };
        deployment
            .spec
            .unwrap()
            .template
            .metadata
            .unwrap()
            .annotations
            .unwrap()
    }

    /// The provisioning order the control plane actually uses: the key is
    /// minted alongside the signup, so it lands before the workload does, and
    /// the pod that comes up must be born keyed.
    #[tokio::test]
    async fn a_key_stored_before_provisioning_births_a_keyed_pod() {
        let h = Harness::with_config(llm_test_config());
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_llm_key("alice", Some("sk-vk-first"), None)
            .await
            .unwrap();
        // Stored, and nothing rolled: there is no workload yet to roll.
        assert!(h.cluster.secret("alice-llm").is_some());
        assert!(!h.cluster.exists(Kind::Deployment, "alice"));

        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        let hash = llm_annotation(&h).expect("the pod was born without the key hash");
        assert_eq!(hash, objects::llm_keys_hash(Some("sk-vk-first"), None));
    }

    /// The same pickup with both keys stored: the pod is born carrying the
    /// combined hash — the exact one a later rotation recomputes, so the two
    /// sites agree on what "unchanged" means.
    #[tokio::test]
    async fn a_pre_stored_assistant_key_reaches_the_pod_being_born() {
        let h = Harness::with_config(llm_test_config());
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_llm_key("alice", Some("sk-vk-triage"), Some("sk-vk-assistant"))
            .await
            .unwrap();

        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        let hash = llm_annotation(&h).expect("the pod was born without the key hash");
        assert_eq!(
            hash,
            objects::llm_keys_hash(Some("sk-vk-triage"), Some("sk-vk-assistant"))
        );
    }

    /// Rotation against a running tenant: the new key's hash is a new pod
    /// spec, and the credential hash beside it does not move.
    #[tokio::test]
    async fn rotating_the_llm_key_rolls_a_running_tenant() {
        let h = Harness::with_config(llm_test_config());
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        // Provisioned unkeyed: no hash on the template, nothing to roll for.
        assert!(llm_annotation(&h).is_none());
        let credential = pod_annotation(&h);

        h.warden
            .set_llm_key("alice", Some("sk-vk-first"), None)
            .await
            .unwrap();
        let first = llm_annotation(&h).expect("the rotation did not reach the pod");
        assert_eq!(
            pod_annotation(&h),
            credential,
            "the seed hash must not move"
        );

        h.warden
            .set_llm_key("alice", Some("sk-vk-second"), None)
            .await
            .unwrap();
        assert_ne!(
            first,
            llm_annotation(&h).unwrap(),
            "a new key must be a new pod spec"
        );
        // The stored Secret is the new key, verbatim — and only the one data
        // key: no assistant key was sent, so none is stored.
        let stored = h.cluster.secret("alice-llm").unwrap();
        assert_eq!(
            crate::provision::secret_value(&stored, objects::LLM_API_KEY_KEY).unwrap(),
            "sk-vk-second"
        );
        assert!(crate::provision::secret_value(&stored, objects::ASSISTANT_API_KEY_KEY).is_none());
    }

    /// Rotating ONLY the assistant key must roll the pod: the combined hash is
    /// the mechanism, and this is the test that proves the assistant half
    /// participates in it. Same triage key throughout.
    #[tokio::test]
    async fn rotating_only_the_assistant_key_rolls_a_running_tenant() {
        let h = Harness::with_config(llm_test_config());
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        let credential = pod_annotation(&h);

        // Triage key alone, then the assistant key arrives: a new pod spec.
        h.warden
            .set_llm_key("alice", Some("sk-vk-triage"), None)
            .await
            .unwrap();
        let unkeyed = llm_annotation(&h).unwrap();
        h.warden
            .set_llm_key("alice", Some("sk-vk-triage"), Some("sk-vk-assistant-1"))
            .await
            .unwrap();
        let first = llm_annotation(&h).unwrap();
        assert_ne!(unkeyed, first, "minting the assistant key must roll");

        // Assistant-only rotation: triage unchanged, hash moves anyway.
        h.warden
            .set_llm_key("alice", Some("sk-vk-triage"), Some("sk-vk-assistant-2"))
            .await
            .unwrap();
        assert_ne!(
            first,
            llm_annotation(&h).unwrap(),
            "an assistant-only rotation must be a new pod spec"
        );
        // Re-sending the same pair is the same pod spec: no spurious roll.
        h.warden
            .set_llm_key("alice", Some("sk-vk-triage"), Some("sk-vk-assistant-2"))
            .await
            .unwrap();
        assert_eq!(
            llm_annotation(&h).unwrap(),
            objects::llm_keys_hash(Some("sk-vk-triage"), Some("sk-vk-assistant-2"))
        );
        assert_eq!(
            pod_annotation(&h),
            credential,
            "the seed hash must not move"
        );

        // Both keys in the Secret, verbatim.
        let stored = h.cluster.secret("alice-llm").unwrap();
        assert_eq!(
            crate::provision::secret_value(&stored, objects::LLM_API_KEY_KEY).unwrap(),
            "sk-vk-triage"
        );
        assert_eq!(
            crate::provision::secret_value(&stored, objects::ASSISTANT_API_KEY_KEY).unwrap(),
            "sk-vk-assistant-2"
        );
    }

    /// The leave-it-alone contract: a PUT that names one slot must not touch
    /// the other. The apply is a force server-side apply, so this only holds
    /// because `set_llm_key` carries the stored slot forward — a regression
    /// here silently clears a live credential.
    #[tokio::test]
    async fn a_one_slot_put_preserves_the_other_slot() {
        let h = Harness::with_config(llm_test_config());
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.warden
            .set_llm_key("alice", Some("sk-vk-triage-1"), Some("sk-vk-assistant-1"))
            .await
            .unwrap();

        // Triage-only rotation: the assistant slot survives, and the roll
        // hash is over the resulting union, not the request.
        h.warden
            .set_llm_key("alice", Some("sk-vk-triage-2"), None)
            .await
            .unwrap();
        let stored = h.cluster.secret("alice-llm").unwrap();
        assert_eq!(
            crate::provision::secret_value(&stored, objects::LLM_API_KEY_KEY).unwrap(),
            "sk-vk-triage-2"
        );
        assert_eq!(
            crate::provision::secret_value(&stored, objects::ASSISTANT_API_KEY_KEY).unwrap(),
            "sk-vk-assistant-1",
            "a triage-only PUT must not clear the assistant slot"
        );
        assert_eq!(
            llm_annotation(&h).unwrap(),
            objects::llm_keys_hash(Some("sk-vk-triage-2"), Some("sk-vk-assistant-1"))
        );

        // Assistant-only rotation: the triage slot survives, same union hash.
        h.warden
            .set_llm_key("alice", None, Some("sk-vk-assistant-2"))
            .await
            .unwrap();
        let stored = h.cluster.secret("alice-llm").unwrap();
        assert_eq!(
            crate::provision::secret_value(&stored, objects::LLM_API_KEY_KEY).unwrap(),
            "sk-vk-triage-2",
            "an assistant-only PUT must not clear the triage slot"
        );
        assert_eq!(
            crate::provision::secret_value(&stored, objects::ASSISTANT_API_KEY_KEY).unwrap(),
            "sk-vk-assistant-2"
        );
        assert_eq!(
            llm_annotation(&h).unwrap(),
            objects::llm_keys_hash(Some("sk-vk-triage-2"), Some("sk-vk-assistant-2"))
        );
    }

    /// The half-failed-mint order: an assistant key can land before any triage
    /// key exists, and the Secret holds just that one slot until the triage
    /// mint catches up.
    #[tokio::test]
    async fn an_assistant_key_can_land_before_the_triage_key() {
        let h = Harness::with_config(llm_test_config());
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_llm_key("alice", None, Some("sk-vk-assistant"))
            .await
            .unwrap();
        let stored = h.cluster.secret("alice-llm").unwrap();
        assert!(crate::provision::secret_value(&stored, objects::LLM_API_KEY_KEY).is_none());
        assert_eq!(
            crate::provision::secret_value(&stored, objects::ASSISTANT_API_KEY_KEY).unwrap(),
            "sk-vk-assistant"
        );

        // The triage key arriving later completes the pair.
        h.warden
            .set_llm_key("alice", Some("sk-vk-triage"), None)
            .await
            .unwrap();
        let stored = h.cluster.secret("alice-llm").unwrap();
        assert_eq!(
            crate::provision::secret_value(&stored, objects::LLM_API_KEY_KEY).unwrap(),
            "sk-vk-triage"
        );
        assert_eq!(
            crate::provision::secret_value(&stored, objects::ASSISTANT_API_KEY_KEY).unwrap(),
            "sk-vk-assistant"
        );
    }

    #[tokio::test]
    async fn an_llm_key_refuses_an_unknown_label_and_a_broken_key() {
        let h = Harness::with_config(llm_test_config());
        assert_eq!(
            h.warden
                .set_llm_key("nobody", Some("sk-vk"), None)
                .await
                .unwrap_err(),
            WardenError::NotFound
        );
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        assert!(matches!(
            h.warden
                .set_llm_key("alice", Some("sk\nvk"), None)
                .await
                .unwrap_err(),
            WardenError::InvalidApiKey(_)
        ));
        // The assistant key is held to the same constraints as the triage key.
        assert!(matches!(
            h.warden
                .set_llm_key("alice", Some("sk-vk"), Some("sk\nassistant"))
                .await
                .unwrap_err(),
            WardenError::InvalidApiKey(_)
        ));
        // A body naming neither slot would install nothing: refused by name.
        assert_eq!(
            h.warden.set_llm_key("alice", None, None).await.unwrap_err(),
            WardenError::NoKeys
        );
        // None of the calls put anything past the identity.
        assert_eq!(h.cluster.applied_names(), vec!["alice-identity"]);
    }

    /// With no gateway URL configured, the key has nowhere to go: the write is
    /// refused before anything touches the cluster, rather than storing a
    /// Secret no pod would ever read and rolling the fleet for nothing.
    #[tokio::test]
    async fn an_llm_key_is_refused_when_the_gateway_is_not_configured() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        assert_eq!(
            h.warden
                .set_llm_key("alice", Some("sk-vk"), None)
                .await
                .unwrap_err(),
            WardenError::LlmNotConfigured
        );
        assert!(h.cluster.secret("alice-llm").is_none());
        assert_eq!(h.cluster.applied_names(), vec!["alice-identity"]);
    }

    #[tokio::test]
    async fn a_pair_exec_that_exits_non_zero_is_a_terse_500() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.cluster.exec_fails();
        assert_eq!(
            h.warden
                .set_credentials("alice", &armored("alice"))
                .await
                .unwrap_err(),
            WardenError::cluster("pair_failed")
        );
    }

    #[tokio::test]
    async fn unparseable_pair_output_is_never_half_a_handoff() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.cluster.exec_prints("Pairing code: ABCD-1234\n");
        assert_eq!(
            h.warden
                .set_credentials("alice", &armored("alice"))
                .await
                .unwrap_err(),
            WardenError::cluster("pair_output_unparsed")
        );
    }

    /// The destructive-action guard, stated as a test: DELETE takes the
    /// workload and leaves the mail.
    #[tokio::test]
    async fn delete_keeps_the_volume_the_identity_and_the_credential() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();

        h.warden.delete("alice").await.unwrap();

        assert_eq!(
            h.cluster.deleted(),
            vec![
                (Kind::Ingress, "alice".to_string()),
                // Before the Deployment, and this test is where that is
                // pinned: `interrupted` reads a Service that outlived its
                // Deployment as a reconcile to resume, so a cancellation must
                // never be able to produce one. See delete().
                (Kind::Service, "alice".to_string()),
                (Kind::Deployment, "alice".to_string()),
                (Kind::NetworkPolicy, "alice".to_string()),
                // The gateway credential goes with the workload; see delete().
                (Kind::Secret, "alice-llm".to_string()),
            ]
        );
        // The three that hold data or keys are untouched, by name: the only
        // Secret this path may delete is the LLM key.
        assert!(h.cluster.secret("alice-identity").is_some());
        assert!(h.cluster.secret("alice-credential").is_some());
        assert!(h.cluster.exists(Kind::Pvc, "alice-data"));
        assert!(h.cluster.deleted().iter().all(|(kind, name)| match kind {
            Kind::Pvc => false,
            Kind::Secret => name == "alice-llm",
            _ => true,
        }));

        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Stopped
        );
    }

    #[tokio::test]
    async fn delete_is_idempotent_and_an_unknown_label_is_a_no_op() {
        let h = Harness::new();
        h.warden.delete("nobody").await.unwrap();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.warden.delete("alice").await.unwrap();
        h.warden.delete("alice").await.unwrap();
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Stopped
        );
    }

    #[tokio::test]
    async fn status_and_pair_refuse_a_label_nobody_minted() {
        let h = Harness::new();
        assert_eq!(
            h.warden.status("nobody").await.unwrap_err(),
            WardenError::NotFound
        );
        assert_eq!(
            h.warden.repair("nobody").await.unwrap_err(),
            WardenError::NotFound
        );
    }

    #[tokio::test]
    async fn repair_mints_a_second_code_and_touches_nothing_else() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        let applied = h.cluster.applied().len();

        h.cluster.exec_prints(&crate::testing::pair_stdout(
            "WXYZ-9876",
            "https://alice.passband.email",
        ));
        let pairing = h.warden.repair("ALICE").await.unwrap();
        assert_eq!(pairing.pair_code, "WXYZ-9876");
        assert_eq!(pairing.pair_url, "https://alice.passband.email");
        // Nothing was applied and nothing was deleted.
        assert_eq!(h.cluster.applied().len(), applied);
        assert!(h.cluster.deleted().is_empty());
    }

    #[tokio::test]
    async fn a_stopped_tenant_cannot_be_paired() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.warden.delete("alice").await.unwrap();
        h.cluster.never_ready();
        assert_eq!(
            h.warden.repair("alice").await.unwrap_err(),
            WardenError::cluster("no_ready_pod")
        );
    }

    #[tokio::test]
    async fn a_cluster_that_will_not_answer_is_a_terse_500() {
        let h = Harness::new();
        h.cluster.break_reads();
        assert_eq!(
            h.warden
                .create_tenant("alice", "alice@example.com")
                .await
                .unwrap_err(),
            WardenError::cluster("cluster_unavailable")
        );
    }

    /// The janitor, and everything it must not touch. A pending record is a
    /// reservation with no mail behind it; every other state has something a
    /// person would miss.
    #[tokio::test]
    async fn the_sweep_collects_abandoned_pending_tenants_only() {
        let mut config = crate::testing::test_config();
        // Everything already written is older than this, which is the whole
        // point: the age arithmetic is tested separately.
        config.pending_ttl = std::time::Duration::from_secs(0);
        let h = Harness::with_config(config);

        // Abandoned at phase one.
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        // Serving.
        h.warden
            .create_tenant("bob", "bob@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("bob", &armored("bob"))
            .await
            .unwrap();
        // Cancelled, mail kept.
        h.warden
            .create_tenant("carol", "carol@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("carol", &armored("carol"))
            .await
            .unwrap();
        h.warden.delete("carol").await.unwrap();

        assert_eq!(h.warden.sweep_pending().await.unwrap(), 1);

        // The pending one is gone, and it is the only IDENTITY Secret ever
        // deleted. (Carol's delete also took her `-llm` Secret, which is the
        // workload's credential, not a tenant record; see `delete`.)
        assert!(h.cluster.secret("alice-identity").is_none());
        assert_eq!(
            h.cluster
                .deleted()
                .iter()
                .filter(|(kind, name)| *kind == Kind::Secret && name.ends_with("-identity"))
                .count(),
            1
        );
        assert!(
            h.cluster
                .deleted()
                .iter()
                .filter(|(kind, _)| *kind == Kind::Secret)
                .all(|(_, name)| name.ends_with("-identity") || name.ends_with("-llm"))
        );
        assert_eq!(
            h.warden.status("alice").await.unwrap_err(),
            WardenError::NotFound
        );

        // The serving tenant and the stopped one kept everything.
        for label in ["bob", "carol"] {
            assert!(h.cluster.secret(&format!("{label}-identity")).is_some());
            assert!(h.cluster.secret(&format!("{label}-credential")).is_some());
            assert!(h.cluster.exists(Kind::Pvc, &format!("{label}-data")));
        }
        assert_eq!(h.warden.status("bob").await.unwrap(), TenantStatus::Active);
        assert_eq!(
            h.warden.status("carol").await.unwrap(),
            TenantStatus::Stopped
        );

        // And a second pass has nothing left to do.
        assert_eq!(h.warden.sweep_pending().await.unwrap(), 0);
    }

    /// A signup in progress is not abandoned. With the shipped TTL nothing
    /// minted in this test is old enough to touch.
    #[tokio::test]
    async fn the_sweep_leaves_a_fresh_pending_tenant_alone() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        assert_eq!(h.warden.sweep_pending().await.unwrap(), 0);
        assert!(h.cluster.secret("alice-identity").is_some());
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Pending
        );
    }

    /// A Secret this warden cannot date is a Secret it does not destroy, at any
    /// TTL. That is the guard against collecting something restored from a
    /// backup, or written by an operator by hand.
    #[tokio::test]
    async fn the_sweep_never_collects_an_undated_identity() {
        let mut config = crate::testing::test_config();
        config.pending_ttl = std::time::Duration::from_secs(0);
        let h = Harness::with_config(config);

        let name = TenantName::parse("alice").unwrap();
        let mut secret = objects::identity_secret(
            &h.config,
            &name,
            &TenantIdentity::mint(),
            "alice@example.com",
            0,
        );
        secret.metadata.annotations = None;
        h.cluster
            .apply(Object::Secret(Box::new(secret)))
            .await
            .unwrap();

        assert_eq!(h.warden.sweep_pending().await.unwrap(), 0);
        assert!(h.cluster.secret("alice-identity").is_some());
    }

    /// The stored Deployment, edited the way `kubectl set env` edits one: a
    /// variable with a secret reference on the seed container, and a
    /// managedFields entry saying somebody else owns it now.
    ///
    /// Both halves matter and neither implies the other. Without the ledger
    /// entry this is a field the warden would take back on the next apply;
    /// with it, the warden's applies converge around the field forever.
    async fn hand_edit_the_deployment(h: &Harness) {
        use k8s_openapi::api::core::v1::{EnvVar, EnvVarSource, SecretKeySelector};
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::{FieldsV1, ManagedFieldsEntry};

        let Some(Object::Deployment(mut deployment)) = h.cluster.object(Kind::Deployment, "alice")
        else {
            panic!("no deployment");
        };
        let seed = &mut deployment
            .spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap()
            .init_containers
            .as_mut()
            .unwrap()[0];
        seed.env = Some(vec![EnvVar {
            name: "SQUELCH_ANTHROPIC_API_KEY".to_string(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: "squelch-anthropic".to_string(),
                    key: "ANTHROPIC_API_KEY".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }]);
        deployment.metadata.managed_fields = Some(vec![
            ManagedFieldsEntry {
                manager: Some(crate::cluster::FIELD_MANAGER.to_string()),
                operation: Some("Apply".to_string()),
                fields_v1: Some(FieldsV1(serde_json::json!({
                    "f:spec": { "f:template": { "f:spec": {
                        "f:initContainers": {
                            "k:{\"name\":\"seed\"}": { ".": {}, "f:image": {} }
                        }
                    }}}
                }))),
                ..Default::default()
            },
            ManagedFieldsEntry {
                manager: Some("kubectl-set".to_string()),
                operation: Some("Update".to_string()),
                fields_v1: Some(FieldsV1(serde_json::json!({
                    "f:spec": { "f:template": { "f:spec": {
                        "f:initContainers": {
                            "k:{\"name\":\"seed\"}": {
                                "f:env": {
                                    "k:{\"name\":\"SQUELCH_ANTHROPIC_API_KEY\"}": {
                                        ".": {},
                                        "f:name": {},
                                        "f:valueFrom": {
                                            "f:secretKeyRef": { ".": {}, "f:key": {}, "f:name": {} }
                                        }
                                    }
                                }
                            }
                        }
                    }}}
                }))),
                ..Default::default()
            },
        ]);
        h.cluster
            .apply(Object::Deployment(deployment))
            .await
            .unwrap();
    }

    /// The incident, end to end. A tenant the warden has never stopped
    /// converging carries a field the warden does not declare, and the only
    /// thing that can see it is this report.
    #[tokio::test]
    async fn drift_finds_a_hand_edit_the_warden_would_never_see() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();

        // Freshly provisioned: nobody else owns anything, and an apply would
        // move nothing.
        let clean = h.warden.drift("alice").await.unwrap();
        assert_eq!(clean.status, "active");
        assert!(clean.deployment_present);
        assert_eq!(clean.foreign, Vec::new());
        assert_eq!(clean.changes, Vec::new());

        hand_edit_the_deployment(&h).await;
        let applied = h.cluster.applied().len();
        let report = h.warden.drift("alice").await.unwrap();

        // The ledger names the editor and everything it took.
        assert_eq!(report.foreign.len(), 1);
        assert_eq!(report.foreign[0].manager, "kubectl-set");
        assert_eq!(report.foreign[0].operation, "Update");
        assert_eq!(
            report.foreign[0].paths,
            vec![
                "spec.template.spec.initContainers[seed].env[SQUELCH_ANTHROPIC_API_KEY].name",
                "spec.template.spec.initContainers[seed].env[SQUELCH_ANTHROPIC_API_KEY].valueFrom.secretKeyRef.key",
                "spec.template.spec.initContainers[seed].env[SQUELCH_ANTHROPIC_API_KEY].valueFrom.secretKeyRef.name",
            ]
        );
        // The ledger is the ONLY place this finding is guaranteed to appear,
        // and that is the whole reason `foreign_managers` exists.
        //
        // Nothing is asserted about `changes` here, deliberately. A real API
        // server answers a dry-run apply with a MERGE, and a field the warden
        // does not declare survives that merge exactly as it survives a real
        // one - so on a cluster this env var is identical on both sides and
        // cancels out of the diff entirely. `MockCluster` answers with the bare
        // render instead, so it would show up here as a change against `null`;
        // asserting on that would be pinning the mock's simplification and
        // teaching the next reader that the diff catches foreign fields, which
        // is the one thing it cannot do. `diff_spec` is held to its real
        // contract by its own unit tests in `drift`.

        // Read-only: the dry run is not an apply and not a store.
        assert_eq!(h.cluster.applied().len(), applied);
        assert!(h.cluster.deleted().is_empty());
    }

    /// A tenant with no workload has nothing to have drifted, in both of the
    /// ways that happens.
    #[tokio::test]
    async fn drift_on_a_tenant_with_no_workload_is_an_empty_report() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();

        let pending = h.warden.drift("alice").await.unwrap();
        assert_eq!(pending.status, "pending");
        assert!(!pending.deployment_present);
        assert_eq!(pending.foreign, Vec::new());
        assert_eq!(pending.changes, Vec::new());

        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.warden.delete("alice").await.unwrap();
        let stopped = h.warden.drift("alice").await.unwrap();
        assert_eq!(stopped.status, "stopped");
        assert!(!stopped.deployment_present);
        assert_eq!(stopped.changes, Vec::new());
    }

    /// The render has to carry the hashes the running pod was rolled for, and
    /// the credential Secret is the only source of the first one. Without it
    /// there is no honest render, and reporting one anyway would call every
    /// pod-template annotation drift.
    #[tokio::test]
    async fn drift_refuses_a_workload_whose_credential_is_gone() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.cluster
            .delete(Kind::Secret, "alice-credential")
            .await
            .unwrap();

        assert_eq!(
            h.warden.drift("alice").await.unwrap_err(),
            WardenError::cluster("credential_missing")
        );
    }

    /// A keyed tenant's render must pick the key's hash back up, or the
    /// annotation that rolls a rotation would look like drift on every keyed
    /// tenant in the fleet.
    #[tokio::test]
    async fn drift_is_clean_for_a_tenant_that_carries_an_llm_key() {
        let h = Harness::with_config(llm_test_config());
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.warden.set_llm_key("alice", Some("sk-vk-first"), None).await.unwrap();

        let report = h.warden.drift("alice").await.unwrap();
        assert_eq!(report.changes, Vec::new());
        assert_eq!(report.foreign, Vec::new());
    }

    #[tokio::test]
    async fn drift_refuses_a_label_nobody_minted() {
        let h = Harness::new();
        assert_eq!(
            h.warden.drift("nobody").await.unwrap_err(),
            WardenError::NotFound
        );
        assert!(matches!(
            h.warden.drift("-nope-").await.unwrap_err(),
            WardenError::InvalidLabel(_)
        ));
    }

    /// The five workload objects a reconcile re-applies, in order. Not a
    /// Secret among them: a reconcile converges shape and never touches an
    /// identity, a credential or a key.
    fn workload_applies() -> Vec<(Kind, String)> {
        workload_applies_for("alice")
    }

    /// [`workload_applies`] for a named tenant, which is what a fleet roll's
    /// applies have to be read against: one tenant's five objects, then the
    /// next tenant's, never interleaved.
    fn workload_applies_for(label: &str) -> Vec<(Kind, String)> {
        vec![
            (Kind::Pvc, format!("{label}-data")),
            (Kind::NetworkPolicy, label.to_string()),
            (Kind::Service, label.to_string()),
            (Kind::Deployment, label.to_string()),
            (Kind::Ingress, label.to_string()),
        ]
    }

    /// A tenant nobody has touched. Everything is re-applied in phase two's
    /// order, nothing is deleted, and the pod rolls only if the render moved -
    /// which here it has not.
    #[tokio::test]
    async fn reconcile_converges_a_clean_tenant() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        let before = h.cluster.applied().len();

        let reconciled = h.warden.reconcile("alice").await.unwrap();
        assert_eq!(reconciled.deployment, "converged");
        assert_eq!(reconciled.status, "active");

        assert_eq!(h.cluster.applied()[before..].to_vec(), workload_applies());
        assert!(h.cluster.deleted().is_empty());
    }

    /// The incident state is the one this route exists for: a pod that will
    /// not come up because somebody stamped a secret reference onto it has no
    /// ready replica, so the tenant reads `failed`, and refusing to act on
    /// `failed` would be refusing exactly when it matters.
    #[tokio::test]
    async fn reconcile_acts_on_a_tenant_that_is_not_serving() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.cluster.never_ready();
        assert!(
            h.warden
                .set_credentials("alice", &armored("alice"))
                .await
                .is_err()
        );
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Failed
        );

        h.cluster.becomes_ready();
        let reconciled = h.warden.reconcile("alice").await.unwrap();
        assert_eq!(reconciled.deployment, "converged");
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );
    }

    /// The purge. A field the warden does not declare survives every forced
    /// apply, so the Deployment is deleted and applied fresh, and what comes
    /// back carries the render and an ownership ledger that starts empty.
    #[tokio::test]
    async fn reconcile_recreates_a_deployment_another_manager_owns() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        hand_edit_the_deployment(&h).await;
        let before = h.cluster.applied().len();

        let reconciled = h.warden.reconcile("alice").await.unwrap();
        assert_eq!(reconciled.deployment, "recreated");

        // The Deployment, and only the Deployment. The volume, the Secrets and
        // the route are not what was owned.
        assert_eq!(
            h.cluster.deleted(),
            vec![(Kind::Deployment, "alice".to_string())]
        );
        assert_eq!(h.cluster.applied()[before..].to_vec(), workload_applies());

        // And the thing no apply could ever have removed is gone.
        let Some(Object::Deployment(deployment)) = h.cluster.object(Kind::Deployment, "alice")
        else {
            panic!("no deployment");
        };
        // No foreign owner survived. The mock stores what it is handed, so an
        // absent ledger is what a fresh object looks like here; on a cluster
        // the same assertion is "exactly one entry, and it is the warden".
        assert!(drift::foreign_managers(&deployment).is_empty());
        let seed = &deployment
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .init_containers
            .unwrap()[0];
        assert_eq!(seed.name, "seed");
        assert!(seed.env.is_none(), "the foreign env survived the recreate");

        // A second pass has nothing foreign left to find.
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap().deployment,
            "converged"
        );
    }

    /// The rule that keeps two daemons off one `ReadWriteOnce` volume: if the
    /// old pod will not go, the new Deployment is not applied at all. A
    /// reconcile that failed loudly is recoverable; a second writer on a
    /// SQLite file is not.
    #[tokio::test]
    async fn reconcile_will_not_apply_while_the_old_pod_holds_the_volume() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        hand_edit_the_deployment(&h).await;
        let before = h.cluster.applied().len();
        h.cluster.pods_linger();

        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::cluster("pods_not_gone")
        );
        // The delete happened, and nothing was applied after it.
        assert_eq!(
            h.cluster.deleted(),
            vec![(Kind::Deployment, "alice".to_string())]
        );
        assert!(!h.cluster.exists(Kind::Deployment, "alice"));
        assert_eq!(
            h.cluster.applied()[before..].to_vec(),
            vec![
                (Kind::Pvc, "alice-data".to_string()),
                (Kind::NetworkPolicy, "alice".to_string()),
                (Kind::Service, "alice".to_string()),
            ]
        );
    }

    /// The other half of the rule above: a reconcile that died in the
    /// delete-recreate window must be finishable by running it again.
    ///
    /// The tenant reads `stopped` at that point, exactly as a cancelled account
    /// does, and a route that refused on the status word alone would refuse to
    /// clean up after its own failure - leaving the long way round (re-consent)
    /// as the only exit from a state this route created. The surviving Service
    /// is what tells the two aparts.
    #[tokio::test]
    async fn reconcile_finishes_what_an_interrupted_reconcile_started() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        hand_edit_the_deployment(&h).await;
        h.cluster.pods_linger();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::cluster("pods_not_gone")
        );

        // The wreckage: no Deployment, so the status word says `stopped`, but
        // the Service the reconcile applied on its way in is still standing.
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Stopped
        );
        assert!(!h.cluster.exists(Kind::Deployment, "alice"));
        assert!(h.cluster.exists(Kind::Service, "alice"));

        // The pod lets go and the operator runs it again. Nothing foreign
        // survived the delete, so this is a plain apply rather than a purge.
        h.cluster.pods_release();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap().deployment,
            "created"
        );
        assert_eq!(h.warden.status("alice").await.unwrap(), TenantStatus::Active);
        let live = match h.cluster.object(Kind::Deployment, "alice").unwrap() {
            Object::Deployment(d) => *d,
            other => panic!("not a Deployment: {:?}", other.kind()),
        };
        assert!(drift::foreign_managers(&live).is_empty());
    }

    /// The invariant [`Warden::interrupted`] rests on, held against a teardown
    /// that dies anywhere in the middle.
    ///
    /// `delete` stops at its first failure, so a cancellation can leave a
    /// tenant in any prefix of its teardown. NONE of those prefixes may be a
    /// Service that outlived its Deployment, because that is the exact shape
    /// `interrupted` reads as "a reconcile died here, finish it" - and
    /// finishing it would put a cancelled mailbox back on the internet.
    #[tokio::test]
    async fn no_half_finished_cancellation_looks_like_an_interrupted_reconcile() {
        for failing in [
            Kind::Ingress,
            Kind::Service,
            Kind::Deployment,
            Kind::NetworkPolicy,
        ] {
            let h = Harness::new();
            h.warden
                .create_tenant("alice", "alice@example.com")
                .await
                .unwrap();
            h.warden
                .set_credentials("alice", &armored("alice"))
                .await
                .unwrap();

            h.cluster.fail_delete_of(failing);
            assert!(
                h.warden.delete("alice").await.is_err(),
                "{failing:?} was supposed to fail the teardown"
            );

            let service = h.cluster.exists(Kind::Service, "alice");
            let deployment = h.cluster.exists(Kind::Deployment, "alice");
            // "A Service implies its Deployment", which is the invariant
            // `interrupted` reads backwards.
            assert!(
                !service || deployment,
                "a teardown that died on {failing:?} left a Service with no Deployment, \
                 which reconcile would read as a job to finish"
            );
        }
    }

    /// A CANCELLED tenant stays refused, which is the distinction
    /// [`Warden::interrupted`] exists to draw: `delete` took the Service down
    /// with the Deployment, so nothing here reads as a job to finish.
    #[tokio::test]
    async fn reconcile_will_not_resurrect_a_cancelled_tenant() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.warden.delete("alice").await.unwrap();
        assert!(!h.cluster.exists(Kind::Service, "alice"));

        let stopped = h.cluster.applied().len();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::NotReconcilable
        );
        assert_eq!(h.cluster.applied().len(), stopped);
    }

    /// Neither tenant without a workload is reconcilable, and the refusal
    /// costs the cluster no writes. Bringing either one up is a different call
    /// with a different meaning: finish the signup, or re-consent.
    #[tokio::test]
    async fn reconcile_refuses_a_tenant_with_no_workload() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        let pending = h.cluster.applied().len();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::NotReconcilable
        );
        assert_eq!(h.cluster.applied().len(), pending);

        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.warden.delete("alice").await.unwrap();
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Stopped
        );
        // A tenant with a sealed credential and no Deployment is `stopped`, so
        // this is also the proof that `created` names a race rather than a
        // state: the only way to reach that branch is a DELETE landing between
        // the status read and the get.
        let stopped = h.cluster.applied().len();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::NotReconcilable
        );
        assert_eq!(h.cluster.applied().len(), stopped);
    }

    /// A workload whose sealed credential is gone has no honest render behind
    /// it, and the refusal lands before anything is written.
    #[tokio::test]
    async fn reconcile_refuses_a_workload_whose_credential_is_gone() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.cluster
            .delete(Kind::Secret, "alice-credential")
            .await
            .unwrap();
        let before = h.cluster.applied().len();

        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::cluster("credential_missing")
        );
        assert_eq!(h.cluster.applied().len(), before);
    }

    /// A keyed tenant keeps its key across a recreate: the hash is recovered
    /// from the stored Secret, so the pod that comes back is the pod the
    /// tenant is entitled to rather than an unkeyed one.
    #[tokio::test]
    async fn reconcile_keeps_the_llm_key_hash_on_the_rebuilt_pod() {
        let h = Harness::with_config(llm_test_config());
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.warden
            .set_llm_key("alice", Some("sk-vk-first"), None)
            .await
            .unwrap();
        let credential = pod_annotation(&h);
        // Whatever `set_llm_key` stamped, rather than a hash spelled out here:
        // a reconcile must reproduce the keyed pod EXACTLY, and a test that
        // recomputed the hash its own way would keep passing while the two
        // drifted apart. That is the bug this assertion exists to catch.
        let keyed = llm_annotation(&h).unwrap();

        hand_edit_the_deployment(&h).await;
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap().deployment,
            "recreated"
        );
        assert_eq!(llm_annotation(&h).unwrap(), keyed);
        assert_eq!(pod_annotation(&h), credential);
    }

    /// The pod not coming back is the operator's problem to see, and it is the
    /// same machine reason a failed phase two gives: the objects are applied,
    /// and what is missing is a replica.
    #[tokio::test]
    async fn a_reconcile_whose_pod_never_returns_is_a_terse_500() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.cluster.never_ready();

        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::cluster("not_ready")
        );
        // Applied anyway: the reconcile happened, the replica did not.
        assert!(h.cluster.exists(Kind::Deployment, "alice"));
    }

    #[tokio::test]
    async fn reconcile_refuses_a_label_nobody_minted() {
        let h = Harness::new();
        assert_eq!(
            h.warden.reconcile("nobody").await.unwrap_err(),
            WardenError::NotFound
        );
        assert!(matches!(
            h.warden.reconcile("-nope-").await.unwrap_err(),
            WardenError::InvalidLabel(_)
        ));
        assert!(h.cluster.applied().is_empty());
    }

    /// A tenant through both phases and serving, for the tests that need a
    /// fleet rather than a tenant.
    async fn serving_tenant(h: &Harness, label: &str) {
        h.warden
            .create_tenant(label, &format!("{label}@example.com"))
            .await
            .unwrap();
        h.warden
            .set_credentials(label, &armored(label))
            .await
            .unwrap();
    }

    /// The image a tenant was provisioned on before somebody bumped
    /// `SQUELCH_WARDEN_IMAGE`. Any value the test config does not name.
    const PREVIOUS_IMAGE: &str = "ghcr.io/braelyn-ai/squelchd:daemon-0.3.0";

    /// Put a tenant's live Deployment a render behind: the daemon image the
    /// warden's config no longer names, with the warden still the only owner of
    /// every field.
    ///
    /// This is the drift a fleet roll exists for, and it is the ORDINARY one.
    /// Nobody edited anything; the warden wrote these objects once at provision
    /// time and never came back, so a tenant provisioned before an image bump
    /// carries the old render until something walks it forward. The hand-edit
    /// fixture next to it is the other kind, and the roll treats them as
    /// opposites.
    async fn age_the_render(h: &Harness, label: &str) {
        let Some(Object::Deployment(mut deployment)) = h.cluster.object(Kind::Deployment, label)
        else {
            panic!("no deployment for {label}");
        };
        let pod = deployment
            .spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap();
        for container in pod
            .containers
            .iter_mut()
            .chain(pod.init_containers.iter_mut().flatten())
        {
            container.image = Some(PREVIOUS_IMAGE.to_string());
        }
        h.cluster
            .apply(Object::Deployment(deployment))
            .await
            .unwrap();
    }

    /// The image the tenant's daemon container currently carries.
    fn daemon_image(h: &Harness, label: &str) -> String {
        let Some(Object::Deployment(deployment)) = h.cluster.object(Kind::Deployment, label) else {
            panic!("no deployment for {label}");
        };
        deployment
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .containers
            .iter()
            .find(|container| container.name == "squelchd")
            .and_then(|container| container.image.clone())
            .expect("no daemon container")
    }

    fn labels_of(fleet: &[TenantName]) -> Vec<&str> {
        fleet.iter().map(TenantName::as_str).collect()
    }

    /// What a roll walks: every tenant in the cluster, sorted, whatever order
    /// they were minted in and whether or not anything else knows about them.
    #[tokio::test]
    async fn the_fleet_is_every_tenant_in_the_cluster_sorted() {
        let h = Harness::new();
        assert_eq!(h.warden.fleet().await.unwrap(), Vec::new());

        serving_tenant(&h, "carol").await;
        serving_tenant(&h, "alice").await;
        // Pending: phase one and nothing since. Still in the fleet - a tenant
        // half a signup away from being recorded anywhere else is exactly the
        // one a sweep must not be blind to - and it is the status check inside
        // the roll, not the enumeration, that decides to leave it alone.
        h.warden
            .create_tenant("bob", "bob@example.com")
            .await
            .unwrap();

        // Carol's and Alice's credential Secrets carry the same managed
        // selector and are not identities, which is the "skip a name that does
        // not parse back to a label" path.
        let fleet = h.warden.fleet().await.unwrap();
        assert_eq!(labels_of(&fleet), vec!["alice", "bob", "carol"]);
    }

    /// The pass that closes the gap write-once provisioning leaves. Two tenants
    /// are a render behind and one is not; the two are converged, in fleet
    /// order rather than in the order they fell behind, and the third is not
    /// written at all.
    #[tokio::test]
    async fn a_roll_converges_only_the_drifted_tenants_in_fleet_order() {
        let h = Harness::new();
        for label in ["alice", "bob", "carol"] {
            serving_tenant(&h, label).await;
        }
        age_the_render(&h, "carol").await;
        age_the_render(&h, "alice").await;
        let before = h.cluster.applied().len();

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.checked, 3);
        assert_eq!(rolled.rolled, vec!["alice", "carol"]);
        assert_eq!(rolled.current, 1);
        assert_eq!(rolled.skipped_foreign, Vec::<String>::new());
        assert_eq!(rolled.skipped_inactive, Vec::<String>::new());
        assert_eq!(rolled.halted_on, None);

        // One tenant's five objects, then the next tenant's. Nothing for Bob,
        // who was already on today's render, and no delete anywhere: with no
        // foreign owner this is the converged path.
        assert_eq!(
            h.cluster.applied()[before..].to_vec(),
            [workload_applies_for("alice"), workload_applies_for("carol")].concat()
        );
        assert!(h.cluster.deleted().is_empty());
        for label in ["alice", "bob", "carol"] {
            assert_eq!(daemon_image(&h, label), h.config.image);
            assert_eq!(h.warden.status(label).await.unwrap(), TenantStatus::Active);
        }

        // And the fleet is converged, so a second pass is all reads.
        let applied = h.cluster.applied().len();
        let second = h.warden.roll(false).await.unwrap();
        assert_eq!(second.current, 3);
        assert_eq!(second.rolled, Vec::<String>::new());
        assert_eq!(h.cluster.applied().len(), applied);
    }

    /// The rule that makes an unattended roll defensible: a Deployment somebody
    /// else owns fields on is left EXACTLY as it is. Repairing one means
    /// deleting it, which is a live mailbox going down to remove a field a
    /// person put there deliberately, and no timer gets to make that call.
    #[tokio::test]
    async fn a_roll_never_touches_a_tenant_another_manager_owns() {
        let h = Harness::new();
        for label in ["alice", "bob"] {
            serving_tenant(&h, label).await;
        }
        age_the_render(&h, "bob").await;
        // Alice is drifted AND hand-edited; the hand edit is what decides.
        age_the_render(&h, "alice").await;
        hand_edit_the_deployment(&h).await;
        let before = h.cluster.applied().len();

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.skipped_foreign, vec!["alice"]);
        assert_eq!(rolled.rolled, vec!["bob"]);
        assert_eq!(rolled.halted_on, None);

        // Not one write against Alice: not an apply, not a delete. A skip that
        // re-applied "just the safe fields" would still be this loop deciding
        // something about somebody else's field.
        assert_eq!(
            h.cluster.applied()[before..].to_vec(),
            workload_applies_for("bob")
        );
        assert!(h.cluster.deleted().is_empty());
        assert_eq!(daemon_image(&h, "alice"), PREVIOUS_IMAGE);

        // The foreign owner and its field are both still there, which is the
        // state the operator has to be shown rather than have cleaned up
        // behind them.
        let Some(Object::Deployment(alice)) = h.cluster.object(Kind::Deployment, "alice") else {
            panic!("no deployment");
        };
        assert_eq!(drift::foreign_managers(&alice).len(), 1);
        let seed = &alice
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .init_containers
            .unwrap()[0];
        assert!(seed.env.is_some(), "the roll removed a foreign field");
    }

    /// Neither tenant without a workload is a shape to converge: pending is a
    /// signup to finish and stopped is an account to reopen, and a roll that
    /// "fixed" either would be starting something nobody asked it to start.
    #[tokio::test]
    async fn a_roll_skips_the_tenants_with_no_workload() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        serving_tenant(&h, "bob").await;
        age_the_render(&h, "bob").await;
        serving_tenant(&h, "carol").await;
        h.warden.delete("carol").await.unwrap();
        let applied = h.cluster.applied().len();
        let deleted = h.cluster.deleted().len();

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.checked, 3);
        assert_eq!(rolled.skipped_inactive, vec!["alice", "carol"]);
        assert_eq!(rolled.rolled, vec!["bob"]);
        assert_eq!(rolled.current, 0);

        assert_eq!(
            h.cluster.applied()[applied..].to_vec(),
            workload_applies_for("bob")
        );
        assert_eq!(h.cluster.deleted().len(), deleted);
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Pending
        );
        assert_eq!(
            h.warden.status("carol").await.unwrap(),
            TenantStatus::Stopped
        );
    }

    /// The whole reason this is a sweep and not a controller: a render that
    /// will not come up costs ONE tenant. The run stops where it broke, says
    /// so, and every tenant after it is untouched.
    #[tokio::test]
    async fn a_tenant_that_will_not_come_back_halts_the_run() {
        let h = Harness::new();
        for label in ["alice", "bob", "carol"] {
            serving_tenant(&h, label).await;
            age_the_render(&h, label).await;
        }
        let before = h.cluster.applied().len();
        // Today's render is a render that does not serve.
        h.cluster.never_ready();

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.halted_on, Some("alice".to_string()));
        assert_eq!(rolled.rolled, Vec::<String>::new());
        // Examined exactly one tenant, out of a fleet of three.
        assert_eq!(rolled.checked, 1);

        // Alice was applied and did not come back; nobody else was written at
        // all, and both of them are still serving the render they were on.
        assert_eq!(
            h.cluster.applied()[before..].to_vec(),
            workload_applies_for("alice")
        );
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Failed
        );
        for label in ["bob", "carol"] {
            assert_eq!(daemon_image(&h, label), PREVIOUS_IMAGE);
            assert_eq!(h.warden.status(label).await.unwrap(), TenantStatus::Active);
        }
    }

    /// What an operator runs BEFORE the bump: every read, no write, and a list
    /// of exactly which tenants the real run would touch.
    #[tokio::test]
    async fn a_dry_run_reports_the_work_and_does_none_of_it() {
        let h = Harness::new();
        for label in ["alice", "bob"] {
            serving_tenant(&h, label).await;
        }
        age_the_render(&h, "alice").await;
        let applied = h.cluster.applied();

        let rolled = h.warden.roll(true).await.unwrap();
        assert_eq!(rolled.checked, 2);
        assert_eq!(rolled.rolled, vec!["alice"]);
        assert_eq!(rolled.current, 1);

        // Nothing was written, including by the drift report's dry-run apply,
        // and Alice is still a render behind.
        assert_eq!(h.cluster.applied(), applied);
        assert!(h.cluster.deleted().is_empty());
        assert_eq!(daemon_image(&h, "alice"), PREVIOUS_IMAGE);
    }

    /// A fleet that is already converged is a no-op, which is what makes this
    /// safe to run on a timer: the steady state costs reads and changes
    /// nothing.
    #[tokio::test]
    async fn a_fleet_already_on_todays_render_is_all_reads() {
        let h = Harness::new();
        for label in ["alice", "bob"] {
            serving_tenant(&h, label).await;
        }
        let applied = h.cluster.applied();

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.checked, 2);
        assert_eq!(rolled.current, 2);
        assert_eq!(rolled.rolled, Vec::<String>::new());
        assert_eq!(rolled.halted_on, None);
        assert_eq!(h.cluster.applied(), applied);
        assert!(h.cluster.deleted().is_empty());
    }

    /// The check the whole roll stands on. A reconcile answers only once the
    /// Deployment's rollout has FINISHED - the controller has observed the
    /// spec and every replica on it is serving - and a rollout that never
    /// completes is the same terse `not_ready` a pod that never came up gives.
    ///
    /// The last assertion is the false green this exists to refuse: a pod
    /// matching the tenant's selector is Ready throughout, because under
    /// `Recreate` the pod being replaced stays Ready while it terminates. A
    /// roller that trusted that would step to the next tenant on the strength
    /// of the pod it had just taken away.
    #[tokio::test]
    async fn a_reconcile_answers_only_once_the_rollout_is_complete() {
        let h = Harness::new();
        serving_tenant(&h, "alice").await;
        assert_eq!(h.warden.reconcile("alice").await.unwrap().status, "active");

        h.cluster.rollout_hangs();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::cluster("not_ready")
        );
        // Applied anyway: the objects landed, the roll did not finish.
        assert!(h.cluster.exists(Kind::Deployment, "alice"));

        let name = TenantName::parse("alice").unwrap();
        assert!(
            h.cluster
                .ready_pod(&objects::pod_selector(&name), h.config.ready_timeout)
                .await
                .is_ok(),
            "the weaker check has to pass here, or this test proves nothing"
        );
    }

    /// A roll that cannot even enumerate the fleet is an ERROR, not an empty
    /// summary. "Nothing to do" and "I could not look" are the same three
    /// zeroes on the way out, and an operator must never be handed the second
    /// one wearing the first one's face.
    #[tokio::test]
    async fn a_roll_that_cannot_read_the_fleet_is_not_an_empty_success() {
        let h = Harness::new();
        serving_tenant(&h, "alice").await;
        h.cluster.break_reads();
        assert_eq!(
            h.warden.roll(false).await.unwrap_err(),
            WardenError::cluster("fleet_list_failed")
        );
    }

    /// And the same failure inside a roll: the tenant it halted on is the
    /// tenant whose rollout hung.
    #[tokio::test]
    async fn a_rollout_that_never_completes_halts_the_run() {
        let h = Harness::new();
        for label in ["alice", "bob"] {
            serving_tenant(&h, label).await;
            age_the_render(&h, label).await;
        }
        h.cluster.rollout_hangs();

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.halted_on, Some("alice".to_string()));
        assert_eq!(rolled.rolled, Vec::<String>::new());
        assert_eq!(daemon_image(&h, "bob"), PREVIOUS_IMAGE);
    }

    #[test]
    fn every_status_has_a_wire_word() {
        assert_eq!(TenantStatus::Pending.as_str(), "pending");
        assert_eq!(TenantStatus::Active.as_str(), "active");
        assert_eq!(TenantStatus::Failed.as_str(), "failed");
        assert_eq!(TenantStatus::Stopped.as_str(), "stopped");
    }
}
