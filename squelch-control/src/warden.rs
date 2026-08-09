//! The client half of the control -> warden wire contract (`docs/HOSTED.md`).
//!
//! The warden is the only thing that touches the VPS: it allocates a port,
//! writes the tenant's credential file, renders the systemd env file, reloads
//! Caddy, starts `squelchd@<label>`, and mints the first pairing code. This
//! module knows none of that. It knows four routes, one bearer, and the shapes
//! that come back.
//!
//! WHAT CROSSES THIS WIRE, and what deliberately does not: the tenant label,
//! the mailbox address, and age CIPHERTEXT. Never a refresh token, never an
//! access token, never the age identity (this process does not have one). The
//! bearer is presented on every route and never logged.
//!
//! The warden's answers are treated as UNTRUSTED INPUT even though the warden
//! is ours. Its `pair_code` is rendered into a page and its `deep_link` would
//! become an `href`, so both are shape-checked here and the URL the page shows
//! is recomputed from this deployment's own base domain rather than echoed.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Ceiling on a warden response body. Every answer in the contract is a few
/// hundred bytes; the credential ciphertext travels the other way.
const MAX_RESPONSE_BODY: usize = 64 * 1024;

/// Bounds on the pairing code the warden reports. Crockford `XXXX-XXXX`, which
/// is what `squelchd pair` prints.
const PAIR_CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Debug, thiserror::Error)]
pub enum WardenError {
    /// The warden could not be reached, or answered something unparseable.
    #[error("the provisioning service could not be reached")]
    Unreachable,
    /// 401/403. A deployment misconfiguration, and one worth shouting about:
    /// nothing will ever provision until it is fixed.
    #[error("the provisioning service refused our credentials")]
    Unauthorized,
    /// 409: the label exists on the box.
    #[error("that address is already taken")]
    LabelTaken,
    /// 422: the warden refused the label. Should be unreachable (this crate
    /// validates first), so it means the two validators have drifted.
    #[error("that address was refused by the provisioning service")]
    LabelRefused,
    /// Any other non-success status.
    #[error("the provisioning service failed to set up the mailbox")]
    Failed,
    /// The warden answered 201 with a body this crate will not render.
    #[error("the provisioning service answered with an unusable pairing code")]
    BadPairing,
    /// The caller handed this client something that is not age ciphertext. A
    /// bug, caught before the socket is opened rather than after the VPS has
    /// written a plaintext token to disk.
    #[error("refusing to send a credential that is not age ciphertext")]
    NotCiphertext,
}

/// `POST /v1/tenants` request body.
#[derive(Debug, Serialize)]
struct ProvisionRequest<'a> {
    label: &'a str,
    account_email: &'a str,
    /// The tenant's Read credential, age ASCII armor. The field name says
    /// "ciphertext" because that is the only thing that may ever be put in it.
    cred_read_ciphertext: &'a str,
}

/// `POST /v1/tenants` 201 body.
#[derive(Debug, Deserialize)]
pub struct Provisioned {
    pub port: u16,
    pub pair_code: String,
    pub pair_url: String,
    pub deep_link: String,
}

/// `GET /v1/tenants/{label}` 200 body.
#[derive(Debug, Deserialize)]
pub struct TenantStatus {
    pub status: String,
    pub port: u16,
}

