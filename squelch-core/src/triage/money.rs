//! Currency amounts lifted out of email text: ONE set of patterns and ONE
//! parse, shared by every detector that reads money.
//!
//! WHY IT IS ITS OWN MODULE. [`receipt`](crate::triage::receipt) and
//! [`deadline`](crate::triage::deadline) each carried a byte-identical copy of
//! these three regexes and its own inline parse. When the fraction bug below was
//! fixed in one of them, the other kept storing $29 trillion bills — the exact
//! "second, drifting definition" failure the shipments merchant key is namespaced
//! to avoid. A detector may decide what counts as an amount IN CONTEXT (a receipt
//! prefers a total-word, a bill takes the first match); none of them gets to
//! re-spell what an amount looks like.

use crate::triage::text::rx;
use regex::Regex;
use std::sync::OnceLock;

/// Currency amount patterns, capturing the number in group 1.
///
/// THE FRACTION IS UNBOUNDED (`[0-9]+`, not `[0-9]{2}`) and that is load-bearing,
/// not sloppiness. A two-digit fraction looks like the only thing a price can
/// have right up until a real sender does its arithmetic in binary floating point
/// and prints the result unrounded — Amazon's shipment mail says
///
/// ```text
/// Total
/// 67.28999999999999 USD
/// ```
///
/// A pattern admitting exactly two decimals cannot match that at all, so the
/// engine gives up on the "67" and resumes scanning INSIDE the number, where
/// `28999999999999 USD` matches beautifully. Five receipts in one real mailbox
/// were stored as their own fractional tail; a $67.29 order rendered as
/// $28,999,999,999,999. Matching the whole number and rounding in
/// [`parse_amount`] is what keeps the value and the cents both honest.
pub fn amount_patterns() -> &'static [Regex] {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        vec![
            // $1,234.56 or $42 or $42.10
            rx(r"\$\s?([0-9][0-9,]*(?:\.[0-9]+)?)"),
            // 1,234.56 USD / 42.00 usd
            rx(r"\b([0-9][0-9,]*(?:\.[0-9]+)?)\s?(?:USD|usd)\b"),
            // USD 1,234.56
            rx(r"\b(?:USD|usd)\s?([0-9][0-9,]*(?:\.[0-9]+)?)"),
        ]
    })
}

/// Parse one captured amount token ("1,234.56") to `f64`, ROUNDED TO CENTS.
///
/// `None` for anything that does not land on a finite number of cents. THE
/// FINITENESS CHECK IS AFTER THE MULTIPLY, deliberately: a 300-digit run parses
/// to a perfectly finite `f64` and only overflows to infinity when scaled, and an
/// email is an attacker-controlled string, so `Some(inf)` was reachable from the
/// inbox and went straight into a `receipts` row.
pub fn parse_amount(raw: &str) -> Option<f64> {
    let cents = (raw.replace(',', "").parse::<f64>().ok()? * 100.0).round();
    cents.is_finite().then(|| cents / 100.0)
}

/// The FIRST currency amount in `text`, by pattern order. What a detector wants
/// when the text has no structure to prefer (a bill states its amount once).
pub fn first_amount(text: &str) -> Option<f64> {
    amount_patterns()
        .iter()
        .find_map(|re| re.captures(text)?.get(1).and_then(|m| parse_amount(m.as_str())))
}

/// The LARGEST currency amount anywhere in `text`. What a receipt wants as its
/// fallback, since the total is almost always the biggest line on the page.
pub fn largest_amount(text: &str) -> Option<f64> {
    amount_patterns()
        .iter()
        .flat_map(|re| re.captures_iter(text))
        .filter_map(|cap| parse_amount(cap.get(1)?.as_str()))
        .fold(None, |best: Option<f64>, v| Some(best.map_or(v, |b| b.max(v))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unrounded_float_parses_whole_and_rounds_to_cents() {
        // The five artifacts Amazon actually sent, and the totals they meant.
        for (printed, want) in [
            ("14.530000000000001", 14.53),
            ("40.099999999999994", 40.10),
            ("96.63999999999999", 96.64),
            ("77.41999999999999", 77.42),
            ("67.28999999999999", 67.29),
        ] {
            assert_eq!(
                first_amount(&format!("{printed} USD")),
                Some(want),
                "{printed} should parse to {want}"
            );
        }
    }

    #[test]
    fn the_fraction_tail_is_never_the_answer() {
        // The precise failure: with a two-decimal-only pattern this returned
        // 28999999999999.0, because no match could START at the 67.
        assert_eq!(first_amount("Total\r\n67.28999999999999 USD"), Some(67.29));
    }

    #[test]
    fn an_overflowing_run_is_refused_not_stored_as_infinity() {
        // Reachable from any inbound email: 300-odd digits parse to a finite
        // f64 and overflow only when scaled to cents. `Some(inf)` used to reach
        // the receipts table.
        let huge = format!("${}.00", "1".to_string() + &"0".repeat(307));
        assert_eq!(first_amount(&huge), None, "must not store an infinity");
        // The same value one order of magnitude down is finite and kept.
        let big = format!("${}.00", "1".to_string() + &"0".repeat(300));
        assert!(first_amount(&big).is_some_and(f64::is_finite));
    }

    #[test]
    fn ordinary_amounts_are_untouched() {
        assert_eq!(first_amount("$1,234.56"), Some(1234.56));
        assert_eq!(first_amount("42 USD"), Some(42.0));
        assert_eq!(first_amount("USD 7.97"), Some(7.97));
        assert_eq!(first_amount("no money here"), None);
    }

    #[test]
    fn largest_wins_over_line_items() {
        assert_eq!(largest_amount("$7.97 and $21.99 and $67.29"), Some(67.29));
        assert_eq!(largest_amount("nothing"), None);
    }
}
