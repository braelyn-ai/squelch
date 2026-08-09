//! Invite codes: the launch gate. A code is minted at the CLI, pasted into the
//! signup form once, and spent.
//!
//! THE SAME THREE RULES AS THE DAEMON'S PAIRING CODES (squelch-core's
//! `store::sqlite::device_tokens`), for the same reasons:
//!
//! 1. **Plaintext exists once.** The code is generated, printed to stdout, and
//!    thereafter lives only as a lowercase hex SHA-256. Nothing here writes a
//!    code, or its hash, to a log line or a `Debug`.
//! 2. **Verification is a point lookup**, so the work done is independent of how
//!    close a guess was.
//! 3. **Failure is one answer.** Unknown, already used, expired, reserved, and
//!    revoked are indistinguishable to the caller, which is why [`crate::store`]
//!    hands back an `Option` rather than a reason.
//!
//! WHERE IT DEPARTS FROM THE PAIRING CODE, and why: an invite is 16 symbols and
//! expires, where a pairing code is 8 and lives for minutes. A pairing code is
//! read aloud, so 40 bits buys the shortest thing that is safe for a credential
//! nobody can grind at: it dies on a clock and only works against a daemon the
//! guesser must already be able to reach. An invite is pasted out of an email,
//! sits in a table for weeks, and one hit against a public form is a free
//! tenant, so 40 bits is a weekend of offline work against a stolen table of
//! hashes. Eighty bits is not, and the extra two groups cost a paste nothing.
//!
//! The alphabet and the dashed shape are still the pairing code's, deliberately:
//! a user who has seen one has seen both. They are copied rather than imported
//! because core's are private to the store module and coupling the hosted signup
//! gate to the daemon's device-pairing internals would be the wrong seam.

use sha2::{Digest, Sha256};

/// Crockford base32: no `I`, `L`, `O`, or `U`, so a code read off a screen and
/// typed on a phone has no ambiguous character and no accidental profanity.
/// Exactly 32 symbols, so five random bits select one with no rejection
/// sampling and no bias.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Symbols per code: 16 x 5 bits = 80 bits, printed `XXXX-XXXX-XXXX-XXXX`.
pub const CODE_LEN: usize = 16;

/// The length codes minted before [`CODE_LEN`] grew. Still accepted, because a
/// code already sitting in somebody's inbox has to keep working until it
/// expires; nothing mints one any more.
pub const LEGACY_CODE_LEN: usize = 8;

/// Characters between dashes. Presentational only.
const GROUP: usize = 4;

/// How long a minted code stays usable, in days, unless the operator overrides
/// it. Long enough to mail out and act on, short enough that a table of hashes
/// stolen today is worth nothing by the time it is ground through.
pub const DEFAULT_TTL_DAYS: i64 = 30;

/// A minted code, plaintext and hash together, handed to exactly one caller.
/// No `Debug`: the plaintext must not be formattable into a log line by
/// accident, and the hash is not for logs either.
pub struct MintedInvite {
    pub code: String,
    pub code_hash: String,
}

/// Mint one code from OS entropy.
pub fn mint() -> Result<MintedInvite, std::io::Error> {
    // One draw, sliced into 5-bit symbols: 16 x 5 bits is 80, which a u128
    // holds whole, so no symbol is correlated with another.
    let mut bytes = [0u8; CODE_LEN * 5 / 8];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    let bits = bytes.iter().fold(0u128, |acc, b| (acc << 8) | u128::from(*b));
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

/// `XXXXXXXXXXXXXXXX` -> `XXXX-XXXX-XXXX-XXXX`. Purely presentational:
/// [`normalize`] strips the dashes before hashing, so this never changes what is
/// stored.
fn format_code(code: &str) -> String {
    // Nothing mints another length or a non-ASCII symbol; printing it whole is a
    // strictly safer failure than slicing at a byte that might not be a char
    // boundary.
    if code.len() != CODE_LEN || !code.is_ascii() {
        return code.to_string();
    }
    (0..CODE_LEN / GROUP)
        .map(|i| &code[i * GROUP..(i + 1) * GROUP])
        .collect::<Vec<_>>()
        .join("-")
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
///
/// Both lengths pass. A code minted before the space grew is still in somebody's
/// inbox, and refusing it here would fail it differently from the way it will
/// eventually fail (expiry), which is the same oracle in a smaller window.
pub fn is_plausible(input: &str) -> bool {
    let n = normalize(input);
    (n.len() == CODE_LEN || n.len() == LEGACY_CODE_LEN)
        && n.bytes().all(|b| ALPHABET.contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `XXXX-XXXX-XXXX-XXXX`: 16 symbols, 80 bits, three dashes.
    #[test]
    fn mints_a_formatted_crockford_code() {
        let m = mint().unwrap();
        assert_eq!(m.code.len(), CODE_LEN + 3, "{}", m.code);
        assert_eq!(m.code.matches('-').count(), 3, "{}", m.code);
        for group in m.code.split('-') {
            assert_eq!(group.len(), GROUP, "{}", m.code);
        }
        assert_eq!(normalize(&m.code).len(), CODE_LEN);
        assert!(is_plausible(&m.code));
        assert_eq!(m.code_hash.len(), 64);
        assert!(m.code_hash.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// Every symbol comes out of one draw, so a run of codes covers the whole
    /// alphabet rather than a byte-aligned slice of it.
    #[test]
    fn draws_symbols_from_the_whole_alphabet() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            seen.extend(normalize(&mint().unwrap().code).bytes());
        }
        assert_eq!(seen.len(), ALPHABET.len(), "{seen:?}");
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
        let canonical = hash("ABCD-EFGH-JKMN-PQRS");
        for spelling in [
            "ABCDEFGHJKMNPQRS",
            "abcd-efgh-jkmn-pqrs",
            "  abcd efgh jkmn pqrs ",
            "AbCd-EfGh-JkMn-PqRs",
        ] {
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
        for bad in [
            "",
            "ABC",
            "ABCDEFGHI",
            "ABCDEFGU",
            "ABCDEF-G",
            "@@@@@@@@",
            "ABCD-EFGH-JKMN-PQR",
            "ABCD-EFGH-JKMN-PQRST",
            "ABCD-EFGH-JKMN-PQRU",
        ] {
            assert!(!is_plausible(bad), "{bad:?}");
        }
        assert!(is_plausible("ABCD-EFGH-JKMN-PQRS"));
        // `U` is not in the alphabet, so it is not a code even though it is the
        // right length after normalization.
        assert!(!is_plausible("UUUUUUUU"));
    }

    /// A code minted before the space grew is still shaped like a code. It stops
    /// working when it EXPIRES, in the store, which is the one refusal every
    /// other invite failure already looks like.
    #[test]
    fn still_accepts_the_shape_of_an_already_issued_code() {
        assert_eq!(LEGACY_CODE_LEN, 8);
        assert!(is_plausible("ABCD-EFGH"));
        assert!(is_plausible("abcdefgh"));
    }
}