/// A warden the control plane can talk to. A trait so a test can stand one up
/// without a VPS, and so a later provisioning backend (the `Provisioner` seam
/// in `docs/HOSTED.md`) slots in without touching the handlers.
#[async_trait::async_trait]
pub trait Warden: Send + Sync {
    /// Provision a tenant. `cred_read_ciphertext` MUST be age armor.
    async fn provision(
        &self,
        label: &str,
        account_email: &str,
        cred_read_ciphertext: &str,
    ) -> Result<Provisioned, WardenError>;

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
            // Redirects refused: this request carries the warden bearer AND a
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
    async fn provision(
        &self,
        label: &str,
        account_email: &str,
        cred_read_ciphertext: &str,
    ) -> Result<Provisioned, WardenError> {
        // Two cheap assertions that hold whatever the caller did. Both are
        // already guaranteed upstream; they are here so that the guarantee does
        // not depend on every future caller remembering it.
        //
        // The label first, because it is interpolated into a URL path on the
        // other method and into a systemd unit name on the far side.
        crate::labels::validate(label).map_err(|_| WardenError::LabelRefused)?;
        // Then the credential: this is the line that would fail loudly if a
        // refactor ever handed this function a plaintext token, instead of the
        // VPS quietly receiving one.
        if !cred_read_ciphertext.starts_with(crate::seal::ARMOR_HEADER) {
            return Err(WardenError::NotCiphertext);
        }

        let resp = self
            .http
            .post(self.url("/v1/tenants"))
            .bearer_auth(&self.token)
            .json(&ProvisionRequest {
                label,
                account_email,
                cred_read_ciphertext,
            })
            .send()
            .await
            .map_err(|_| WardenError::Unreachable)?;

        match resp.status().as_u16() {
            201 => {
                let body = read_capped(resp).await?;
                let provisioned: Provisioned =
                    serde_json::from_slice(&body).map_err(|_| WardenError::BadPairing)?;
                validate_pairing(&provisioned)?;
                Ok(provisioned)
            }
            401 | 403 => Err(WardenError::Unauthorized),
            409 => Err(WardenError::LabelTaken),
            422 => Err(WardenError::LabelRefused),
            _ => Err(WardenError::Failed),
        }
    }

    async fn status(&self, label: &str) -> Result<Option<TenantStatus>, WardenError> {
        // The label goes into a URL PATH. It is validated before it ever gets
        // here, and it is validated again here, because "somebody upstream
        // checked it" is how a `../` reaches a path.
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

/// Hold the warden's 201 body to the shape the success page can render.
///
/// The `deep_link` is checked but NOT used: [`crate::pages`] rebuilds it from
/// the validated code and this deployment's own base domain, so a warden that
/// answered with somebody else's URL cannot put it in an anchor on our page.
fn validate_pairing(p: &Provisioned) -> Result<(), WardenError> {
    let code = p.pair_code.trim();
    let bare: String = code.chars().filter(|c| *c != '-').collect();
    let shaped = bare.len() == 8 && bare.bytes().all(|b| PAIR_CODE_ALPHABET.contains(&b));
    if !shaped || p.port == 0 {
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

    fn provisioned(code: &str, port: u16) -> Provisioned {
        Provisioned {
            port,
            pair_code: code.to_string(),
            pair_url: "https://ada.passband.email".into(),
            deep_link: "passband://pair?url=x&code=y".into(),
        }
    }

    #[test]
    fn accepts_the_pairing_shape_squelchd_prints() {
        assert!(validate_pairing(&provisioned("ABCD-EFGH", 9100)).is_ok());
        assert!(validate_pairing(&provisioned("ABCDEFGH", 9100)).is_ok());
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
            assert!(
                validate_pairing(&provisioned(bad, 9100)).is_err(),
                "{bad:?}"
            );
        }
        assert!(validate_pairing(&provisioned("ABCD-EFGH", 0)).is_err());
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
            .provision("ada", "ada@example.com", "{\"refresh_token\":\"secret\"}")
            .await
            .unwrap_err();
        assert!(matches!(err, WardenError::NotCiphertext));
    }

    /// The label reaches a URL path and, on the far side, a unit name and a
    /// directory. This client refuses one it would not have built itself.
    #[tokio::test]
    async fn refuses_a_label_it_would_not_have_validated() {
        let w = offline_client();
        for bad in ["../../etc", "ada/../root", "ada%2f..", "WWW", "a b", ""] {
            let armor = format!("{}\nbody\n", crate::seal::ARMOR_HEADER);
            assert!(
                matches!(
                    w.provision(bad, "ada@example.com", &armor).await,
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
