//! Sealing a tenant's credential to THAT TENANT'S own age recipient.
//!
//! This is the module the trust split lives in. The control plane never holds an
//! age identity, and as of wire v2 it does not hold a long-lived recipient
//! either: the warden mints a fresh identity per tenant, keeps it in memory only
//! long enough to write it into that tenant's k8s Secret, and hands this process
//! back the RECIPIENT (the public half) in the 201 of the first call. So the
//! blast radius of one recipient is one mailbox, and a full compromise of this
//! service still opens nothing.
//!
//! THE PLAINTEXT SHAPE IS CORE'S, NOT OURS. It is the exact bytes the daemon's
//! file credential backend reads after decrypting, and it is easy to get subtly
//! wrong: a bare [`StoredToken`] JSON deserializes into an EMPTY slot map
//! without complaint (serde ignores unknown fields), which surfaces days later
//! as "no stored credentials" on a box nobody can easily debug. So the bytes
//! come from [`squelch_core::credentials::credentials_file_plaintext`] and the
//! encryption from [`squelch_core::credentials::seal_credentials_for_recipient`],
//! and this module is the thin, well-commented seam between them.
//!
//! ASCII armor rather than binary: the ciphertext rides inside a JSON body to
//! the warden and is written into a Secret an operator may well look at while
//! debugging. Armor also gives the reader on the other side a header to detect,
//! which is how core tells an encrypted credential file from a legacy plaintext
//! one.

use age::x25519::Recipient;
use squelch_core::credentials::{
    CredentialKind, StoredToken, credentials_file_plaintext, seal_credentials_for_recipient,
};

/// The first line of an age ASCII-armored file. Callers assert on it (and core
/// detects with it), so it is named once here.
pub const ARMOR_HEADER: &str = "-----BEGIN AGE ENCRYPTED FILE-----";

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    /// The recipient is not an age public key. In v2 this string arrives from
    /// the WARDEN, so it is untrusted input and is checked before it is used
    /// rather than trusted because it came from our own service.
    #[error("not an age recipient (expected an `age1...` public key): {0}")]
    Recipient(String),
    /// Rendering the credentials-file plaintext. Cannot happen for our own
    /// struct of strings, but it is not worth an `unwrap` on the one path that
    /// handles a live refresh token.
    #[error("rendering the credential failed")]
    Render,
    /// Encrypting. The message deliberately carries no context beyond the
    /// operation: everything in scope at the call site is secret.
    #[error("sealing the credential to the tenant recipient failed")]
    Encrypt,
}

/// Parse a recipient string (`age1...`).
///
/// Used to shape-check what the warden answers with BEFORE a refresh token is
/// encrypted to it. [`seal_credentials`] parses again inside core; that is one
/// cheap parse of a sixty-character string and it keeps each layer's guarantee
/// local rather than assumed.
pub fn parse_recipient(s: &str) -> Result<Recipient, SealError> {
    s.trim()
        .parse::<Recipient>()
        // age's parse error is a `&'static str` describing the shape problem;
        // the INPUT is never echoed. A recipient is public, but a value that
        // failed to parse as one is not necessarily a recipient at all, and
        // reflecting it into a log is how a secret pasted into the wrong slot
        // gets published.
        .map_err(|e| SealError::Recipient(e.to_string()))
}

