//! Query operators for human-door search: `from:`, `after:`, `before:`.
//!
//! The raw string the reader types is one field, but it carries two things: the
//! words to rank on, and structured constraints. [`parse_search_query`] splits
//! them ONCE, at the edge, so every retrieval leg downstream sees the same
//! `(text, filter)` pair and nobody re-parses a raw query string.
//!
//! Parsing NEVER fails. A token that looks like an operator but carries a value
//! we cannot read (`after:soon`) stays in the search text, because silently
//! dropping a word the reader typed is worse than ranking on it.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};

use crate::types::SearchHit;

/// The structured half of a parsed search query: constraints lifted out of the
/// raw string, applied in SQL on the keyword path and post-hoc on the recall
/// paths (see [`SearchFilter::matches`]).
///
/// Both date bounds are half-open around UTC midnight: `after` is INCLUSIVE
/// (00:00:00 UTC of that day is in range), `before` is EXCLUSIVE (00:00:00 UTC
/// of that day is out of range). So `after:2026-01-01 before:2026-01-02` is
/// exactly the first of January.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchFilter {
    /// Substring to look for in the sender's address OR display name,
    /// case-insensitive. Not an exact address match: `from:jane` finds
    /// `jane@example.com` and `Jane Doe` alike.
    pub from: Option<String>,
    /// Inclusive lower bound on `received_at`.
    pub after: Option<DateTime<Utc>>,
    /// Exclusive upper bound on `received_at`.
    pub before: Option<DateTime<Utc>>,
}

impl SearchFilter {
    /// No constraint at all — the caller can take the unfiltered fast path.
    pub fn is_empty(&self) -> bool {
        self.from.is_none() && self.after.is_none() && self.before.is_none()
    }

    /// Does this hit satisfy every constraint? The post-hoc twin of the SQL
    /// predicates, for the recall legs (semantic/hybrid), which rank first and
    /// filter after.
    ///
    /// Case folding is ASCII-only on purpose: it has to agree with SQLite's
    /// `LIKE`, which is itself ASCII-only, or the same query would mean two
    /// different things depending on the mode.
    pub fn matches(&self, hit: &SearchHit) -> bool {
        if let Some(from) = &self.from {
            let needle = from.to_ascii_lowercase();
            let addr = hit.from_addr.to_ascii_lowercase();
            let name = hit
                .from_name
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !addr.contains(&needle) && !name.contains(&needle) {
                return false;
            }
        }
        if let Some(after) = self.after {
            if hit.received_at < after {
                return false;
            }
        }
        if let Some(before) = self.before {
            if hit.received_at >= before {
                return false;
            }
        }
        true
    }
}

/// Split a raw search string into `(search text, filter)`.
///
/// Tokens are whitespace-separated, with double quotes holding a token together
/// (`from:"jane doe"`). Operator prefixes are case-insensitive (`From:`,
/// `AFTER:`). Rules that keep the result predictable:
///
/// - A repeated operator takes its LAST value — the reader edited their mind.
/// - An operator with an EMPTY value (`from:`) constrains nothing and is
///   dropped, rather than being handed to FTS5, where a bare `:` is a syntax
///   error.
/// - A date that does not parse as `YYYY-MM-DD` leaves the whole token in the
///   search text, quoted as an FTS5 phrase. `before:tuesday` is then just two
///   words to rank on — unquoted, its colon would read as a column filter and
///   fail the whole query, which is the one way "keep the reader's word" could
///   turn into an error.
/// - Everything not consumed as an operator is rejoined with single spaces, so
///   the search text is the reader's words minus the operators.
pub fn parse_search_query(raw: &str) -> (String, SearchFilter) {
    let mut filter = SearchFilter::default();
    let mut words: Vec<String> = Vec::new();

    for token in tokenize(raw) {
        // Each arm: an empty value means "no constraint, drop the token"; a
        // value we cannot read means "not an operator after all, keep the word".
        if let Some(value) = strip_prefix_ci(&token, "from:") {
            let value = unquote(value);
            if !value.is_empty() {
                filter.from = Some(value.to_string());
            }
            continue;
        }
        if let Some(value) = strip_prefix_ci(&token, "after:") {
            let value = unquote(value);
            if value.is_empty() {
                continue;
            }
            match parse_day(value) {
                Some(d) => filter.after = Some(d),
                None => words.push(as_fts_phrase(&token)),
            }
            continue;
        }
        if let Some(value) = strip_prefix_ci(&token, "before:") {
            let value = unquote(value);
            if value.is_empty() {
                continue;
            }
            match parse_day(value) {
                Some(d) => filter.before = Some(d),
                None => words.push(as_fts_phrase(&token)),
            }
            continue;
        }
        // ANY other colon token gets the same phrase treatment as a bounced
        // operator: unquoted, FTS5 reads `re:contract` as a filter on a column
        // named `re` and errors the whole query. A token the reader already
        // quoted is a phrase and passes through untouched.
        if token.contains(':') && !token.starts_with('"') {
            words.push(as_fts_phrase(&token));
            continue;
        }
        words.push(token);
    }

    (words.join(" "), filter)
}

