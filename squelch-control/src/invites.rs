//! Invite codes: the launch gate. A code is minted at the CLI, read aloud or
//! pasted into the signup form once, and spent.
//!
//! THE SAME THREE RULES AS THE DAEMON'S PAIRING CODES (squelch-core's
//! `store::sqlite::device_tokens`), for the same reasons:
//!
//! 1. **Plaintext exists once.** The code is generated, printed to stdout, and
//!    thereafter lives only as a lowercase hex SHA-256. Nothing here writes a
//!    code, or its hash, to a log line or a `Debug`.
//! 2. **Verification is a point lookup**, so the work done is independent of how
//!    close a guess was.
//! 3. **Failure is one answer.** Unknown, already used, and revoked are
//!    indistinguishable to the caller, which is why [`crate::store`] hands back
//!    an `Option` rather than a reason.
//!
//! The alphabet and the `XXXX-XXXX` shape are the pairing code's, deliberately:
//! a user who has seen one has seen both. It is copied rather than imported
//! because core's is private to the store module and coupling the hosted signup
//! gate to the daemon's device-pairing internals would be the wrong seam.

use sha2::{Digest, Sha256};

/// Crockford base32: no `I`, `L`, `O`, or `U`, so a code read off a screen and
/// typed on a phone has no ambiguous character and no accidental profanity.
/// Exactly 32 symbols, so five random bits select one with no rejection
/// sampling and no bias.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Symbols per code: 8 x 5 bits = 40 bits. Short enough to read aloud, and safe
/// at that size because a code is single-use and the signup route is rate
/// limited per client. Unlike a pairing code it does not expire on a clock, so
/// the operator revokes what they do not want outstanding.
pub const CODE_LEN: usize = 8;

/// A minted code, plaintext and hash together, handed to exactly one caller.
/// No `Debug`: the plaintext must not be formattable into a log line by
/// accident, and the hash is not for logs either.
pub struct MintedInvite {
    pub code: String,
    pub code_hash: String,
}

/// Mint one code from OS entropy.
pub fn mint() -> Result<MintedInvite, std::io::Error> {
    let mut bytes = [0u8; 5];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    let bits = bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
    let raw: String = (0..CODE_LEN)
        .map(|i| {
            let shift = 5 * (CODE_LEN - 1 - i);
            ALPHABET[((bits >> shift) & 0x1f) as usize] as char
        })
        .collect();
    let code_hash = hash(&raw);
    Ok(MintedInvite {
        code: format_code(&raw),
        code_hash,
    })
}

/// `XXXXXXXX` -> `XXXX-XXXX`. Purely presentational: [`normalize`] strips the
/// dash before hashing, so this never changes what is stored.
fn format_code(code: &str) -> String {
    match code.len() {
        CODE_LEN => format!("{}-{}", &code[..4], &code[4..]),
        // Nothing mints another length; printing it whole is a strictly safer
        // failure than slicing at a byte that might not be a char boundary.
        _ => code.to_string(),
    }
}

/// The form a code is hashed in: uppercase, whitespace and dashes stripped, and
/// the three Crockford confusables folded to the digits they look like. So
/// `xxxx-xxxx`, `XXXX XXXX`, and `XXXXXXXX` are one credential, and a user who
/// reads `0` as `O` still gets in.
pub fn normalize(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(|c| c.to_uppercase())
        .map(|c| match c {
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        })
        .collect()
}

/// Lowercase hex SHA-256 of the normalized code. The ONLY form of a code that
/// touches disk.
pub fn hash(code: &str) -> String {
    use std::fmt::Write as _;
    Sha256::digest(normalize(code).as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            // Writing to a String cannot fail.
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Whether a submitted string is even shaped like a code. Cheap enough to run
/// before the store lookup and, more importantly, it costs the SAME answer:
/// callers must map a shape failure onto the identical refusal a wrong code
/// gets, or this becomes an oracle for the code space.
pub fn is_plausible(input: &str) -> bool {
    let n = normalize(input);
    n.len() == CODE_LEN && n.bytes().all(|b| ALPHABET.contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_a_formatted_crockford_code() {
        let m = mint().unwrap();
        assert_eq!(m.code.len(), CODE_LEN + 1, "{}", m.code);
        assert_eq!(&m.code[4..5], "-");
        assert!(is_plausible(&m.code));
        assert_eq!(m.code_hash.len(), 64);
        assert!(m.code_hash.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn mints_distinct_codes() {
        let a = mint().unwrap();
        let b = mint().unwrap();
        assert_ne!(a.code, b.code);
        assert_ne!(a.code_hash, b.code_hash);
    }

    /// The stored hash is over the NORMALIZED code, which is what makes all the
    /// ways a human retypes it the same credential.
    #[test]
    fn hashes_every_spelling_of_one_code_the_same() {
        let canonical = hash("ABCD-EFGH");
        for spelling in ["ABCDEFGH", "abcd-efgh", "  abcd efgh ", "AbCd-EfGh"] {
            assert_eq!(hash(spelling), canonical, "{spelling}");
        }
    }

    /// Crockford's whole point: the characters that look like digits ARE the
    /// digits, so a misread does not cost the user their one code.
    #[test]
    fn folds_crockford_confusables() {
        assert_eq!(normalize("i1l"), "111");
        assert_eq!(normalize("O0o"), "000");
        assert_eq!(hash("1ABCDEFG"), hash("IABCDEFG"));
    }

    #[test]
    fn rejects_implausible_shapes_before_the_store() {
        for bad in ["", "ABC", "ABCDEFGHI", "ABCDEFGU", "ABCDEF-G", "@@@@@@@@"] {
            assert!(!is_plausible(bad), "{bad:?}");
        }
        assert!(is_plausible("ABCD-EFGH"));
        // `U` is not in the alphabet, so it is not a code even though it is the
        // right length after normalization.
        assert!(!is_plausible("UUUUUUUU"));
    }
}