/// Seal one tenant's credential into BOTH slots, returning ASCII armor.
///
/// ONE TOKEN, TWO SLOTS. Hosted signup takes a single consent covering
/// readonly + modify + send, so the grant behind it opens the sync path and the
/// action path alike. The daemon does not go looking: its read path loads the
/// Read slot and its write path loads the Write slot, each bound to a
/// [`CredentialKind`] that can never return the other's token. A blob carrying
/// only Read would provision a tenant whose Archive and Send buttons 403 with
/// nothing in the logs but "no stored credentials for the Write slot".
///
/// The two slots are the SAME refresh token by design rather than by accident:
/// Google issued one grant, and splitting a single grant across two slots is
/// exactly what [`squelch_core::credentials::credentials_file_plaintext`] is
/// shaped for (`email` and `email#write`).
///
/// The plaintext is built, encrypted, and dropped inside this function. The ONLY
/// thing that leaves is ciphertext.
pub fn seal_credentials(
    recipient: &str,
    account_email: &str,
    token: &StoredToken,
) -> Result<String, SealError> {
    // Parsed here so a warden that answered with something that is not a
    // recipient fails on THIS line, with the plaintext still in hand and
    // droppable, rather than inside core with a message shaped for a different
    // caller.
    parse_recipient(recipient)?;

    // Core owns the on-disk shape: a slot map keyed by `CredentialKind::slot_key`,
    // which is the bare account email for Read and `email#write` for Write. The
    // clones are core's signature, not a choice; they are two more copies of the
    // refresh token in this function's frame and they die with the frame.
    let plaintext = credentials_file_plaintext(
        account_email,
        &[
            (CredentialKind::Read, token.clone()),
            (CredentialKind::Write, token.clone()),
        ],
    )
    .map_err(|_| SealError::Render)?;

    let armored = seal_credentials_for_recipient(recipient, plaintext.as_bytes())
        .map_err(|_| SealError::Encrypt)?;

    // `plaintext` holds a refresh token. Rust drops it here either way; this is
    // the note that says so on purpose rather than by luck.
    drop(plaintext);

    // The one thing the caller asserts on, asserted here too: a "success" that
    // did not produce armor would be sent to the warden and written verbatim.
    if !armored.starts_with(ARMOR_HEADER) {
        return Err(SealError::Encrypt);
    }
    Ok(armored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    const ACCESS: &str = "ya29.ACCESS-TOKEN-PLAINTEXT";
    const REFRESH: &str = "1//REFRESH-TOKEN-PLAINTEXT";
    const MAILBOX: &str = "ada@example.com";

    fn token() -> StoredToken {
        StoredToken {
            access_token: ACCESS.into(),
            refresh_token: Some(REFRESH.into()),
            expires_at: Some(chrono::Utc::now()),
        }
    }

    /// Open armor with an identity, the way the daemon's reader does.
    fn open(armor: &str, identity: &age::x25519::Identity) -> String {
        let decryptor =
            age::Decryptor::new(age::armor::ArmoredReader::new(armor.as_bytes())).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(identity as &dyn age::Identity))
            .unwrap();
        let mut plaintext = String::new();
        reader.read_to_string(&mut plaintext).unwrap();
        plaintext
    }

    #[test]
    fn parses_a_recipient_and_refuses_anything_else() {
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public().to_string();
        assert!(parse_recipient(&recipient).is_ok());
        assert!(parse_recipient(&format!("  {recipient}  ")).is_ok());

        // An IDENTITY is not a recipient. A warden that answered with a private
        // key must fail loudly rather than "work". age keeps the identity's
        // string in a `SecretBox`, which is exactly why this is the only place
        // in the crate that ever exposes one.
        use age::secrecy::ExposeSecret as _;
        let secret = identity.to_string();
        for bad in ["", "age1", "not-a-key", secret.expose_secret().trim()] {
            assert!(parse_recipient(bad).is_err(), "{bad:?}");
        }
    }

    /// The test that proves the trust split end to end: what the control plane
    /// produces is armor, carries no plaintext, and opens ONLY with the identity
    /// the warden kept.
    #[test]
    fn seals_a_credential_only_that_tenants_identity_can_open() {
        let identity = age::x25519::Identity::generate();
        let armor =
            seal_credentials(&identity.to_public().to_string(), MAILBOX, &token()).unwrap();

        assert!(armor.starts_with(ARMOR_HEADER), "{armor}");
        assert!(armor.contains("-----END AGE ENCRYPTED FILE-----"));
        assert!(!armor.contains(REFRESH));
        assert!(!armor.contains(ACCESS));

        let plaintext = open(&armor, &identity);
        let parsed: serde_json::Value = serde_json::from_str(&plaintext).unwrap();
        // THE SHAPE THAT MATTERS: a slot map keyed by the account email, not a
        // bare token. A bare `StoredToken` here would decrypt into an empty slot
        // map on the daemon and the failure would surface days later.
        assert_eq!(parsed["slots"][MAILBOX]["refresh_token"], REFRESH);
        assert_eq!(parsed["slots"][MAILBOX]["access_token"], ACCESS);
        assert!(parsed.get("refresh_token").is_none(), "{plaintext}");
    }

    /// BOTH SLOTS, from the one consent hosted signup takes. The Write slot is
    /// what the daemon's action path loads, and it is keyed `email#write` by
    /// core's `slot_key`; a blob with only the Read slot reads as "no stored
    /// credentials" on every archive, label, and send.
    #[test]
    fn seals_the_grant_into_both_the_read_and_the_write_slot() {
        let identity = age::x25519::Identity::generate();
        let armor =
            seal_credentials(&identity.to_public().to_string(), MAILBOX, &token()).unwrap();

        let plaintext = open(&armor, &identity);
        let parsed: serde_json::Value = serde_json::from_str(&plaintext).unwrap();

        // The keys, spelled the way core spells them, derived rather than typed.
        let read_key = CredentialKind::Read.slot_key(MAILBOX);
        let write_key = CredentialKind::Write.slot_key(MAILBOX);
        assert_eq!(read_key, MAILBOX);
        assert_eq!(write_key, format!("{MAILBOX}#write"));

        let slots = parsed["slots"].as_object().expect("a slot map");
        assert_eq!(slots.len(), 2, "{plaintext}");
        for key in [&read_key, &write_key] {
            // Both slots carry the REFRESH token: an access token alone dies
            // within the hour, and the write path would then be dead for good.
            assert_eq!(slots[key]["refresh_token"], REFRESH, "{key}");
            assert_eq!(slots[key]["access_token"], ACCESS, "{key}");
        }
    }

    /// Per-tenant keys are the point of v2: another tenant's identity gets
    /// nothing. Trivially true of age, asserted here because "sealed to exactly
    /// the recipient that tenant holds" is the property the design rests on.
    #[test]
    fn another_tenants_identity_cannot_open_it() {
        let ours = age::x25519::Identity::generate();
        let theirs = age::x25519::Identity::generate();
        let armor = seal_credentials(&ours.to_public().to_string(), MAILBOX, &token()).unwrap();

        let decryptor =
            age::Decryptor::new(age::armor::ArmoredReader::new(armor.as_bytes())).unwrap();
        assert!(
            decryptor
                .decrypt(std::iter::once(&theirs as &dyn age::Identity))
                .is_err()
        );
    }

    /// Two seals of the same token to two recipients are two different
    /// ciphertexts, each openable by exactly one identity.
    #[test]
    fn each_tenant_gets_its_own_ciphertext() {
        let a = age::x25519::Identity::generate();
        let b = age::x25519::Identity::generate();
        let armor_a = seal_credentials(&a.to_public().to_string(), MAILBOX, &token()).unwrap();
        let armor_b =
            seal_credentials(&b.to_public().to_string(), "grace@example.com", &token()).unwrap();
        assert_ne!(armor_a, armor_b);
        let opened: serde_json::Value = serde_json::from_str(&open(&armor_b, &b)).unwrap();
        assert_eq!(opened["slots"]["grace@example.com"]["refresh_token"], REFRESH);
    }

    /// A recipient this process will not parse never gets a token encrypted to
    /// it. The refusal happens before any encryption, which is what makes
    /// "the warden's answer is untrusted input" true rather than aspirational.
    #[test]
    fn refuses_to_seal_to_something_that_is_not_a_recipient() {
        for bad in ["", "age1", "not-a-key", "<script>", "age1verylongbutnotakey"] {
            assert!(
                matches!(
                    seal_credentials(bad, MAILBOX, &token()),
                    Err(SealError::Recipient(_))
                ),
                "{bad:?}"
            );
        }
    }

    /// Armor is line-wrapped base64 between two markers, so it survives a JSON
    /// body and a Secret an operator may open. Assert the shape rather than the
    /// bytes, which are random per encryption.
    #[test]
    fn the_armor_is_plain_text_lines() {
        let identity = age::x25519::Identity::generate();
        let armor =
            seal_credentials(&identity.to_public().to_string(), MAILBOX, &token()).unwrap();
        assert!(armor.lines().count() >= 3, "{armor}");
        assert!(armor.is_ascii());
    }
}