/// Whitespace-split, except inside double quotes. Quote characters are KEPT in
/// the token: a plain `"exact phrase"` has to reach FTS5 intact (there it means
/// a phrase query), and a token bounced back into the search text should read
/// the way the reader typed it.
fn tokenize(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in raw.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
            cur.push(c);
        } else if c.is_whitespace() && !in_quotes {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Case-insensitive prefix strip. Guards the char boundary so a multi-byte
/// leading character cannot panic the slice.
fn strip_prefix_ci<'a>(token: &'a str, prefix: &str) -> Option<&'a str> {
    let n = prefix.len();
    if token.len() >= n && token.is_char_boundary(n) && token[..n].eq_ignore_ascii_case(prefix) {
        Some(&token[n..])
    } else {
        None
    }
}

/// Strip surrounding double quotes from an operator VALUE. Unbalanced quotes
/// (`from:"jane`, from an unterminated quote) strip too — the reader's intent is
/// unambiguous and erroring would be theatre.
fn unquote(value: &str) -> &str {
    value.trim_matches('"')
}

/// Wrap a bounced operator token as an FTS5 PHRASE. `after:soon` reaches FTS5
/// as a filter on a column named `after` and errors the query; `"after:soon"`
/// is two ordinary tokens. Embedded quotes are doubled, FTS5's own escape.
fn as_fts_phrase(token: &str) -> String {
    format!("\"{}\"", token.replace('"', "\"\""))
}

