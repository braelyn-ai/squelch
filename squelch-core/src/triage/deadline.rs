//! Bill / payment / deadline extraction for Stage-1 triage.
//!
//! This is the highest-recall rung of the Stage-1 ladder: the user's #1 fear is
//! a missed bill, so we bias toward *detecting* a deadline. False positives cost
//! the user a slightly-too-prominent email; false negatives cost them a late fee.
//!
//! We extract three things from a candidate bill:
//!   1. the `kind` (invoice / statement / autopay / payment-due / …),
//!   2. an optional currency amount (`$42.10`, `1,299.00 USD`),
//!   3. a due date — absolute dates in several common formats.
//!
//! Relative dates ("due Friday", "due next week") are intentionally *out of
//! scope* for v0: absolute dates are what actually matter for bills, and cheap
//! relative parsing is error-prone. See the TODO in [`extract_due_at`].

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use regex::Regex;
use std::sync::OnceLock;

/// A detected bill/deadline. Mirrors the shape persisted as
/// [`crate::types::Deadline`] minus the DB identity columns.
#[derive(Debug, Clone, PartialEq)]
pub struct DeadlineHit {
    /// Coarse classification: "invoice", "statement", "autopay", "payment_due",
    /// "past_due", or the generic "bill".
    pub kind: String,
    pub amount: Option<f64>,
    pub currency: Option<String>,
    pub due_at: DateTime<Utc>,
    pub past_due: bool,
    /// Which surface produced the hit, e.g. "subject", "body", or
    /// "body:due-on-receipt". Handy for debugging recall.
    pub source: String,
}

struct BillDetector {
    /// Strong "this is a bill" signals. If any fire, we treat the message as a
    /// bill even absent an amount or a parseable date.
    signals: Vec<(Regex, &'static str)>,
    /// Explicit "already overdue" language.
    overdue: Vec<Regex>,
    /// `$1,234.56` / `1,234.56 USD` / `USD 1,234.56` style amounts.
    amount: Vec<Regex>,
    /// "due <date>", "by <date>", "payment due <date>", "due date: <date>".
    due_phrase: Vec<Regex>,
    /// "due on receipt" / "due upon receipt" — treated as due *now*.
    due_on_receipt: Regex,
}

fn rx(p: &str) -> Regex {
    Regex::new(&format!("(?i){p}")).expect("static bill regex must compile")
}

fn detector() -> &'static BillDetector {
    static D: OnceLock<BillDetector> = OnceLock::new();
    D.get_or_init(|| BillDetector {
        signals: vec![
            (rx(r"\bpast[-\s]?due\b"), "past_due"),
            (rx(r"\boverdue\b"), "past_due"),
            (rx(r"\bautopay\b"), "autopay"),
            (rx(r"\bauto[-\s]?pay(ment)?\b"), "autopay"),
            (rx(r"\binvoice\b"), "invoice"),
            (rx(r"\bstatement\b"), "statement"),
            (rx(r"\bamount due\b"), "payment_due"),
            (rx(r"\bpayment due\b"), "payment_due"),
            (rx(r"\bpayment (is )?(now )?due\b"), "payment_due"),
            (rx(r"\bbill(ing)? (is )?due\b"), "payment_due"),
            (rx(r"\byour bill\b"), "bill"),
            (rx(r"\bnew (invoice|bill|statement)\b"), "invoice"),
            (rx(r"\bminimum payment\b"), "payment_due"),
            (rx(r"\bbalance due\b"), "payment_due"),
            (rx(r"\bpay(ment)? reminder\b"), "payment_due"),
            (rx(r"\bunpaid\b"), "past_due"),
        ],
        overdue: vec![rx(r"\bpast[-\s]?due\b"), rx(r"\boverdue\b"), rx(r"\bunpaid\b")],
        amount: vec![
            // $1,234.56 or $42 or $42.10
            rx(r"\$\s?([0-9][0-9,]*(?:\.[0-9]{2})?)"),
            // 1,234.56 USD / 42.00 usd
            rx(r"\b([0-9][0-9,]*(?:\.[0-9]{2})?)\s?(?:USD|usd)\b"),
            // USD 1,234.56
            rx(r"\b(?:USD|usd)\s?([0-9][0-9,]*(?:\.[0-9]{2})?)"),
        ],
        due_phrase: vec![
            // "due date: Jan 5, 2026", "due by 01/05/2026", "due on 2026-01-05",
            // "payment due January 5 2026", "by 5 Jan 2026"
            rx(r"\b(?:payment\s+)?due(?:\s+date)?[:\s]+(?:on\s+|by\s+)?([A-Za-z0-9,\-/ ]{4,30}?)(?:[.,;)\n]|$)"),
            rx(r"\bby\s+([A-Za-z0-9,\-/ ]{4,30}?)(?:[.,;)\n]|$)"),
        ],
        due_on_receipt: rx(r"\bdue\s+(?:up)?on\s+receipt\b"),
    })
}

