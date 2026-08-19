//! The client half of the control -> warden wire contract, v2.
//!
//! The warden is the only thing that touches the cluster: it mints the tenant's
//! age identity, writes it into that tenant's Secret, renders the typed k8s
//! objects, waits for the pod, and execs `squelchd pair` inside it. This module
//! knows none of that. It knows a handful of routes, one bearer, and the shapes
//! that come back.
//!
//! PROVISIONING IS TWO CALLS, and the split is the whole point of v2:
//!
//! 1. `POST /v1/tenants {label, account_email}` -> `201 {recipient}`. The warden
//!    mints an identity for THIS tenant and answers with its public half. The
//!    tenant is now `pending`: it holds no credential and runs nothing.
//! 2. `PUT /v1/tenants/{label}/credentials {cred_read_ciphertext}` -> `200`
//!    pairing. The warden installs the credential Secret, applies the workload,
//!    and mints the first pairing code.
//!
//! A failure between the two leaves a pending tenant, which is RETRIABLE by
//! design: a second call 1 for a pending label with the same `account_email`
//! answers 201 with the SAME recipient. A different account gets 409, which is
//! what stops a stranger from finishing somebody else's half-done signup.
//!
//! WHAT CROSSES THIS WIRE, and what deliberately does not: the tenant label, the
//! mailbox address, and age CIPHERTEXT. Never a refresh token, never an access
//! token, never an age identity (this process has none, and the recipient it
//! learns is a public key). The bearer is presented on every route and never
//! logged.
//!
//! The warden's answers are treated as UNTRUSTED INPUT even though the warden is
//! ours. Its `recipient` is what a live refresh token gets encrypted to, so it
//! is parsed as an age public key before it is used; its `pair_code` is rendered
//! into a page and its `deep_link` would become an `href`, so both are
//! shape-checked here and the URL the page shows is recomputed from this
//! deployment's own base domain rather than echoed.
//!
//! THE OPERATOR ROUTES ARE A SEPARATE SURFACE. `GET /v1/tenants/{label}/drift`
//! and `POST /v1/tenants/{label}/reconcile` answer a human at a shell, never a
//! signup: nothing in the serving path calls them, so they hang off
//! [`HttpWarden`] itself rather than the [`Warden`] trait, whose implementors
//! are otherwise standing in for a cluster in a test. Their answers get the same
//! treatment with a different destination in mind: a drift report quotes cluster
//! state back, some of it written by whoever caused the drift, and it lands in a
//! terminal that OBEYS what it is sent. Every string is [`declaw`]ed on arrival
//! and every value is printed as JSON.
//!
//! They also disagree about time, and that is the one place a budget is part of
//! this contract. `drift` is a read and finishes in a round trip. `reconcile`
//! can delete a Deployment and wait minutes for a replacement, and giving up on
//! it mid-flight stops the warden rather than merely losing the answer, so it
//! carries [`crate::config::RECONCILE_TIMEOUT`] instead of the client's.
//!
//! A Deployment spec holds no secret material — Secrets reach a tenant pod as
//! references by name — so quoting spec fields at an operator is safe. Nothing
//! here may be pointed at a Secret's contents.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Ceiling on a warden response body. Every answer in the contract is a few
/// hundred bytes; the credential ciphertext travels the other way.
const MAX_RESPONSE_BODY: usize = 64 * 1024;

/// Ceiling on a drift report, which is the one answer whose size is decided by
/// the tenant's own object rather than by this contract.
///
/// It has to sit ABOVE what a Deployment can hold, and deliberately so. The
/// report grows with the number of foreign-owned fields, and the principal who
/// can add those is the principal this whole feature exists to catch: a cap
/// that a padded object could cross would let them push their own tenant's
/// report over it and have the answer come back as "the warden is
/// unreachable", which reads as a network problem and not as a finding. A
/// Kubernetes object cannot exceed roughly 1.5 MB, so nothing that fits in the
/// cluster can fail to fit here.
const MAX_DRIFT_BODY: usize = 8 * 1024 * 1024;

/// Bounds on the pairing code the warden reports. Crockford `XXXX-XXXX`, which
/// is what `squelchd pair` prints.
const PAIR_CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A tenant the warden has recorded but not provisioned: call 1 landed, call 2
/// did not. The one status this crate treats as retriable.
pub const STATUS_PENDING: &str = "pending";

#[derive(Debug, thiserror::Error)]
pub enum WardenError {
    /// The warden could not be reached, or answered something unparseable.
    /// Everything a [`reqwest::Error`] can be EXCEPT a timeout, which is
    /// [`WardenError::TimedOut`] and says the opposite thing.
    #[error("the provisioning service could not be reached")]
    Unreachable,
    /// The request outlasted its budget. NOT [`WardenError::Unreachable`], and
    /// the distinction is the whole variant: "could not be reached" reads as
    /// NOTHING HAPPENED, and a timeout is the one failure where something did.
    ///
    /// A reconcile is delete, wait, apply, wait. When the client gives up
    /// partway the warden is not told; it is holding a half-finished
    /// destructive operation, and on 2026-08-19 an operator who read "could not
    /// be reached" walked away from a tenant whose Deployment had been deleted
    /// and not yet recreated. So this message says the warden may still be
    /// working and names the one place that can settle it.
    ///
    /// The label is interpolated because the check is useless without it, and
    /// it is safe to interpolate because every route validates it against
    /// [`crate::labels`] before the socket is opened.
    #[error(
        "the provisioning service did not answer in time and may still be working: check \
         `kubectl -n warden logs deploy/squelch-warden | grep \"tenant={label} \"` before \
         assuming nothing happened"
    )]
    TimedOut { label: String },
    /// 401/403. A deployment misconfiguration, and one worth shouting about:
    /// nothing will ever provision until it is fixed.
    #[error("the provisioning service refused our credentials")]
    Unauthorized,
    /// 409 on call 1: the label belongs to somebody else, or is already live.
    #[error("that address is already taken")]
    LabelTaken,
    /// 422: the warden refused the label. Should be unreachable (this crate
    /// validates first), so it means the two validators have drifted.
    #[error("that address was refused by the provisioning service")]
    LabelRefused,
    /// 404 on call 2: the pending tenant call 1 created is gone. A warden
    /// restart mid-signup, or an operator cleaning up.
    #[error("the provisioning service lost the half-finished mailbox")]
    UnknownTenant,
    /// 409 on call 2: this label already has a credential installed. Two tabs,
    /// or a retry that raced the original.
    #[error("that mailbox has already been set up")]
    AlreadyProvisioned,
    /// Any other non-success status.
    #[error("the provisioning service failed to set up the mailbox")]
    Failed,
    /// Call 1 answered 201 with something this crate will not encrypt a live
    /// refresh token to.
    #[error("the provisioning service answered with an unusable tenant key")]
    BadRecipient,
    /// Call 2 answered 200 with a body this crate will not render.
    #[error("the provisioning service answered with an unusable pairing code")]
    BadPairing,
    /// The caller handed this client something that is not age ciphertext. A
    /// bug, caught before the socket is opened rather than after the cluster has
    /// written a plaintext token into a Secret.
    #[error("refusing to send a credential that is not age ciphertext")]
    NotCiphertext,
    /// 503 on the llm-key route: the warden has no LLM gateway configured, so
    /// no key can be installed for anyone. Distinct from `Failed` because the
    /// remedy is deployment configuration, not a retry — with the trio set
    /// here and the gateway absent there, the two deployments disagree.
    #[error("the provisioning service has no LLM gateway configured")]
    LlmNotConfigured,
    /// The caller handed this client something it will not present as an API
    /// key. A bug on our side, caught before the socket is opened.
    #[error("refusing to send an unusable API key")]
    BadApiKey,
    /// 409 on reconcile: the tenant exists but has no workload to converge,
    /// because it is pending or stopped. Worth its own variant because the
    /// remedy is a different operation entirely — finish the signup, or start
    /// the tenant back up — and a retry will never do it.
    ///
    /// Its message is written for an OPERATOR, unlike the variants a signup page
    /// renders: this one is reachable from a shell and nowhere else.
    #[error("that tenant has no workload to reconcile: it is pending or stopped")]
    NotReconcilable,
}

