//! Sealing a tenant's credential to the VPS.
//!
//! This is the module the trust split lives in. The control plane holds an age
//! RECIPIENT, which is a public key: it can encrypt to the box and it cannot
//! decrypt anything, ever, because there is no identity here to decrypt with.
//! The ciphertext travels to the warden, which writes it verbatim into the
//! tenant's credential file, and only the daemon (handed the identity file by
//! systemd, via `SQUELCH_CRED_AGE_IDENTITY`) can open it.
//!
//! The plaintext is [`squelch_core::credentials::StoredToken`] as JSON, which
//! is byte-for-byte what the daemon's file credential backend expects to read
//! after decrypting. Nothing about the format is invented here.
//!
//! ASCII armor rather than binary: the ciphertext rides inside a JSON body to
//! the warden and is written to a file that an operator may well `cat` while
//! debugging. Armor also gives the reader on the other side a header to detect,
//! which is how core tells an encrypted credential file from a legacy plaintext
//! one.

use std::io::Write as _;

use age::armor::{ArmoredWriter, Format};
use age::x25519::Recipient;
use squelch_core::credentials::StoredToken;

/// The first line of an age ASCII-armored file. Callers assert on it (and core
/// detects with it), so it is named once here.
pub const ARMOR_HEADER: &str = "-----BEGIN AGE ENCRYPTED FILE-----";

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    /// The configured recipient is not an age public key. Loud, and at boot:
    /// discovering this at the first signup would mean a user has already
    /// granted consent with nowhere to put the result.
    #[error("not an age recipient (expected an `age1...` public key): {0}")]
    Recipient(String),
    /// Serializing the token. Cannot happen for our own struct of strings, but
    /// it is not worth an `unwrap` on the one path that handles a live refresh
    /// token.
    #[error("serializing the credential")]
    Serialize,
    /// Encrypting. The message deliberately carries no context beyond the
    /// operation: everything in scope at the call site is secret.
    #[error("sealing the credential to the box recipient failed")]
    Encrypt,
}

/// Parse a recipient string (`age1...`). Used at boot to validate config and
/// once per signup to build the encryptor.
pub fn parse_recipient(s: &str) -> Result<Recipient, SealError> {
    s.trim()
        .parse::<Recipient>()
        // age's parse error is a `&'static str` describing the shape problem;
        // the INPUT is never echoed. A recipient is public, but a mistyped
        // environment variable is not necessarily a recipient at all, and
        // reflecting whatever was in the variable into a log is how a secret
        // pasted into the wrong slot gets published.
        .map_err(|e| SealError::Recipient(e.to_string()))
}

/// Encrypt a [`StoredToken`] to `recipient`, returning ASCII armor.
///
/// The plaintext JSON is built, encrypted, and dropped inside this function.
/// It is never returned, logged, or written anywhere: the ONLY thing that
/// leaves is ciphertext.
pub fn seal_token(recipient: &Recipient, token: &StoredToken) -> Result<String, SealError> {
    let json = serde_json::to_vec(token).map_err(|_| SealError::Serialize)?;
    let armored = seal_bytes(recipient, &json);
    // `json` holds a refresh token. Rust will drop it here either way; this is
    // the note that says so on purpose rather than by luck.
    drop(json);
    armored
}

