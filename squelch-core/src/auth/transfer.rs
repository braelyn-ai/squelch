//! The credential transfer blob: one whole credential set, carried between
//! machines as a single pasteable line.
//!
//! Google delivers an authorization code only to loopback on the machine running
//! the browser, and a Desktop-type client may register no other redirect target,
//! so a headless daemon (docker on a NAS, a VPS) cannot receive a code at all.
//! This module moves the TOKEN instead: consent runs where a browser is, and the
//! refresh token it yields is bound to the OAuth client and the granted scopes
//! rather than to a machine. It is therefore equally valid on the daemon
//! PROVIDED both sides use the same `client_id`/`client_secret` — a refresh
//! against a different client is `invalid_client`, whatever the token says.
//!
//! SECURITY: an encoded blob is a **live refresh token in plaintext**. base64url
//! is an encoding, not encryption: whoever holds the string and the OAuth client
//! reads the mailbox until the grant is revoked at
//! <https://myaccount.google.com/permissions>. It belongs on stdout, on stdin,
//! and nowhere else — not argv, not a log line, not a chat scrollback that
//! outlives the paste. Nothing here derives `Debug`; the hand-written ones
//! redact.

use crate::credentials::{CredentialKind, StoredToken};
use crate::error::{CoreError, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The literal head of every blob. Load-bearing twice over: it lets a mis-paste
/// (a URL, half a token, the wrong clipboard entry) be refused by name instead
/// of as a serde error, and it makes the string greppable in a support thread.
pub const TRANSFER_PREFIX: &str = "squelch-cred-v1.";

/// The payload version the prefix above promises. Carried inside the JSON too,
/// so a hand-edited payload is caught rather than half-understood.
pub const TRANSFER_VERSION: u32 = 1;

/// One credential in a blob: which slot it belongs in, and the token itself.
#[derive(Clone, Serialize, Deserialize)]
pub struct TransferCredential {
    /// The slot this token was minted for. Read and Write never share one.
    pub kind: CredentialKind,
    pub token: StoredToken,
}

/// A blob's decoded contents: which mailbox Google named, and one entry per
/// credential.
#[derive(Clone, Serialize, Deserialize)]
pub struct CredentialTransfer {
    pub version: u32,
    /// The mailbox Google reported for these tokens, as discovered by the
    /// exchange. The importing daemon holds this against its own configured
    /// account: a token stored under the wrong address syncs a stranger's mail.
    pub account: String,
    pub credentials: Vec<TransferCredential>,
}

impl CredentialTransfer {
    /// Build a transfer at the current version.
    pub fn new(account: String, credentials: Vec<TransferCredential>) -> Self {
        Self {
            version: TRANSFER_VERSION,
            account,
            credentials,
        }
    }
}

/// Redacting `Debug`: the token is the whole secret, and a derived impl is how
/// it ends up in the first error someone formats with `{:?}`.
impl fmt::Debug for TransferCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransferCredential")
            .field("kind", &self.kind)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Redacting `Debug`. The account is not a secret and naming it is the point of
/// every message this type appears in; the tokens under it never print.
impl fmt::Debug for CredentialTransfer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialTransfer")
            .field("version", &self.version)
            .field("account", &self.account)
            .field("credentials", &self.credentials)
            .finish()
    }
}

/// Encode a transfer as the single line that crosses machines.
///
/// One line with no internal whitespace is what survives a copy-paste through a
/// terminal, an SSH session, and a chat client: any of them may rewrap or eat a
/// line break, and base64url survives a shell without quoting.
pub fn encode_transfer(transfer: &CredentialTransfer) -> Result<String> {
    if transfer.credentials.is_empty() {
        return Err(CoreError::Credential(
            "refusing to encode a credential blob with no credentials in it".to_string(),
        ));
    }
    let json = serde_json::to_vec(transfer)
        .map_err(|e| CoreError::Credential(format!("serializing the credential blob: {e}")))?;
    Ok(format!("{TRANSFER_PREFIX}{}", URL_SAFE_NO_PAD.encode(json)))
}