/// `POST /v1/tenants` request body. NO CREDENTIAL: at this point in the flow the
/// recipient it would be sealed to does not exist yet.
#[derive(Debug, Serialize)]
struct CreateRequest<'a> {
    label: &'a str,
    account_email: &'a str,
}

/// `POST /v1/tenants` 201 body.
#[derive(Debug, Deserialize)]
pub struct Created {
    /// This tenant's age recipient (`age1...`), the public half of an identity
    /// that exists only inside the tenant's Secret. Validated on arrival.
    pub recipient: String,
}

/// `PUT /v1/tenants/{label}/credentials` request body.
#[derive(Debug, Serialize)]
struct CredentialsRequest<'a> {
    /// The tenant's credential, age ASCII armor. The field name says
    /// "ciphertext" because that is the only thing that may ever be put in it.
    ///
    /// The `read` in the name is the WIRE's, kept because the warden and every
    /// deployed control plane agree on it; what rides in it is the whole
    /// credentials file, which since hosted signup started asking for one
    /// consent covering readonly + modify + send carries BOTH slots (`email` and
    /// `email#write`). The warden writes the blob into the tenant's Secret
    /// verbatim and never looks inside, so the name is stale, not wrong.
    cred_read_ciphertext: &'a str,
}

/// `PUT /v1/tenants/{label}/llm-key` request body. NO Debug derive, unlike its
/// siblings: both fields are the tenant's live LLM bearers. Each is serialized
/// only when present — a mint that half-failed installs the half that exists,
/// and a field that never rides the wire keeps it clean for wardens that
/// predate it (they ignore unknown fields, but hygiene is cheap).
#[derive(Serialize)]
struct LlmKeyRequest<'a> {
    /// The TRIAGE key.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<&'a str>,
    /// The ASSISTANT key.
    #[serde(skip_serializing_if = "Option::is_none")]
    assistant_api_key: Option<&'a str>,
}

/// `PUT /v1/tenants/{label}/credentials` 200 body, and the same shape
/// `POST /v1/tenants/{label}/pair` answers with.
#[derive(Debug, Deserialize)]
pub struct Pairing {
    pub pair_code: String,
    /// The warden's idea of the tenant's URL. Parsed for completeness and
    /// deliberately NOT rendered: [`crate::pages`] rebuilds it from this
    /// deployment's own base domain.
    pub pair_url: String,
    /// Likewise: never put in an `href`.
    pub deep_link: String,
}

/// `GET /v1/tenants/{label}` 200 body.
#[derive(Debug, Deserialize)]
pub struct TenantStatus {
    /// `pending` | `active` | `failed` | `stopped`.
    pub status: String,
}

impl TenantStatus {
    /// Whether this tenant is a half-finished signup that the SAME mailbox may
    /// come back and complete. Anything else (active, failed, stopped, or a
    /// status this crate does not know) is treated as taken, which is the
    /// conservative direction: it refuses a signup rather than inviting one that
    /// the warden will 409.
    pub fn is_pending(&self) -> bool {
        self.status == STATUS_PENDING
    }
}

/// `GET /v1/tenants/{label}/drift` 200 body: what is on a tenant's workload
/// that the warden did not put there, and what an apply of today's render would
/// change.
///
/// The two arrays answer INDEPENDENT questions and neither implies the other.
/// `foreign` comes from the API server's own record of which field manager owns
/// which field, and it is the one that catches a hand edit: server-side apply
/// owns fields, not objects, so a field the warden never declares is invisible
/// to every apply it will ever make. `changes` is what a re-apply would move,
/// which covers the fields the warden does declare.
#[derive(Debug, Clone, Deserialize)]
pub struct DriftReport {
    /// `pending` | `active` | `failed` | `stopped`.
    pub status: String,
    /// Whether a Deployment exists at all. False for a tenant that never
    /// provisioned and for one that was stopped; both arrays are empty there,
    /// because there is no object to have drifted.
    pub deployment_present: bool,
    /// Field managers other than the warden that own part of the workload.
    #[serde(default)]
    pub foreign: Vec<ForeignManager>,
    /// Fields an apply of today's render would move.
    #[serde(default)]
    pub changes: Vec<FieldChange>,
}

/// One other writer on a tenant's workload, and what it owns.
///
/// The presence of an entry IS the finding: any surviving path means somebody
/// took ownership of part of a tenant's Deployment, and the warden will
/// converge around that field forever rather than over it.
#[derive(Debug, Clone, Deserialize)]
pub struct ForeignManager {
    /// The `fieldManager` string the API server recorded (`kubectl-set`,
    /// `kubectl-edit`, a controller's name). Chosen by whoever wrote the field.
    pub manager: String,
    /// `Apply` or `Update`.
    pub operation: String,
    /// Dotted paths into the object, as the warden spelled them.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// One field an apply would move, with both sides of it.
#[derive(Debug, Clone, Deserialize)]
pub struct FieldChange {
    /// Dotted, rooted at `spec`, keyed list items in brackets:
    /// `spec.template.spec.containers[squelchd].image`.
    pub path: String,
    /// What the object carries now. `null` when the field exists only in the
    /// render.
    #[serde(default)]
    pub live: serde_json::Value,
    /// What an apply would leave. `null` when the field exists only live.
    #[serde(default)]
    pub rendered: serde_json::Value,
}

impl DriftReport {
    /// Whether the tenant is still exactly what the warden rendered. A tenant
    /// with no Deployment is clean by this measure: there is nothing there to
    /// have drifted, and its status is the thing to read instead.
    pub fn is_clean(&self) -> bool {
        self.foreign.is_empty() && self.changes.is_empty()
    }