/// Encrypt arbitrary bytes to `recipient`, returning ASCII armor. Split out so
/// the encryption itself can be tested against a generated identity without a
/// token in the picture.
pub fn seal_bytes(recipient: &Recipient, plaintext: &[u8]) -> Result<String, SealError> {
    let encryptor = age::Encryptor::with_recipients(std::iter::once(recipient as _))
        .map_err(|_| SealError::Encrypt)?;

    let mut out = Vec::new();
    let armor =
        ArmoredWriter::wrap_output(&mut out, Format::AsciiArmor).map_err(|_| SealError::Encrypt)?;
    let mut writer = encryptor.wrap_output(armor).map_err(|_| SealError::Encrypt)?;
    writer
        .write_all(plaintext)
        .map_err(|_| SealError::Encrypt)?;
    // TWO finishes, and both matter: the inner one flushes age's last STREAM
    // chunk, the outer one writes the armor footer. Skipping either produces a
    // file that looks plausible and cannot be decrypted.
    writer
        .finish()
        .map_err(|_| SealError::Encrypt)?
        .finish()
        .map_err(|_| SealError::Encrypt)?;

    String::from_utf8(out).map_err(|_| SealError::Encrypt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn token() -> StoredToken {
        StoredToken {
            access_token: "ya29.ACCESS-TOKEN-PLAINTEXT".into(),
            refresh_token: Some("1//REFRESH-TOKEN-PLAINTEXT".into()),
            expires_at: Some(chrono::Utc::now()),
        }
    }

    #[test]
    fn parses_a_recipient_and_refuses_anything_else() {
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public().to_string();
        assert!(parse_recipient(&recipient).is_ok());
        assert!(parse_recipient(&format!("  {recipient}  ")).is_ok());

        // An IDENTITY is not a recipient. Pointing the control plane at a
        // private key must fail loudly rather than "work". age keeps the
        // identity's string in a `SecretBox`, which is exactly why this is the
        // only place in the crate that ever exposes one.
        use age::secrecy::ExposeSecret as _;
        let secret = identity.to_string();
        for bad in ["", "age1", "not-a-key", secret.expose_secret().trim()] {
            assert!(parse_recipient(bad).is_err(), "{bad:?}");
        }
    }

    /// The one test that proves the trust split end to end: what the control
    /// plane produces is armor, carries no plaintext, and opens ONLY with the
    /// identity that never leaves the VPS.
    #[test]
    fn seals_a_token_that_only_the_identity_can_open() {
        let identity = age::x25519::Identity::generate();
        let recipient = parse_recipient(&identity.to_public().to_string()).unwrap();
        let t = token();

        let armor = seal_token(&recipient, &t).unwrap();
        assert!(armor.starts_with(ARMOR_HEADER), "{armor}");
        assert!(armor.contains("-----END AGE ENCRYPTED FILE-----"));
        assert!(!armor.contains("REFRESH-TOKEN-PLAINTEXT"));
        assert!(!armor.contains("ACCESS-TOKEN-PLAINTEXT"));

        // The armor has to be stripped before the age header is parsed, which
        // is the same thing the daemon's reader does on the other side.
        let decryptor =
            age::Decryptor::new(age::armor::ArmoredReader::new(armor.as_bytes())).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .unwrap();
        let mut plaintext = String::new();
        reader.read_to_string(&mut plaintext).unwrap();

        let round_tripped: StoredToken = serde_json::from_str(&plaintext).unwrap();
        assert_eq!(round_tripped, t);
    }

    /// A different box's identity gets nothing. Trivially true of age, asserted
    /// here because "encrypted to the right recipient" is the property the
    /// whole hosted design rests on.
    #[test]
    fn another_identity_cannot_open_it() {
        let ours = age::x25519::Identity::generate();
        let theirs = age::x25519::Identity::generate();
        let recipient = parse_recipient(&ours.to_public().to_string()).unwrap();

        let armor = seal_token(&recipient, &token()).unwrap();
        let decryptor =
            age::Decryptor::new(age::armor::ArmoredReader::new(armor.as_bytes())).unwrap();
        assert!(
            decryptor
                .decrypt(std::iter::once(&theirs as &dyn age::Identity))
                .is_err()
        );
    }

    /// Armor is line-wrapped base64 between two markers, so it survives a JSON
    /// body and a file an operator may open. Assert the shape rather than the
    /// bytes, which are random per encryption.
    #[test]
    fn the_armor_is_plain_text_lines() {
        let identity = age::x25519::Identity::generate();
        let recipient = parse_recipient(&identity.to_public().to_string()).unwrap();
        let armor = seal_bytes(&recipient, b"hello").unwrap();
        assert!(armor.lines().count() >= 3, "{armor}");
        assert!(armor.is_ascii());
    }
}
