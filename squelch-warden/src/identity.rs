//! Per-tenant age identities: minted here, written straight into the tenant's
//! Secret, and forgotten.
//!
//! This is the one place the warden holds key material, and the rules around it
//! are the whole reason the hosted pitch can say what it says:
//!
//! - **One identity per tenant.** The v1 box had a single identity for every
//!   mailbox on it, which meant "encryption at rest" and nothing more. A
//!   per-tenant key means a leaked Secret is one tenant, and a tenant's key can
//!   be destroyed without touching anyone else's.
//! - **In memory only.** [`TenantIdentity`] exists between `mint` and the apply
//!   that puts it in a Secret. Nothing writes it to a file, a log, a response
//!   body, or the warden's own state, because the warden has no state.
//! - **No escrow.** The control plane learns the RECIPIENT and never the
//!   identity, so it can seal and can never open. If a tenant's Secret is lost,
//!   the recovery is re-consent with Google, not a key we kept somewhere.
//!
//! [`TenantIdentity`] has a manual `Debug` and no `Display`, `Serialize` or
//! `Clone`: the only way to get the secret out is [`TenantIdentity::expose`],
//! which is called exactly once, by [`crate::objects::identity_secret`].

use age::secrecy::ExposeSecret;

/// A freshly minted tenant key pair.
pub struct TenantIdentity {
    /// `AGE-SECRET-KEY-1...`. Private, and never printed.
    identity: String,
    /// `age1...`. Public: it goes back to the control plane in the 201, and it
    /// is the only half of this pair that ever leaves the cluster.
    recipient: String,
}

impl TenantIdentity {
    /// Generate a new x25519 identity from the OS CSPRNG.
    pub fn mint() -> Self {
        let key = age::x25519::Identity::generate();
        Self {
            identity: key.to_string().expose_secret().to_string(),
            recipient: key.to_public().to_string(),
        }
    }

    /// The public recipient, safe to log, safe to return, safe to paste.
    pub fn recipient(&self) -> &str {
        &self.recipient
    }

    /// The private half. Named `expose` rather than `identity` so that a reader
    /// of a call site sees what it is doing; there is exactly one call site,
    /// and it hands these bytes to a Secret's data map.
    pub fn expose(&self) -> &str {
        &self.identity
    }
}

impl std::fmt::Debug for TenantIdentity {
    /// Manual, and the reason is the one field. A derived `Debug` here would
    /// put a tenant's private key into the first `dbg!` anyone reached for.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantIdentity")
            .field("identity", &"<redacted>")
            .field("recipient", &self.recipient)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_a_usable_age_pair() {
        let id = TenantIdentity::mint();
        assert!(id.recipient().starts_with("age1"), "{}", id.recipient());
        assert!(id.expose().starts_with("AGE-SECRET-KEY-1"));
        // The recipient parses as what the control plane will seal to.
        assert!(id.recipient().parse::<age::x25519::Recipient>().is_ok());
    }

    #[test]
    fn every_tenant_gets_a_different_key() {
        let a = TenantIdentity::mint();
        let b = TenantIdentity::mint();
        assert_ne!(a.recipient(), b.recipient());
        assert_ne!(a.expose(), b.expose());
    }

    /// The pair really is a pair: what the control plane seals to the recipient
    /// is what the daemon opens with the identity. Proven here rather than
    /// assumed, because a mismatch would surface as a tenant that cannot sync
    /// and no error anyone could trace back to this file.
    #[test]
    fn what_the_recipient_seals_the_identity_opens() {
        let minted = TenantIdentity::mint();
        let recipient: age::x25519::Recipient = minted.recipient().parse().unwrap();
        let armored = age::encrypt_and_armor(&recipient, b"tenant credentials").unwrap();
        assert!(armored.starts_with(crate::validate::AGE_ARMOR_BEGIN));

        let identity: age::x25519::Identity = minted.expose().parse().unwrap();
        let opened = age::decrypt(&identity, armored.as_bytes()).unwrap();
        assert_eq!(opened, b"tenant credentials");
    }

    #[test]
    fn debug_redacts_the_private_half() {
        let id = TenantIdentity::mint();
        let rendered = format!("{id:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(id.expose()));
        // The public half is fine, and is what makes the line useful.
        assert!(rendered.contains(id.recipient()));
    }
}