    /// Hold every string in the report to what a terminal will merely print.
    fn declaw(&mut self) {
        self.status = declaw(&self.status);
        for f in &mut self.foreign {
            f.manager = declaw(&f.manager);
            f.operation = declaw(&f.operation);
            for path in &mut f.paths {
                *path = declaw(path);
            }
        }
        for c in &mut self.changes {
            c.path = declaw(&c.path);
        }
    }
}

/// `POST /v1/tenants/{label}/reconcile` 200 body: what the warden had to do to
/// put the tenant back onto its render.
#[derive(Debug, Clone, Deserialize)]
pub struct Reconciled {
    /// `created`, `converged`, or `recreated`. The last means the Deployment
    /// was deleted and applied fresh, because another field manager owned part
    /// of it and no apply can take a field back.
    pub deployment: String,
    /// `active`. A reconcile that answers at all has waited for a ready pod.
    pub status: String,
}

impl Reconciled {
    fn declaw(&mut self) {
        self.deployment = declaw(&self.deployment);
        self.status = declaw(&self.status);
    }
}

/// Neutralize a string that came off the wire and is headed for a terminal.
///
/// A `fieldManager` is named by whoever wrote the field, and a path can name a
/// container or an environment variable somebody else created — which is the
/// exact case a drift report exists to surface. A terminal OBEYS ESC sequences,
/// so a name carrying one could repaint the run, erase the finding above it, or
/// park the cursor somewhere else entirely; bidirectional overrides can reorder
/// a path so it reads as one thing and says another. Both classes are replaced
/// rather than dropped, so the operator sees that something was there.
///
/// The `live` and `rendered` values need no such pass: they are printed as JSON,
/// and JSON escaping is exactly this guarantee.
fn declaw(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            c if c.is_control() => REPLACEMENT,
            // The trojan-source set: bidi marks, embeddings, overrides, and
            // isolates. Printable by the letter of Unicode, and a lie by the
            // time a terminal has laid them out.
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                REPLACEMENT
            }
            c => c,
        })
        .collect()
}

/// What a declawed character becomes. U+FFFD, because "something unprintable
/// was here" is information the operator wants.
const REPLACEMENT: char = '\u{fffd}';

/// A warden the control plane can talk to. A trait so a test can stand one up
/// without a cluster, and so a later provisioning backend slots in without
/// touching the handlers.
#[async_trait::async_trait]
pub trait Warden: Send + Sync {
    /// Call 1: record the tenant and learn the recipient to seal to. Idempotent
    /// for a pending label with the same `account_email`.
    async fn create_tenant(&self, label: &str, account_email: &str)
    -> Result<Created, WardenError>;

    /// Call 2: install the sealed credential and provision. `cred_read_ciphertext`
    /// MUST be age armor.
    async fn put_credentials(
        &self,
        label: &str,
        cred_read_ciphertext: &str,
    ) -> Result<Pairing, WardenError>;

    /// Install or rotate the tenant's LLM-gateway virtual keys — triage
    /// (`api_key`) and assistant — in one call. `None` skips a slot rather
    /// than clearing it, so a half-failed mint installs the half that exists;
    /// both `None` is a caller bug and refused. The values are SECRET: held
    /// for the length of this call, never logged, and nothing comes back.
    async fn put_llm_key(
        &self,
        label: &str,
        api_key: Option<&str>,
        assistant_api_key: Option<&str>,
    ) -> Result<(), WardenError>;

    /// The tenant's state, or `None` when the warden has never heard of it.
    /// Used before consent to check that a label is free.
    async fn status(&self, label: &str) -> Result<Option<TenantStatus>, WardenError>;

    /// Mint a FRESH pairing code for a tenant that already exists, by running
    /// `squelchd pair` inside its pod.
    ///
    /// The console login's last step, and the reason that login needs no new
    /// credential of its own: the ticket it hands the browser is an ordinary
    /// pairing code, one-shot and short-lived, which the daemon claims into an
    /// ordinary device token. A browser is just another paired device, so
    /// revocation, audit, and expiry are the ones already shipped.
    async fn pair(&self, label: &str) -> Result<Pairing, WardenError>;
}

