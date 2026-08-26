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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Secret;

use crate::cluster::{Cluster, ClusterError, Kind, Object};
use crate::config::Config;
use crate::devices;
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
///
/// Not `Serialize`, deliberately. `roll` is a library call and not an HTTP route
/// ([`crate`]'s binary docs say why), so this answers an exit code and a block
/// of text on a terminal; a derive here would be a standing invitation to put
/// the fleet-wide label list on a wire, which is the one thing the paragraph
/// above is careful about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rolled {
    /// How many tenants this run reached a VERDICT on: the buckets below plus
    /// the one named in [`Rolled::halted_on`], and nothing else. It is a sum
    /// rather than a tally of tenants looked at, so it can be checked against
    /// the rest of the summary by adding it up, and a number nobody can check
    /// is a number nobody reads.
    ///
    /// The gap between it and the fleet's size is the tenants the read pass
    /// never reached, because it stopped.
    pub checked: usize,
    /// Converged onto today's render. AT MOST ONE per run, which is the pacing
    /// [`Warden::roll`] is built around rather than a count that happens to be
    /// small: one tenant is converged and the run returns, and the next tick
    /// re-reads the whole fleet before it picks the next one.
    ///
    /// In a dry run it is every tenant that is behind, because a dry run
    /// applies nothing and so no tenant goes first; the number of RUNS that
    /// list costs is its length.
    pub rolled: Vec<String>,
    /// Marked for a roll and left for a later run: every tenant the read pass
    /// queued that this run did not take.
    ///
    /// Normally that is the queue BEHIND the one tenant it took, whatever became
    /// of that one. Zero and a converged [`Rolled::rolled`] is the fleet being
    /// current; non-zero says the fleet is not there yet, which is the
    /// distinction the exit code has to carry.
    ///
    /// When the READ pass stopped - a casualty, or a tenant it could not read -
    /// it is everything already marked when it stopped, and [`Rolled::rolled`]
    /// is empty because nothing was applied. Discarding that count instead would
    /// make [`Rolled::checked`] report a three-tenant fleet as one.
    ///
    /// Behind the tenant this run took and not including it, even when the run
    /// halted on it: [`Rolled::tally`] counts a halted tenant through
    /// [`Rolled::halted_on`], and a tenant in both buckets would push `checked`
    /// past the size of the fleet.
    ///
    /// Zero in a dry run that finished, where the whole queue is in
    /// [`Rolled::rolled`]; the halt case above applies to a dry run too.
    pub remaining: usize,
    /// Already matched the render. A re-run of a finished roll is all of these
    /// and no writes at all.
    pub current: usize,
    /// Skipped because another field manager owns part of the Deployment.
    /// Repairing one costs it its pod, so a person decides; see
    /// [`Warden::roll`].
    pub skipped_foreign: Vec<String>,
    /// Skipped because there is nothing to converge and nothing is wrong:
    /// pending is a signup to finish, and a cancelled account
    /// ([`objects::CANCELLED_AT_ANNOTATION`]) is an account to reopen. Neither
    /// one is a tenant that is down.
    pub skipped_inactive: Vec<String>,
    /// Left with no workload by a job that did not finish: the volume and the
    /// credential are still standing, no cancellation was ever recorded, and
    /// the Deployment is gone, which is a mailbox that is DOWN. The roll does
    /// not repair one - finishing somebody else's half-done repair unattended
    /// is the same judgement call a foreign field is - so it names them
    /// instead, and they are the reason a run that rolled everything it could
    /// can still refuse to call the fleet converged.
    pub stranded: Vec<String>,
    /// A workload standing over a credential Secret that is gone, so this
    /// warden cannot render the tenant at all: `credential_missing`.
    ///
    /// Its own bucket because it is the one per-tenant READ failure that is
    /// permanent. Every other one means the API server stopped answering, which
    /// stops the run - the right answer, because the next tenant would be rolled
    /// on the strength of a cluster that has just gone quiet. This one is a fact
    /// about one tenant that no retry changes, and stopping on it would park
    /// every tenant after it in [`Warden::fleet`] order behind a run that halts
    /// at the same label every fifteen minutes, forever.
    ///
    /// The pod is probably still up - the daemon installed its credential onto
    /// its own volume long ago - so this is not [`Rolled::stranded`]. It is a
    /// mailbox with no way back onto today's render, and the remedy is a person
    /// restoring the Secret, not a run trying again.
    pub unrenderable: Vec<String>,
    /// Identity Secrets whose label does not validate, from [`Warden::fleet`].
    /// A COUNT and never the names, because the name is precisely the string
    /// that failed validation.
    ///
    /// These are tenants this warden can see and cannot address: they are in no
    /// roll, now or ever, and no run will converge them however many times it
    /// runs. That is a fleet which is not on today's render, so it reaches the
    /// exit code - a cluster holding one exiting green is the silence this
    /// count exists to break.
    pub unreadable: usize,
    /// The label the run stopped at, if it stopped. Everything after it in
    /// [`Warden::fleet`] order was left exactly as it was.
    pub halted_on: Option<String>,
    /// Set when the stop was a CASUALTY of an earlier run rather than a failure
    /// of this one: this tenant carries today's render and is not serving it,
    /// so the render itself is under suspicion and this run applied NOTHING.
    /// Always also in [`Rolled::halted_on`]; see [`Warden::roll`].
    pub casualty: Option<String>,
}

impl Rolled {
    /// Fill in [`Rolled::checked`] from the verdicts, once, on the way out.
    ///
    /// [`Rolled::remaining`] counts here: the read pass reached a verdict on
    /// those tenants too, and it was "roll this one" - what the write pass did
    /// with the queue is pacing, not a second opinion. [`Rolled::unreadable`]
    /// does not: those are not tenants this run could check, which is the whole
    /// complaint about them.
    fn tally(&mut self) {
        self.checked = self.rolled.len()
            + self.remaining
            + self.current
            + self.skipped_foreign.len()
            + self.skipped_inactive.len()
            + self.stranded.len()
            + self.unrenderable.len()
            + usize::from(self.halted_on.is_some());
    }
}

/// What the read pass of a [`Warden::roll`] decided about one tenant. Internal:
/// what leaves the roll is [`Rolled`], which is these counted up after the write
/// pass has turned [`Step::Roll`] into an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Behind today's render, with nobody else owning any of it. The only
    /// verdict that asks for a write.
    Roll,
    Current,
    Foreign,
    Inactive,
    /// No workload, nothing recording a cancellation: [`Rolled::stranded`].
    Stranded,
    /// A workload this warden cannot render, permanently:
    /// [`Rolled::unrenderable`].
    Unrenderable,
    /// On today's render and not serving it: [`Rolled::casualty`]. The verdict
    /// that stops the run before it writes anything.
    Casualty,
}

