//! Parsing what `squelchd token first-paired` printed inside a tenant pod.
//!
//! The activation signal (issue #89) is ONE TIMESTAMP and nothing else. The
//! daemon's side of that contract is one line on stdout — an RFC3339 instant, or
//! the word `none` — and this module is the whole of the warden's side of it.
//!
//! [`crate::pair`] is the neighbour to read first; this is the same shape with a
//! smaller answer. The difference that matters is what the two are parsing. A
//! pairing is a live credential, so nothing there may be logged. This is a
//! timestamp, which is not a secret — but the caller
//! ([`crate::provision::Warden::first_paired`]) still never logs the output, and
//! that is deliberate: the rule "exec output does not go in the log" is cheaper
//! to keep whole than to carve exceptions out of, and the next command someone
//! adds may not be so harmless.
//!
//! REFUSAL IS AN ANSWER HERE. `Option<Option<..>>` is not clever nesting for its
//! own sake: the outer layer is "did the daemon answer the question at all", the
//! inner is "and the answer was nobody". A daemon too old to have the subcommand
//! exits non-zero and never reaches this; one that answered something this
//! module does not understand is a `None`, which the caller turns into a 500. It
//! must never become `Some(None)`, because the control plane reads that as a
//! standing "not activated" and would keep polling a tenant it can never learn
//! anything about — or worse, a future output format would silently read as
//! "nobody ever paired" for the whole fleet.

use chrono::{DateTime, Utc};

/// Read `squelchd token first-paired`'s stdout.
///
/// - `Some(Some(ts))` — a client device first paired at `ts`.
/// - `Some(None)` — the daemon answered, and nobody has ever paired.
/// - `None` — the output is not this command's output. Never half-guessed.
///
/// The FIRST NON-EMPTY LINE decides, and everything after it is ignored. That is
/// the tolerant half of a strict contract: the daemon prints one line and the
/// exec transport may add a trailing newline or a stray blank, neither of which
/// is a disagreement about the answer. A second line carrying a DIFFERENT answer
/// cannot arise from any daemon that ships this command; if one ever did, the
/// first line is the one the daemon put there first.
///
/// The timestamp is normalized to UTC on the way through. The daemon renders `Z`
/// already, so this only matters if a future one ever renders a local offset:
/// the instant is the same either way, and the control plane must not have to
/// know which spelling it got.
pub fn parse_first_paired(stdout: &str) -> Option<Option<DateTime<Utc>>> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    if line == "none" {
        return Some(None);
    }
    // Held to RFC3339 exactly, which is what the daemon promises to print. A
    // looser parse would accept half a date and call it an activation.
    let parsed = DateTime::parse_from_rfc3339(line).ok()?;
    Some(Some(parsed.with_timezone(&Utc)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{first_paired_none_stdout, first_paired_stdout};

    /// The fixture is a captured copy of the shipped output. When the daemon's
    /// one line changes, this is the test that says so.
    #[test]
    fn parses_the_daemons_real_first_paired_output() {
        let parsed = parse_first_paired(&first_paired_stdout("2026-03-01T09:30:00Z"))
            .expect("the daemon answered")
            .expect("with a timestamp");
        assert_eq!(parsed.to_rfc3339(), "2026-03-01T09:30:00+00:00");
    }

    /// `none` is an ANSWER, not a failure: this mailbox exists, is running, and
    /// nobody has ever paired a client with it.
    #[test]
    fn none_is_an_answer_and_not_a_refusal() {
        assert_eq!(parse_first_paired(&first_paired_none_stdout()), Some(None));
        // The transport's trailing blank lines say nothing about the answer.
        assert_eq!(parse_first_paired("\n\nnone\n\n"), Some(None));
    }

    /// An offset that is not `Z` is the same instant, and comes back as UTC so
    /// the control plane never sees two spellings of one moment.
    #[test]
    fn a_non_utc_offset_normalizes_rather_than_travelling_onward() {
        let parsed = parse_first_paired("2026-03-01T04:30:00-05:00\n")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-03-01T09:30:00+00:00");
    }

    /// Anything else is a refusal. `Some(None)` here would be the control plane
    /// told "nobody ever paired" by a daemon that said no such thing.
    #[test]
    fn garbage_is_refused_rather_than_read_as_nobody() {
        for output in [
            "",
            "\n",
            "   \n",
            "None\n",
            "NONE\n",
            "none of your business\n",
            "2026-03-01\n",
            "2026-03-01 09:30:00\n",
            "first paired: 2026-03-01T09:30:00Z\n",
            "error: unrecognized subcommand 'first-paired'\n",
            // A listing, which is the shape the separate subcommand exists to
            // make impossible: it is not one line and it is not a timestamp.
            "   ID  NAME     CREATED\n    1  iPhone   2026-03-01\n",
        ] {
            assert_eq!(parse_first_paired(output), None, "{output:?}");
        }
    }
}
