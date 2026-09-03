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

/// HOW A SEARCH ORDERS ITS RESULTS: the reader's standing answer to "when I
/// search, do I mean the best match or the one I am probably thinking of?"
///
/// This is a preference, not an operator. It is not parsed out of the query
/// text by [`parse_search_query`] and never will be — it arrives beside the
/// query from whichever door asked (`sort=` on the human door, a tool argument
/// on the agent door), because it belongs to the ASKER and not to the asking.
/// One reader who wants recency does not want to retype it every search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchSort {
    /// Relevance with recency as a CONSIDERATION: the ranking every leg blends
    /// through [`crate::store::recency`]. The default, because mail is not a
    /// document corpus and the thread you want is usually the one that moved.
    #[default]
    Recent,
    /// Relevance ALONE — bm25 on the keyword leg, pure fusion on the recall
    /// legs, no time decay anywhere. For the search where you know the words
    /// and do not know the year: an old thread stops being penalised for
    /// being old.
    BestMatch,
}

impl SearchSort {
    /// Parse the wire value. Unknown values are `None` so a door can 400 rather
    /// than silently serving an order nobody asked for.
    pub fn parse(s: &str) -> Option<SearchSort> {
        match s {
            "recent" => Some(SearchSort::Recent),
            "best_match" => Some(SearchSort::BestMatch),
            _ => None,
        }
    }

    /// The wire value, echoed back on a response so a client can tell what it
    /// actually got.
    pub fn as_str(self) -> &'static str {
        match self {
            SearchSort::Recent => "recent",
            SearchSort::BestMatch => "best_match",
        }
    }

    /// Whether the recency curve applies. The ONE place the two variants turn
    /// into behaviour, so a third variant later cannot half-land: every leg
    /// asks this question rather than matching on the enum itself.
    pub fn considers_recency(self) -> bool {
        matches!(self, SearchSort::Recent)
    }
}

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
        if let Some(after) = self.after
            && hit.received_at < after
        {
            return false;
        }
        if let Some(before) = self.before
            && hit.received_at >= before
        {
            return false;
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

/// THE TWO EXPRESSIONS ONE SEARCH RUNS, built from the reader's words by
/// [`FtsQuery::build`]. See there for why there are two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsQuery {
    /// Every term, ANDed: `"a" AND "b" AND "c"`. Empty when the reader typed
    /// nothing an index can look up.
    pub strict: String,
    /// Every term, ORed: `"a" OR "b" OR "c"`. Empty under the same condition.
    pub any: String,
    /// The terms as they are ranked, in the order typed, stripped of syntax and
    /// unquoted. What the door reports as `diagnostics.terms`.
    pub terms: Vec<String>,
    /// The FTS5 expression each term was actually matched by, parallel to
    /// `terms`. Usually just the quoted word; the as-you-type tail is a whole
    /// OR group (see [`partial_tail_expr`]).
    ///
    /// It exists so a per-term COUNT can be taken with the SAME expression the
    /// ranking used. `search_diagnostics` used to rebuild `"{term}"*` by hand,
    /// which is how a df could describe a search nobody ran.
    pub term_exprs: Vec<String>,
}

/// FTS5 syntax characters. They SPLIT a token rather than being escaped or
/// deleted, because the reader is typing WORDS into a search box, not writing a
/// query language: `-` starts no NOT, `*` matches no prefix, `co:` filters no
/// column. Splitting rather than deleting is what the INDEX does with them —
/// unicode61 treats every one of these as a separator — so `re:contract`
/// becomes the two terms the index actually holds instead of the one word
/// `recontract`, which it holds nowhere.
///
/// `"` is here too even though quoting is FTS5's own escape (`""`). A phrase
/// the reader quoted survives as an AND of its words, which finds the same mail
/// a rank or two differently, and that is a better trade than carrying quote
/// state through a tokenizer that exists to make one class of error impossible.
const FTS_SYNTAX: &[char] = &['"', '(', ')', '*', '^', ':', '-', '+'];