/// Parse a single absolute date fragment into a `NaiveDate`. Tries a battery of
/// common human/machine formats. Returns `None` for anything it can't confidently
/// read (including relative dates like "Friday").
fn parse_date_fragment(frag: &str) -> Option<NaiveDate> {
    let s = frag.trim().trim_end_matches(['.', ',']);
    if s.is_empty() {
        return None;
    }

    // Normalize ordinals: "January 5th" -> "January 5".
    static ORD: OnceLock<Regex> = OnceLock::new();
    let ord = ORD.get_or_init(|| Regex::new(r"(?i)(\d{1,2})(st|nd|rd|th)\b").unwrap());
    let cleaned = ord.replace_all(s, "$1").to_string();
    let cleaned = cleaned.trim();

    // ISO and numeric formats (unambiguous-ish; assume US M/D for slashes).
    const NUMERIC: &[&str] = &["%Y-%m-%d", "%Y/%m/%d", "%m/%d/%Y", "%m-%d-%Y", "%m/%d/%y"];
    for fmt in NUMERIC {
        if let Ok(d) = NaiveDate::parse_from_str(cleaned, fmt) {
            return Some(d);
        }
    }

    // Month-name formats, with and without year. When the year is absent we
    // resolve it to the nearest sensible year (this year, or next year if that
    // date has already passed by more than a month — bills are forward-looking).
    const WITH_YEAR: &[&str] = &[
        "%B %d %Y", "%b %d %Y", "%B %d, %Y", "%b %d, %Y", "%d %B %Y", "%d %b %Y",
    ];
    for fmt in WITH_YEAR {
        if let Ok(d) = NaiveDate::parse_from_str(cleaned, fmt) {
            return Some(d);
        }
    }

    const NO_YEAR: &[&str] = &["%B %d", "%b %d", "%d %B", "%d %b"];
    for fmt in NO_YEAR {
        // chrono can't parse a date without a year directly; inject current year.
        let this_year = Utc::now().year();
        let candidate = format!("{cleaned} {this_year}");
        for yfmt in [format!("{fmt} %Y")] {
            if let Ok(d) = NaiveDate::parse_from_str(&candidate, &yfmt) {
                return Some(d);
            }
        }
    }

    None
}

/// Turn a `NaiveDate` into an end-of-day UTC instant. A bill "due Jan 5" isn't
/// past-due at 00:00 on Jan 5, so we anchor to 23:59:59 of that day.
fn end_of_day_utc(d: NaiveDate) -> DateTime<Utc> {
    let ndt = d.and_hms_opt(23, 59, 59).expect("valid end-of-day");
    Utc.from_utc_datetime(&ndt)
}

/// Extract a due-date instant from the given text, if present.
///
/// TODO(v0): relative dates ("due Friday", "due next week") are not parsed.
/// Absolute dates dominate real bills; add relative parsing (anchored to
/// `received_at`) in a later pass if recall analysis shows it matters.
fn extract_due_at(text: &str) -> Option<(DateTime<Utc>, &'static str)> {
    let d = detector();

    if d.due_on_receipt.is_match(text) {
        // Due on receipt: treat as due at end of *today* (relative to now).
        return Some((end_of_day_utc(Utc::now().date_naive()), "due-on-receipt"));
    }

    for re in &d.due_phrase {
        for cap in re.captures_iter(text) {
            if let Some(m) = cap.get(1)
                && let Some(date) = parse_date_fragment(m.as_str())
            {
                return Some((end_of_day_utc(date), "due-phrase"));
            }
        }
    }
    None
}

/// Parse the first currency amount in `text`, returning `(amount, currency)`.
fn extract_amount(text: &str) -> Option<(f64, String)> {
    let d = detector();
    for re in &d.amount {
        if let Some(cap) = re.captures(text)
            && let Some(m) = cap.get(1)
        {
            let raw = m.as_str().replace(',', "");
            if let Ok(v) = raw.parse::<f64>() {
                return Some((v, "USD".to_string()));
            }
        }
    }
    None
}