/// The real client: one reqwest client, one bearer, one base URL.
pub struct HttpWarden {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl HttpWarden {
    /// `base_url` is a canonical origin (no trailing slash).
    pub fn new(base_url: String, token: String, timeout: Duration) -> Result<Self, WardenError> {
        let http = reqwest::Client::builder()
            // Redirects refused: these requests carry the warden bearer AND a
            // tenant's sealed credential, and a redirect is how both end up at
            // a host nobody chose.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| WardenError::Unreachable)?;
        Ok(Self {
            base_url,
            token,
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

#[async_trait::async_trait]
impl Warden for HttpWarden {
    async fn create_tenant(
        &self,
        label: &str,
        account_email: &str,
    ) -> Result<Created, WardenError> {
        // Already guaranteed upstream; asserted here so the guarantee does not
        // depend on every future caller remembering it. The label reaches a URL
        // path on the other two methods and a typed k8s object name on the far
        // side.
        crate::labels::validate(label).map_err(|_| WardenError::LabelRefused)?;

        let resp = self
            .http
            .post(self.url("/v1/tenants"))
            .bearer_auth(&self.token)
            .json(&CreateRequest {
                label,
                account_email,
            })
            .send()
            .await
            .map_err(|e| transport_error(e, label))?;

        match resp.status().as_u16() {
            201 => {
                let body = read_capped(resp, label).await?;
                let created: Created =
                    serde_json::from_slice(&body).map_err(|_| WardenError::BadRecipient)?;
                // A live refresh token is about to be encrypted to this string.
                // It is parsed as an age public key HERE, before the plaintext
                // is anywhere near it.
                crate::seal::parse_recipient(&created.recipient)
                    .map_err(|_| WardenError::BadRecipient)?;
                Ok(created)
            }
            401 | 403 => Err(WardenError::Unauthorized),
            409 => Err(WardenError::LabelTaken),
            422 => Err(WardenError::LabelRefused),
            _ => Err(WardenError::Failed),
        }
    }

    async fn put_credentials(
        &self,
        label: &str,
        cred_read_ciphertext: &str,
    ) -> Result<Pairing, WardenError> {
        // The label goes into a URL PATH. Validated upstream, validated again
        // here, because "somebody upstream checked it" is how a `../` reaches a
        // path.
        crate::labels::validate(label).map_err(|_| WardenError::LabelRefused)?;
        // The line that would fail loudly if a refactor ever handed this
        // function a plaintext token, instead of the cluster quietly writing one
        // into a Secret.
        if !cred_read_ciphertext.starts_with(crate::seal::ARMOR_HEADER) {
            return Err(WardenError::NotCiphertext);
        }

        let resp = self
            .http
            .put(self.url(&format!("/v1/tenants/{label}/credentials")))
            .bearer_auth(&self.token)
            .json(&CredentialsRequest {
                cred_read_ciphertext,
            })
            .send()
            .await
            .map_err(|e| transport_error(e, label))?;

        match resp.status().as_u16() {
            200 => {
                let body = read_capped(resp, label).await?;
                let pairing: Pairing =
                    serde_json::from_slice(&body).map_err(|_| WardenError::BadPairing)?;
                validate_pairing(&pairing)?;
                Ok(pairing)
            }
            401 | 403 => Err(WardenError::Unauthorized),
            404 => Err(WardenError::UnknownTenant),
            409 => Err(WardenError::AlreadyProvisioned),
            // NOT `LabelRefused`, which is a thing to say about an ADDRESS. By
            // call 2 the warden has already accepted this label and reserved it;
            // a 422 on the credential body is our two sides disagreeing about
            // the wire, and the person who typed the address had nothing to do
            // with it. It collapses into the generic retriable failure so the
            // page says "not finished yet" instead of blaming their input.
            422 => Err(WardenError::Failed),
            _ => Err(WardenError::Failed),
        }
    }

    async fn put_llm_key(
        &self,
        label: &str,
        api_key: Option<&str>,
        assistant_api_key: Option<&str>,
    ) -> Result<(), WardenError> {
        // The label goes into a URL path, same as the other routes.
        crate::labels::validate(label).map_err(|_| WardenError::LabelRefused)?;
        // A call with nothing to install is a bug, not a no-op to swallow.
        if api_key.is_none() && assistant_api_key.is_none() {
            return Err(WardenError::BadApiKey);
        }
        // The Bifrost client shape-checks what it mints; this holds every
        // OTHER caller to the same bar, before the socket is opened. Both
        // slots, one bar.
        for key in [api_key, assistant_api_key].into_iter().flatten() {
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(WardenError::BadApiKey);
            }
        }

        let resp = self
            .http
            .put(self.url(&format!("/v1/tenants/{label}/llm-key")))
            .bearer_auth(&self.token)
            .json(&LlmKeyRequest {
                api_key,
                assistant_api_key,
            })
            .send()
            .await
            .map_err(|e| transport_error(e, label))?;

        match resp.status().as_u16() {
            // The 200 body is `{}` by contract: the key came in, nothing goes
            // out. Nothing to read.
            200 => Ok(()),
            401 | 403 => Err(WardenError::Unauthorized),
            404 => Err(WardenError::UnknownTenant),
            503 => Err(WardenError::LlmNotConfigured),
            // 422 (`invalid_api_key`) collapses here: it is the two sides
            // disagreeing about what Bifrost mints, and the guard above should
            // make it unreachable.
            _ => Err(WardenError::Failed),
        }
    }

    async fn pair(&self, label: &str) -> Result<Pairing, WardenError> {
        // Into a URL path again, so validated again, for the reason spelled out
        // on call 2: "somebody upstream checked it" is how a `../` reaches a path.
        crate::labels::validate(label).map_err(|_| WardenError::LabelRefused)?;

        // NO BODY. The tenant is the whole request, and the answer is a live
        // pairing code, so nothing about this call may be logged.
        let resp = self
            .http
            .post(self.url(&format!("/v1/tenants/{label}/pair")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| transport_error(e, label))?;

        match resp.status().as_u16() {
            200 => {
                let body = read_capped(resp, label).await?;
                let pairing: Pairing =
                    serde_json::from_slice(&body).map_err(|_| WardenError::BadPairing)?;
                validate_pairing(&pairing)?;
                Ok(pairing)
            }
            401 | 403 => Err(WardenError::Unauthorized),
            // The tenant is gone, or never existed. This crate's own record said
            // otherwise, which is drift worth its own variant rather than a
            // generic failure.
            404 => Err(WardenError::UnknownTenant),
            422 => Err(WardenError::LabelRefused),
            _ => Err(WardenError::Failed),
        }
    }

    async fn status(&self, label: &str) -> Result<Option<TenantStatus>, WardenError> {
        crate::labels::validate(label).map_err(|_| WardenError::LabelRefused)?;
        let resp = self
            .http
            .get(self.url(&format!("/v1/tenants/{label}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| transport_error(e, label))?;

        match resp.status().as_u16() {
            200 => {
                let body = read_capped(resp, label).await?;
                let status: TenantStatus =
                    serde_json::from_slice(&body).map_err(|_| WardenError::Unreachable)?;
                Ok(Some(status))
            }
            404 => Ok(None),
            401 | 403 => Err(WardenError::Unauthorized),
            _ => Err(WardenError::Failed),
        }
    }
}

/// The operator surface: the two routes a human runs at a shell.
///
/// INHERENT, not on the [`Warden`] trait, and deliberately. The serving path
/// never asks whether a tenant has drifted — signup provisions, the console
/// pairs — so putting these on the trait would oblige every stand-in for a
/// cluster to answer a question no request will ask. The commands that do ask
/// hold a real [`HttpWarden`] and nothing else.
impl HttpWarden {
    /// What is on this tenant's workload that the warden did not put there.
    ///
    /// Read-only on both sides: the warden's own apply for the comparison is a
    /// dry run. A tenant with nothing running answers 200 with
    /// `deployment_present: false` and empty arrays, which is why this is safe
    /// to walk over a whole fleet without deciding anything first.
    pub async fn drift(&self, label: &str) -> Result<DriftReport, WardenError> {
        // Into a URL path, so validated here, for the reason the provisioning
        // routes give: "somebody upstream checked it" is how a `../` reaches a
        // path. Here the caller upstream is a shell.
        crate::labels::validate(label).map_err(|_| WardenError::LabelRefused)?;

        let resp = self
            .http
            .get(self.url(&format!("/v1/tenants/{label}/drift")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| transport_error(e, label))?;

        match resp.status().as_u16() {
            200 => {
                // The drift cap, not the contract's: a report is as big as the
                // drift it found, and the tenant that drifted most must not be
                // the one that comes back unreadable. See [`MAX_DRIFT_BODY`].
                let body = read_capped_to(resp, MAX_DRIFT_BODY, label).await?;
                let mut report: DriftReport =
                    serde_json::from_slice(&body).map_err(|_| WardenError::Unreachable)?;
                report.declaw();
                Ok(report)
            }
            401 | 403 => Err(WardenError::Unauthorized),
            404 => Err(WardenError::UnknownTenant),
            422 => Err(WardenError::LabelRefused),
            _ => Err(WardenError::Failed),
        }
    }

    /// Put this tenant's workload back onto the warden's render, deleting and
    /// recreating the Deployment if another field manager owns part of it.
    ///
    /// WRITES, and rolls a pod: the answer arrives only after a ready one is
    /// back, so a slow node reads as a failure with the workload already
    /// replaced. That is the shape the caller must be built for, and it is why
    /// this is one tenant per invocation.
    ///
    /// THE ONLY CALL HERE WITH ITS OWN TIMEOUT, and the only one that needs
    /// one: on the delete-recreate path this runs for minutes, and giving up on
    /// it early does not just lose the answer, it stops the warden mid-operation
    /// with the tenant's Deployment already deleted. See
    /// [`crate::config::RECONCILE_TIMEOUT`] for the arithmetic and for the
    /// outage that produced it.
    pub async fn reconcile(&self, label: &str) -> Result<Reconciled, WardenError> {
        crate::labels::validate(label).map_err(|_| WardenError::LabelRefused)?;

        // NO BODY. The tenant is the whole request.
        let resp = self
            .http
            .post(self.url(&format!("/v1/tenants/{label}/reconcile")))
            .bearer_auth(&self.token)
            // Overrides the client's budget for this request only. Every other
            // route on this client is a small round trip and keeps the short
            // one, so a warden that has genuinely stopped answering still fails
            // a signup in thirty seconds rather than in ten minutes.
            .timeout(crate::config::RECONCILE_TIMEOUT)
            .send()
            .await
            .map_err(|e| transport_error(e, label))?;

        match resp.status().as_u16() {
            200 => {
                let body = read_capped(resp, label).await?;
                let mut reconciled: Reconciled =
                    serde_json::from_slice(&body).map_err(|_| WardenError::Unreachable)?;
                reconciled.declaw();
                Ok(reconciled)
            }
            401 | 403 => Err(WardenError::Unauthorized),
            404 => Err(WardenError::UnknownTenant),
            // The tenant is real and has no workload: pending or stopped.
            409 => Err(WardenError::NotReconcilable),
            422 => Err(WardenError::LabelRefused),
            // Everything else, `not_ready` included: the workload may well have
            // been replaced, and the operator's next move is to look rather
            // than to hammer this call.
            _ => Err(WardenError::Failed),
        }
    }
}

/// Hold the warden's pairing answer to the shape the success page can render.
///
/// The `deep_link` is checked but NOT used: [`crate::pages`] rebuilds it from
/// the validated code and this deployment's own base domain, so a warden that
/// answered with somebody else's URL cannot put it in an anchor on our page.
fn validate_pairing(p: &Pairing) -> Result<(), WardenError> {
    let code = p.pair_code.trim();
    let bare: String = code.chars().filter(|c| *c != '-').collect();
    let shaped = bare.len() == 8 && bare.bytes().all(|b| PAIR_CODE_ALPHABET.contains(&b));
    if !shaped {
        return Err(WardenError::BadPairing);
    }
    Ok(())
}

/// Which failure a [`reqwest::Error`] actually was.
///
/// One `map_err(|_| WardenError::Unreachable)` used to stand at every call site
/// here, flattening timeout, connection refused, DNS and TLS into a single
/// sentence. Most of those really do mean the request never landed and nothing
/// was started. A TIMEOUT means the opposite, and on 2026-08-19 that cost a
/// live mailbox eight minutes: an operator was told "the provisioning service
/// could not be reached" about a reconcile the warden had accepted, begun, and
/// been cut off in the middle of.
///
/// So the one case this crate can actually tell apart gets told apart.
/// `is_timeout()` covers both the client-level budget and a per-request
/// override, which is how the reconcile route's longer budget lands here too.
/// Everything else keeps the old variant, whose message is written for the case
/// where the warden never heard from us.
fn transport_error(e: reqwest::Error, label: &str) -> WardenError {
    if e.is_timeout() {
        WardenError::TimedOut {
            label: label.to_string(),
        }
    } else {
        WardenError::Unreachable
    }
}

async fn read_capped(resp: reqwest::Response, label: &str) -> Result<Vec<u8>, WardenError> {
    read_capped_to(resp, MAX_RESPONSE_BODY, label).await
}

/// The label rides in so that a body read which runs out the clock reports the
/// timeout, exactly as the send that preceded it does. Exceeding the CAP stays
/// [`WardenError::Unreachable`]: nothing timed out there, and the reasoning
/// that sizes [`MAX_DRIFT_BODY`] depends on that message.
async fn read_capped_to(
    mut resp: reqwest::Response,
    cap: usize,
    label: &str,
) -> Result<Vec<u8>, WardenError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| transport_error(e, label))? {
        if out.len() + chunk.len() > cap {
            return Err(WardenError::Unreachable);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairing(code: &str) -> Pairing {
        Pairing {
            pair_code: code.to_string(),
            pair_url: "https://ada.passband.email".into(),
            deep_link: "passband://pair?url=x&code=y".into(),
        }
    }

    #[test]
    fn accepts_the_pairing_shape_squelchd_prints() {
        assert!(validate_pairing(&pairing("ABCD-EFGH")).is_ok());
        assert!(validate_pairing(&pairing("ABCDEFGH")).is_ok());
    }

    /// The code is rendered into a page. A warden that answered with markup, a
    /// wrong length, or a character outside Crockford is a bug or a compromise,
    /// and either way it does not reach the browser.
    #[test]
    fn refuses_a_pairing_code_it_will_not_render() {
        for bad in [
            "",
            "ABC",
            "ABCDEFGHI",
            "ABCD-EFGU",
            "<script>alert(1)</script>",
            "ABCD EFGH",
        ] {
            assert!(validate_pairing(&pairing(bad)).is_err(), "{bad:?}");
        }
    }

    /// Only `pending` is retriable. Everything else, including a status this
    /// crate has never heard of, reads as taken.
    #[test]
    fn only_a_pending_tenant_is_retriable() {
        let status = |s: &str| TenantStatus { status: s.into() };
        assert!(status(STATUS_PENDING).is_pending());
        for taken in ["active", "failed", "stopped", "", "PENDING", "whatever"] {
            assert!(!status(taken).is_pending(), "{taken:?}");
        }
    }

    fn offline_client() -> HttpWarden {
        // Port 1: nothing listens, so any test here that reached the socket
        // would fail rather than pass by accident.
        HttpWarden::new(
            "http://127.0.0.1:1".into(),
            "token".into(),
            Duration::from_millis(50),
        )
        .unwrap()
    }

    /// The guard that would catch a refactor putting a plaintext token on the
    /// wire. Async, but it refuses before any socket is opened, so no server is
    /// needed.
    #[tokio::test]
    async fn refuses_to_send_anything_that_is_not_armor() {
        let err = offline_client()
            .put_credentials("ada", "{\"refresh_token\":\"secret\"}")
            .await
            .unwrap_err();
        assert!(matches!(err, WardenError::NotCiphertext));
    }

    /// A 422 on call 2 is the two sides of the wire disagreeing about the
    /// credential body, not a verdict on the address: by then the warden has
    /// already accepted this label and reserved it for this mailbox. It must not
    /// come back as the error that means "that address is no good", because that
    /// is the one the pages above turn into a message about what the user typed.
    #[tokio::test]
    async fn a_refused_credential_body_is_not_a_label_problem() {
        use axum::{Router, routing::put};

        let app = Router::new().route(
            "/v1/tenants/{label}/credentials",
            put(|| async { axum::http::StatusCode::UNPROCESSABLE_ENTITY }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let w = HttpWarden::new(
            format!("http://{addr}"),
            "token".into(),
            Duration::from_secs(5),
        )
        .unwrap();
        let armor = format!("{}\nbody\n", crate::seal::ARMOR_HEADER);
        assert!(matches!(
            w.put_credentials("ada", &armor).await,
            Err(WardenError::Failed)
        ));
    }

    /// The label reaches a URL path and, on the far side, typed object names.
    /// This client refuses one it would not have built itself, on every route.
    #[tokio::test]
    async fn refuses_a_label_it_would_not_have_validated() {
        let w = offline_client();
        for bad in ["../../etc", "ada/../root", "ada%2f..", "WWW", "a b", ""] {
            let armor = format!("{}\nbody\n", crate::seal::ARMOR_HEADER);
            assert!(
                matches!(
                    w.create_tenant(bad, "ada@example.com").await,
                    Err(WardenError::LabelRefused)
                ),
                "{bad:?}"
            );
            assert!(
                matches!(
                    w.put_credentials(bad, &armor).await,
                    Err(WardenError::LabelRefused)
                ),
                "{bad:?}"
            );
            assert!(
                matches!(
                    w.put_llm_key(bad, Some("sk-bf-key"), Some("sk-bf-key-a"))
                        .await,
                    Err(WardenError::LabelRefused)
                ),
                "{bad:?}"
            );
            assert!(
                matches!(w.status(bad).await, Err(WardenError::LabelRefused)),
                "{bad:?}"
            );
            assert!(
                matches!(w.pair(bad).await, Err(WardenError::LabelRefused)),
                "{bad:?}"
            );
            assert!(
                matches!(w.drift(bad).await, Err(WardenError::LabelRefused)),
                "{bad:?}"
            );
            assert!(
                matches!(w.reconcile(bad).await, Err(WardenError::LabelRefused)),
                "{bad:?}"
            );
        }
    }

    /// An empty key, or one carrying whitespace or control bytes, is refused
    /// before any socket is opened: it is a bug here, not a 422 there. The
    /// same bar for BOTH slots, and a call with neither is a bug too.
    #[tokio::test]
    async fn refuses_an_api_key_it_would_not_present() {
        let w = offline_client();
        for bad in ["", "with space", "with\nnewline", "with\ttab"] {
            assert!(
                matches!(
                    w.put_llm_key("ada", Some(bad), None).await,
                    Err(WardenError::BadApiKey)
                ),
                "{bad:?}"
            );
            assert!(
                matches!(
                    w.put_llm_key("ada", Some("sk-bf-fine"), Some(bad)).await,
                    Err(WardenError::BadApiKey)
                ),
                "assistant: {bad:?}"
            );
        }
        assert!(
            matches!(
                w.put_llm_key("ada", None, None).await,
                Err(WardenError::BadApiKey)
            ),
            "nothing to install is a bug, not a no-op"
        );
    }

    /// The llm-key route on the wire: the key rides as `{"api_key": ...}` with
    /// the bearer, a 200 is silence, and each refusal maps to its own error —
    /// 503 distinctly, because "this deployment has no gateway" needs a log
    /// line that says so rather than a generic failure.
    #[tokio::test]
    async fn put_llm_key_speaks_the_wire_and_maps_its_refusals() {
        use axum::{
            Json, Router,
            extract::{Path, State},
            http::{HeaderMap, StatusCode, header},
            response::IntoResponse,
            routing::put,
        };
        use serde_json::{Value, json};
        use std::sync::{Arc, Mutex};

        // `(bearer, body)` per request, so the wire itself is asserted on.
        type Seen = Arc<Mutex<Vec<(String, Value)>>>;
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));

        // The mock picks its answer by label, so one server covers the map.
        let app = Router::new()
            .route(
                "/v1/tenants/{label}/llm-key",
                put(
                    |State(seen): State<Seen>,
                     Path(label): Path<String>,
                     headers: HeaderMap,
                     body: String| async move {
                        let bearer = headers
                            .get(header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                        seen.lock().unwrap().push((bearer, parsed));
                        match label.as_str() {
                            "okay" => (StatusCode::OK, Json(json!({}))).into_response(),
                            "ghost" => (StatusCode::NOT_FOUND, Json(json!({"error":"not_found"})))
                                .into_response(),
                            "nollm" => (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error":"llm_not_configured"})),
                            )
                                .into_response(),
                            _ => (
                                StatusCode::UNPROCESSABLE_ENTITY,
                                Json(json!({"error":"invalid_api_key"})),
                            )
                                .into_response(),
                        }
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let w = HttpWarden::new(
            format!("http://{addr}"),
            "token".into(),
            Duration::from_secs(5),
        )
        .unwrap();

        w.put_llm_key(
            "okay",
            Some("sk-bf-THE-KEY"),
            Some("sk-bf-THE-ASSISTANT-KEY"),
        )
        .await
        .unwrap();
        assert!(matches!(
            w.put_llm_key(
                "ghost",
                Some("sk-bf-THE-KEY"),
                Some("sk-bf-THE-ASSISTANT-KEY")
            )
            .await,
            Err(WardenError::UnknownTenant)
        ));
        assert!(matches!(
            w.put_llm_key(
                "nollm",
                Some("sk-bf-THE-KEY"),
                Some("sk-bf-THE-ASSISTANT-KEY")
            )
            .await,
            Err(WardenError::LlmNotConfigured)
        ));
        assert!(matches!(
            w.put_llm_key(
                "other",
                Some("sk-bf-THE-KEY"),
                Some("sk-bf-THE-ASSISTANT-KEY")
            )
            .await,
            Err(WardenError::Failed)
        ));

        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 4);
            for (bearer, body) in seen.iter() {
                assert_eq!(bearer, "Bearer token");
                assert_eq!(body["api_key"], "sk-bf-THE-KEY");
                assert_eq!(body["assistant_api_key"], "sk-bf-THE-ASSISTANT-KEY");
            }
        }

        // A half-present pair serializes ONLY the half that exists: an absent
        // slot must not ride as null, which a strict warden could read as
        // "clear it".
        w.put_llm_key("okay", None, Some("sk-bf-THE-ASSISTANT-KEY"))
            .await
            .unwrap();
        w.put_llm_key("okay", Some("sk-bf-THE-KEY"), None)
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        let (_, assistant_only) = &seen[4];
        assert!(assistant_only.get("api_key").is_none(), "{assistant_only}");
        assert_eq!(
            assistant_only["assistant_api_key"],
            "sk-bf-THE-ASSISTANT-KEY"
        );
        let (_, triage_only) = &seen[5];
        assert_eq!(triage_only["api_key"], "sk-bf-THE-KEY");
        assert!(
            triage_only.get("assistant_api_key").is_none(),
            "{triage_only}"
        );
    }

    /// The pairing route answers with the same shape call 2 does, and it is held
    /// to the same standard: a code this crate would not render is a refusal,
    /// not something to put in a redirect.
    #[tokio::test]
    async fn a_pairing_answer_is_validated_like_every_other() {
        use axum::{Router, routing::post};

        let app = Router::new()
            .route(
                "/v1/tenants/good/pair",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "pair_code": "ABCD-EFGH",
                        "pair_url": "https://good.passband.test",
                        "deep_link": "passband://pair?url=x&code=ABCD-EFGH",
                    }))
                }),
            )
            .route(
                "/v1/tenants/bad/pair",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "pair_code": "<script>alert(1)</script>",
                        "pair_url": "https://bad.passband.test",
                        "deep_link": "passband://pair",
                    }))
                }),
            )
            .route(
                "/v1/tenants/gone/pair",
                post(|| async { axum::http::StatusCode::NOT_FOUND }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let w = HttpWarden::new(
            format!("http://{addr}"),
            "token".into(),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(w.pair("good").await.unwrap().pair_code, "ABCD-EFGH");
        assert!(matches!(w.pair("bad").await, Err(WardenError::BadPairing)));
        assert!(matches!(
            w.pair("gone").await,
            Err(WardenError::UnknownTenant)
        ));
    }

    /// Stand up a mock warden that answers by label and record the bearer of
    /// every request it takes. The operator routes are `GET`/`POST` with no
    /// body, so the label and the bearer are the whole wire.
    async fn mock_warden(app: axum::Router) -> HttpWarden {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        HttpWarden::new(
            format!("http://{addr}"),
            "token".into(),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    /// The `Authorization` header of each request the mock took.
    type Bearers = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    fn record_bearer(seen: &Bearers, headers: &axum::http::HeaderMap) {
        let bearer = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        seen.lock().unwrap().push(bearer);
    }

    /// The drift route on the wire: a tenant somebody hand-edited, a tenant that
    /// still matches its render, a tenant with nothing running (whose arrays the
    /// warden may leave out entirely), and each refusal.
    #[tokio::test]
    async fn drift_reports_foreign_owners_and_pending_changes() {
        use axum::{
            Json, Router,
            extract::{Path, State},
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::get,
        };
        use serde_json::json;

        let seen: Bearers = Bearers::default();
        let app = Router::new()
            .route(
                "/v1/tenants/{label}/drift",
                get(
                    |State(seen): State<Bearers>,
                     Path(label): Path<String>,
                     headers: HeaderMap| async move {
                        record_bearer(&seen, &headers);
                        match label.as_str() {
                            "drifted" => (
                                StatusCode::OK,
                                Json(json!({
                                    "status": "active",
                                    "deployment_present": true,
                                    "foreign": [{
                                        "manager": "kubectl-set",
                                        "operation": "Update",
                                        "paths": [
                                            "spec.template.spec.initContainers[seed].env",
                                        ],
                                    }],
                                    "changes": [{
                                        "path": "spec.template.spec.containers[squelchd].image",
                                        "live": "squelchd:daemon-0.0.1",
                                        "rendered": "squelchd:daemon-0.0.2",
                                    }, {
                                        "path": "spec.template.spec.replicas",
                                        "live": 2,
                                        "rendered": null,
                                    }],
                                })),
                            )
                                .into_response(),
                            "settled" => (
                                StatusCode::OK,
                                Json(json!({
                                    "status": "active",
                                    "deployment_present": true,
                                    "foreign": [],
                                    "changes": [],
                                })),
                            )
                                .into_response(),
                            // Arrays omitted, not empty: a tenant with no
                            // workload has nothing to say about one.
                            "halfway" => (
                                StatusCode::OK,
                                Json(json!({"status": "pending", "deployment_present": false})),
                            )
                                .into_response(),
                            "refused" => (
                                StatusCode::UNPROCESSABLE_ENTITY,
                                Json(json!({"error": "invalid_label"})),
                            )
                                .into_response(),
                            "broken" => (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": "cluster_unavailable"})),
                            )
                                .into_response(),
                            _ => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"})))
                                .into_response(),
                        }
                    },
                ),
            )
            .with_state(seen.clone());
        let w = mock_warden(app).await;

        let report = w.drift("drifted").await.unwrap();
        assert_eq!(report.status, "active");
        assert!(report.deployment_present);
        assert!(!report.is_clean());
        assert_eq!(report.foreign.len(), 1);
        assert_eq!(report.foreign[0].manager, "kubectl-set");
        assert_eq!(report.foreign[0].operation, "Update");
        assert_eq!(
            report.foreign[0].paths,
            ["spec.template.spec.initContainers[seed].env"]
        );
        assert_eq!(report.changes.len(), 2);
        assert_eq!(
            report.changes[0].path,
            "spec.template.spec.containers[squelchd].image"
        );
        assert_eq!(report.changes[0].live, "squelchd:daemon-0.0.1");
        assert_eq!(report.changes[0].rendered, "squelchd:daemon-0.0.2");
        // A field that exists only live: `null` on the other side, not absent.
        assert!(report.changes[1].rendered.is_null());

        let settled = w.drift("settled").await.unwrap();
        assert!(settled.is_clean());
        assert!(settled.deployment_present);

        let halfway = w.drift("halfway").await.unwrap();
        assert_eq!(halfway.status, "pending");
        assert!(!halfway.deployment_present);
        assert!(halfway.is_clean());

        assert!(matches!(
            w.drift("refused").await,
            Err(WardenError::LabelRefused)
        ));
        assert!(matches!(w.drift("broken").await, Err(WardenError::Failed)));
        assert!(matches!(
            w.drift("nobody").await,
            Err(WardenError::UnknownTenant)
        ));

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 6);
        assert!(seen.iter().all(|b| b == "Bearer token"));
    }

    /// The reconcile route on the wire: the outcome words an operator reads,
    /// and the 409 that means "this tenant has no workload to converge" — a
    /// different operation, never a retry.
    #[tokio::test]
    async fn reconcile_reports_its_outcome_and_maps_its_refusals() {
        use axum::{
            Json, Router,
            extract::{Path, State},
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::post,
        };
        use serde_json::json;

        let seen: Bearers = Bearers::default();
        let app = Router::new()
            .route(
                "/v1/tenants/{label}/reconcile",
                post(
                    |State(seen): State<Bearers>,
                     Path(label): Path<String>,
                     headers: HeaderMap| async move {
                        record_bearer(&seen, &headers);
                        match label.as_str() {
                            "settled" => (
                                StatusCode::OK,
                                Json(json!({"deployment": "converged", "status": "active"})),
                            )
                                .into_response(),
                            "drifted" => (
                                StatusCode::OK,
                                Json(json!({"deployment": "recreated", "status": "active"})),
                            )
                                .into_response(),
                            "halfway" => (
                                StatusCode::CONFLICT,
                                Json(json!({"error": "not_reconcilable"})),
                            )
                                .into_response(),
                            "unauthed" => (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({"error": "unauthorized"})),
                            )
                                .into_response(),
                            "stalled" => (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"error": "not_ready"})),
                            )
                                .into_response(),
                            _ => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"})))
                                .into_response(),
                        }
                    },
                ),
            )
            .with_state(seen.clone());
        let w = mock_warden(app).await;

        let converged = w.reconcile("settled").await.unwrap();
        assert_eq!(converged.deployment, "converged");
        assert_eq!(converged.status, "active");
        assert_eq!(
            w.reconcile("drifted").await.unwrap().deployment,
            "recreated"
        );

        assert!(matches!(
            w.reconcile("halfway").await,
            Err(WardenError::NotReconcilable)
        ));
        assert!(matches!(
            w.reconcile("unauthed").await,
            Err(WardenError::Unauthorized)
        ));
        // `not_ready` is a rolled pod that did not come back. Generic on
        // purpose: the next move is to look, not to call again.
        assert!(matches!(
            w.reconcile("stalled").await,
            Err(WardenError::Failed)
        ));
        assert!(matches!(
            w.reconcile("nobody").await,
            Err(WardenError::UnknownTenant)
        ));

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 6);
        assert!(seen.iter().all(|b| b == "Bearer token"));
    }

