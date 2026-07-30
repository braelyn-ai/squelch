//! Shared text helpers for the triage modules: char-safe truncation of the
//! UNTRUSTED text every stage stores (model output, email bodies), and the
//! constructors each detector builds its static regex battery from.

use regex::Regex;

// ===========================================================================
// Truncation. Every caller bounds untrusted text, so all of these cut on a
// char boundary and never a byte one.
// ===========================================================================

/// Truncate to at most `max` chars (char-boundary safe).
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect()
    } else {
        s.to_string()
    }
}

/// [`truncate_chars`], also reporting whether it actually cut.
pub(crate) fn truncate_flagged(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() > max {
        (s.chars().take(max).collect(), true)
    } else {
        (s.to_string(), false)
    }
}

/// [`truncate_chars`] over the trimmed input.
pub(crate) fn truncate_trimmed(s: &str, max: usize) -> String {
    truncate_chars(s.trim(), max)
}

// ===========================================================================
// Static regex batteries.
// ===========================================================================

/// Compile one case-insensitive pattern for a detector's static battery. Every
/// caller passes a hard-coded literal, so a bad pattern is a build bug, not
/// input — panicking is correct.
pub(crate) fn rx(p: &str) -> Regex {
    Regex::new(&format!("(?i){p}"))
        .unwrap_or_else(|_| panic!("static triage regex must compile: {p}"))
}

/// Does any regex in the battery match any haystack?
pub(crate) fn any(res: &[Regex], hay: &[&str]) -> bool {
    res.iter().any(|re| hay.iter().any(|h| re.is_match(h)))
}
