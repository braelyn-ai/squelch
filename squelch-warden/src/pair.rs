//! Parsing what `squelchd pair` printed inside a tenant pod.
//!
//! PRIVACY: the string this module reads holds a LIVE pairing code, good for
//! one device claim. Nothing here logs its input, and the caller
//! ([`crate::provision`]) never logs the exec output at any level. That rule is
//! blanket rather than per-call because there is no log level at which a
//! credential on stdout is acceptable.

use crate::validate;

/// A pairing handoff, exactly as `squelchd pair` minted it.
#[derive(Clone)]
pub struct Pairing {
    pub pair_code: String,
    pub pair_url: String,
    pub deep_link: String,
}

impl std::fmt::Debug for Pairing {
    /// Manual, and the reason is two of the three fields. `pair_code` is a live
    /// credential and `deep_link` contains a copy of it, so a derived `Debug`
    /// would put a working pairing code into the first `dbg!` anyone reached
    /// for, or into the panic message of an `unwrap` in a test.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pairing")
            .field("pair_code", &"<redacted>")
            .field("pair_url", &self.pair_url)
            .field("deep_link", &"<redacted>")
            .finish()
    }
}

/// Pull the pairing code and the deep link out of a `squelchd pair` run.
///
/// Held to the daemon's real output (`squelchd/src/bin/squelchd.rs::cmd_pair`):
/// a `Pairing code: XXXX-XXXX` line, then an indented `passband://pair?...`
/// line, both on stdout, with the warnings on stderr. Parsed rather than
/// re-derived because the code exists only in that tenant's store and only that
/// process ever sees the plaintext.
///
/// Both streams are scanned. As shipped the daemon prints the code and the link
/// on stdout; scanning both means a future version that moves one of them is a
/// warning in a diff rather than a signup that dies at the last step.
///
/// Returns `None` unless BOTH are found and the code has the shape the daemon
/// mints. A partial parse would hand the control plane a success it would show
/// a user, with half a handoff in it.
pub fn parse(output: &str, url: &str) -> Option<Pairing> {
    let mut code = None;
    let mut deep_link = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Pairing code:") {
            let candidate = rest.trim();
            if validate::is_pairing_code(candidate) {
                code = Some(candidate.to_string());
            }
        } else if line.starts_with("passband://pair?") && line.contains("code=") {
            deep_link = Some(line.to_string());
        }
    }
    Some(Pairing {
        pair_code: code?,
        pair_url: url.to_string(),
        deep_link: deep_link?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::pair_stdout;

    /// The fixture is a captured copy of the shipped output. When the daemon's
    /// wording changes, this is the test that says so.
    #[test]
    fn parses_the_daemons_real_pair_output() {
        let url = "https://alice.passband.email";
        let parsed = parse(&pair_stdout("ABCD-1234", url), url).unwrap();
        assert_eq!(parsed.pair_code, "ABCD-1234");
        assert_eq!(parsed.pair_url, url);
        assert!(parsed.deep_link.starts_with("passband://pair?url="));
        assert!(parsed.deep_link.contains("code=ABCD-1234"));
    }

    #[test]
    fn a_link_on_the_other_stream_still_parses() {
        let url = "https://alice.passband.email";
        let combined = concat!(
            "Pairing code: JKMN-6789\n",
            "\n",
            "  passband://pair?url=https%3A%2F%2Falice.passband.email&code=JKMN-6789\n",
        );
        let parsed = parse(combined, url).unwrap();
        assert_eq!(parsed.pair_code, "JKMN-6789");
    }

    #[test]
    fn refuses_a_half_parse() {
        let url = "https://alice.passband.email";
        // A code with no link, and a link with no code, are both nothing.
        assert!(parse("Pairing code: ABCD-1234\n", url).is_none());
        assert!(parse("  passband://pair?url=x&code=ABCD-1234\n", url).is_none());
        assert!(parse("", url).is_none());
        // A code that is not the shape the daemon mints is not a code.
        assert!(
            parse(
                "Pairing code: nonsense\n  passband://pair?url=x&code=nonsense\n",
                url
            )
            .is_none()
        );
    }
}