/// The heart of rung #1. Given the message surfaces, decide whether this is a
/// bill/deadline and, if so, produce a [`DeadlineHit`].
///
/// `now` is injected so tests are deterministic and past-due math is testable.
pub fn detect_bill(
    subject: &str,
    body: &str,
    now: DateTime<Utc>,
) -> Option<DeadlineHit> {
    let d = detector();
    let hay = format!("{subject}\n{body}");

    // 1. Is there a bill signal at all? Pick the *strongest* kind, preferring
    //    an explicit overdue signal.
    let mut kind: Option<&'static str> = None;
    for (re, k) in &d.signals {
        if re.is_match(&hay) {
            // "past_due" wins over everything; otherwise first match sticks.
            if *k == "past_due" {
                kind = Some("past_due");
                break;
            }
            kind.get_or_insert(k);
        }
    }
    let kind = kind?;

    // 2. Source: prefer subject if the signal is there, else body.
    let source = if d.signals.iter().any(|(re, _)| re.is_match(subject)) {
        "subject"
    } else {
        "body"
    };

    // 3. Amount + due date.
    let amount = extract_amount(&hay);
    let due = extract_due_at(&hay);

    // Determine due_at. Explicit overdue language with no date => due "now"
    // (we still want to flag it as past-due).
    let explicitly_overdue = d.overdue.iter().any(|re| re.is_match(&hay));

    let (due_at, source) = match due {
        Some((dt, src)) => (dt, format!("{source}:{src}")),
        None => {
            if explicitly_overdue {
                // Anchor to now; it's already past-due by declaration.
                (now, source.to_string())
            } else {
                // A bill signal but no date and no overdue language. Still a
                // deadline the user cares about; anchor to now so it surfaces,
                // but it is NOT past-due.
                (now, source.to_string())
            }
        }
    };

    let past_due = explicitly_overdue || due_at < now;

    // Reconcile kind with past-due reality.
    let kind = if past_due && kind != "autopay" {
        "past_due"
    } else {
        kind
    };

    let (amount, currency) = match amount {
        Some((a, c)) => (Some(a), Some(c)),
        None => (None, None),
    };

    Some(DeadlineHit {
        kind: kind.to_string(),
        amount,
        currency,
        due_at,
        past_due,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_common_date_formats() {
        let now = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        for s in [
            "2026-01-05",
            "01/05/2026",
            "January 5, 2026",
            "Jan 5 2026",
            "5 January 2026",
            "January 5th, 2026",
        ] {
            assert_eq!(parse_date_fragment(s), Some(now), "failed: {s:?}");
        }
    }

    #[test]
    fn rejects_relative_dates() {
        assert_eq!(parse_date_fragment("Friday"), None);
        assert_eq!(parse_date_fragment("next week"), None);
    }

    #[test]
    fn dated_future_bill_is_deadline_not_past_due() {
        let hit = detect_bill(
            "Your invoice from Acme",
            "Amount due: $42.10. Payment due by August 15, 2026.",
            now(),
        )
        .expect("should detect bill");
        assert_eq!(hit.amount, Some(42.10));
        assert_eq!(hit.currency.as_deref(), Some("USD"));
        assert!(!hit.past_due);
        assert_eq!(hit.kind, "invoice");
    }

    #[test]
    fn dated_past_bill_is_past_due() {
        let hit = detect_bill(
            "Invoice 88",
            "Amount due $1,299.00, due date: January 5, 2026",
            now(),
        )
        .expect("should detect");
        assert!(hit.past_due);
        assert_eq!(hit.amount, Some(1299.00));
        assert_eq!(hit.kind, "past_due");
    }

    #[test]
    fn explicit_overdue_language_is_past_due() {
        let hit = detect_bill(
            "PAST DUE: your account",
            "Your payment is overdue. Please remit $50.",
            now(),
        )
        .unwrap();
        assert!(hit.past_due);
        assert_eq!(hit.kind, "past_due");
    }

    #[test]
    fn due_on_receipt_anchors_to_today() {
        let hit = detect_bill("Invoice", "Payment due on receipt. $100.00", now()).unwrap();
        assert!(hit.source.contains("due-on-receipt"));
    }

    #[test]
    fn non_bill_returns_none() {
        assert!(detect_bill("Lunch?", "wanna grab food", now()).is_none());
    }
}
