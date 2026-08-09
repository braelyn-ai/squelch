//! The client half of the control -> warden wire contract, v2.
//!
//! The warden is the only thing that touches the cluster: it mints the tenant's
//! age identity, writes it into that tenant's Secret, renders the typed k8s
//! objects, waits for the pod, and execs `squelchd pair` inside it. This module
//! knows none of that. It knows three routes, one bearer, and the shapes that
//! come back.
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

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Ceiling on a warden response body. Every answer in the contract is a few
/// hundred bytes; the credential ciphertext travels the other way.
const MAX_RESPONSE_BODY: usize = 64 * 1024;

/// Bounds on the pairing code the warden reports. Crockford `XXXX-XXXX`, which
/// is what `squelchd pair` prints.
const PAIR_CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A tenant the warden has recorded but not provisioned: call 1 landed, call 2
/// did not. The one status this crate treats as retriable.
pub const STATUS_PENDING: &str = "pending";

#[derive(Debug, thiserror::Error)]
pub enum WardenError {
    /// The warden could not be reached, or answered something unparseable.
    #[error("the provisioning service could not be reached")]
    Unreachable,
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
    /// The tenant's Read credential, age ASCII armor. The field name says
    /// "ciphertext" because that is the only thing that may ever be put in it.
    cred_read_ciphertext: &'a str,
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

/// A warden the control plane can talk to. A trait so a test can stand one up
/// without a cluster, and so a later provisioning backend slots in without
/// touching the handlers.
#[async_trait::async_trait]
pub trait Warden: Send + Sync {
    /// Call 1: record the tenant and learn the recipient to seal to. Idempotent
    /// for a pending label with the same `account_email`.
    async fn create_tenant(
        &self,
        label: &str,
        account_email: &str,
    ) -> Result<Created, WardenError>;

    /// Call 2: install the sealed credential and provision. `cred_read_ciphertext`
    /// MUST be age armor.
    async fn put_credentials(
        &self,
        label: &str,
        cred_read_ciphertext: &str,
    ) -> Result<Pairing, WardenError>;

    /// The tenant's state, or `None` when the warden has never heard of it.
    /// Used before consent to check that a label is free.
    async fn status(&self, label: &str) -> Result<Option<TenantStatus>, WardenError>;
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
            .map_err(|_| WardenError::Unreachable)?;

        match resp.status().as_u16() {
            201 => {
                let body = read_capped(resp).await?;
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
            .map_err(|_| WardenError::Unreachable)?;

        match resp.status().as_u16() {
            200 => {
                let body = read_capped(resp).await?;
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

    async fn status(&self, label: &str) -> Result<Option<TenantStatus>, WardenError> {
        crate::labels::validate(label).map_err(|_| WardenError::LabelRefused)?;
        let resp = self
            .http
            .get(self.url(&format!("/v1/tenants/{label}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|_| WardenError::Unreachable)?;

        match resp.status().as_u16() {
            200 => {
                let body = read_capped(resp).await?;
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

async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>, WardenError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| WardenError::Unreachable)? {
        if out.len() + chunk.len() > MAX_RESPONSE_BODY {
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
                matches!(w.status(bad).await, Err(WardenError::LabelRefused)),
                "{bad:?}"
            );
        }
    }
}