impl FtsQuery {
    /// Turn the reader's text into the two MATCH expressions the keyword leg
    /// runs, in this order: STRICT (every word) first, ANY (some word) second.
    ///
    /// WHY TWO. FTS5 reads a bare query as an implicit AND of every token, so
    /// one word the document lacks matches nothing however rare and decisive
    /// the others are. The mail that started this work says "wifi" and
    /// "abstract" and never says "conference" or "password", and "wifi" occurs
    /// in exactly one message in the mailbox: ANDed it is unfindable, ORed and
    /// ranked by bm25 it is the top hit. So the leg serves the strict matches
    /// first and then, below them, the mail that carries only some of the
    /// words. Nothing that matches everything can ever sink below something
    /// that matches less.
    ///
    /// WHY QUOTED. Every surviving token is wrapped in double quotes, which
    /// makes it a PHRASE — the one FTS5 construct that reads its contents as
    /// text rather than as syntax. Together with [`FTS_SYNTAX`] that means the
    /// reader's words can no longer form an expression FTS5 refuses to parse,
    /// and "a malformed MATCH silently means zero hits" stops being reachable
    /// from a search box. The defensive empty-on-error handling downstream
    /// stays anyway: this is not the only caller SQLite has.
    ///
    /// `partial` widens the LAST token only, the word still being typed (see
    /// [`partial_tail_expr`]). The panel's debounced fetch asks for it; the
    /// agent door never does, because an agent sends settled words.
    pub fn build(text: &str, partial: bool) -> FtsQuery {
        let terms: Vec<String> = text
            .split(|c: char| c.is_whitespace() || FTS_SYNTAX.contains(&c))
            // A token with no letter or digit in it is not a word the index
            // holds — and `""` is not even a legal FTS5 phrase, so passing one
            // on would fail the whole MATCH. Dropping it is what makes a query
            // of pure punctuation an empty expression rather than a syntax
            // error.
            .filter(|t| t.chars().any(char::is_alphanumeric))
            .map(|t| t.to_string())
            .collect();
        if terms.is_empty() {
            return FtsQuery {
                strict: String::new(),
                any: String::new(),
                terms,
                term_exprs: Vec::new(),
            };
        }
        let last = terms.len() - 1;
        let exprs: Vec<String> = terms
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if partial && i == last {
                    partial_tail_expr(t)
                } else {
                    format!("\"{t}\"")
                }
            })
            .collect();
        FtsQuery {
            strict: exprs.join(" AND "),
            any: exprs.join(" OR "),
            terms,
            term_exprs: exprs,
        }
    }

    /// Nothing to look up: the reader typed only operators, or only
    /// punctuation. Callers run NO MATCH at all rather than an empty one.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

/// The shortest truncation [`partial_tail_expr`] will look for as a whole word.
/// Below three characters a truncation stops describing the word being typed
/// and starts matching the alphabet: `"a"` is in half the mailbox and votes for
/// nothing.
const PARTIAL_MIN_TRUNCATION: usize = 3;

/// THE WORD THE READER IS STILL TYPING, as an FTS5 expression that cannot
/// vanish mid-word.
///
/// The obvious form is `"<tail>"*`, and on an unstemmed index it would be
/// right. `messages_fts` is `porter unicode61`, so the index holds STEMS, and
/// fts5 runs the tokenizer over the query token as well — including the prefix
/// token. That makes the naive prefix a stem-of-a-fragment matched against
/// stems, and it is NOT monotone in the number of characters typed. Measured
/// against the shipped schema, on a body reading "your parcel shipped this
/// morning":
///
/// ```text
/// "ship"*  -> 1   "shipp"*  -> 0   "shippi"* -> 0   "shipping"* -> 1
/// "confe"* -> 1   "conferen"* -> 0                  "conference"* -> 1
/// ```
///
/// The reader watches their result blink out at the fifth keystroke and come
/// back at the ninth, which is the exact behaviour `partial` exists to prevent.
/// The cause is that a stem is SHORTER than the word it came from: once the
/// fragment is longer than `stem(word)`, no prefix of the fragment can be the
/// index term, because the index term is a prefix of the FRAGMENT instead.
///
/// So the tail matches two ways at once, ORed:
///
/// - `"<tail>"*` — any indexed word starting with what has been typed. This is
///   the half that works while the fragment is still shorter than the stem.
/// - `"<truncation>"` for every prefix of the tail down to
///   [`PARTIAL_MIN_TRUNCATION`] — the stem itself, matched as a whole word,
///   for when the fragment has grown past it. `shipp` therefore still finds
///   `shipped` through the exact term `ship`.
///
/// Only ONE of these can be right for any given document, and both are cheap
/// term lookups, so the OR costs a doclist merge and buys monotonicity. The
/// widening applies to the tail alone: a settled word is never touched.
fn partial_tail_expr(tail: &str) -> String {
    let mut alts = vec![format!("\"{tail}\"*")];
    // Longest truncation first, which is also most-specific first — it reads
    // the way the expression is meant to be understood.
    let chars: Vec<char> = tail.chars().collect();
    for len in (PARTIAL_MIN_TRUNCATION..chars.len()).rev() {
        let prefix: String = chars[..len].iter().collect();
        alts.push(format!("\"{prefix}\""));
    }
    if alts.len() == 1 {
        // A tail at or under the floor is just its prefix query; wrapping one
        // alternative in parentheses would only make the expression harder to
        // read in a log.
        return alts.remove(0);
    }
    // Parenthesised because this whole group is ONE term's worth of the strict
    // AND-join: without them the OR would swallow the terms beside it.
    format!("({})", alts.join(" OR "))
}

