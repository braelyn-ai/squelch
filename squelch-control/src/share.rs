//! Share tokens: the credential a tenant's own daemon presents to mint an
//! invite for a friend.
//!
//! WHY A TENANT CAN MINT AT ALL. Until now the only thing that could turn a
//! code out of [`crate::invites::mint`] was an operator with the admin token,
//! because a code is a free tenant and the launch gate is the whole point. A
//! share token widens that to "and a mailbox we already provisioned, a few at a
//! time", which is a different trade and gets its own credential rather than a
//! carve-out in the admin one: the two can be revoked, counted, and reasoned
//! about separately, and no route reachable with a share token can do anything
//! an operator does.
//!
//! HOW IT DIFFERS FROM AN INVITE CODE, and why the shape is not shared:
//!
//! - An invite code is TYPED BY A HUMAN off a screen or out of an email, so it
//!   is Crockford base32, dashed, and 80 bits, and [`crate::invites::normalize`]
//!   forgives every way a person retypes it.
//! - A share token is read by a daemon out of an environment variable and
//!   presented as a bearer. Nobody spells it, so it is 256 bits of base64url
//!   with no folding and no forgiveness, and one wrong byte is one wrong token.
//!
//! WHAT THEY DO SHARE is the discipline, which is the same three rules
//! [`crate::invites`] spells out:
//!
//! 1. **Plaintext exists once.** Minted here, printed to stdout by the CLI on
//!    its way to the warden's Secret, and thereafter only a lowercase hex
//!    SHA-256 in `tenants.share_token_hash`. Nothing writes the token, or its
//!    hash, to a log line or a `Debug`.
//! 2. **Verification is a point lookup** ([`crate::store::ControlStore::tenant_by_share_token`])
//!    on a unique index, so the work done is independent of how close a guess
//!    was.
//! 3. **Failure is one answer.** Unknown, revoked, and torn-down are
//!    indistinguishable to the caller.
//!
//! THE PREFIX IS NOT A SECRET AND NOT A CHECK. `pbs_` exists so that a token
//! found loose in a Secret, an env dump, or a support paste is recognizable as
//! what it is and can be revoked; it is matched nowhere, because a shape check
//! ahead of the lookup would answer "that was not even a token" faster than "no
//! such token", which is the oracle rule 2 exists to avoid.

use sha2::{Digest, Sha256};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

/// Entropy behind one token. 256 bits: this is a bearer with no expiry and no
/// rate limit tight enough to matter to an offline guesser, so it is sized to
/// be the thing nobody attacks.
const TOKEN_BYTES: usize = 32;

/// What every minted token starts with. See the module header: an aid to a
/// human reading a Secret, never a validity check.
const PREFIX: &str = "pbs_";

/// How far back the quota counts. A ROLLING WINDOW rather than a lifetime cap,
/// deliberately: a lifetime cap tells the product's happiest user, the one who
/// has already brought five people, that they are finished, and it can only be
/// raised by an operator editing a number. A window bounds how fast codes can
/// leave without ever closing the door.
pub const QUOTA_WINDOW_DAYS: i64 = 30;

/// How many codes one tenant may mint inside [`QUOTA_WINDOW_DAYS`].
///
/// Ten is a person sharing with their friends. It is not a growth channel, and
/// it is deliberately small enough that a stolen share token is a nuisance
/// (ten free tenants, traceable to the mailbox they were minted under, all of
/// them revocable) rather than an incident.
pub const QUOTA_PER_WINDOW: i64 = 10;

/// A minted token, plaintext and hash together, handed to exactly one caller.
/// No `Debug`: the plaintext must not be formattable into a log line by
/// accident, and the hash is not for logs either.
pub struct MintedToken {
    pub token: String,
    pub token_hash: String,
}

/// Mint one share token from OS entropy.
pub fn mint() -> Result<MintedToken, std::io::Error> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    let token = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
    let token_hash = hash(&token);
    Ok(MintedToken { token, token_hash })
}

/// Lowercase hex SHA-256 of the token exactly as presented. The ONLY form of a
/// share token that touches disk.
///
/// NO NORMALIZATION, which is the one place this deliberately departs from
/// [`crate::invites::hash`]: nothing types a share token, so there is no
/// misreading to forgive, and case folding a bearer would throw away two bits
/// per character for nobody's benefit.
pub fn hash(token: &str) -> String {
    use std::fmt::Write as _;
    Sha256::digest(token.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            // Writing to a String cannot fail.
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_a_prefixed_high_entropy_token() {
        let m = mint().unwrap();
        assert!(m.token.starts_with(PREFIX), "{}", m.token);
        // 32 bytes is 43 unpadded base64url characters, plus the prefix.
        assert_eq!(m.token.len(), PREFIX.len() + 43);
        assert!(m.token.bytes().all(|b| b.is_ascii_graphic()), "{}", m.token);
        assert_eq!(m.token_hash.len(), 64);
        assert!(m.token_hash.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn mints_distinct_tokens() {
        let a = mint().unwrap();
        let b = mint().unwrap();
        assert_ne!(a.token, b.token);
        assert_ne!(a.token_hash, b.token_hash);
    }

    /// The departure from the invite code, asserted so it cannot be "fixed"
    /// into forgiveness later: a bearer is matched byte for byte.
    #[test]
    fn hashes_exactly_what_it_is_given() {
        let m = mint().unwrap();
        assert_eq!(hash(&m.token), m.token_hash);
        assert_ne!(hash(&m.token.to_uppercase()), m.token_hash);
        assert_ne!(hash(&format!(" {} ", m.token)), m.token_hash);
    }

    /// A share token must never be mistakable for an invite code, in either
    /// direction: they are different credentials with different powers, and the
    /// prefix plus the alphabet is what keeps a paste into the wrong field a
    /// plain refusal.
    #[test]
    fn is_not_shaped_like_an_invite_code() {
        let m = mint().unwrap();
        assert!(!crate::invites::is_plausible(&m.token), "{}", m.token);
    }
}