/// What a [`Warden::roll`]'s read pass makes of one tenant's two reads.
///
/// `None` is the answer that needs a third read: this tenant has no workload at
/// all, and whether that is an account somebody closed or a job somebody did not
/// finish is a question only the cancellation marker answers. See
/// [`Warden::workloadless`].
///
/// Pure, and kept apart from the reads for that reason: this is the rule that
/// decides whether a fleet gets rolled at all, and a rule that is a table can be
/// held to one. `status` is [`Warden::status_of`]'s word, which is `Active` or
/// `Failed` by the time a drift report exists to pair it with.
fn verdict_of(status: TenantStatus, report: &DriftReport) -> Option<Step> {
    if !report.deployment_present {
        // A status word that says there is a workload over a report that says
        // there is not. [`Warden::inspect`] derives both from ONE read, so it
        // cannot hand this pairing in; a report built from two reads with a
        // delete between them can, and so can any other caller of this
        // function. Reading an empty report as "no changes, therefore fine"
        // would file a mailbox that just disappeared under converged, so the
        // rule is stated here rather than left to whoever assembles the two.
        return None;
    }
    if status == TenantStatus::Failed && report.changes.is_empty() {
        // Nothing to apply, and nothing serving. This render is already on this
        // tenant and the mailbox did not come back on it, so the render is the
        // suspect and no other tenant may be handed it.
        //
        // FIRST, ahead of the foreign check, and that order is the rule rather
        // than a detail of it: a `kubectl rollout restart` or a `kubectl edit`
        // is the first thing anybody does to a mailbox that is down, and it
        // stamps a foreign field manager onto the Deployment. Asking about
        // owners first would let debugging the casualty turn it into an
        // ordinary foreign skip, and the run would go back to handing the
        // suspect render to every tenant behind it while reporting that nothing
        // is broken. A tenant that is failed with today's render on it stops
        // the run whoever has touched it since.
        return Some(Step::Casualty);
    }
    if !report.foreign.is_empty() {
        // Somebody else owns part of it and this render is not on trial here,
        // because the tenant is either serving or behind. Repairing one means
        // deleting a live workload, which is not this timer's call.
        return Some(Step::Foreign);
    }
    if !report.changes.is_empty() {
        // A FAILED tenant included, which is the incident this whole feature
        // exists for: a pod wedged on somebody's Secret reference, or one a
        // render behind and crashing. A tenant that has not been given today's
        // render yet is a tenant today's render may fix.
        return Some(Step::Roll);
    }
    Some(Step::Current)
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
    /// The tenant is real and there is no pod to ask: it is pending, stopped or
    /// failed, and the question this refuses
    /// ([`Warden::first_paired`]) can only be answered by a running daemon.
    ///
    /// Its own variant rather than [`WardenError::NotReconcilable`]'s 409,
    /// because the two say different things to the caller. A 409 means "do
    /// something else"; this means "ask again later", and later is exactly what
    /// the control plane's poller does anyway. See the wire mapping in
    /// [`crate::handlers`].
    #[error("tenant not running")]
    NotRunning,
    /// The account behind this tenant is CLOSED
    /// ([`objects::CANCELLED_AT_ANNOTATION`]), so nothing here may put a
    /// workload up, roll one, or hand out a key to one.
    ///
    /// Its own variant rather than a second meaning for
    /// [`WardenError::NotReconcilable`], because the two want different things
    /// done about them and one of them is not about shape at all: a cancelled
    /// account has an EXIT, and it is [`Warden::set_credentials`] - the account
    /// holder re-consenting. Same 409 on the wire, so the control plane's
    /// existing handling is unchanged; the word is what changes, in the log and
    /// in the body.
    #[error("account cancelled")]
    Cancelled,
    #[error("{reason}")]
    Cluster { reason: &'static str },
}

impl WardenError {
    pub(crate) fn cluster(reason: &'static str) -> Self {
        Self::Cluster { reason }
    }
}

/// The machine reason [`Warden::reconcile_converging`] refuses with, and the one
/// failure [`Warden::roll`] does not halt on: a caller that may not delete found
/// a Deployment that could only be repaired by deleting it. One constant because
/// the refusal and the caller that reads it have to agree on the spelling, and a
/// roll that misread it would halt the fleet on a tenant it merely skipped.
const RECREATE_REFUSED: &str = "recreate_refused";

/// A tenant with a workload and no sealed credential behind it: a state this
/// warden never writes, and one it cannot render its way out of.
///
/// One constant for the same reason [`RECREATE_REFUSED`] is one: three writers
/// refuse with it ([`Warden::set_llm_key`], [`Warden::drift`],
/// [`Warden::reconcile_inner`]) and [`Warden::inspect`] reads it to tell a
/// tenant that will NEVER be readable from a cluster that has stopped
/// answering. Those two want opposite things from a fleet roll - one tenant
/// named for a person, or the whole run stopped - and a misspelling here would
/// silently pick the wrong one.
const CREDENTIAL_MISSING: &str = "credential_missing";

/// How long [`Warden::first_paired`] waits for a Ready pod before giving up.
///
/// DELIBERATELY NOT [`crate::config::Config::ready_timeout`], which is what
/// every other exec path on this service uses. Those paths are a person's
/// signup: something is being provisioned, somebody is watching a spinner, and
/// waiting the rollout out is the difference between a finished signup and a
/// failed one. This one is a periodic poll for an analytics fact, and ITS RETRY
/// IS THE WAIT — the control plane comes back in minutes regardless, so holding
/// a warden request open for a whole ready_timeout buys a stamp that was going
/// to be collected on the next tick anyway, at the price of a thread parked on
/// a tenant that is mid-roll.
///
/// Five seconds rather than zero because a settled tenant answers immediately
/// and this is only ever spent on one that is moving.
const DEVICES_POD_WAIT: Duration = Duration::from_secs(5);

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

/// Whether this identity Secret records a cancelled account, from
/// [`objects::CANCELLED_AT_ANNOTATION`].
///
/// PRESENCE and not the value. The annotation is stamped with a timestamp for a
/// person reading `kubectl get secret -o yaml`, and reading it as a number here
/// would mean deciding what an unparseable one meant: an account holder's
/// cancellation would come back to life because somebody hand-edited the
/// annotation into a date format, which is the opposite of the direction this
/// marker is allowed to fail in. A key that is there says cancelled, whatever it
/// says next to it.
fn is_cancelled(secret: &Secret) -> bool {
    secret
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(objects::CANCELLED_AT_ANNOTATION))
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
        let Some(identity) = self.identity(&name).await? else {
            return Err(WardenError::NotFound);
        };
        let reopening = is_cancelled(&identity);
        // "Already provisioned" means SERVING, not merely "some objects exist".
        // A phase two that died waiting for a pod left objects behind, and the
        // control plane's retry has to be able to converge on them rather than
        // bounce off a 409 forever.
        //
        // A CANCELLED account is exempt from it, and that exemption is what
        // keeps this route from having a state with no exit. `delete` stops at
        // its first error, so a teardown that failed on the Ingress leaves a
        // closed account whose Deployment is still up and still serving - which
        // this check would read as "already provisioned" and refuse, while
        // `reconcile` and the roll refuse it for being cancelled. Nothing could
        // move it. Of the two readings of that tenant, "the account holder is
        // re-consenting" has somewhere to go and "somebody is claiming a
        // running mailbox" does not: the mailbox is running on a credential its
        // owner already cancelled, and replacing it is the repair.
        let status = self.status_of(&name).await?;
        if !reopening && status == TenantStatus::Active {
            return Err(WardenError::Conflict);
        }

        self.install_credentials(&name, &ciphertext, status, reopening)
            .await
    }

    /// REPLACE a live tenant's credential, because its owner re-consented.
    ///
    /// The sibling of [`Warden::set_credentials`] and deliberately not a
    /// relaxation of it. That route's 409 for an ACTIVE tenant is load bearing
    /// — a second phase two against a serving mailbox is somebody claiming it —
    /// and the way to admit the one caller who legitimately has a replacement
    /// is a door with its own key, not a weaker lock on the existing one.
    ///
    /// THE KEY IS THE MAILBOX. `account_email` is matched against the tenant's
    /// own identity Secret, so this route cannot install a credential into a
    /// mailbox the caller has not proved they own. The control plane checks the
    /// same thing against its store before calling; this is the second check,
    /// made by a different service from a different record, and it is the one
    /// the cluster is authoritative for.
    ///
    /// A CANCELLED account is refused rather than reopened. `set_credentials`
    /// owns reopening, and it is a decision about billing and consent that a
    /// re-consent link must not be able to make on its own.
    pub async fn replace_credentials(
        &self,
        raw_label: &str,
        raw_email: &str,
        raw_ciphertext: &str,
    ) -> Result<Pairing, WardenError> {
        let name = TenantName::parse(raw_label)?;
        let account_email = validate::validate_account_email(raw_email)?;
        let ciphertext = validate::validate_ciphertext(raw_ciphertext)?;

        let Some(identity) = self.identity(&name).await? else {
            return Err(WardenError::NotFound);
        };
        // A closed account is not reopened by re-consenting. See above.
        if is_cancelled(&identity) {
            return Err(WardenError::Conflict);
        }
        match secret_value(&identity, objects::ACCOUNT_EMAIL_KEY) {
            Some(stored) if stored == account_email => {}
            // A DIFFERENT mailbox, or a Secret this warden did not write. Both
            // are the same refusal: which one it was is exactly what somebody
            // probing would want to learn.
            _ => return Err(WardenError::Conflict),
        }

        // AND THE STATUS, which is the half the control plane's own check is not
        // independent of. Its store only maps a mailbox to an ACTIVE tenant, so
        // today nothing pending reaches here — but that is ONE gate, and this
        // route exists to be the second one. A PENDING tenant is a signup that
        // stopped between its two calls, and installing a credential into it
        // would finish that signup: a mailbox provisioned with no invite spent,
        // through a public link. STOPPED is deliberately down and must not be
        // brought back up by re-consenting. FAILED is admitted with Active on
        // purpose: a tenant whose pod is not serving is often one whose
        // credential is exactly what died.
        let status = self.status_of(&name).await?;
        if !matches!(status, TenantStatus::Active | TenantStatus::Failed) {
            return Err(WardenError::Conflict);
        }
        self.install_credentials(&name, &ciphertext, status, false)
            .await
    }

    /// Everything both credential routes do once their own guard has passed:
    /// store the blob, apply the workload, wait for the new pod, and mint a
    /// pairing. Shared rather than copied because the ORDER in it is the
    /// contract (volume, policy, service, workload, ingress) and two copies of
    /// an order are one edit away from disagreeing.
    async fn install_credentials(
        &self,
        name: &TenantName,
        ciphertext: &str,
        status: TenantStatus,
        reopening: bool,
    ) -> Result<Pairing, WardenError> {
        // Verbatim. The warden does not parse, re-serialize or pretty-print
        // this: it is somebody else's ciphertext and the only correct thing to
        // do with it is put it where the daemon will look.
        self.apply(
            name,
            Object::Secret(Box::new(objects::credential_secret(
                &self.config,
                name,
                ciphertext,
            ))),
            "credential_write_failed",
        )
        .await?;

        // Order is the contract. The volume exists before anything wants it;
        // the NetworkPolicy exists before the pod it polices, so a tenant is
        // never briefly reachable; the Ingress is last, so the hostname starts
        // answering only once there is something behind it.
        self.apply(
            name,
            Object::Pvc(Box::new(objects::data_pvc(&self.config, name))),
            "volume_failed",
        )
        .await?;
        self.apply(
            name,
            Object::NetworkPolicy(Box::new(objects::network_policy(&self.config, name))),
            "network_policy_failed",
        )
        .await?;
        self.apply(
            name,
            Object::Service(Box::new(objects::service(&self.config, name))),
            "service_failed",
        )
        .await?;
        // A virtual key stored before provisioning must reach the pod being
        // born: "PUT llm-key then PUT credentials" is a legal order, and the
        // pod that comes up here has to carry the key's hash or the first
        // rotation would find nothing to differ from. The Secret can only
        // exist if `llm_base_url` was configured when `set_llm_key` accepted
        // it, so this pickup cannot stamp the annotation with the feature off.
        let llm_hash = self.llm_hash(name).await?;
        // The same pickup, for the same reason: "PUT control-token then PUT
        // credentials" is a legal order too, and a pod born without the
        // annotation would never differ from the first rotation.
        let share_hash = self.share_hash(name).await?;
        // The hash of what was just stored, not of what is running: this is the
        // whole mechanism by which a re-consent reaches the daemon.
        self.apply(
            name,
            Object::Deployment(Box::new(objects::deployment(
                &self.config,
                name,
                &objects::credential_hash(ciphertext),
                llm_hash.as_deref(),
                share_hash.as_deref(),
            ))),
            "workload_failed",
        )
        .await?;
        self.apply(
            name,
            Object::Ingress(Box::new(objects::ingress(&self.config, name))),
            "ingress_failed",
        )
        .await?;

        // REPLACING a workload, so the pod that answers `ready_pod` may be the
        // one this apply just condemned: the strategy is `Recreate`, and the
        // old pod is Ready and matching this tenant's selector for as long as
        // it takes to terminate. `squelchd pair` execed into that pod writes a
        // live pairing code into a container the kubelet is killing, and the
        // handoff is gone with it. Waiting for the ROLLOUT first is what makes
        // the next line's answer the new pod; see [`Cluster::rollout_complete`].
        //
        // Only when there was something to replace. A signup's first phase two
        // has no old pod to be confused with, and making every new tenant wait
        // out `minReadySeconds` as well would spend a good part of the deadline
        // on a race that cannot happen to it.
        if matches!(status, TenantStatus::Active | TenantStatus::Failed) {
            self.cluster
                .rollout_complete(name.as_str(), self.config.ready_timeout)
                .await
                .map_err(|e| fail(name.as_str(), "not_ready", &e))?;
        }
        let pod = self
            .cluster
            .ready_pod(&objects::pod_selector(name), self.config.ready_timeout)
            .await
            .map_err(|e| fail(name.as_str(), "not_ready", &e))?;

        // The reopen is DONE, so the marker comes off - and it comes off here,
        // after the workload this call promised is up, rather than on the way
        // in. What a half-finished call leaves behind has to be readable, and
        // the two orders leave opposite things:
        //
        // Cleared last, a reopen that dies partway leaves the marker on, over a
        // tenant whose objects are somewhere between the two states. Every
        // reader refuses it, which is correct - the account is still closed
        // until this call says otherwise - and the retry is this same call,
        // which is exempt from the 409 above for exactly that reason. Nothing
        // is stuck.
        //
        // Cleared FIRST, the same failure leaves a mailbox that is up and
        // serving on the credential its owner cancelled, with nothing on record
        // saying anybody cancelled it: `reconcile` converges it, the roll rolls
        // it, and the closed account is now an ordinary active tenant. That is
        // the one direction this marker may never fail in.
        if reopening {
            self.cluster
                .annotate_secret(
                    &name.identity_secret(),
                    objects::CANCELLED_AT_ANNOTATION,
                    None,
                )
                .await
                .map_err(|e| fail(name.as_str(), "cancel_marker_failed", &e))?;
            tracing::info!(tenant = %name, "reopened a cancelled account");
        }

        let pairing = self.mint_pairing(name, &pod).await?;
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
        let Some(identity) = self.identity(&name).await? else {
            return Err(WardenError::NotFound);
        };
        // Intent before shape, the same order `reconcile` and the roll use. Two
        // things happen below and a closed account may have neither: a live
        // gateway credential is stored against it, and - if a teardown stopped
        // partway and left the Deployment standing - that Deployment is
        // re-rendered and re-applied, which rolls the pod of a mailbox its owner
        // closed. `delete` removes the LLM Secret precisely because the key is a
        // credential rather than tenant data, so writing one back here would
        // undo the teardown's one irreversible step.
        //
        // Reopening is `set_credentials`, and the control plane mints a fresh
        // key for a reopened account anyway; a key stored against a cancelled
        // one would be a revoked token waiting to roll a pod.
        if is_cancelled(&identity) {
            tracing::warn!(
                tenant = %name,
                "refusing to store an llm key for a cancelled account"
            );
            return Err(WardenError::Cancelled);
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
                    reason = CREDENTIAL_MISSING,
                    "a workload exists but its credential Secret does not"
                );
                return Err(WardenError::cluster(CREDENTIAL_MISSING));
            };
            // CARRIED FORWARD, not recomputed from this request: a re-render
            // that dropped the share annotation would roll every sharing
            // tenant's pod off its own token on the next key rotation.
            let share_hash = self.share_hash(&name).await?;
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
                    share_hash.as_deref(),
                ))),
                "workload_failed",
            )
            .await?;
        }

        tracing::info!(tenant = %name, "llm key stored");
        Ok(())
    }

    /// Store (or rotate, or remove) a tenant's share token and roll its pod
    /// onto it.
    ///
    /// `share_token` of `None` is a REMOVAL, which is the one place this
    /// departs from [`Self::set_llm_key`]'s shape: that Secret has two slots
    /// and a missing one means "leave the other alone", so it has no way to
    /// spell "take it away". This Secret has one slot and one meaning, so
    /// `share revoke` can be the same call as `share mint` with nothing in it,
    /// and the pod rolls back to a daemon that answers "sharing is not
    /// available here". Without that, revoking on the control plane would
    /// leave a live-looking token in the pod's env, and the tenant would find
    /// out it was revoked only by having an invite refused.
    ///
    /// NO FEATURE GATE, unlike the LLM path. The env this feeds is rendered
    /// whenever the control plane's origin is configured, which is the same
    /// condition that makes a control plane exist to mint against.
    pub async fn set_share_token(
        &self,
        raw_label: &str,
        raw_share_token: Option<&str>,
    ) -> Result<(), WardenError> {
        let name = TenantName::parse(raw_label)?;
        let share_token = raw_share_token
            .map(validate::validate_share_token)
            .transpose()?;
        let Some(identity) = self.identity(&name).await? else {
            return Err(WardenError::NotFound);
        };
        // Intent before shape, the same order everything else here uses. The
        // reasoning is `set_llm_key`'s exactly: storing a live credential
        // against a closed account, and re-rendering a Deployment a teardown
        // left standing, are both things a cancelled mailbox must not get.
        // A REMOVAL is exempt, because taking a credential away from a
        // cancelled account is the direction teardown was already going.
        if share_token.is_some() && is_cancelled(&identity) {
            tracing::warn!(
                tenant = %name,
                "refusing to store a share token for a cancelled account"
            );
            return Err(WardenError::Cancelled);
        }

        match &share_token {
            Some(token) => {
                self.apply(
                    &name,
                    Object::Secret(Box::new(objects::control_secret(
                        &self.config,
                        &name,
                        token,
                    ))),
                    "share_token_write_failed",
                )
                .await?
            }
            // Tolerant of already-gone, the way every other delete here is:
            // revoking twice is a thing an operator does when the first answer
            // was lost, and it must not be an error.
            None => self
                .cluster
                .delete(Kind::Secret, &name.control_secret())
                .await
                .map_err(|e| fail(name.as_str(), "share_token_delete_failed", &e))?,
        }

        let deployment = self
            .cluster
            .get_deployment(name.as_str())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?;
        if deployment.is_some() {
            // The ciphertext the running pod was rolled for, byte-exact from
            // the Secret. Same read, same refusal, and the same reason as
            // `set_llm_key`: a workload with no sealed credential behind it is
            // a state this warden never writes.
            let ciphertext = self
                .cluster
                .get_secret(&name.credential_secret())
                .await
                .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?
                .as_ref()
                .and_then(|secret| secret_value(secret, objects::CREDENTIAL_KEY));
            let Some(ciphertext) = ciphertext else {
                tracing::error!(
                    tenant = %name,
                    reason = CREDENTIAL_MISSING,
                    "a workload exists but its credential Secret does not"
                );
                return Err(WardenError::cluster(CREDENTIAL_MISSING));
            };
            // The LLM pair carried forward, for the reason the share hash is
            // carried through `set_llm_key`: neither rotation may knock the
            // other's annotation off the pod template.
            let llm_hash = self.llm_hash(&name).await?;
            self.apply(
                &name,
                Object::Deployment(Box::new(objects::deployment(
                    &self.config,
                    &name,
                    &objects::credential_hash(&ciphertext),
                    llm_hash.as_deref(),
                    // Over what was just written, not what was asked for. On a
                    // removal that is `None`, which is what takes the
                    // annotation off and rolls the pod without the variable.
                    share_token
                        .as_deref()
                        .map(objects::credential_hash)
                        .as_deref(),
                ))),
                "workload_failed",
            )
            .await?;
        }

        // PRIVACY: whether one was stored, never the token.
        tracing::info!(tenant = %name, stored = share_token.is_some(), "share token set");
        Ok(())
    }

    /// Re-mint a pairing code for a later device. Nothing else about the tenant
    /// is touched, and the previous code is superseded, which is the daemon's
    /// documented behaviour: one live pairing code per account.
    ///
    /// A CANCELLED account is refused, and this is the route where that matters
    /// most. A pairing code is full access to a mailbox, so the one thing a
    /// closed account may never produce is a fresh one - and a teardown that
    /// stopped partway leaves exactly the tenant this route would otherwise
    /// serve: the marker written, the Deployment still up, a pod still Ready.
    /// The shape says "here is a running tenant"; only
    /// [`objects::CANCELLED_AT_ANNOTATION`] says whose it still is.
    pub async fn repair(&self, raw_label: &str) -> Result<Pairing, WardenError> {
        let name = TenantName::parse(raw_label)?;
        let Some(identity) = self.identity(&name).await? else {
            return Err(WardenError::NotFound);
        };
        if is_cancelled(&identity) {
            tracing::warn!(
                tenant = %name,
                "refusing to mint a pairing code for a cancelled account; \
                 reopening one is a re-consent"
            );
            return Err(WardenError::Cancelled);
        }
        // The pod that is going to SURVIVE. This route applies nothing, but
        // something else may be mid-apply on this tenant right now - a roll, or
        // an operator's reconcile - and under `Recreate` the pod being replaced
        // stays Ready and stays in this selector until it terminates. A code
        // minted in that one is a code that dies with it. Waiting the rollout
        // out costs a call on a settled tenant and answers at once
        // ([`Cluster::rollout_complete`] reads four integers), and a tenant with
        // no Deployment at all fails here immediately rather than spending the
        // whole deadline waiting for a pod nothing is going to create.
        //
        // A known tenant with nothing running is not a 404 (the tenant exists)
        // and not a 409 (nothing conflicts); it is this cluster being unable to
        // do the thing right now.
        self.cluster
            .rollout_complete(name.as_str(), self.config.ready_timeout)
            .await
            .map_err(|e| fail(name.as_str(), "no_ready_pod", &e))?;
        let pod = self
            .cluster
            .ready_pod(&objects::pod_selector(&name), self.config.ready_timeout)
            .await
            .map_err(|e| fail(name.as_str(), "no_ready_pod", &e))?;
        self.mint_pairing(&name, &pod).await
    }

    /// When a client device FIRST paired with this tenant's mailbox, or `None`
    /// if none ever has: the activation signal (issue #89).
    ///
    /// ONE TIMESTAMP CROSSES THIS BOUNDARY. Not a device count, not a name, not
    /// a listing — the daemon subcommand this execs was made separate from
    /// `token list` precisely so that over-sharing is impossible here rather
    /// than merely avoided. What the control plane learns is whether somebody
    /// ever ran the app, which is the question it could not answer at all.
    ///
    /// ITS OWN ROUTE, not a field on [`Warden::status`]. A status is a cheap GET
    /// that the control plane makes freely, and an exec into a pod is not cheap;
    /// putting one behind `status()` would turn every incidental status check
    /// into a pod round trip.
    ///
    /// The refusals, in the order they are reached:
    ///
    /// - an unknown label is [`WardenError::NotFound`], decided off the identity
    ///   Secret exactly as `status` decides it, so the two routes agree about
    ///   what a tenant IS;
    /// - anything but [`TenantStatus::Active`] is [`WardenError::NotRunning`]
    ///   (503, not 409): there is no pod to ask, the caller's next move is to
    ///   ask again later, and it was going to do that anyway;
    /// - a non-zero exit or output this warden cannot read is a terse 500 with a
    ///   machine reason. NEVER A GUESS: an unparseable answer must not become
    ///   "nobody has ever paired", because the control plane would store that as
    ///   a standing fact about a tenant it never actually heard from.
    ///
    /// NO ROLLOUT WAIT, unlike [`Warden::repair`], and the difference is what
    /// the two are asking for. A pairing code minted in a pod that is about to
    /// terminate is a code that dies with the pod, so `repair` has to reach the
    /// pod that will SURVIVE. A read is not like that: every pod on this tenant
    /// mounts the same volume and reads the same store, so an answer from a
    /// terminating-but-Ready pod is exactly as true as one from its replacement.
    ///
    /// The exec output is not logged, at any level, on any branch. That is
    /// [`Warden::mint_pairing`]'s blanket rule and this inherits it whole: a
    /// timestamp would be harmless in a log line, and a rule with one harmless
    /// exception in it is a rule nobody can apply to the next command.
    pub async fn first_paired(
        &self,
        raw_label: &str,
    ) -> Result<Option<DateTime<Utc>>, WardenError> {
        let name = TenantName::parse(raw_label)?;
        if self.identity(&name).await?.is_none() {
            return Err(WardenError::NotFound);
        }
        if self.status_of(&name).await? != TenantStatus::Active {
            return Err(WardenError::NotRunning);
        }
        let pod = self
            .cluster
            .ready_pod(&objects::pod_selector(&name), DEVICES_POD_WAIT)
            .await
            .map_err(|e| fail(name.as_str(), "no_ready_pod", &e))?;
        let output = self
            .cluster
            .exec(&pod, &objects::first_paired_argv())
            .await
            .map_err(|e| fail(name.as_str(), "first_paired_failed", &e))?;
        if !output.ok {
            // The state an old daemon image is in: no such subcommand, non-zero
            // exit. Terse on purpose — the control plane's poller eats the 500
            // and the next fleet roll is the fix, so there is nothing here for
            // anyone to act on beyond the reason word.
            tracing::error!(
                tenant = %name,
                reason = "first_paired_failed",
                "squelchd token first-paired exited non-zero"
            );
            return Err(WardenError::cluster("first_paired_failed"));
        }
        // STDOUT ONLY. The daemon puts its one machine-readable line there and
        // everything a human reads on stderr, so folding the two together (which
        // `mint_pairing` does, for a parser that scans for a prefix) would let a
        // warning line become the answer.
        devices::parse_first_paired(&output.stdout).ok_or_else(|| {
            tracing::error!(
                tenant = %name,
                reason = "first_paired_unparsed",
                "squelchd token first-paired succeeded but printed neither a timestamp nor none"
            );
            WardenError::cluster("first_paired_unparsed")
        })
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
        let (live, status) = self.workload(&name).await?;
        self.drift_of(&name, status, live).await
    }

    /// [`Warden::drift`]'s body, over a Deployment and a status word the caller
    /// has already read.
    ///
    /// Split out for [`Warden::inspect`], which reads both to reach its own
    /// verdict and would otherwise pay for them twice - and it walks the WHOLE
    /// FLEET every tick, where the public route runs once for one label. Passing
    /// the reads in also means one tenant's verdict is decided from ONE view of
    /// it: a Deployment deleted between two reads cannot leave the status word
    /// and the report disagreeing about whether there is a workload.
    async fn drift_of(
        &self,
        name: &TenantName,
        status: TenantStatus,
        live: Option<Deployment>,
    ) -> Result<DriftReport, WardenError> {
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
                reason = CREDENTIAL_MISSING,
                "a workload exists but its credential Secret does not"
            );
            return Err(WardenError::cluster(CREDENTIAL_MISSING));
        };
        let llm_hash = self.llm_hash(name).await?;
        let share_hash = self.share_hash(name).await?;

        let rendered = objects::deployment(
            &self.config,
            name,
            &objects::credential_hash(&ciphertext),
            llm_hash.as_deref(),
            share_hash.as_deref(),
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
    /// That refusal has a cost, and the cancellation marker is what keeps it
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
    /// A CANCELLED tenant is [`WardenError::Cancelled`] whatever its status word
    /// and whatever objects are still standing, because
    /// [`objects::CANCELLED_AT_ANNOTATION`] is the account holder's decision
    /// written down and no shape repair overrides one. That covers the state
    /// this route used to guess at - a teardown that stopped partway, leaving a
    /// Deployment alive with its Service and Ingress gone - which read as an
    /// ordinary drifted tenant and would have been force-applied straight back
    /// onto the internet. Reopening is [`Warden::set_credentials`], which clears
    /// the marker.
    ///
    /// The marker is asked THREE times, and the two extra reads are the two
    /// windows this route cannot close by asking once: the apply window, where
    /// the Deployment goes away between the read and the write, and the rollout
    /// wait, which is a whole `ready_timeout` wide. Each one re-reads the marker
    /// rather than guessing from the missing workload.
    ///
    /// [`TenantStatus::Stopped`] WITHOUT the marker is a job nobody finished,
    /// and it proceeds: the credential and the volume are where they were and
    /// only the workload is missing, which is exactly what the delete-recreate
    /// window above leaves behind. With ONE exception, and it is a migration
    /// rather than a rule - a tenant cancelled before the marker existed reads
    /// exactly like that too, and [`Warden::torn_down_before_the_marker`] is
    /// what keeps this route from resurrecting one on the first tick after the
    /// deploy.
    ///
    /// Secrets are never rewritten. This converges SHAPE; identities,
    /// credentials and keys are what they were when it started, and the two
    /// hashes on the pod template are recovered from the stored Secrets so the
    /// render is the one this tenant is entitled to rather than a new one.
    ///
    /// This is the OPERATOR's entry point, and the delete above is why it has
    /// its own: it is reached from one route, called about one label, by
    /// somebody who has read that tenant's drift report. The unattended caller
    /// takes [`Warden::reconcile_converging`] instead, which cannot delete
    /// anything.
    pub async fn reconcile(&self, raw_label: &str) -> Result<Reconciled, WardenError> {
        self.reconcile_inner(raw_label, true).await
    }

    /// [`Warden::reconcile`] with the delete-and-recreate branch TAKEN AWAY:
    /// converge what applies can converge, and refuse outright on a Deployment
    /// another field manager owns part of.
    ///
    /// What the fleet roll calls, and the reason it is a second entry point
    /// rather than a check the roll performs before calling the first one. The
    /// roll does look at [`Warden::drift`] first, and that look is an
    /// optimization: it is one read, several API calls before the read
    /// `reconcile` makes for itself, and a foreign owner that appears in the
    /// gap would be found only by the second one. A refusal that lives in the
    /// caller is a refusal that races. This one lives at the write, so an
    /// unattended timer is not able to delete somebody's live mailbox rather
    /// than merely disinclined to ask.
    ///
    /// The refusal is [`RECREATE_REFUSED`], which the roll records as a foreign
    /// skip. By the time it fires, the volume, the NetworkPolicy and the
    /// Service have been re-applied - the warden's own objects, converged with
    /// no delete, no downtime and nothing of anyone else's touched - and the
    /// Deployment is exactly as it was.
    async fn reconcile_converging(&self, raw_label: &str) -> Result<Reconciled, WardenError> {
        self.reconcile_inner(raw_label, false).await
    }

    /// The body of both entry points. `allow_recreate` decides the one question
    /// they disagree on: whether a Deployment another field manager owns part
    /// of may be deleted and applied fresh.
    async fn reconcile_inner(
        &self,
        raw_label: &str,
        allow_recreate: bool,
    ) -> Result<Reconciled, WardenError> {
        let name = TenantName::parse(raw_label)?;
        let Some(identity) = self.identity(&name).await? else {
            return Err(WardenError::NotFound);
        };
        // Intent before shape, and before any read of the objects: a cancelled
        // account is refused whatever is still standing. `delete` stamps the
        // marker before it removes anything, so a teardown that died halfway
        // through carries it too, and that is the case this ordering exists for
        // - a Deployment still running with its Service already gone is a
        // cancellation in progress, and it is indistinguishable from ordinary
        // drift to anything that only looks at the objects.
        if is_cancelled(&identity) {
            tracing::warn!(
                tenant = %name,
                "refusing to reconcile a cancelled account; reopening one is a re-consent"
            );
            return Err(WardenError::Cancelled);
        }
        match self.status_of(&name).await? {
            TenantStatus::Active | TenantStatus::Failed => {}
            // Stopped with nothing recording a cancellation: a job that did not
            // finish, and finishing it is the whole point of the route - unless
            // this tenant was cancelled before there was a marker to record it
            // with, which is what the bridge above answers. See
            // [`Warden::torn_down_before_the_marker`]; the reads either side of
            // it are the whole reason it is asked here and not at the top.
            TenantStatus::Stopped => {
                if self.torn_down_before_the_marker(&name).await? {
                    tracing::warn!(
                        tenant = %name,
                        "refusing to rebuild a workload nothing routes and nothing claims; \
                         a teardown that predates the cancellation marker looks exactly like this"
                    );
                    return Err(WardenError::Cancelled);
                }
                tracing::info!(
                    tenant = %name,
                    "resuming a reconcile that did not finish; nothing cancelled this tenant"
                );
            }
            TenantStatus::Pending => return Err(WardenError::NotReconcilable),
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
        let share_hash = self.share_hash(&name).await?;

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
            share_hash.as_deref(),
        );
        let live = self
            .cluster
            .get_deployment(name.as_str())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?;
        let outcome = match live.as_ref().map(drift::foreign_managers) {
            // The Deployment was there when the status was read and is gone
            // now, so something deleted it while this ran - and the only thing
            // that deletes a live tenant's workload is a cancellation. Re-read
            // the marker before rebuilding it: `delete` stamps that annotation
            // BEFORE it removes anything, so a teardown that has got as far as
            // the Deployment has certainly got as far as the marker, and
            // applying now would race a cancelled mailbox back onto the
            // internet.
            //
            // This is the same question the top of the route asked, and it is
            // asked twice on purpose: the answer there was read before the
            // window this arm is inside of.
            None => {
                if self.cancelled(&name).await? {
                    tracing::warn!(
                        tenant = %name,
                        "the workload was deleted while reconciling; refusing to rebuild it"
                    );
                    return Err(WardenError::Cancelled);
                }
                "created"
            }
            Some(foreign) if !foreign.is_empty() => {
                // The structural half of the fleet roll's foreign rule. The
                // roll asked this question a few API calls ago and got a clean
                // answer; this is the read that decides, and a caller that may
                // not delete stops here whatever the earlier one said.
                if !allow_recreate {
                    tracing::warn!(
                        tenant = %name,
                        managers = foreign.len(),
                        "refusing to recreate a Deployment other field managers own fields on; \
                         deleting a live workload is a decision for a person"
                    );
                    return Err(WardenError::cluster(RECREATE_REFUSED));
                }
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
        //
        // The window this wait opens is the WIDEST one on the route - a whole
        // `ready_timeout`, where the apply window above is a few API calls - so
        // the same question gets asked a third time on the way out. A
        // [`ClusterError::NoPod`] here means the Deployment went away while the
        // rollout was being watched, and the only thing that deletes a live
        // tenant's workload is a cancellation. Reported as a cluster failure it
        // would halt the whole fleet roll and page somebody at midnight to
        // discover that an account holder closed their account, which is the
        // system working; reported as what it is, the roll files it as a skip
        // and moves on. See [`Warden::roll`].
        if let Err(e) = self
            .cluster
            .rollout_complete(name.as_str(), self.config.ready_timeout)
            .await
        {
            if matches!(e, ClusterError::NoPod) && self.cancelled(&name).await.unwrap_or(false) {
                tracing::warn!(
                    tenant = %name,
                    "the workload was deleted while its rollout was being watched; \
                     the account was cancelled under this reconcile"
                );
                return Err(WardenError::Cancelled);
            }
            return Err(fail(name.as_str(), "not_ready", &e));
        }

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
        // The marker goes on BEFORE the first delete, and a failure to write it
        // ends the call with every object still standing.
        //
        // See [`objects::CANCELLED_AT_ANNOTATION`]. The readers that have to
        // tell a closed account from an unfinished repair ask this annotation,
        // and the loop below stops at its first error, so a teardown that began
        // without one could leave a half-removed tenant that reads as a repair
        // to finish - and finishing it would put a cancelled mailbox back on
        // the internet. Refusing here leaves a tenant fully up and a
        // cancellation the caller can retry, which is the survivable half of
        // the two.
        self.cluster
            .annotate_secret(
                &name.identity_secret(),
                objects::CANCELLED_AT_ANNOTATION,
                Some(&now_secs().to_string()),
            )
            .await
            .map_err(|e| fail(name.as_str(), "cancel_marker_failed", &e))?;
        // Ingress first: no new requests get routed at a pod that is about to
        // go. NetworkPolicy last: the pod stays policed for the whole of its
        // termination. The Service goes before the Deployment so the endpoint
        // drains before the pod does, which is the same direction the Ingress
        // rule is reasoning in.
        //
        // Nothing about telling a cancellation apart rides on that order any
        // more. The marker above does it, and it does it for the states an
        // ordering cannot reach: this loop can leave ANY prefix of the four
        // standing, and every one of those prefixes now reads as cancelled.
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
        // And the share token, for exactly the same reason: it is a live
        // bearer against the control plane, not tenant data. Leaving it would
        // let a cancelled mailbox's Secret go on naming an active tenant if the
        // control-plane row outlived the teardown by a step.
        self.cluster
            .delete(Kind::Secret, &name.control_secret())
            .await
            .map_err(|e| fail(name.as_str(), "share_token_delete_failed", &e))?;
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

    /// Every tenant this warden has provisioned, sorted, and how many identity
    /// Secrets it had to skip.
    ///
    /// The count is RETURNED and not merely logged, which is the difference
    /// between an operator finding out and an operator having to go looking. It
    /// ends up in [`Rolled::unreadable`], and from there in the exit code: a run
    /// that walked a fleet with a tenant it can never address must not be able
    /// to exit as though the fleet were whole.
    ///
    /// Read from the CLUSTER, by the same enumeration [`Warden::sweep_pending`]
    /// walks: the identity Secrets carrying [`objects::MANAGED_SELECTOR`], with
    /// any name that does not parse back into a label skipped rather than
    /// guessed at. The cluster is the record; there is no list of tenants
    /// anywhere in this service to fall out of date with it.
    ///
    /// A skip that is an identity Secret is COUNTED and logged. Most of what
    /// the selector matches is not an identity at all - every tenant's
    /// credential and LLM Secrets carry the same managed-by label - and those
    /// are ordinary. A name ending in [`validate::IDENTITY_SUFFIX`] whose label
    /// will not parse is not: it is a tenant record this warden can see and
    /// cannot address, so it is excluded from every roll from now until
    /// somebody notices, and silence is how nobody ever does. The COUNT and not
    /// the name, because the name is precisely the string that failed
    /// validation.
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
    pub async fn fleet(&self) -> Result<(Vec<TenantName>, usize), WardenError> {
        let secrets = self
            .cluster
            .list_secrets(objects::MANAGED_SELECTOR)
            .await
            // No tenant to name: this failure is the enumeration's, not
            // anyone's.
            .map_err(|e| fail("-", "fleet_list_failed", &e))?;
        let mut fleet: Vec<TenantName> = Vec::new();
        let mut unreadable = 0usize;
        for name in secrets.iter().filter_map(|s| s.metadata.name.as_deref()) {
            match TenantName::from_identity_secret(name) {
                Some(tenant) => fleet.push(tenant),
                None if name.ends_with(validate::IDENTITY_SUFFIX) => unreadable += 1,
                // A credential or an LLM Secret: labelled by this warden,
                // never a tenant record, and nothing to report.
                None => {}
            }
        }
        if unreadable > 0 {
            tracing::error!(
                unreadable,
                fleet = fleet.len(),
                "identity Secrets whose label does not validate; those tenants are in no roll"
            );
        }
        fleet.sort();
        Ok((fleet, unreadable))
    }

    /// Read the whole fleet, and roll AT MOST ONE tenant onto today's render.
    ///
    /// The warden writes a tenant's objects once, at provision time, and never
    /// revisits them. So changing [`crate::config::Config::image`] - or
    /// anything else [`objects::deployment`] renders - changes what the NEXT
    /// signup gets and nothing at all about the tenants already running. This
    /// is the pass that closes that gap, and it is the only thing in the
    /// service that touches more than one tenant's workload.
    ///
    /// ## One tenant per run, and the gap between runs is the safety
    ///
    /// A run converges one tenant and exits. Not a batch, and not "keep going
    /// until something fails": one, after which the CronJob's schedule holds
    /// the fleet still until the next tick.
    ///
    /// That pacing is the safety model, and it is deliberately not a CHECK.
    /// Every earlier version of this walked the fleet in one pass, which meant
    /// deciding in the seconds after an apply whether the tenant it had just
    /// rolled was healthy - and nothing available in those seconds answers
    /// that. [`Cluster::rollout_complete`] is the strongest signal on offer and
    /// it says only that the controller observed this spec with a ready replica
    /// on it. By default a tenant's readiness probe is a TCP accept on the
    /// daemon's door, and squelchd binds that socket before it builds the
    /// embedder, on purpose - so a pod reports Ready and then dies. A pass that
    /// trusted the answer would hand the same render to every mailbox it has,
    /// at machine speed, and exit converged.
    ///
    /// So the run does not try to know. It rolls one tenant and stops, and what
    /// stands between a bad render and the SECOND mailbox is a scheduling
    /// interval of a real daemon doing real work, plus the read pass on the
    /// next tick - which finds a tenant carrying today's render and not serving
    /// it, calls it a casualty, and refuses to roll anything at all.
    ///
    /// The cost is the schedule, and it is not hidden: a fleet with N tenants
    /// behind takes N ticks. [`Rolled::remaining`] is what says how many are
    /// left, and it is why a run that made progress needs an exit code distinct
    /// from both "converged" and "something is wrong".
    ///
    /// [`crate::config::Config::http_readiness`] makes the individual step
    /// stronger by putting `/healthz` behind the probe, and changes none of the
    /// above. It is off by default, it cannot be turned on until the whole
    /// fleet is on a daemon that serves the route, and a roller that needed it
    /// would be a roller that could not run during the rollout that gets the
    /// fleet there.
    ///
    /// ## Halting is the safety property, not a limitation
    ///
    /// A render that cannot come up is the failure this design fears, because
    /// a warden that kept going would apply it to every tenant it has. The
    /// tenant this run took either converges or ends the run without one:
    /// [`Rolled::halted_on`] names it, [`Rolled::remaining`] counts it back
    /// onto the queue, and the answer is the summary of what happened rather
    /// than an error. A bad render costs exactly one tenant, which is the price
    /// that makes running this unattended defensible at all.
    ///
    /// That covers a read that fails as well as a reconcile that fails. A
    /// tenant this warden could not even inspect is not a tenant it may skip
    /// past - the next one would be rolled on the strength of a cluster that
    /// has just stopped answering - and throwing the summary away with an `Err`
    /// would discard the record of the tenants it already changed. Every read
    /// happens before any write, so a failed read costs the run and not a
    /// half-rolled fleet.
    ///
    /// With ONE exception, and it is the difference between a cluster that has
    /// gone quiet and a tenant that is broken. A workload whose credential
    /// Secret is gone can never be rendered, by this run or any other, so
    /// halting on it would park every tenant after it in [`Warden::fleet`] order
    /// behind a run that stops at the same label every tick, forever. That one
    /// is named in [`Rolled::unrenderable`] and the walk goes on.
    ///
    /// ## Two passes, because one halt only protects one run
    ///
    /// Halting inside a single pass is not enough on its own, and the gap is
    /// the reason this is written as a READ pass over the whole fleet followed
    /// by a WRITE pass over what the first one marked.
    ///
    /// `reconcile` applies the render and only then waits for the rollout. A
    /// tenant whose rollout never finishes has therefore already RECEIVED that
    /// render: its live spec is what this warden renders, so its drift is
    /// clean, and a single-pass roll would find no changes on the next tick,
    /// call it current, walk past it, and hand the same render to the next
    /// tenant. Once per tick, alphabetically, until the fleet is down - and the
    /// last run of that sequence exits converged, because by then every tenant
    /// carries the render and none of them are serving it.
    ///
    /// So a tenant that is [`TenantStatus::Failed`] with CLEAN drift - no
    /// changes, no foreign owners, a Deployment present - is read as a
    /// CASUALTY: this render was applied here and the mailbox did not come
    /// back. Finding one stops the run where it stands, with
    /// [`Rolled::casualty`] and [`Rolled::halted_on`] both naming it and
    /// nothing applied anywhere. The read pass covers every tenant before the
    /// write pass touches any, so a casualty at the end of the fleet blocks the
    /// run as surely as one at the start.
    ///
    /// Failed WITH drift is the opposite verdict and stays rollable, because it
    /// is the incident this whole feature exists for: a pod wedged on a
    /// foreign Secret reference, or one still on last month's render and
    /// crashing. Only failed-and-already-current is the stop signal.
    ///
    /// A transient failure - a node rebooting, a tenant mid-crashloop for its
    /// own reasons - blocks the run too, and that is the deliberate direction
    /// rather than a cost of the design: the fleet is not rolled while a
    /// mailbox is down. The run says which tenant, the next tick tries again,
    /// and a tenant that stays down is a page for a person either way.
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
    /// The read pass finding the foreign owner is how that usually happens, and
    /// [`Warden::reconcile_converging`] is why it holds even when the owner
    /// arrives between the read and the write: the write pass has no delete in
    /// it to reach.
    ///
    /// A tenant that is [`TenantStatus::Pending`] or [`TenantStatus::Stopped`]
    /// is skipped too, for the reason `reconcile` refuses them: pending is a
    /// signup to finish and stopped is an account to reopen, and neither is a
    /// shape to converge. Those land in [`Rolled::skipped_inactive`], which is
    /// the bucket for tenants that are absent on purpose.
    ///
    /// A tenant a job left mid-repair reads `stopped` as well, and it is
    /// nothing like the other two: the volume and the credential are standing,
    /// nothing recorded a cancellation, and only the workload is missing, which
    /// is a mailbox that is DOWN. This run does not finish that repair either -
    /// resuming somebody's half-done delete-and-recreate unattended is the same
    /// judgement call the foreign rule refuses - but it does not file it under
    /// "nothing to do". It goes to [`Rolled::stranded`], where an operator can
    /// see it and call [`Warden::reconcile`] on that one label.
    /// [`objects::CANCELLED_AT_ANNOTATION`] is what tells the two apart, and it
    /// is a record of the account holder's intent rather than a shape worked
    /// out from whichever objects a teardown happened to leave standing. The
    /// one tenant the marker cannot speak for is one cancelled before it
    /// existed; [`Warden::torn_down_before_the_marker`] is the bridge that keeps
    /// those out of `stranded`, because naming a closed account as a mailbox
    /// that is down sends an operator to the call that would reopen it.
    ///
    /// Which leaves [`TenantStatus::Active`] and [`TenantStatus::Failed`], the
    /// two that have a workload for today's render to apply to; the casualty
    /// rule above is what decides which failed tenants are among them.
    ///
    /// `dry_run` does every read and no write: it reports what it WOULD have
    /// rolled, which is what an operator runs before the bump rather than
    /// after. The read pass runs either way, so a dry run halts on a casualty
    /// exactly as a real one does.
    ///
    /// PRIVACY: this walks every tenant in the fleet and logs as it goes. What
    /// goes in a line is a count, a status word or a LABEL, never a mailbox
    /// address and never an API error string; the per-tenant failure detail
    /// goes through [`fail`] like every other cluster error in this file.
    pub async fn roll(&self, dry_run: bool) -> Result<Rolled, WardenError> {
        let (fleet, unreadable) = self.fleet().await?;
        tracing::info!(
            fleet = fleet.len(),
            unreadable,
            dry_run,
            "fleet roll starting; every tenant read before any is written, then AT MOST ONE rolled"
        );

        let (mut summary, queue) = self.read_pass(&fleet).await;
        summary.unreadable = unreadable;
        // Empty whenever the read pass stopped, so a casualty or a failed read
        // costs the whole write pass rather than the rest of it.
        if dry_run {
            // Every one of them, because a dry run applies nothing and so no
            // tenant goes first. What it is reporting is the QUEUE, and the
            // length of that list is how many runs the operator is about to
            // sign up for.
            for name in &queue {
                tracing::info!(tenant = %name, "would roll this tenant onto today's render");
                summary.rolled.push(name.as_str().to_string());
            }
        } else if let Some((first, rest)) = queue.split_first() {
            let label = first.as_str().to_string();
            summary.remaining = rest.len();
            match self.reconcile_converging(first.as_str()).await {
                Ok(reconciled) => {
                    tracing::info!(
                        tenant = %first,
                        deployment = reconciled.deployment,
                        remaining = summary.remaining,
                        "rolled a tenant onto today's render; the rest wait for the next run"
                    );
                    summary.rolled.push(label);
                }
                // A foreign owner that arrived after the read pass looked. It
                // is a skip and not a halt: nothing is wrong with the render,
                // and the refusal already logged why this tenant was left.
                //
                // The run still ends here rather than moving down the queue.
                // The budget this pacing spends is an ATTEMPT and not a
                // success: a refused recreate has already applied this tenant's
                // volume, policy and Service, and "keep going until something
                // works" is the shape of loop that would let one bad tick touch
                // the whole fleet. Nothing stalls on it either - the owner that
                // caused the refusal is still there on the next tick, where the
                // read pass sees it and never queues this tenant at all.
                Err(WardenError::Cluster {
                    reason: RECREATE_REFUSED,
                }) => summary.skipped_foreign.push(label),
                // This tenant stopped being a shape to converge between the
                // read pass and the write - almost always a cancellation that
                // landed in the gap, which `reconcile` refuses at its own read
                // of the marker and again if the workload disappears under it.
                //
                // A SKIP and not a halt, and the distinction is a page nobody
                // should get: somebody closing their account while a roll is in
                // flight is the system working, and reporting it as "the fleet
                // roll halted" would put an operator in front of a log at
                // midnight to discover exactly that. Nothing was applied that
                // matters, the next tick's read pass files the tenant as
                // inactive without ever queueing it, and the run says so here.
                //
                // Both words, and the cancellation is the one that matters
                // here: `reconcile` asks the marker three times - once on the
                // way in, once if the workload vanishes under the apply, and
                // once if it vanishes under the rollout wait, which is a whole
                // `ready_timeout` wide - and a DELETE landing in any of the
                // three answers `Cancelled`. `NotReconcilable` is the same
                // shape of event without the intent: a tenant that stopped
                // being a workload between the read pass and the write.
                Err(WardenError::Cancelled | WardenError::NotReconcilable) => {
                    tracing::info!(
                        tenant = %first,
                        "the tenant this run took is no longer a shape to converge; skipping it"
                    );
                    summary.skipped_inactive.push(label);
                }
                // The reconcile already logged what failed and why, through
                // `fail`. What this adds is that this tenant is going back on
                // the queue rather than being counted as done, which is the
                // part an operator has to see.
                //
                // It stays OUT of `remaining`, which counts the queue behind
                // it: [`Rolled::tally`] counts the halted tenant through
                // `halted_on`, and putting it in both would make `checked`
                // exceed the fleet.
                Err(_) => {
                    tracing::error!(
                        tenant = %first,
                        remaining = summary.remaining,
                        "fleet roll halted on the tenant it took; nothing else was touched"
                    );
                    summary.halted_on = Some(label);
                }
            }
        }

        summary.tally();
        tracing::info!(
            checked = summary.checked,
            fleet = fleet.len(),
            rolled = summary.rolled.len(),
            remaining = summary.remaining,
            current = summary.current,
            skipped_foreign = summary.skipped_foreign.len(),
            skipped_inactive = summary.skipped_inactive.len(),
            stranded = summary.stranded.len(),
            unrenderable = summary.unrenderable.len(),
            unreadable = summary.unreadable,
            halted = summary.halted_on.is_some(),
            casualty = summary.casualty.is_some(),
            dry_run,
            "fleet roll finished"
        );
        Ok(summary)
    }

    /// The read pass: every tenant's verdict, and nothing written anywhere.
    ///
    /// Returns the summary with every bucket but [`Rolled::rolled`] already
    /// filled, plus the tenants the write pass is to roll, in fleet order.
    ///
    /// A stop - a casualty, or a tenant that could not be read - returns an
    /// EMPTY queue along with the halted summary, which is what makes "applies
    /// nothing" a property of the shape here rather than of a flag the write
    /// pass has to remember to check. The tenants it had already marked are
    /// counted into [`Rolled::remaining`] on the way out, because the read pass
    /// DID reach a verdict on them and [`Rolled::checked`] is a sum of verdicts:
    /// dropping them would report a three-tenant fleet as one checked, and the
    /// one number in the summary an operator can add up would be the one number
    /// that lies.
    async fn read_pass<'a>(&self, fleet: &'a [TenantName]) -> (Rolled, Vec<&'a TenantName>) {
        let mut summary = Rolled::default();
        let mut queue: Vec<&TenantName> = Vec::new();
        for name in fleet {
            let label = name.as_str().to_string();
            match self.inspect(name).await {
                Ok(Step::Roll) => queue.push(name),
                Ok(Step::Current) => summary.current += 1,
                Ok(Step::Foreign) => summary.skipped_foreign.push(label),
                Ok(Step::Inactive) => summary.skipped_inactive.push(label),
                Ok(Step::Stranded) => summary.stranded.push(label),
                Ok(Step::Unrenderable) => summary.unrenderable.push(label),
                Ok(Step::Casualty) => {
                    tracing::error!(
                        tenant = %name,
                        "this tenant is on today's render and is not serving it; \
                         rolling anything else would hand the same render to the next mailbox"
                    );
                    summary.casualty = Some(label.clone());
                    summary.halted_on = Some(label);
                    summary.remaining = queue.len();
                    return (summary, Vec::new());
                }
                // Already logged with its machine reason. A tenant this warden
                // could not inspect is not one it may walk past: the next one
                // would be rolled on the strength of a cluster that has just
                // stopped answering.
                Err(_) => {
                    tracing::error!(
                        tenant = %name,
                        "fleet roll halted while reading the fleet; nothing was applied"
                    );
                    summary.halted_on = Some(label);
                    summary.remaining = queue.len();
                    return (summary, Vec::new());
                }
            }
        }
        (summary, queue)
    }

    /// One tenant's verdict, read-only. The decisions and their reasons are
    /// documented on [`Warden::roll`]; this is the order the two reads are made
    /// in, which is cheapest first: the status word, then the drift report.
    async fn inspect(&self, name: &TenantName) -> Result<Step, WardenError> {
        // Intent before shape, and ahead of the status word, exactly as
        // `reconcile` orders it - a closed account is skipped whatever is still
        // standing.
        //
        // Not just the workload-less ones, and that is the whole reason this
        // read is up here. `delete` stops at its first error, so a cancellation
        // that failed on the Ingress leaves a tenant fully ACTIVE, which drifts
        // like any other tenant and would be queued like any other tenant. The
        // write pass would then refuse it - `reconcile` asks the marker - and
        // the run would halt on it. Every tick. First in the queue every time,
        // refused every time, and nothing else in the fleet ever converged: one
        // account somebody closed would stop the roller permanently.
        //
        // A tenant whose identity Secret has gone between the fleet listing and
        // this read is the same answer, for [`Warden::cancelled`]'s reason: the
        // absence of the one record `delete` deliberately keeps is a harder
        // teardown than a cancellation, not a softer one.
        let Some(identity) = self.identity(name).await? else {
            return Ok(Step::Inactive);
        };
        if is_cancelled(&identity) {
            return Ok(Step::Inactive);
        }
        // ONE read of the Deployment for both the status word and the drift
        // report below. This runs over every tenant on every tick, and it is
        // also the only way one tenant's verdict is reached from one view of it:
        // two reads with a delete between them produce a status that says there
        // is a workload and a report that says there is not.
        let (live, status) = self.workload(name).await?;
        let status = match status {
            status @ (TenantStatus::Active | TenantStatus::Failed) => status,
            // A signup that never reached phase two: no workload has ever
            // existed here, so there is nothing to be down.
            TenantStatus::Pending => return Ok(Step::Inactive),
            TenantStatus::Stopped => return self.workloadless(name).await,
        };

        let report = match self.drift_of(name, status, live).await {
            Ok(report) => report,
            // A tenant this warden can see and can NEVER render: the workload
            // is there and the Secret its render is built from is not. Every
            // other error out of a read is the cluster failing to answer, which
            // is a reason to stop the whole run - but this one is a fact about
            // one tenant that no number of retries changes, and halting on it
            // would park the fleet behind it forever, in sorted order, with
            // every tenant after it never converged again.
            Err(WardenError::Cluster {
                reason: CREDENTIAL_MISSING,
            }) => {
                tracing::error!(
                    tenant = %name,
                    "a workload with no sealed credential behind it; no run can render \
                     this tenant until a person puts one back"
                );
                return Ok(Step::Unrenderable);
            }
            Err(e) => return Err(e),
        };
        let Some(step) = verdict_of(status, &report) else {
            return self.workloadless(name).await;
        };
        match step {
            // COUNT, not names, for the reason `reconcile` gives: a
            // `fieldManager` is chosen by whoever wrote the field and this
            // formatter writes one line per event. The names are in the drift
            // report, which is structured and escaped.
            Step::Foreign => tracing::warn!(
                tenant = %name,
                managers = report.foreign.len(),
                "skipping a tenant another field manager owns fields on; repairing it \
                 means deleting its workload, which is a decision for a person"
            ),
            Step::Roll => tracing::info!(
                tenant = %name,
                changes = report.changes.len(),
                status = status.as_str(),
                "a tenant behind today's render"
            ),
            _ => {}
        }
        Ok(step)
    }

    /// A tenant with no workload at all, and what to make of it.
    ///
    /// An account somebody closed cannot arrive here with its marker on:
    /// [`Warden::inspect`] reads the identity Secret before it reads a status,
    /// and a tenant whose identity Secret has gone is caught by the same read.
    /// What is left is a volume and a credential still standing over a missing
    /// Deployment, which is a mailbox that is DOWN - [`Rolled::stranded`], named
    /// for a person.
    ///
    /// Except for the one tenant that shape cannot be told from: an account
    /// cancelled before the marker existed. That is
    /// [`Warden::torn_down_before_the_marker`]'s question, it costs one GET on
    /// this path alone, and getting it wrong here is not a miscount - the
    /// summary would name a closed account as a mailbox that is down, exit 1
    /// over it every fifteen minutes, and send an operator to
    /// `squelch-control reconcile <label>`, which is the call that would put it
    /// back on the internet.
    async fn workloadless(&self, name: &TenantName) -> Result<Step, WardenError> {
        if self.torn_down_before_the_marker(name).await? {
            tracing::info!(
                tenant = %name,
                "no workload, no route and no marker; reading it as a cancellation that \
                 predates the marker rather than as a mailbox that is down"
            );
            return Ok(Step::Inactive);
        }
        tracing::warn!(
            tenant = %name,
            "a tenant with no workload and no cancellation on record; \
             a job that did not finish left it down"
        );
        Ok(Step::Stranded)
    }

    /// The tenant's identity Secret, if phase one ever ran.
    async fn identity(&self, name: &TenantName) -> Result<Option<Secret>, WardenError> {
        self.cluster
            .get_secret(&name.identity_secret())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))
    }

    /// Whether the account behind this tenant is CLOSED, as
    /// [`objects::CANCELLED_AT_ANNOTATION`] records it.
    ///
    /// For the callers that have not already read the identity Secret for
    /// something else. Every one of them is deciding whether it may put a
    /// workload up, or whether there is a repair here worth naming to a person,
    /// and both of those answer "no" to a closed account.
    ///
    /// A MISSING identity Secret counts as closed, which is the one part of
    /// this that is a judgement rather than a lookup. It is the record
    /// [`Warden::delete`] deliberately keeps, so its absence is a harder
    /// teardown than a cancellation and not a softer one: the tenant cannot be
    /// reconciled, reopened, or addressed at all. Reading it the other way
    /// would make the total absence of a record the one state that licenses a
    /// rebuild.
    async fn cancelled(&self, name: &TenantName) -> Result<bool, WardenError> {
        Ok(self
            .identity(name)
            .await?
            .is_none_or(|secret| is_cancelled(&secret)))
    }

    /// The idempotent-retry path of phase one.
    /// The age recipient of a tenant that ALREADY EXISTS, for the control plane
    /// to seal a REPLACEMENT credential to.
    ///
    /// [`Warden::create_tenant`] deliberately cannot answer this: it 409s for
    /// anything past `pending`, because a second POST for a serving tenant is
    /// somebody claiming a taken subdomain. That is the right rule for signup
    /// and the wrong one for a re-consent, which is a request about a mailbox
    /// the caller has just proved ownership of.
    ///
    /// `account_email` is REQUIRED and matched, so this route cannot be used to
    /// enumerate recipients or to aim a credential at somebody else's mailbox.
    /// The control plane checks the same thing from its own store first; this
    /// is the second check, made by a different service from a different
    /// record, and it is the one that is authoritative about custody.
    ///
    /// Returning the recipient is safe by construction: it is the PUBLIC half
    /// of the tenant's identity. The private half never leaves the Secret this
    /// reads, and a recipient is exactly what the control plane is already
    /// handed at signup.
    pub async fn recipient_for(
        &self,
        raw_label: &str,
        raw_email: &str,
    ) -> Result<Created, WardenError> {
        let name = TenantName::parse(raw_label)?;
        let account_email = validate::validate_account_email(raw_email)?;

        let Some(existing) = self.identity(&name).await? else {
            return Err(WardenError::NotFound);
        };
        let stored_email = secret_value(&existing, objects::ACCOUNT_EMAIL_KEY);
        let recipient = secret_value(&existing, objects::RECIPIENT_KEY);
        match (stored_email, recipient) {
            (Some(stored), Some(recipient)) if stored == account_email => {
                tracing::info!(tenant = %name, "re-issued the recipient for a re-consent");
                Ok(Created { recipient })
            }
            // A DIFFERENT mailbox, or a Secret this warden did not write. Both
            // are 409 and not 404: the tenant exists, and which of the two it
            // was is not something a caller gets to distinguish.
            _ => Err(WardenError::Conflict),
        }
    }

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

    /// The hash of this tenant's stored share token, or `None` when it has
    /// none. The mirror of [`Self::llm_hash`], and simpler: one data key, so
    /// the hash is over the value itself rather than over a pair.
    async fn share_hash(&self, name: &TenantName) -> Result<Option<String>, WardenError> {
        Ok(self
            .cluster
            .get_secret(&name.control_secret())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?
            .as_ref()
            .and_then(|secret| secret_value(secret, objects::SHARE_TOKEN_KEY))
            .as_deref()
            .map(objects::credential_hash))
    }

    /// Derive the status from what exists. See [`TenantStatus`].
    async fn status_of(&self, name: &TenantName) -> Result<TenantStatus, WardenError> {
        Ok(self.workload(name).await?.1)
    }

    /// The tenant's Deployment and the status word it implies, from one read of
    /// it.
    ///
    /// [`Warden::status_of`] is still the single definition of what the four
    /// words mean; this is that definition plus the object it was derived from,
    /// for the callers that need both. Handing back the Deployment is what lets
    /// [`Warden::inspect`] and [`Warden::drift`] reach a verdict without a
    /// second GET of an object they are already holding.
    async fn workload(
        &self,
        name: &TenantName,
    ) -> Result<(Option<Deployment>, TenantStatus), WardenError> {
        let deployment = self
            .cluster
            .get_deployment(name.as_str())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?;

        if let Some(deployment) = deployment {
            let ready = deployment
                .status
                .as_ref()
                .and_then(|s| s.ready_replicas)
                .unwrap_or(0);
            let status = if ready >= 1 {
                TenantStatus::Active
            } else {
                TenantStatus::Failed
            };
            return Ok((Some(deployment), status));
        }
        // No workload. Whether that is "never started" or "deleted" is the
        // difference between a signup to finish and an account to reopen, and
        // the sealed credential is what tells them apart.
        let sealed = self
            .cluster
            .get_secret(&name.credential_secret())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?;
        Ok((
            None,
            if sealed.is_some() {
                TenantStatus::Stopped
            } else {
                TenantStatus::Pending
            },
        ))
    }

    /// Whether a workload-less tenant that carries NO cancellation marker was
    /// torn down by a `delete` that ran before the marker existed.
    ///
    /// A MIGRATION BRIDGE, and the last place in this file that reads intent out
    /// of shape. Every tenant cancelled from now on carries
    /// [`objects::CANCELLED_AT_ANNOTATION`], because [`Warden::delete`] writes
    /// it before it removes anything and refuses to remove anything if it
    /// cannot. Tenants cancelled by the warden this one replaces carry nothing:
    /// their identity Secret is intact, their credential is sealed, their
    /// Deployment is gone, and they are byte-for-byte indistinguishable from a
    /// reconcile that died in its own delete-recreate window. Without this, the
    /// first tick after the deploy reads every one of them as a job to finish
    /// and puts a closed mailbox back on the internet on its stored credential.
    ///
    /// The signal is the one the old warden used, and it is sound in the
    /// direction that refuses:
    ///
    /// - Every path that puts a workload up applies the SERVICE FIRST -
    ///   [`Warden::set_credentials`] and [`Warden::reconcile`] both - so a call
    ///   that died anywhere near the Deployment left the Service standing.
    /// - Every teardown takes the Service BEFORE the Deployment, in both the old
    ///   order and the current one, so a teardown that got as far as removing
    ///   the workload had already removed the Service.
    ///
    /// So a Service standing over a missing Deployment is a job to finish, and
    /// its ABSENCE is a teardown - which is the reading this returns. The
    /// Ingress would not do: it is the object an operator is most likely to have
    /// applied by hand, and a hand-applied Ingress must not read as consent to
    /// restart a cancelled mailbox.
    ///
    /// Delete this, its `get` on services in `deploy/hosted/10-warden-rbac.yaml`
    /// and [`Cluster::get_service`] together, once no tenant cancelled by the
    /// old warden is left. Until then it costs one GET on the one path that
    /// reaches it: a tenant with no workload and no marker.
    async fn torn_down_before_the_marker(&self, name: &TenantName) -> Result<bool, WardenError> {
        Ok(self
            .cluster
            .get_service(name.as_str())
            .await
            .map_err(|e| fail(name.as_str(), "cluster_unavailable", &e))?
            .is_none())
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

    /// A phase two that REPLACES a workload waits for the rollout before it
    /// takes a pod to exec in, and a phase two that creates one does not.
    ///
    /// `Recreate` keeps the old pod Ready, and in this tenant's own selector,
    /// until it terminates - so "the first Ready pod" can be the pod this apply
    /// just condemned, and `squelchd pair` execed into it writes a live pairing
    /// code into a container the kubelet is killing. The handoff goes with it.
    ///
    /// A fresh signup has no old pod to be confused with, and making every new
    /// tenant wait out `minReadySeconds` as well would spend a good part of the
    /// deadline on a race it cannot lose.
    #[tokio::test]
    async fn a_re_consent_waits_for_the_rollout_and_a_first_provision_does_not() {
        // Fresh: the controller never observes the new spec, and the signup
        // still completes, because nothing here is waiting on it.
        let h = Harness::new();
        h.cluster.rollout_hangs();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        assert!(
            h.warden
                .set_credentials("alice", &armored("first"))
                .await
                .is_ok()
        );

        // Replacing: the same hung rollout is now a refusal, because the answer
        // this route gives is a pairing code and the pod it came from has to be
        // the one that survives.
        let h = Harness::new();
        h.warden
            .create_tenant("bob", "bob@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("bob", &armored("first"))
            .await
            .unwrap();
        stop_serving(&h, "bob").await;
        h.cluster.rollout_hangs();
        assert_eq!(
            h.warden
                .set_credentials("bob", &armored("second"))
                .await
                .unwrap_err(),
            WardenError::cluster("not_ready")
        );
    }

    /// A reopen that fails partway leaves the account CLOSED.
    ///
    /// The marker is the account holder's decision, and the two orders this
    /// call could clear it in fail in opposite directions. Cleared last - what
    /// this asserts - a failed reopen leaves a tenant every reader still
    /// refuses, and the retry is this same call, which is exempt from the 409
    /// for exactly that reason. Cleared FIRST, the same failure leaves a
    /// mailbox up and serving on the credential its owner cancelled with
    /// nothing on record saying anybody cancelled it, and the next roll
    /// converges it like any other tenant.
    #[tokio::test]
    async fn a_reopen_that_does_not_finish_leaves_the_account_closed() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("first"))
            .await
            .unwrap();
        // The teardown that failed on the Ingress: closed account, Deployment
        // still up. The state the reopen exemption exists for.
        h.cluster.fail_delete_of(Kind::Ingress);
        assert!(h.warden.delete("alice").await.is_err());
        assert!(is_cancelled(&h.cluster.secret("alice-identity").unwrap()));

        // The reopen gets as far as applying the new render and no further.
        h.cluster.never_ready();
        assert_eq!(
            h.warden
                .set_credentials("alice", &armored("second"))
                .await
                .unwrap_err(),
            WardenError::cluster("not_ready")
        );
        assert!(
            is_cancelled(&h.cluster.secret("alice-identity").unwrap()),
            "a reopen that did not finish took the cancellation off the record"
        );
        // So nothing else will touch it, and the roll will not converge it.
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::Cancelled
        );

        // And the retry - this same call - is what finishes it.
        h.cluster.becomes_ready();
        assert!(
            h.warden
                .set_credentials("alice", &armored("second"))
                .await
                .is_ok()
        );
        assert!(!is_cancelled(&h.cluster.secret("alice-identity").unwrap()));
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

    // ---- the activation signal -------------------------------------------

    /// A tenant that is up and has been paired with: put through both phases,
    /// then asked. The argv is asserted STRING FOR STRING because it is a wire
    /// contract with the daemon's CLI, and the pod because a read aimed at the
    /// wrong one would answer somebody else's fact.
    #[tokio::test]
    async fn first_paired_reads_one_timestamp_out_of_the_tenants_pod() {
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
            .exec_prints(&crate::testing::first_paired_stdout("2026-03-01T09:30:00Z"));
        let at = h
            .warden
            .first_paired("alice")
            .await
            .unwrap()
            .expect("a client paired");
        assert_eq!(
            at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "2026-03-01T09:30:00Z"
        );

        let (pod, argv) = h.cluster.last_exec().unwrap();
        assert_eq!(pod, "alice-abc123");
        assert_eq!(
            argv,
            vec!["/usr/local/bin/squelchd", "token", "first-paired"],
            "the argv is a contract with the daemon's CLI"
        );
        // The command takes no label, so not even this tenant's name reaches
        // that command line.
        assert!(argv.iter().all(|arg| !arg.contains("alice")));
    }

    /// `none` is an ANSWER. A running tenant nobody has paired a client with
    /// reports exactly that, and it is not a failure and not a retry.
    #[tokio::test]
    async fn first_paired_answers_for_a_mailbox_nobody_has_paired_with() {
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
            .exec_prints(&crate::testing::first_paired_none_stdout());
        assert_eq!(h.warden.first_paired("alice").await.unwrap(), None);
    }

    /// An unknown label is the same 404 `status` gives, decided off the same
    /// identity Secret, and it never reaches a pod.
    #[tokio::test]
    async fn first_paired_404s_a_tenant_that_does_not_exist() {
        let h = Harness::new();
        assert_eq!(
            h.warden.first_paired("nobody").await.unwrap_err(),
            WardenError::NotFound
        );
        assert!(h.cluster.last_exec().is_none());
        // A label that is not a label is refused before the cluster is touched.
        assert!(matches!(
            h.warden.first_paired("-nope-").await.unwrap_err(),
            WardenError::InvalidLabel(_)
        ));
    }

    /// A tenant with no pod is NOT RUNNING, which is neither "no such tenant"
    /// nor a conflict: there is nothing to ask right now, and the caller is a
    /// poller whose next tick is the whole remedy. Both workload-less states
    /// answer it, and neither execs.
    #[tokio::test]
    async fn a_tenant_with_no_pod_is_not_running_rather_than_missing() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        // Pending: phase two never ran.
        assert_eq!(
            h.warden.first_paired("alice").await.unwrap_err(),
            WardenError::NotRunning
        );
        assert!(h.cluster.last_exec().is_none());

        // Stopped: the workload was taken down and the mail kept.
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();
        h.warden.delete("alice").await.unwrap();
        assert_eq!(
            h.warden.first_paired("alice").await.unwrap_err(),
            WardenError::NotRunning
        );
    }

    /// The state every daemon image from before this subcommand is in: the exec
    /// exits non-zero. A terse 500, which the control plane's poller eats and
    /// the next fleet roll fixes.
    #[tokio::test]
    async fn a_first_paired_exec_that_exits_non_zero_is_a_terse_500() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();

        h.cluster.exec_fails();
        assert_eq!(
            h.warden.first_paired("alice").await.unwrap_err(),
            WardenError::cluster("first_paired_failed")
        );
    }

    /// Output this warden cannot read is a FAILURE, never `None`. Reading it as
    /// "nobody has ever paired" would hand the control plane a fact the daemon
    /// never stated, and the control plane would store it.
    #[tokio::test]
    async fn unreadable_first_paired_output_is_never_half_an_answer() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();

        for garbage in [
            "error: unrecognized subcommand 'first-paired'\n",
            "2026-03-01\n",
            "",
        ] {
            h.cluster.exec_prints(garbage);
            assert_eq!(
                h.warden.first_paired("alice").await.unwrap_err(),
                WardenError::cluster("first_paired_unparsed"),
                "{garbage:?}"
            );
        }
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
                // Before the Deployment, so the endpoint drains before the pod
                // does. Nothing about telling a cancellation apart rides on
                // this order any more; the marker does that. See delete().
                (Kind::Service, "alice".to_string()),
                (Kind::Deployment, "alice".to_string()),
                (Kind::NetworkPolicy, "alice".to_string()),
                // The gateway credential goes with the workload; see delete().
                (Kind::Secret, "alice-llm".to_string()),
                // And the share token, for the same reason: a live bearer
                // against the control plane, not tenant data.
                (Kind::Secret, "alice-control".to_string()),
            ]
        );
        // The three that hold data or the mailbox's own credential are
        // untouched, by name: the only Secrets this path may delete are the
        // two that are credentials to something ELSE - the LLM gateway and the
        // control plane.
        assert!(h.cluster.secret("alice-identity").is_some());
        assert!(h.cluster.secret("alice-credential").is_some());
        assert!(h.cluster.exists(Kind::Pvc, "alice-data"));
        assert!(h.cluster.deleted().iter().all(|(kind, name)| match kind {
            Kind::Pvc => false,
            Kind::Secret => name == "alice-llm" || name == "alice-control",
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

    /// A pairing code is full access to a mailbox, so the two ways a tenant can
    /// have no pod are two different refusals - and only one of them is about
    /// the cluster.
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

        // Nothing running, and nobody cancelled anything: this cluster cannot
        // do the thing right now.
        h.cluster.delete(Kind::Deployment, "alice").await.unwrap();
        assert_eq!(
            h.warden.repair("alice").await.unwrap_err(),
            WardenError::cluster("no_ready_pod")
        );

        // Cancelled, and that is not a cluster problem at all.
        h.warden.delete("alice").await.unwrap();
        assert_eq!(
            h.warden.repair("alice").await.unwrap_err(),
            WardenError::Cancelled
        );
    }

    /// The state the marker exists for, on the route where getting it wrong is
    /// worst: a teardown that failed on the Ingress leaves the account closed,
    /// the Deployment up and a pod Ready. Every shape says "a running tenant",
    /// and minting here would hand a new device full access to a mailbox its
    /// owner already closed.
    #[tokio::test]
    async fn a_cancelled_account_that_is_still_up_cannot_be_paired() {
        let h = Harness::new();
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();

        h.cluster.fail_delete_of(Kind::Ingress);
        assert!(h.warden.delete("alice").await.is_err());
        // The workload really is still serving, so nothing about the SHAPE of
        // this tenant refuses the call.
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );
        assert_eq!(
            h.warden.repair("alice").await.unwrap_err(),
            WardenError::Cancelled
        );

        // And reopening it - the one exit a closed account has - restores the
        // route along with everything else.
        h.warden
            .set_credentials("alice", &armored("again"))
            .await
            .unwrap();
        assert!(h.warden.repair("alice").await.is_ok());
    }

    /// Same tenant, same reasoning, on the other route that touches a cancelled
    /// account's objects: an LLM key is a live gateway credential, `delete`
    /// removes it on purpose, and storing one back would both undo that and
    /// roll the pod of a mailbox nobody is entitled to run.
    #[tokio::test]
    async fn a_cancelled_account_takes_no_llm_key() {
        let h = Harness::with_config(crate::testing::llm_test_config());
        h.warden
            .create_tenant("alice", "alice@example.com")
            .await
            .unwrap();
        h.warden
            .set_credentials("alice", &armored("alice"))
            .await
            .unwrap();

        h.cluster.fail_delete_of(Kind::Ingress);
        assert!(h.warden.delete("alice").await.is_err());
        let applied = h.cluster.applied();
        assert_eq!(
            h.warden
                .set_llm_key("alice", Some("sk-live-key"), None)
                .await
                .unwrap_err(),
            WardenError::Cancelled
        );
        // Refused before anything was written: no Secret, and no roll.
        assert_eq!(h.cluster.applied(), applied);
        assert!(!h.cluster.exists(Kind::Secret, "alice-llm"));
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
        // deleted. (Carol's delete also took her `-llm` and `-control`
        // Secrets, which are the workload's credentials, not tenant records;
        // see `delete`.)
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
                .all(|(_, name)| name.ends_with("-identity")
                    || name.ends_with("-llm")
                    || name.ends_with("-control"))
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
        h.warden
            .set_llm_key("alice", Some("sk-vk-first"), None)
            .await
            .unwrap();

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
    /// as the only exit from a state this route created. The ABSENCE of
    /// [`objects::CANCELLED_AT_ANNOTATION`] is what tells the two apart:
    /// nothing here was a cancellation, so nothing here recorded one.
    #[tokio::test]
    async fn reconcile_finishes_a_job_that_did_not_finish() {
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

        // The wreckage: no Deployment, so the status word says `stopped`, and
        // nothing on the identity Secret says anybody cancelled anything.
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Stopped
        );
        assert!(!h.cluster.exists(Kind::Deployment, "alice"));
        assert!(!is_cancelled(&h.cluster.secret("alice-identity").unwrap()));

        // The pod lets go and the operator runs it again. Nothing foreign
        // survived the delete, so this is a plain apply rather than a purge.
        h.cluster.pods_release();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap().deployment,
            "created"
        );
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );
        let live = match h.cluster.object(Kind::Deployment, "alice").unwrap() {
            Object::Deployment(d) => *d,
            other => panic!("not a Deployment: {:?}", other.kind()),
        };
        assert!(drift::foreign_managers(&live).is_empty());
    }

    /// The invariant the cancellation marker exists for, held against a
    /// teardown that dies anywhere in the middle.
    ///
    /// `delete` stops at its first failure, so a cancellation can leave a
    /// tenant in ANY prefix of its teardown - including one where the
    /// Deployment is still running and serving mail, because the Ingress delete
    /// was the call that failed. Every one of those prefixes is a closed
    /// account, and the only thing that can say so is a record written before
    /// the first delete: the objects that survived cannot, because "Deployment
    /// present, Ingress gone" is also what an ordinary drifted tenant can look
    /// like, and a reconcile that read the wreckage would force-apply a
    /// cancelled mailbox straight back onto the internet.
    ///
    /// So this asserts the marker on every prefix, and then asserts the
    /// consequence on EVERY route that would otherwise act on the wreckage:
    /// each one refuses, and each one writes nothing. A guard that only
    /// `reconcile` holds is not a guard - `repair` would mint a live pairing
    /// code for the closed account, and `set_llm_key` would store a gateway
    /// credential against it and roll its pod.
    #[tokio::test]
    async fn every_prefix_of_a_failed_teardown_is_still_a_cancelled_account() {
        for failing in [
            Kind::Ingress,
            Kind::Service,
            Kind::Deployment,
            Kind::NetworkPolicy,
        ] {
            // With the gateway configured, so `set_llm_key` gets as far as the
            // marker instead of stopping at the feature gate.
            let h = Harness::with_config(crate::testing::llm_test_config());
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

            assert!(
                is_cancelled(&h.cluster.secret("alice-identity").unwrap()),
                "a teardown that died on {failing:?} left no record that it was a cancellation"
            );
            let applied = h.cluster.applied().len();
            assert_eq!(
                h.warden.reconcile("alice").await.unwrap_err(),
                WardenError::Cancelled,
                "a teardown that died on {failing:?} was reconcilable"
            );
            assert_eq!(
                h.warden.repair("alice").await.unwrap_err(),
                WardenError::Cancelled,
                "a teardown that died on {failing:?} would still mint a pairing code"
            );
            assert_eq!(
                h.warden
                    .set_llm_key("alice", Some("sk-live-key"), None)
                    .await
                    .unwrap_err(),
                WardenError::Cancelled,
                "a teardown that died on {failing:?} would still take an llm key"
            );
            assert_eq!(h.cluster.applied().len(), applied);
        }

        // The prefix worth calling out, because it is the one no arrangement of
        // deletes could have signalled: the FIRST delete failed, so every
        // object is still standing and the tenant is serving mail. Nothing
        // about its shape differs from a healthy tenant, and it is still
        // refused.
        let h = Harness::new();
        serving_tenant(&h, "alice").await;
        h.cluster.fail_delete_of(Kind::Ingress);
        assert!(h.warden.delete("alice").await.is_err());
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );
        assert!(h.cluster.exists(Kind::Deployment, "alice"));
        assert!(h.cluster.exists(Kind::Service, "alice"));
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::Cancelled
        );
        // And a roll walks past it rather than converging it, for the same
        // reason: it is an account somebody closed, not a shape to repair.
        age_the_render(&h, "alice").await;
        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.rolled, Vec::<String>::new());
        assert_eq!(rolled.skipped_inactive, vec!["alice"]);
    }

    /// The ordering `delete` is built on, stated as the thing that breaks
    /// without it: a teardown that cannot RECORD the cancellation does not
    /// start one.
    ///
    /// The alternative is a tenant with objects removed and no marker, which
    /// every reader here would take for a repair to finish. Refusing leaves a
    /// tenant fully up and a call the control plane can retry, which is the
    /// survivable half of the two.
    #[tokio::test]
    async fn a_teardown_that_cannot_record_the_cancellation_deletes_nothing() {
        let h = Harness::new();
        serving_tenant(&h, "alice").await;
        h.cluster.fail_annotate();

        assert!(h.warden.delete("alice").await.is_err());
        assert!(h.cluster.deleted().is_empty());
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );
        assert!(!is_cancelled(&h.cluster.secret("alice-identity").unwrap()));
    }

    /// The tenants this change inherits: cancelled by the warden this one
    /// replaces, so their identity Secret carries NO marker at all.
    ///
    /// Nothing about their shape distinguishes them from a reconcile that died
    /// in its own delete-recreate window - identity intact, credential sealed,
    /// no Deployment - and reading them as that is not a miscount but a
    /// resurrection: the route would rebuild a closed mailbox on its stored
    /// credential, and the roller would name it as DOWN and send an operator to
    /// the exact call that does it.
    ///
    /// The one thing that does distinguish them is what the OLD warden's
    /// teardown took with the workload. See
    /// [`Warden::torn_down_before_the_marker`].
    #[tokio::test]
    async fn a_cancellation_that_predates_the_marker_is_still_a_cancellation() {
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
        // Roll the clock back: this is the tenant as the old warden left it,
        // with the marker the new one writes taken off again.
        h.cluster
            .annotate_secret("alice-identity", objects::CANCELLED_AT_ANNOTATION, None)
            .await
            .unwrap();
        assert!(!is_cancelled(&h.cluster.secret("alice-identity").unwrap()));
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Stopped
        );

        let stopped = h.cluster.applied().len();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::Cancelled
        );
        assert_eq!(h.cluster.applied().len(), stopped);

        // And the roller files her under "nothing to converge" rather than
        // naming a closed account as a mailbox that is down.
        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.skipped_inactive, vec!["alice"]);
        assert_eq!(rolled.stranded, Vec::<String>::new());
        assert_eq!(rolled.checked, 1);
    }

    /// The other side of that bridge, and the reason it is the SERVICE it asks
    /// about: a reconcile that died in its own delete-recreate window has to
    /// stay finishable.
    ///
    /// Both tenants read `stopped` with no marker. The one whose Service is
    /// still standing was left that way by a call that applies the Service
    /// before it touches the Deployment, which is a job to finish; the one
    /// whose Service is gone was torn down by a `delete`, which took the
    /// Service first.
    #[tokio::test]
    async fn a_reconcile_that_died_in_its_own_window_is_still_finishable() {
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
        // The window: the Deployment is deleted and the pod will not let go of
        // the volume, so the reconcile refuses before it can apply a new one.
        h.cluster.pods_linger();
        assert!(h.warden.reconcile("alice").await.is_err());
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Stopped
        );
        assert!(!is_cancelled(&h.cluster.secret("alice-identity").unwrap()));
        // The Service the reconcile applied on its way in is still there, and
        // that is the whole difference from the tenant above.
        assert!(h.cluster.exists(Kind::Service, "alice"));

        h.cluster.pods_release();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap().deployment,
            "created"
        );
        assert!(h.cluster.exists(Kind::Deployment, "alice"));
    }

    /// Reopening is a re-consent, and it is the one path that takes the marker
    /// off. A tenant that has just been re-credentialed is not cancelled any
    /// more, and everything that refused it starts working again.
    #[tokio::test]
    async fn reopening_a_cancelled_account_clears_the_marker() {
        let h = Harness::new();
        serving_tenant(&h, "alice").await;
        h.warden.delete("alice").await.unwrap();
        assert!(is_cancelled(&h.cluster.secret("alice-identity").unwrap()));

        h.warden
            .set_credentials("alice", &armored("alice-again"))
            .await
            .unwrap();
        assert!(!is_cancelled(&h.cluster.secret("alice-identity").unwrap()));
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );

        // And it is an ordinary tenant again: reconcilable, and in the roll.
        h.warden.reconcile("alice").await.unwrap();
        age_the_render(&h, "alice").await;
        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.rolled, vec!["alice"]);
    }

    /// The state with no exit, and the exemption that gives it one.
    ///
    /// A teardown that failed on its FIRST delete leaves a closed account whose
    /// Deployment is still up. `reconcile` refuses it for being cancelled and
    /// the roll skips it for the same reason, so if the reopen path also
    /// refused it - `status` reads `active`, which normally means "already
    /// provisioned" - nothing in the service could move that tenant, and the
    /// mailbox would go on serving on a credential its owner had cancelled.
    #[tokio::test]
    async fn a_cancelled_account_can_be_reopened_even_while_its_pod_is_still_up() {
        let h = Harness::new();
        serving_tenant(&h, "alice").await;
        h.cluster.fail_delete_of(Kind::Ingress);
        assert!(h.warden.delete("alice").await.is_err());

        // Closed, and serving: the two readings this exemption is between.
        assert!(is_cancelled(&h.cluster.secret("alice-identity").unwrap()));
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );

        h.warden
            .set_credentials("alice", &armored("alice-again"))
            .await
            .unwrap();
        assert!(!is_cancelled(&h.cluster.secret("alice-identity").unwrap()));

        // And an ACTIVE tenant nobody cancelled still gets the 409, which is
        // the guard this exemption had to leave standing.
        assert_eq!(
            h.warden
                .set_credentials("alice", &armored("again-again"))
                .await
                .unwrap_err(),
            WardenError::Conflict
        );
    }

    /// A CANCELLED tenant stays refused, which is the distinction
    /// [`objects::CANCELLED_AT_ANNOTATION`] exists to draw: the account
    /// holder's decision is on the record, and no shape repair overrides one.
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
        assert!(is_cancelled(&h.cluster.secret("alice-identity").unwrap()));

        let stopped = h.cluster.applied().len();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::Cancelled
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
        //
        // `Cancelled` rather than `NotReconcilable`, and the two words are the
        // point of the pair above: pending is a signup nobody finished, and
        // this is an account somebody closed. Same 409, opposite next move.
        let stopped = h.cluster.applied().len();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::Cancelled
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
        assert_eq!(h.warden.fleet().await.unwrap(), (Vec::new(), 0));

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
        let (fleet, unreadable) = h.warden.fleet().await.unwrap();
        assert_eq!(labels_of(&fleet), vec!["alice", "bob", "carol"]);
        assert_eq!(unreadable, 0);
    }

    /// The pass that closes the gap write-once provisioning leaves, and the
    /// pacing that stops it closing the gap on the whole fleet at once.
    ///
    /// Two tenants are a render behind and one is not. The FIRST in fleet order
    /// is converged, the other is counted as remaining and not written at all,
    /// and it takes a second run to reach it. That second tenant staying
    /// untouched is the guarantee the whole design rests on: an apply against
    /// it here would be a fleet rolling at machine speed on a render nothing
    /// has confirmed yet.
    #[tokio::test]
    async fn a_roll_converges_one_drifted_tenant_per_run_in_fleet_order() {
        let h = Harness::new();
        for label in ["alice", "bob", "carol"] {
            serving_tenant(&h, label).await;
        }
        age_the_render(&h, "carol").await;
        age_the_render(&h, "alice").await;
        let before = h.cluster.applied().len();

        let first = h.warden.roll(false).await.unwrap();
        assert_eq!(first.checked, 3);
        assert_eq!(first.rolled, vec!["alice"]);
        assert_eq!(first.remaining, 1);
        assert_eq!(first.current, 1);
        assert_eq!(first.skipped_foreign, Vec::<String>::new());
        assert_eq!(first.skipped_inactive, Vec::<String>::new());
        assert_eq!(first.halted_on, None);

        // Alice's five objects and nobody else's. Not Bob, who was already on
        // today's render, and not Carol, whose turn is the next run. No delete
        // either: with no foreign owner this is the converged path.
        assert_eq!(
            h.cluster.applied()[before..].to_vec(),
            workload_applies_for("alice")
        );
        assert!(h.cluster.deleted().is_empty());
        assert_eq!(daemon_image(&h, "alice"), h.config.image);
        assert_eq!(daemon_image(&h, "carol"), PREVIOUS_IMAGE);

        // The next tick takes the next one, having re-read the whole fleet.
        let before = h.cluster.applied().len();
        let second = h.warden.roll(false).await.unwrap();
        assert_eq!(second.rolled, vec!["carol"]);
        assert_eq!(second.remaining, 0);
        assert_eq!(second.current, 2);
        assert_eq!(second.halted_on, None);
        assert_eq!(
            h.cluster.applied()[before..].to_vec(),
            workload_applies_for("carol")
        );
        for label in ["alice", "bob", "carol"] {
            assert_eq!(daemon_image(&h, label), h.config.image);
            assert_eq!(h.warden.status(label).await.unwrap(), TenantStatus::Active);
        }

        // And the fleet is converged, so a third pass is all reads.
        let applied = h.cluster.applied().len();
        let third = h.warden.roll(false).await.unwrap();
        assert_eq!(third.current, 3);
        assert_eq!(third.rolled, Vec::<String>::new());
        assert_eq!(third.remaining, 0);
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
        // The two behind it are NAMED as still to do rather than dropped, which
        // is what makes the summary add up to the fleet it walked: one halted,
        // two waiting, three checked.
        assert_eq!(rolled.remaining, 2);
        assert_eq!(rolled.checked, 3);

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

    /// Take a tenant's pod away without moving its render: the Deployment is
    /// applied again exactly as it stands, while nothing comes up.
    ///
    /// The shape a tenant is left in by a roll that handed it a render it
    /// cannot serve, and the reason the halt has to survive the end of a run:
    /// this tenant now CARRIES today's render, so an apply would change nothing
    /// about it and its drift report is clean.
    ///
    /// The cluster is put back afterwards, so the tenants around it would roll
    /// perfectly well if the run were willing to reach them.
    async fn stop_serving(h: &Harness, label: &str) {
        let Some(deployment) = h.cluster.object(Kind::Deployment, label) else {
            panic!("no deployment for {label}");
        };
        h.cluster.never_ready();
        h.cluster.apply(deployment).await.unwrap();
        h.cluster.becomes_ready();
    }

    /// The property the two-pass shape exists for, and the one a per-run halt
    /// cannot provide on its own.
    ///
    /// `reconcile` applies the render and THEN waits for the rollout, so a
    /// tenant whose rollout never finished has already received it: its live
    /// spec is what the warden renders, its drift is clean, and a roll that
    /// only asked "is anything different" would call it current, walk past it,
    /// and hand the same render to the next mailbox. Once per tick, until the
    /// fleet is dark and the last run exits converged.
    #[tokio::test]
    async fn a_tenant_that_took_a_bad_render_stops_every_run_after_it() {
        let h = Harness::new();
        for label in ["alice", "bob", "carol"] {
            serving_tenant(&h, label).await;
            age_the_render(&h, label).await;
        }
        // Today's render does not serve. The first run costs exactly one
        // tenant, which is the price this design accepts.
        h.cluster.never_ready();
        let first = h.warden.roll(false).await.unwrap();
        assert_eq!(first.halted_on, Some("alice".to_string()));
        assert_eq!(first.casualty, None);
        h.cluster.becomes_ready();

        // Alice IS today's render now: nothing to apply, and nothing serving.
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Failed
        );
        assert_eq!(h.warden.drift("alice").await.unwrap().changes, Vec::new());

        let applied = h.cluster.applied();
        let second = h.warden.roll(false).await.unwrap();
        assert_eq!(second.casualty, Some("alice".to_string()));
        assert_eq!(second.halted_on, Some("alice".to_string()));
        assert_eq!(second.rolled, Vec::<String>::new());
        assert_eq!(second.current, 0);
        // One verdict, and it was a stop.
        assert_eq!(second.checked, 1);

        // Not one write, anywhere in the fleet. Bob and Carol are still on the
        // render they were serving before any of this started.
        assert_eq!(h.cluster.applied(), applied);
        assert!(h.cluster.deleted().is_empty());
        for label in ["bob", "carol"] {
            assert_eq!(daemon_image(&h, label), PREVIOUS_IMAGE);
            assert_eq!(h.warden.status(label).await.unwrap(), TenantStatus::Active);
        }
    }

    /// And the read pass covers the WHOLE fleet before the write pass touches
    /// any of it, so a casualty at the end of the alphabet blocks the run as
    /// surely as one at the start. Rolling the tenants before it and stopping
    /// there would be handing out the render already under suspicion.
    #[tokio::test]
    async fn a_casualty_anywhere_in_the_fleet_blocks_the_whole_run() {
        let h = Harness::new();
        for label in ["alice", "bob", "carol"] {
            serving_tenant(&h, label).await;
        }
        age_the_render(&h, "alice").await;
        age_the_render(&h, "bob").await;
        stop_serving(&h, "carol").await;
        let applied = h.cluster.applied();

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.casualty, Some("carol".to_string()));
        assert_eq!(rolled.halted_on, Some("carol".to_string()));
        assert_eq!(rolled.rolled, Vec::<String>::new());
        assert_eq!(h.cluster.applied(), applied);
        for label in ["alice", "bob"] {
            assert_eq!(daemon_image(&h, label), PREVIOUS_IMAGE);
        }
        // And the summary adds up. Alice and bob were both marked for a roll
        // before the walk reached carol, so they are verdicts this run reached
        // and `checked` has to count them: a three-tenant fleet reported as one
        // checked would make the one number an operator can add up the one
        // number that lies about how far the run got.
        assert_eq!(rolled.remaining, 2);
        assert_eq!(rolled.checked, 3);

        // A dry run is the same refusal: the read pass is the whole of it.
        let dry = h.warden.roll(true).await.unwrap();
        assert_eq!(dry.casualty, Some("carol".to_string()));
        assert_eq!(dry.rolled, Vec::<String>::new());
        assert_eq!(dry.remaining, 2);
        assert_eq!(dry.checked, 3);
    }

    /// The other half of the casualty rule, and the case this whole feature was
    /// built for: a tenant that is FAILED and BEHIND is rolled. A pod wedged on
    /// somebody's Secret reference, or one a render back and crashlooping, is
    /// exactly the tenant a new render is likely to fix, and a roller that
    /// refused every failed tenant would refuse every incident.
    #[tokio::test]
    async fn a_failed_tenant_that_is_behind_is_still_rolled() {
        let h = Harness::new();
        serving_tenant(&h, "alice").await;
        // A render behind AND not serving: the shape a previous render broke.
        h.cluster.never_ready();
        age_the_render(&h, "alice").await;
        h.cluster.becomes_ready();
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Failed
        );

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.rolled, vec!["alice"]);
        assert_eq!(rolled.casualty, None);
        assert_eq!(rolled.halted_on, None);
        assert_eq!(daemon_image(&h, "alice"), h.config.image);
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );
    }

    /// The refusal that makes the foreign rule a property of the code rather
    /// than of the order two reads happened in.
    ///
    /// The roll's own drift read is several API calls before the read
    /// `reconcile` makes for itself, and a field manager can arrive in that
    /// gap. When it does, the tenant is SKIPPED - not deleted, not recreated,
    /// and not reported as rolled.
    ///
    /// The run then ENDS, rather than moving down the queue, because the budget
    /// this pacing spends is an attempt and not a success: the refusal came
    /// after this tenant's volume, policy and Service had already been applied.
    /// The second half of the test is the other side of that - the fleet does
    /// not stall on the skipped tenant, because the owner that caused the
    /// refusal is still there on the next tick, where the read pass sees it
    /// first and never queues her at all.
    #[tokio::test]
    async fn a_field_manager_that_arrives_mid_run_costs_a_skip_and_not_a_workload() {
        let h = Harness::new();
        for label in ["alice", "bob"] {
            serving_tenant(&h, label).await;
            age_the_render(&h, label).await;
        }
        let before = h.cluster.applied().len();
        // Alice reads clean and is queued; somebody edits her the moment the
        // write pass starts moving.
        h.cluster.foreign_arrives_on_next_apply("alice");

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.skipped_foreign, vec!["alice"]);
        assert_eq!(rolled.rolled, Vec::<String>::new());
        assert_eq!(rolled.remaining, 1);
        assert_eq!(rolled.halted_on, None);
        assert_eq!(rolled.checked, 2);

        // The refusal is at the write, so it stopped before the Deployment:
        // Alice keeps her workload, her render and her pod, and nothing was
        // deleted anywhere. Bob was not touched at all.
        assert_eq!(
            h.cluster.applied()[before..].to_vec(),
            vec![
                (Kind::Pvc, "alice-data".to_string()),
                (Kind::NetworkPolicy, "alice".to_string()),
                (Kind::Service, "alice".to_string()),
            ]
        );
        assert!(h.cluster.deleted().is_empty());
        assert_eq!(daemon_image(&h, "alice"), PREVIOUS_IMAGE);
        assert_eq!(daemon_image(&h, "bob"), PREVIOUS_IMAGE);
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );

        // The next tick: Alice's foreign owner is on her Deployment now, so the
        // READ pass skips her and Bob is first in the queue. One tick lost, no
        // tenant stranded.
        let before = h.cluster.applied().len();
        let second = h.warden.roll(false).await.unwrap();
        assert_eq!(second.skipped_foreign, vec!["alice"]);
        assert_eq!(second.rolled, vec!["bob"]);
        assert_eq!(second.remaining, 0);
        assert_eq!(
            h.cluster.applied()[before..].to_vec(),
            workload_applies_for("bob")
        );
        assert_eq!(daemon_image(&h, "bob"), h.config.image);
    }

    /// An account cancelled WHILE the run was rolling it. The reconcile refuses
    /// it - the marker goes on before the workload comes down, so the arm that
    /// finds the Deployment missing finds the marker too - and the run files it
    /// as inactive rather than as a halt.
    ///
    /// Which is the point of the test. A halt is `HALTED on <label>`, exit 1,
    /// and a person reading a log at midnight to discover that somebody closed
    /// their account. Nothing here is wrong, and the run has to be able to say
    /// so.
    #[tokio::test]
    async fn an_account_cancelled_mid_roll_is_a_skip_and_not_a_halt() {
        let h = Harness::new();
        for label in ["alice", "bob"] {
            serving_tenant(&h, label).await;
            age_the_render(&h, label).await;
        }
        // Alice reads clean and is queued; the DELETE lands the moment the
        // write pass starts moving.
        h.cluster.cancelled_arrives_on_next_apply("alice");

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.skipped_inactive, vec!["alice"]);
        assert_eq!(rolled.halted_on, None);
        assert_eq!(rolled.casualty, None);
        assert_eq!(rolled.rolled, Vec::<String>::new());
        assert_eq!(rolled.remaining, 1);
        assert_eq!(rolled.checked, 2);

        // Her workload was NOT rebuilt, which is the safety half: the reconcile
        // was mid-flight when the cancellation landed and it did not put the
        // mailbox back.
        assert!(!h.cluster.exists(Kind::Deployment, "alice"));

        // And the next tick never queues her at all, so this costs one tick and
        // not a stall.
        let second = h.warden.roll(false).await.unwrap();
        assert_eq!(second.skipped_inactive, vec!["alice"]);
        assert_eq!(second.rolled, vec!["bob"]);
        assert_eq!(second.halted_on, None);
    }

    /// The same cancellation, in the window that is a hundred times wider.
    ///
    /// `reconcile` applies everything and THEN waits for the rollout, and that
    /// wait is a whole `ready_timeout` - where the apply window above is a few
    /// API calls. A `DELETE` landing in it takes the Deployment out from under
    /// the wait, which the cluster answers with `NoPod`, which reads exactly
    /// like a render that could not come up. Reported that way it is a halt,
    /// exit 1, and the midnight page the test above exists to prevent - so the
    /// marker is re-read here too, and the answer is the same skip.
    #[tokio::test]
    async fn an_account_cancelled_during_the_rollout_wait_is_also_a_skip() {
        let h = Harness::new();
        for label in ["alice", "bob"] {
            serving_tenant(&h, label).await;
            age_the_render(&h, label).await;
        }
        h.cluster.cancelled_arrives_during_the_rollout("alice");

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.skipped_inactive, vec!["alice"]);
        assert_eq!(rolled.halted_on, None);
        assert_eq!(rolled.rolled, Vec::<String>::new());
        assert_eq!(rolled.remaining, 1);
        assert!(!h.cluster.exists(Kind::Deployment, "alice"));

        // A rollout that hangs for any OTHER reason is still a halt. The
        // distinction is the marker and nothing else about the shape: both
        // tenants end the run with no finished rollout behind them.
        let h = Harness::new();
        serving_tenant(&h, "alice").await;
        age_the_render(&h, "alice").await;
        h.cluster.rollout_hangs();
        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.halted_on, Some("alice".to_string()));
        assert_eq!(rolled.skipped_inactive, Vec::<String>::new());
    }

    /// The two entry points, on one hand-edited tenant. An unattended caller
    /// has no delete branch to reach at all; the operator's route still
    /// repairs the same tenant the only way server-side apply allows.
    #[tokio::test]
    async fn only_the_operators_entry_point_may_delete_a_workload() {
        let h = Harness::new();
        serving_tenant(&h, "alice").await;
        hand_edit_the_deployment(&h).await;

        assert_eq!(
            h.warden.reconcile_converging("alice").await.unwrap_err(),
            WardenError::cluster(RECREATE_REFUSED)
        );
        assert!(h.cluster.deleted().is_empty());
        assert!(h.cluster.exists(Kind::Deployment, "alice"));
        assert_eq!(
            h.warden.status("alice").await.unwrap(),
            TenantStatus::Active
        );
        let Some(Object::Deployment(alice)) = h.cluster.object(Kind::Deployment, "alice") else {
            panic!("no deployment");
        };
        assert_eq!(drift::foreign_managers(&alice).len(), 1);

        // Same tenant, same drift, a person asking: the purge happens.
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap().deployment,
            "recreated"
        );
        assert_eq!(
            h.cluster.deleted(),
            vec![(Kind::Deployment, "alice".to_string())]
        );
    }

    /// A tenant with no workload and no cancellation on record is DOWN, and it
    /// is not the same fact as a cancelled account. Both read `stopped`; the
    /// roll finishes neither, but it files them apart, and the marker is the
    /// only thing that can tell it which is which. One is a mailbox waiting for
    /// a person, the other is a tenant that is absent on purpose.
    #[tokio::test]
    async fn a_tenant_a_job_left_down_is_not_filed_as_inactive() {
        let h = Harness::new();
        for label in ["alice", "bob"] {
            serving_tenant(&h, label).await;
        }
        // Bob cancelled his account, which is on the record.
        h.warden.delete("bob").await.unwrap();
        // Alice's reconcile died between the delete and the apply. Same status
        // word, nothing recording a cancellation.
        hand_edit_the_deployment(&h).await;
        h.cluster.pods_linger();
        assert_eq!(
            h.warden.reconcile("alice").await.unwrap_err(),
            WardenError::cluster("pods_not_gone")
        );
        h.cluster.pods_release();
        let applied = h.cluster.applied();

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.stranded, vec!["alice"]);
        assert_eq!(rolled.skipped_inactive, vec!["bob"]);
        assert_eq!(rolled.rolled, Vec::<String>::new());
        assert_eq!(rolled.halted_on, None);
        assert_eq!(rolled.checked, 2);

        // Named, not repaired: finishing somebody's half-done recreate
        // unattended is the same judgement call the foreign rule refuses.
        assert_eq!(h.cluster.applied(), applied);
        assert!(!h.cluster.exists(Kind::Deployment, "alice"));
    }

    /// A tenant this warden cannot READ stops the run in the READ pass, before
    /// anything is applied to anyone - and the summary still adds up, because
    /// [`Rolled::checked`] counts verdicts and the tenants already queued when
    /// the walk stopped were verdicts too.
    ///
    /// The read that fails is about one tenant, but what it says is that this
    /// cluster is not answering, and the next tenant would be rolled on the
    /// strength of that. So the run stops where it stands.
    #[tokio::test]
    async fn a_tenant_that_cannot_be_read_halts_the_run_before_the_write_pass() {
        let h = Harness::new();
        for label in ["alice", "bob", "carol", "dave"] {
            serving_tenant(&h, label).await;
        }
        // Alice is behind and would have been rolled; carol is behind and is
        // never reached. The API server stops answering about bob, between them.
        age_the_render(&h, "alice").await;
        age_the_render(&h, "carol").await;
        h.cluster.break_reads_for("bob");
        let applied = h.cluster.applied();

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.halted_on, Some("bob".to_string()));
        assert_eq!(rolled.casualty, None);
        assert_eq!(rolled.rolled, Vec::<String>::new());
        // Alice was queued before the walk stopped, and the count says so: one
        // queued, one halted on, and nothing else reached. A summary that
        // dropped alice would report a four-tenant fleet as one checked.
        assert_eq!(rolled.remaining, 1);
        assert_eq!(rolled.current, 0);
        assert_eq!(rolled.checked, 2);

        // Nothing was applied to anyone, alice included: the read pass covers
        // the whole fleet before the write pass touches one tenant.
        assert_eq!(h.cluster.applied(), applied);
        assert_eq!(daemon_image(&h, "alice"), PREVIOUS_IMAGE);
        assert_eq!(daemon_image(&h, "carol"), PREVIOUS_IMAGE);
    }

    /// The one per-tenant read failure that must NOT stop the run, because no
    /// number of retries changes it.
    ///
    /// A workload whose sealed credential Secret is gone cannot be rendered by
    /// this run or any other. Halting on it would park every tenant after it in
    /// fleet order behind a run that stops at the same label every fifteen
    /// minutes, forever - so it is named for a person and the walk goes on.
    #[tokio::test]
    async fn a_tenant_that_can_never_be_rendered_is_named_and_not_a_halt() {
        let h = Harness::new();
        for label in ["alice", "bob", "carol"] {
            serving_tenant(&h, label).await;
        }
        age_the_render(&h, "carol").await;
        // Bob's sealed credential is gone, so there is no render to compare
        // his workload with.
        let bob = TenantName::parse("bob").unwrap();
        h.cluster
            .delete(Kind::Secret, &bob.credential_secret())
            .await
            .unwrap();

        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.halted_on, None);
        assert_eq!(rolled.unrenderable, vec!["bob"]);
        assert_eq!(rolled.current, 1);
        // Carol is BEHIND bob in fleet order and still got rolled, which is the
        // whole point: one broken tenant does not park the fleet behind it.
        assert_eq!(rolled.rolled, vec!["carol"]);
        assert_eq!(rolled.checked, 3);
        assert_eq!(daemon_image(&h, "carol"), h.config.image);

        // And it is not a green run: a mailbox nobody can render is a fleet
        // that is not on today's render, permanently.
        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.unrenderable, vec!["bob"]);
        assert_eq!(rolled.current, 2);
        assert_eq!(rolled.rolled, Vec::<String>::new());
    }

    /// An identity Secret whose label will not parse is a tenant this warden
    /// can see and cannot address: it is in no roll, now or ever. Being counted
    /// is the whole of the fix, because the failure mode is silence.
    #[tokio::test]
    async fn an_identity_secret_with_an_unusable_label_is_counted_out_of_the_fleet() {
        let h = Harness::new();
        serving_tenant(&h, "alice").await;

        let name = TenantName::parse("alice").unwrap();
        let mut orphan = objects::identity_secret(
            &h.config,
            &name,
            &TenantIdentity::mint(),
            "alice@example.com",
            0,
        );
        orphan.metadata.name = Some("-nope-identity".to_string());
        h.cluster
            .apply(Object::Secret(Box::new(orphan)))
            .await
            .unwrap();

        // Out of the fleet, counted, and the tenants around it are not.
        let (fleet, unreadable) = h.warden.fleet().await.unwrap();
        assert_eq!(labels_of(&fleet), vec!["alice"]);
        assert_eq!(unreadable, 1);

        // And the count REACHES the summary. `checked` deliberately does not
        // include it - this is not a tenant the run checked - which is why it
        // needs a field of its own to be visible at all.
        let rolled = h.warden.roll(false).await.unwrap();
        assert_eq!(rolled.unreadable, 1);
        assert_eq!(rolled.checked, 1);
        assert_eq!(rolled.current, 1);
    }

    /// The read pass's whole rule, as the table it is. Two rows of it are the
    /// difference between a bad render costing one tenant and costing the
    /// fleet, and one is the difference between a mailbox that vanished and a
    /// mailbox that is fine.
    #[test]
    fn the_read_pass_rule_in_full() {
        use crate::drift::{FieldChange, ForeignManager};
        use TenantStatus::{Active, Failed};

        let editor = ForeignManager {
            manager: "kubectl-set".to_string(),
            operation: "Update".to_string(),
            paths: vec!["spec.template.spec.containers[squelchd].env".to_string()],
        };
        let behind = FieldChange {
            path: "spec.template.spec.containers[squelchd].image".to_string(),
            live: serde_json::Value::String("daemon-0.3.0".to_string()),
            rendered: serde_json::Value::String("daemon-0.4.0".to_string()),
        };
        let verdict = |status: TenantStatus, present: bool, foreign: bool, changes: bool| {
            verdict_of(
                status,
                &DriftReport {
                    status: status.as_str(),
                    deployment_present: present,
                    foreign: Vec::from_iter(foreign.then(|| editor.clone())),
                    changes: Vec::from_iter(changes.then(|| behind.clone())),
                },
            )
        };

        // Serving what the warden renders: the steady state, and no writes.
        assert_eq!(verdict(Active, true, false, false), Some(Step::Current));
        // Carrying what the warden renders and not serving it. The stop.
        assert_eq!(verdict(Failed, true, false, false), Some(Step::Casualty));
        // Behind, whatever the pod is doing. The incident this rolls.
        assert_eq!(verdict(Active, true, false, true), Some(Step::Roll));
        assert_eq!(verdict(Failed, true, false, true), Some(Step::Roll));
        // Somebody else owns part of it, so the render is not the suspect and
        // the workload is not this timer's to delete. Serving, or down and
        // still behind: either way this tenant is not evidence against today's
        // render, because it either has not got it or is running fine on it.
        assert_eq!(verdict(Active, true, true, false), Some(Step::Foreign));
        assert_eq!(verdict(Failed, true, true, true), Some(Step::Foreign));
        // But a foreign owner does NOT downgrade a casualty, and this row is
        // the reason the casualty rule is checked first. `kubectl rollout
        // restart` is the first thing anybody does to a mailbox that is down,
        // and it stamps a foreign field manager. Asking about owners first
        // would turn debugging the casualty into an ordinary skip, and the run
        // would go back to handing the suspect render to every tenant behind it
        // while reporting that nothing is wrong.
        assert_eq!(verdict(Failed, true, true, false), Some(Step::Casualty));
        // The workload went away between the two reads. An empty report is not
        // a clean one, and this tenant has no pod at all.
        assert_eq!(verdict(Active, false, false, false), None);
        assert_eq!(verdict(Failed, false, false, false), None);
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