/// What retrieval made of the reader's words, reported on the wire beside the
/// hits (`GET /client/search`'s `diagnostics`).
///
/// The classifier that decides whether a query is a lookup or a question reads
/// these: three or more terms with `strict_hits == 0` means the words are right
/// and no single message uses all of them, which is exactly the shape a
/// question has. Facts beat a guess, and these are cheap.
///
/// SECURITY: every count here is account-scoped and excludes sealed and spam
/// rows, for the same reason the hit queries do. A term frequency is a yes/no
/// oracle over message text, so a count that saw sealed mail would answer
/// questions about sealed mail one word at a time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchDiagnostics {
    /// Messages matching EVERY term.
    pub strict_hits: u32,
    /// Messages matching AT LEAST ONE term.
    pub any_hits: u32,
    /// Per-term document frequency, in the order the terms were typed.
    pub terms: Vec<TermDf>,
}

/// One term and how many messages contain it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TermDf {
    pub text: String,
    pub df: u32,
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

    #[test]
    fn the_sort_round_trips_its_wire_values() {
        for sort in [SearchSort::Recent, SearchSort::BestMatch] {
            assert_eq!(SearchSort::parse(sort.as_str()), Some(sort));
        }
        assert_eq!(
            SearchSort::default(),
            SearchSort::Recent,
            "recency is the default"
        );
        assert!(SearchSort::Recent.considers_recency());
        assert!(!SearchSort::BestMatch.considers_recency());
    }

    #[test]
    fn an_unknown_sort_is_refused_rather_than_guessed() {
        // The door 400s on None. Serving SOME order for a value the caller did
        // not ask for is the one outcome nobody can debug.
        for bad in [
            "",
            "newest",
            "best match",
            "best-match",
            "RECENT",
            "relevance",
        ] {
            assert_eq!(SearchSort::parse(bad), None, "{bad:?} must not parse");
        }
    }

    // ---- THE FTS EXPRESSION BUILDER ------------------------------------

    #[test]
    fn every_term_is_quoted_and_joined_both_ways() {
        let q = FtsQuery::build("abstract conference wifi password", false);
        assert_eq!(
            q.strict,
            r#""abstract" AND "conference" AND "wifi" AND "password""#
        );
        assert_eq!(
            q.any,
            r#""abstract" OR "conference" OR "wifi" OR "password""#
        );
        assert_eq!(q.terms, vec!["abstract", "conference", "wifi", "password"]);
        assert!(!q.is_empty());
    }

    #[test]
    fn a_single_term_is_the_same_expression_twice() {
        // Nothing to AND and nothing to OR: strict and any are one phrase, so
        // the any-only pass finds nothing the strict pass did not.
        let q = FtsQuery::build("wifi", false);
        assert_eq!(q.strict, r#""wifi""#);
        assert_eq!(q.any, r#""wifi""#);
    }

    #[test]
    fn fts_syntax_characters_are_stripped_not_escaped() {
        // Each of these is an operator to FTS5 and a word to the reader. None
        // of them may survive into the expression.
        let q = FtsQuery::build(
            r#"-invoice re:contract "quoted" (paren) star* ^caret +plus"#,
            false,
        );
        assert_eq!(
            q.terms,
            vec![
                "invoice", "re", "contract", "quoted", "paren", "star", "caret", "plus"
            ],
            "a syntax character SPLITS, exactly as unicode61 splits it in the index"
        );
        for expr in [&q.strict, &q.any] {
            for bad in ['*', '(', ')', '^', ':', '-', '+'] {
                assert!(!expr.contains(bad), "{bad:?} survived into {expr:?}");
            }
        }
    }

    #[test]
    fn partial_widens_only_the_last_token() {
        // The word still being typed, and only that one: widening the earlier
        // terms would widen a settled word the reader has finished with.
        let q = FtsQuery::build("abstract wif", true);
        assert_eq!(q.strict, r#""abstract" AND "wif"*"#);
        assert_eq!(q.any, r#""abstract" OR "wif"*"#);
        assert_eq!(q.terms, vec!["abstract", "wif"], "the term itself is bare");

        let one = FtsQuery::build("wif", true);
        assert_eq!(one.strict, r#""wif"*"#);
        assert_eq!(one.any, r#""wif"*"#);
    }

    #[test]
    fn a_tail_past_the_floor_carries_its_truncations_as_whole_words() {
        // The index holds STEMS, so once the fragment is longer than the stem
        // no prefix query can reach it. The truncations are the other half.
        let q = FtsQuery::build("shipp", true);
        assert_eq!(q.strict, r#"("shipp"* OR "ship" OR "shi")"#);
        assert_eq!(q.any, q.strict, "one term: both joins are the same group");

        // Grouped, so the OR cannot swallow the settled term beside it.
        let two = FtsQuery::build("parcel shipp", true);
        assert_eq!(two.strict, r#""parcel" AND ("shipp"* OR "ship" OR "shi")"#);
        assert_eq!(two.any, r#""parcel" OR ("shipp"* OR "ship" OR "shi")"#);
        assert_eq!(two.terms, vec!["parcel", "shipp"]);
    }

    #[test]
    fn the_truncation_floor_keeps_one_and_two_letter_fragments_out() {
        // `"a"` as a whole word is in half the mailbox and says nothing. Below
        // the floor the tail is its prefix query and nothing else.
        for tail in ["s", "sh", "shi"] {
            let q = FtsQuery::build(tail, true);
            assert_eq!(q.strict, format!("\"{tail}\"*"), "{tail:?}");
            assert!(!q.strict.contains(" OR "), "{tail:?} needs no group");
        }
    }

    #[test]
    fn every_term_reports_the_expression_it_was_matched_by() {
        // The counting path reads these rather than rebuilding the strings, so
        // a df always describes the search that actually ran.
        let q = FtsQuery::build("parcel shipp", true);
        assert_eq!(
            q.term_exprs,
            vec![r#""parcel""#, r#"("shipp"* OR "ship" OR "shi")"#]
        );
        assert_eq!(q.terms.len(), q.term_exprs.len());

        let settled = FtsQuery::build("parcel shipped", false);
        assert_eq!(settled.term_exprs, vec![r#""parcel""#, r#""shipped""#]);
    }

    #[test]
    fn a_query_of_pure_punctuation_builds_no_expression_at_all() {
        // `""` is not a legal FTS5 phrase, so a token that strips to nothing
        // has to be dropped rather than quoted — otherwise the one class of
        // error this builder exists to remove comes back through the door it
        // came in by.
        for text in ["***", "-- ++", "\"\"", "( )", "^", ":::"] {
            let q = FtsQuery::build(text, false);
            assert!(q.is_empty(), "{text:?} must build nothing");
            assert_eq!(q.strict, "");
            assert_eq!(q.any, "");
            assert!(q.terms.is_empty());
            assert!(q.term_exprs.is_empty());
        }
        assert!(FtsQuery::build("   ", true).is_empty());
    }

    #[test]
    fn unicode_words_survive_intact() {
        // The tokenizer is unicode61: non-ASCII letters are letters. Stripping
        // by an ASCII rule would have turned these into nothing.
        let q = FtsQuery::build("café 会議 naïve", false);
        assert_eq!(q.terms, vec!["café", "会議", "naïve"]);
        assert_eq!(q.strict, r#""café" AND "会議" AND "naïve""#);
    }

    #[test]
    fn the_builder_runs_after_the_operator_parse() {
        // The two halves in the order the door uses them: operators lifted out
        // first, then whatever words are left become the expression. A bounced
        // operator (`after:soon`) reaches the builder as a quoted phrase and
        // comes out as its two ordinary words.
        let (text, filter) = parse_search_query("invoice from:jane after:soon");
        assert_eq!(filter.from.as_deref(), Some("jane"));
        let q = FtsQuery::build(&text, false);
        assert_eq!(q.terms, vec!["invoice", "after", "soon"]);
    }

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