    /// A warden that takes `delay` to answer either operator route, behind a
    /// client that gives up after `budget`. `delay` is the only thing these
    /// tests care about, so the bodies are just enough to parse.
    async fn slow_warden(delay: Duration, budget: Duration) -> HttpWarden {
        use axum::{
            Json, Router,
            routing::{get, post},
        };
        use serde_json::json;

        let app = Router::new()
            .route(
                "/v1/tenants/{label}/reconcile",
                post(move || async move {
                    tokio::time::sleep(delay).await;
                    Json(json!({"deployment": "recreated", "status": "active"}))
                }),
            )
            .route(
                "/v1/tenants/{label}/drift",
                get(move || async move {
                    tokio::time::sleep(delay).await;
                    Json(json!({"status": "active", "deployment_present": true}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        HttpWarden::new(format!("http://{addr}"), "token".into(), budget).unwrap()
    }

    /// The defect [`crate::config::RECONCILE_TIMEOUT`] exists for. A reconcile
    /// on the delete-recreate path legitimately runs for minutes, so it carries
    /// its own budget and outlasts the one sized for a signup round trip;
    /// `drift` is a read and keeps the short one, because nothing about a drift
    /// report justifies holding a shell open for ten minutes. One slow warden,
    /// two answers.
    #[tokio::test]
    async fn reconcile_outlasts_the_budget_the_read_routes_keep() {
        // Six times the client budget, so a loaded machine cannot pass this by
        // accident, and a rounding error next to RECONCILE_TIMEOUT.
        let w = slow_warden(Duration::from_millis(600), Duration::from_millis(100)).await;

        assert_eq!(w.reconcile("ellie").await.unwrap().deployment, "recreated");
        assert!(matches!(
            w.drift("ellie").await,
            Err(WardenError::TimedOut { .. })
        ));
    }

    /// A timeout must not claim unreachability, which is what every one of them
    /// did until 2026-08-19: an operator was told the provisioning service
    /// "could not be reached" about a reconcile the warden had accepted, begun,
    /// and been cut off inside, and left a mailbox down for eight minutes on the
    /// strength of it. The message has to say the opposite, and name the one
    /// place that settles which it was.
    #[tokio::test]
    async fn a_timeout_says_the_warden_may_still_be_working() {
        let w = slow_warden(Duration::from_millis(600), Duration::from_millis(100)).await;
        let err = w.drift("ellie").await.unwrap_err();

        assert!(matches!(err, WardenError::TimedOut { .. }));
        let said = err.to_string();
        assert!(said.contains("may still be working"), "{said}");
        // The check, with THIS tenant in it: the grep is useless without it.
        assert!(
            said.contains("kubectl -n warden logs deploy/squelch-warden"),
            "{said}"
        );
        assert!(said.contains("tenant=ellie "), "{said}");
        // The sentence this variant exists to stop saying.
        assert!(!said.contains("could not be reached"), "{said}");
    }

    /// The other half of the split. A refused connection really does mean
    /// nothing reached the warden and nothing was started, so it keeps the old
    /// variant: pointing an operator at the warden's log for a call that was
    /// never made is the same lie in the other direction.
    #[tokio::test]
    async fn a_refused_connection_is_still_unreachability() {
        // Port 1 again, but with a budget generous enough that a loaded machine
        // cannot turn an instant ECONNREFUSED into a timeout and make this pass
        // for the wrong reason.
        let w = HttpWarden::new(
            "http://127.0.0.1:1".into(),
            "token".into(),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(matches!(
            w.drift("ellie").await,
            Err(WardenError::Unreachable)
        ));
        assert!(matches!(
            w.reconcile("ellie").await,
            Err(WardenError::Unreachable)
        ));
    }

    /// A field manager is named by whoever wrote the field, and this report is
    /// read in a terminal. Escape sequences and bidi overrides do not survive
    /// the trip, so the worst a hand edit can do to the run that finds it is
    /// look ugly.
    #[tokio::test]
    async fn a_drift_report_cannot_repaint_the_operators_terminal() {
        use axum::{Json, Router, routing::get};
        use serde_json::json;

        let app = Router::new().route(
            "/v1/tenants/drifted/drift",
            get(|| async {
                Json(json!({
                    "status": "act\u{1b}[2Jive",
                    "deployment_present": true,
                    "foreign": [{
                        "manager": "kubectl-\u{1b}[31mset",
                        "operation": "Up\u{7}date",
                        "paths": ["spec.\u{202e}tnemyolped"],
                    }],
                    "changes": [{
                        "path": "spec.template.spec.containers[squelchd]\u{0}.image",
                        "live": "a",
                        "rendered": "b",
                    }],
                }))
            }),
        );
        let w = mock_warden(app).await;

        let report = w.drift("drifted").await.unwrap();
        let printed = format!(
            "{}{}{}{}{}",
            report.status,
            report.foreign[0].manager,
            report.foreign[0].operation,
            report.foreign[0].paths[0],
            report.changes[0].path
        );
        assert!(!printed.chars().any(|c| c.is_control()), "{printed:?}");
        assert!(!printed.contains('\u{202e}'), "{printed:?}");
        // Declawed, not swallowed: the operator still reads a manager name.
        assert!(report.foreign[0].manager.contains("kubectl-"));
        assert!(report.foreign[0].manager.contains("set"));
    }

    /// The values are printed as JSON, so their escaping is serde's and the
    /// characters survive to be seen. What must not survive is a string that
    /// reaches the terminal raw.
    #[test]
    fn declaw_replaces_what_a_terminal_would_obey() {
        assert_eq!(declaw("kubectl-set"), "kubectl-set");
        assert_eq!(declaw("spec.replicas"), "spec.replicas");
        for hostile in [
            '\u{1b}', '\u{7}', '\n', '\r', '\u{202e}', '\u{2066}', '\u{200f}',
        ] {
            let out = declaw(&format!("a{hostile}b"));
            assert_eq!(out, format!("a{REPLACEMENT}b"), "{hostile:?}");
        }
        // The ESC is what a terminal obeys; the rest of the sequence is
        // ordinary text and stays, so the operator sees the shape of it.
        assert_eq!(declaw("a\u{1b}[2Jb"), format!("a{REPLACEMENT}[2Jb"));
    }
}
