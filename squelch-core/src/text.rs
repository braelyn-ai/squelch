//! Char-boundary-safe truncation. Callers bound UNTRUSTED text (model output,
//! email bodies), so every cut lands on a Unicode scalar boundary, never a byte.

/// Truncate to at most `max` Unicode scalar values.
pub fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((byte, _)) => s[..byte].to_string(),
        None => s.to_string(),
    }
}

/// Truncate so the result is at most `max` scalar values *including* the `…`
/// that marks the cut. For anything rendered into a fixed budget, where the
/// marker has to fit inside the budget rather than overflow it by one.
pub fn truncate_ellipsis(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    match s.char_indices().nth(max) {
        Some(_) => format!("{}…", truncate_chars(s, max - 1)),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_unicode_boundaries() {
        assert_eq!(truncate_chars("aé日z", 3), "aé日");
        assert_eq!(truncate_chars("aé", 2), "aé");
    }

    #[test]
    fn ellipsis_fits_inside_the_budget() {
        assert_eq!(truncate_ellipsis("aé日z", 3), "aé…");
        assert_eq!(truncate_ellipsis("aé日", 3), "aé日");
        assert_eq!(truncate_ellipsis("abc", 0), "");
        assert_eq!(truncate_ellipsis("abcd", 1), "…");
    }
}