/// `YYYY-MM-DD` at 00:00:00 UTC. Anything else is `None`, and the caller keeps
/// the token as search text.
fn parse_day(value: &str) -> Option<DateTime<Utc>> {
    let day = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    Some(Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
    }

    #[test]
    fn plain_text_has_no_filter() {
        let (text, filter) = parse_search_query("quarterly invoice");
        assert_eq!(text, "quarterly invoice");
        assert!(filter.is_empty());
    }

    #[test]
    fn operators_are_lifted_out_of_the_text() {
        let (text, filter) =
            parse_search_query("invoice from:jane after:2026-01-01 before:2026-02-01 overdue");
        assert_eq!(text, "invoice overdue");
        assert_eq!(filter.from.as_deref(), Some("jane"));
        assert_eq!(filter.after, Some(day(2026, 1, 1)));
        assert_eq!(filter.before, Some(day(2026, 2, 1)));
    }

    #[test]
    fn operator_prefixes_are_case_insensitive() {
        let (text, filter) = parse_search_query("From:Jane AFTER:2026-03-04 report");
        assert_eq!(text, "report");
        // The VALUE keeps its case (matching folds later); only the prefix is
        // case-insensitive.
        assert_eq!(filter.from.as_deref(), Some("Jane"));
        assert_eq!(filter.after, Some(day(2026, 3, 4)));
    }

    #[test]
    fn quoted_values_survive_the_space() {
        let (text, filter) = parse_search_query(r#"from:"jane doe" contract"#);
        assert_eq!(text, "contract");
        assert_eq!(filter.from.as_deref(), Some("jane doe"));
    }

    #[test]
    fn quoted_search_phrase_keeps_its_quotes() {
        // FTS5 reads `"..."` as a phrase query, so the quotes must reach it.
        let (text, filter) = parse_search_query(r#""signed contract" from:vendor"#);
        assert_eq!(text, r#""signed contract""#);
        assert_eq!(filter.from.as_deref(), Some("vendor"));
    }

    #[test]
    fn unterminated_quote_still_yields_a_value() {
        let (text, filter) = parse_search_query(r#"from:"jane doe"#);
        assert_eq!(text, "");
        assert_eq!(filter.from.as_deref(), Some("jane doe"));
    }

    #[test]
    fn bad_dates_stay_in_the_search_text() {
        // Bounced tokens come back QUOTED, so FTS5 reads the colon as part of a
        // phrase instead of as a column filter.
        let (text, filter) = parse_search_query("after:soon before:2026-13-45 lunch");
        assert_eq!(text, r#""after:soon" "before:2026-13-45" lunch"#);
        assert!(filter.is_empty(), "no date parsed => no bound applied");
    }

    #[test]
    fn empty_operator_values_are_dropped() {
        // A bare `from:` would be an FTS5 syntax error; it constrains nothing,
        // so it never reaches the index.
        let (text, filter) = parse_search_query("from: after: invoice");
        assert_eq!(text, "invoice");
        assert!(filter.is_empty());
    }

    #[test]
    fn repeated_operator_takes_the_last_value() {
        let (text, filter) = parse_search_query("from:jane from:bob");
        assert_eq!(text, "");
        assert_eq!(filter.from.as_deref(), Some("bob"));
    }

    #[test]
    fn operator_only_query_leaves_empty_text() {
        let (text, filter) = parse_search_query("  from:jane  ");
        assert_eq!(text, "");
        assert!(!filter.is_empty());
    }

    #[test]
    fn a_colon_that_is_not_an_operator_is_quoted_for_fts() {
        // Kept as search text, but as PHRASES: a bare colon token is an FTS5
        // column filter and errors the whole query.
        let (text, filter) = parse_search_query("subject:invoice re:contract");
        assert_eq!(text, r#""subject:invoice" "re:contract""#);
        assert!(filter.is_empty());
    }

    #[test]
    fn a_quoted_phrase_with_a_colon_is_not_double_quoted() {
        let (text, filter) = parse_search_query(r#""re: your invoice""#);
        assert_eq!(text, r#""re: your invoice""#);
        assert!(filter.is_empty());
    }

    #[test]
    fn matches_folds_case_and_checks_both_sender_fields() {
        let hit = SearchHit {
            id: 1,
            thread_id: "t".into(),
            from_addr: "Jane@Example.com".into(),
            from_name: Some("Jane Doe".into()),
            subject: "s".into(),
            received_at: day(2026, 5, 5),
            snippet: "".into(),
        };
        let (_, f) = parse_search_query("from:JANE@example");
        assert!(f.matches(&hit), "address match, case-folded");
        let (_, f) = parse_search_query(r#"from:"jane doe""#);
        assert!(f.matches(&hit), "display-name match");
        let (_, f) = parse_search_query("from:bob");
        assert!(!f.matches(&hit));
    }

    #[test]
    fn matches_bounds_are_inclusive_after_exclusive_before() {
        let mut hit = SearchHit {
            id: 1,
            thread_id: "t".into(),
            from_addr: "a@b.c".into(),
            from_name: None,
            subject: "s".into(),
            received_at: day(2026, 5, 5),
            snippet: "".into(),
        };
        let (_, f) = parse_search_query("after:2026-05-05");
        assert!(f.matches(&hit), "midnight of the after: day is IN range");
        let (_, f) = parse_search_query("before:2026-05-05");
        assert!(
            !f.matches(&hit),
            "midnight of the before: day is OUT of range"
        );

        hit.received_at = day(2026, 5, 4);
        let (_, f) = parse_search_query("before:2026-05-05");
        assert!(f.matches(&hit));
        let (_, f) = parse_search_query("after:2026-05-05");
        assert!(!f.matches(&hit));
    }
}