/// Decode a pasted blob.
///
/// Surrounding whitespace is trimmed because a paste carries whatever the
/// terminal added; internal whitespace is not, and falls out as a base64 error.
/// No failure here echoes the payload: the thing being rejected may still be a
/// live token, and half of it in a log is enough to matter.
pub fn decode_transfer(blob: &str) -> Result<CredentialTransfer> {
    let blob = blob.trim();
    let payload = blob.strip_prefix(TRANSFER_PREFIX).ok_or_else(|| {
        CoreError::Credential(format!(
            "this is not a squelch credential blob: it does not start with `{TRANSFER_PREFIX}`. \
             Paste the whole single line that `squelchd auth --export` printed."
        ))
    })?;

    let json = URL_SAFE_NO_PAD.decode(payload).map_err(|_| {
        CoreError::Credential(
            "the credential blob is damaged after its prefix (not valid base64url); it was \
             probably truncated or line-wrapped in transit. Re-run `squelchd auth --export` \
             and paste the whole single line."
                .to_string(),
        )
    })?;

    // serde's `Display` embeds the offending value, which in this payload is a
    // token. Only the error class and position.
    let transfer: CredentialTransfer = serde_json::from_slice(&json).map_err(|e| {
        CoreError::Credential(format!(
            "the credential blob decoded to something this build cannot read: {:?} error at \
             line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;

    if transfer.version != TRANSFER_VERSION {
        return Err(CoreError::Credential(format!(
            "this credential blob is version {}, and this build speaks version {}; \
             update whichever side is older and export again",
            transfer.version, TRANSFER_VERSION
        )));
    }
    if transfer.credentials.is_empty() {
        return Err(CoreError::Credential(
            "this credential blob carries no credentials, so importing it would store nothing; \
             re-run `squelchd auth --export`"
                .to_string(),
        ));
    }
    // Two entries for one slot means the second silently overwrites the first,
    // and which token survives depends on iteration order. Refuse instead.
    for (i, cred) in transfer.credentials.iter().enumerate() {
        if transfer.credentials[..i]
            .iter()
            .any(|c| c.kind == cred.kind)
        {
            return Err(CoreError::Credential(format!(
                "this credential blob carries two {:?} credentials, so one would silently \
                 overwrite the other; re-run `squelchd auth --export`",
                cred.kind
            )));
        }
    }
    Ok(transfer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};

    fn token(access: &str, refresh: Option<&str>) -> StoredToken {
        StoredToken {
            access_token: access.to_string(),
            refresh_token: refresh.map(|r| r.to_string()),
            expires_at: Some(Utc::now() + ChronoDuration::hours(1)),
        }
    }

    fn both_kinds() -> CredentialTransfer {
        CredentialTransfer::new(
            "you@gmail.com".to_string(),
            vec![
                TransferCredential {
                    kind: CredentialKind::Write,
                    token: token("write-access", Some("write-refresh")),
                },
                TransferCredential {
                    kind: CredentialKind::Read,
                    token: token("read-access", Some("read-refresh")),
                },
            ],
        )
    }

    #[test]
    fn round_trips_every_field() {
        let before = both_kinds();
        let blob = encode_transfer(&before).unwrap();
        let after = decode_transfer(&blob).unwrap();

        assert_eq!(after.version, TRANSFER_VERSION);
        assert_eq!(after.account, before.account);
        assert_eq!(after.credentials.len(), 2);
        assert_eq!(after.credentials[0].kind, CredentialKind::Write);
        assert_eq!(after.credentials[0].token, before.credentials[0].token);
        assert_eq!(after.credentials[1].kind, CredentialKind::Read);
        assert_eq!(after.credentials[1].token, before.credentials[1].token);
    }

    /// The blob crosses a terminal, an SSH session, and a chat client. Anything
    /// that rewraps or splits on whitespace would corrupt it.
    #[test]
    fn the_blob_is_one_line_with_no_internal_whitespace() {
        let blob = encode_transfer(&both_kinds()).unwrap();
        assert!(blob.starts_with(TRANSFER_PREFIX), "{blob}");
        assert!(
            !blob.chars().any(char::is_whitespace),
            "a blob must survive a copy-paste: {blob}"
        );
        // Unquoted on a command line and unescaped in a URL.
        assert!(
            blob[TRANSFER_PREFIX.len()..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    /// Whatever the terminal added around the paste is not part of the blob.
    #[test]
    fn surrounding_whitespace_is_trimmed_off_a_paste() {
        let blob = encode_transfer(&both_kinds()).unwrap();
        let pasted = format!("  \n\t{blob}\n\n");
        assert_eq!(decode_transfer(&pasted).unwrap().account, "you@gmail.com");
    }

    /// A mis-paste is refused by name rather than as a serde error, which is the
    /// whole reason the prefix exists.
    #[test]
    fn a_blob_without_the_prefix_is_refused_by_name() {
        for wrong in [
            "",
            "hello",
            "https://accounts.google.com/o/oauth2/v2/auth?client_id=x",
            "squelch-cred-v2.eyJ2IjoxfQ",
            // The payload alone, prefix stripped by a careless copy.
            &encode_transfer(&both_kinds()).unwrap()[TRANSFER_PREFIX.len()..],
        ] {
            let err = decode_transfer(wrong).unwrap_err().to_string();
            assert!(err.contains(TRANSFER_PREFIX), "{wrong}: {err}");
        }
    }

    #[test]
    fn a_damaged_payload_is_refused() {
        // Right prefix, not base64url.
        let err = decode_transfer("squelch-cred-v1.not base64!!")
            .unwrap_err()
            .to_string();
        assert!(err.contains("base64url"), "{err}");

        // Valid base64url, not JSON.
        let junk = format!("{TRANSFER_PREFIX}{}", URL_SAFE_NO_PAD.encode("not json"));
        assert!(decode_transfer(&junk).is_err());

        // Valid JSON, wrong shape.
        let wrong_shape = format!("{TRANSFER_PREFIX}{}", URL_SAFE_NO_PAD.encode(r#"{"a":1}"#));
        assert!(decode_transfer(&wrong_shape).is_err());
    }

    /// A newer exporter's blob is refused with the fix, not read as far as it
    /// happens to parse.
    #[test]
    fn a_wrong_version_is_refused_and_names_both() {
        let payload = r#"{"version":2,"account":"you@gmail.com","credentials":[]}"#;
        let blob = format!("{TRANSFER_PREFIX}{}", URL_SAFE_NO_PAD.encode(payload));
        let err = decode_transfer(&blob).unwrap_err().to_string();
        assert!(err.contains("version 2"), "{err}");
        assert!(err.contains("version 1"), "{err}");
    }

    /// An empty blob would import "successfully" and store nothing, which is the
    /// failure nobody connects back to this paste.
    #[test]
    fn an_empty_credential_list_is_refused_both_ways() {
        let empty = CredentialTransfer::new("you@gmail.com".to_string(), vec![]);
        assert!(encode_transfer(&empty).is_err());

        let payload = r#"{"version":1,"account":"you@gmail.com","credentials":[]}"#;
        let blob = format!("{TRANSFER_PREFIX}{}", URL_SAFE_NO_PAD.encode(payload));
        let err = decode_transfer(&blob).unwrap_err().to_string();
        assert!(err.contains("no credentials"), "{err}");
    }

    /// Two entries for one slot make the stored token depend on iteration order.
    #[test]
    fn duplicate_kinds_are_refused() {
        let dup = CredentialTransfer::new(
            "you@gmail.com".to_string(),
            vec![
                TransferCredential {
                    kind: CredentialKind::Read,
                    token: token("a", Some("ra")),
                },
                TransferCredential {
                    kind: CredentialKind::Read,
                    token: token("b", Some("rb")),
                },
            ],
        );
        let err = decode_transfer(&encode_transfer(&dup).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("two Read credentials"), "{err}");
    }

    /// `{:?}` on anything in this module must not print token material. This is
    /// the one type whose whole content is a secret.
    #[test]
    fn debug_never_prints_token_material() {
        let transfer = both_kinds();
        let printed = format!("{transfer:?}");
        for secret in [
            "write-access",
            "write-refresh",
            "read-access",
            "read-refresh",
        ] {
            assert!(!printed.contains(secret), "{secret} leaked into: {printed}");
        }
        assert!(printed.contains("<redacted>"), "{printed}");
        // The mailbox is not a secret, and every message about a blob names it.
        assert!(printed.contains("you@gmail.com"), "{printed}");
    }

    /// A blob decoded on the daemon must be byte-identical to what was minted,
    /// including an absent expiry and an absent refresh token. The refusal for
    /// the latter belongs to the importer, not the codec.
    #[test]
    fn optional_fields_survive_the_trip() {
        let sparse = CredentialTransfer::new(
            "you@gmail.com".to_string(),
            vec![TransferCredential {
                kind: CredentialKind::Read,
                token: StoredToken {
                    access_token: "a".to_string(),
                    refresh_token: None,
                    expires_at: None,
                },
            }],
        );
        let back = decode_transfer(&encode_transfer(&sparse).unwrap()).unwrap();
        assert_eq!(back.credentials[0].token.refresh_token, None);
        assert_eq!(back.credentials[0].token.expires_at, None);
    }
}
