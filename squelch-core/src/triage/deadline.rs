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
    /// Screamy / scammy phrasing that legit billers rarely use. A corroborating
    /// signal for the Stage-1 confidence dampener (bug #3), NOT for tier.
    scammy: Vec<Regex>,
    /// INBOUND-MONEY phrasing: refund / return / reimbursement / credit-to-you /
    /// cash-back / payout / "you will receive". When any of these fire (and no
    /// genuine payment-obligation signal does), the message is money flowing TO
    /// the user, not a bill — it must NOT produce a bill tier or past_due.
    inbound_money: Vec<Regex>,
    /// PAST-TRANSACTION phrasing: receipts / payment confirmations / order
    /// confirmations. Past/completed-tense language ("payment received", "thank
    /// you for your payment", "order confirmation") means the money has ALREADY
    /// moved — the message is a RECORD, not a deadline. When any of these fire
    /// (and no genuine payment-obligation signal does), the bill classification
    /// is suppressed so a receipt with a stray amount/date drops to the rung-5
    /// receipt-noise rung instead of screaming as a bill.
    past_transaction: Vec<Regex>,
}

/// Sanity bounds on a resolved due date, relative to the message's `received_at`.
/// A date more than [`MAX_DAYS_PAST`] before the message, or more than
/// [`MAX_DAYS_FUTURE`] after it, is treated as a PARSE FAILURE (absurd dates are
/// parser bugs, not two-year-late bills). See bug: eBay "July 13th" mis-yeared to
/// ~2 years past and shown as "104 weeks after due date".
const MAX_DAYS_PAST: i64 = 365;
const MAX_DAYS_FUTURE: i64 = 365 * 3;

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
            // "(next )payment of $54.99 is due ...": an amount sits between
            // "payment" and "due", so the contiguous pattern above misses it.
            (rx(r"\bpayment\b.{0,40}?\bis (now )?due\b"), "payment_due"),
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
            // ORDER MATTERS — year-bearing "Month D, YYYY" patterns come FIRST so
            // the day/year comma ("January 5, 2026") is captured whole instead of
            // being truncated to "January 5" by the generic terminator below.
            // (The regex crate has no look-around, so we can't special-case the
            // comma inside one pattern — we try the explicit shapes first.)
            rx(r"\b(?:payment\s+)?due(?:\s+date)?[:\s]+(?:on\s+|by\s+)?([A-Za-z]{3,9}\.?\s+\d{1,2}(?:st|nd|rd|th)?,\s*\d{4})"),
            rx(r"\bby\s+([A-Za-z]{3,9}\.?\s+\d{1,2}(?:st|nd|rd|th)?,\s*\d{4})"),
            // Generic fallback: any date-ish fragment up to a terminator (covers
            // ISO/numeric, "Month D YYYY" without a comma, and bare year-less
            // "Month D" which the year-less resolver then anchors to received_at).
            rx(r"\b(?:payment\s+)?due(?:\s+date)?[:\s]+(?:on\s+|by\s+)?([A-Za-z0-9\-/ ]{4,30}?)(?:[.,;)\n]|$)"),
            rx(r"\bby\s+([A-Za-z0-9\-/ ]{4,30}?)(?:[.,;)\n]|$)"),
        ],
        due_on_receipt: rx(r"\bdue\s+(?:up)?on\s+receipt\b"),
        scammy: vec![
            rx(r"penalties are adding up"),
            rx(r"\bfinal notice\b"),
            rx(r"\bact (now|immediately)\b"),
            rx(r"\bavoid (further |additional )?(penalt|prosecution|legal|suspension|arrest)"),
            rx(r"\bimmediate (action|payment) (is )?required\b"),
            rx(r"\byour account (will|may) be (suspended|terminated|closed)\b"),
            // Excessive urgency punctuation: "!!!", "!!", "NOW!!" etc.
            rx(r"!{2,}"),
        ],
        inbound_money: vec![
            rx(r"\brefund(ed|s|ing)?\b"),
            rx(r"\breturn\s+(refund|credit|of)\b"),
            rx(r"\breimburs(e|ed|ement|ing)\b"),
            rx(r"\bcash[-\s]?back\b"),
            rx(r"\bpay ?out\b"),
            rx(r"\bstore credit\b"),
            rx(r"\bcredit(ed)?\s+to\s+(you|your)\b"),
            rx(r"\bwill be (issued|credited|refunded|deposited)\b"),
            rx(r"\byou('| wi)ll receive\b"),
            rx(r"\byou have (been|received)\b.*\b(refund|credit|reimburs)"),
            rx(r"\bmoney back\b"),
        ],
        past_transaction: vec![
            rx(r"\bpayment (was )?received\b"),
            rx(r"\bthank you for your payment\b"),
            rx(r"\bpayment (processed|successful|complete|confirmation)\b"),
            rx(r"\byour (payment|order) has been\b"),
            rx(r"\border confirmation\b"),
            rx(r"\breceipt for (your )?"),
            rx(r"\byou (were|have been) charged\b"),
            rx(r"\bwas charged\b"),
            rx(r"\btransaction (complete|receipt)\b"),
            rx(r"\binvoice (paid|has been paid)\b"),
            rx(r"\bsuccessfully paid\b"),
            rx(r"\bauto-?pay(ment)? (processed|complete)\b"),
            rx(r"\bthis is (your )?receipt\b"),
            rx(r"\bpayment posted\b"),
        ],
    })
}

/// Does the text carry screamy/scammy phrasing (a bug #3 confidence dampener)?
/// Purely advisory — it never raises a tier, only shaves confidence.
pub fn has_scammy_phrasing(subject: &str, body: &str) -> bool {
    let d = detector();
    d.scammy
        .iter()
        .any(|re| re.is_match(subject) || re.is_match(body))
}

/// Does the text carry INBOUND-MONEY phrasing (refund / return / reimbursement /
/// credit-to-you / cash-back / payout / "you will receive")? Money flowing TO the
/// user is never a bill. Advisory to [`detect_bill`], which suppresses the bill
/// classification when this fires WITHOUT a genuine payment-obligation signal.
fn has_inbound_money_phrasing(text: &str) -> bool {
    detector().inbound_money.iter().any(|re| re.is_match(text))
}

/// Does the text carry PAST-TRANSACTION phrasing (a receipt / payment
/// confirmation / order confirmation)? Past/completed-tense money language means
/// the money has ALREADY moved: the message is a RECORD, not a deadline. Advisory
/// to [`detect_bill`], which suppresses the bill classification when this fires
/// WITHOUT a genuine payment-obligation signal — so receipts drop to the rung-5
/// receipt-noise rung instead of screaming as a bill. Receipts are records, not
/// deadlines.
fn has_past_transaction_phrasing(text: &str) -> bool {
    detector().past_transaction.iter().any(|re| re.is_match(text))
}

/// Genuine payment-OBLIGATION phrasing: the user owes money. Used to break the
/// tie when BOTH a bill-obligation phrase AND refund phrasing appear — we prefer
/// the BILL reading (conservative for the user's missed-bill fear). Anchored on
/// obligation-shaped wording so a bare "credit" (e.g. "store credit") does not
/// count, but "credit card payment due" / "amount due" / "past due" do.
fn has_payment_obligation_phrasing(text: &str) -> bool {
    static OBLIGATION: OnceLock<Vec<Regex>> = OnceLock::new();
    let res = OBLIGATION.get_or_init(|| {
        vec![
            rx(r"\bpast[-\s]?due\b"),
            rx(r"\boverdue\b"),
            rx(r"\bunpaid\b"),
            rx(r"\bamount due\b"),
            rx(r"\bpayment (is )?(now )?due\b"),
            rx(r"\bpayment due\b"),
            rx(r"\bbalance due\b"),
            rx(r"\bminimum payment\b"),
            rx(r"\bbill(ing)? (is )?due\b"),
            rx(r"\bplease (pay|remit)\b"),
            rx(r"\bpay(ment)? (your|this|the) (bill|invoice|balance)\b"),
            rx(r"\bpay by\b"),
            // "(next )payment of $54.99 is due ...": obligation phrasing where an
            // amount sits between "payment" and "due", so the contiguous
            // "payment due" patterns above miss it.
            rx(r"\bpayment\b.{0,40}?\bis (now )?due\b"),
        ]
    });
    res.iter().any(|re| re.is_match(text))
}

/// Parse a single absolute date fragment into a `NaiveDate`. Tries a battery of
/// common human/machine formats. Returns `None` for anything it can't confidently
/// read (including relative dates like "Friday").
///
/// `received_at` anchors year-less dates: a date written without a year ("July
/// 13", "January 3") is resolved to its NEAREST OCCURRENCE relative to the
/// message's receipt, NOT to wall-clock now (so backfill is deterministic and a
/// message received in 2024 does not get today's year). See
/// [`resolve_yearless_date`]. This was the root cause of the eBay "104 weeks
/// after due date" bug: the old code slapped `Utc::now().year()` on the fragment
/// and never consulted `received_at` or rolled the year forward.
fn parse_date_fragment(frag: &str, received_at: DateTime<Utc>) -> Option<NaiveDate> {
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

    // Month-name formats WITH an explicit year: take them verbatim.
    const WITH_YEAR: &[&str] = &[
        "%B %d %Y", "%b %d %Y", "%B %d, %Y", "%b %d, %Y", "%d %B %Y", "%d %b %Y",
    ];
    for fmt in WITH_YEAR {
        if let Ok(d) = NaiveDate::parse_from_str(cleaned, fmt) {
            return Some(d);
        }
    }

    // Year-LESS month-name formats: parse the month/day against a placeholder
    // year, then resolve the real year to the nearest occurrence relative to
    // `received_at`.
    const NO_YEAR: &[&str] = &["%B %d", "%b %d", "%d %B", "%d %b"];
    for fmt in NO_YEAR {
        // Use a leap year (2000) as the placeholder so "Feb 29" still parses; the
        // year is discarded by `resolve_yearless_date`.
        let candidate = format!("{cleaned} 2000");
        let yfmt = format!("{fmt} %Y");
        if let Ok(d) = NaiveDate::parse_from_str(&candidate, &yfmt) {
            return resolve_yearless_date(d.month(), d.day(), received_at);
        }
    }

    None
}

/// Resolve a year-less (month, day) to a full `NaiveDate` using the NEAREST
/// OCCURRENCE relative to the message's `received_at`:
///   1. take the occurrence in the SAME year as `received_at`;
///   2. if that lands MORE THAN 14 DAYS BEFORE `received_at`, roll forward one
///      year (a bill/deadline written without a year is forward-looking; a
///      recently-passed date — within 14 days — stays in the receipt year so a
///      genuinely just-missed bill still reads as past-due by days).
///
/// Examples (all deterministic in `received_at`, never wall-clock now):
///   * "July 13"  received 2026-07-09 => 2026-07-13 (future, this year).
///   * "July 1"   received 2026-07-09 => 2026-07-01 (past by 8 days, kept).
///   * "January 3" received 2026-12-28 => 2027-01-03 (rolled forward).
fn resolve_yearless_date(
    month: u32,
    day: u32,
    received_at: DateTime<Utc>,
) -> Option<NaiveDate> {
    let recv = received_at.date_naive();
    let same_year = NaiveDate::from_ymd_opt(recv.year(), month, day)?;
    if (recv - same_year).num_days() > 14 {
        // More than two weeks before receipt: roll forward a year.
        NaiveDate::from_ymd_opt(recv.year() + 1, month, day)
    } else {
        Some(same_year)
    }
}

/// Turn a `NaiveDate` into an end-of-day UTC instant. A bill "due Jan 5" isn't
/// past-due at 00:00 on Jan 5, so we anchor to 23:59:59 of that day.
fn end_of_day_utc(d: NaiveDate) -> DateTime<Utc> {
    let ndt = d.and_hms_opt(23, 59, 59).expect("valid end-of-day");
    Utc.from_utc_datetime(&ndt)
}

/// Is this date within the sane window around `received_at`? A date more than
/// [`MAX_DAYS_PAST`] before, or more than [`MAX_DAYS_FUTURE`] after, the message
/// is treated as a parse failure (an absurd date is a parser bug, never a real
/// two-year-late bill).
fn date_is_sane(due: DateTime<Utc>, received_at: DateTime<Utc>) -> bool {
    let days = (due - received_at).num_days();
    (-MAX_DAYS_PAST..=MAX_DAYS_FUTURE).contains(&days)
}

/// Extract a due-date instant from the given text, if present. `received_at`
/// anchors year-less dates and the sanity bounds.
///
/// TODO(v0): relative dates ("due Friday", "due next week") are not parsed.
/// Absolute dates dominate real bills; add relative parsing (anchored to
/// `received_at`) in a later pass if recall analysis shows it matters.
fn extract_due_at(
    text: &str,
    received_at: DateTime<Utc>,
) -> Option<(DateTime<Utc>, &'static str)> {
    let d = detector();

    if d.due_on_receipt.is_match(text) {
        // Due on receipt: due at end of the day the message was received
        // (received_at, not wall-clock now, so backfill is deterministic).
        return Some((end_of_day_utc(received_at.date_naive()), "due-on-receipt"));
    }

    for re in &d.due_phrase {
        for cap in re.captures_iter(text) {
            if let Some(m) = cap.get(1)
                && let Some(date) = parse_date_fragment(m.as_str(), received_at)
            {
                // The FIRST fragment that parses to a real date is authoritative
                // for this phrase. Sanity guard: if that date is out of bounds it
                // is a parser bug — return no deadline rather than silently
                // downgrading to a looser (e.g. year-less) reinterpretation of the
                // same text, which would resurrect the "104 weeks" class of bug.
                let due = end_of_day_utc(date);
                return date_is_sane(due, received_at).then_some((due, "due-phrase"));
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
/// `now` is the message's `received_at`: it anchors year-less dates, the
/// due-date sanity bounds, and the past-due math, all deterministically (never
/// wall-clock now, so a backfilled message resolves the same way every run).
pub fn detect_bill(
    subject: &str,
    body: &str,
    now: DateTime<Utc>,
) -> Option<DeadlineHit> {
    let d = detector();
    let hay = format!("{subject}\n{body}");

    // 0. INBOUND-MONEY EXCLUSION. A refund / return / reimbursement / cash-back /
    //    payout / "you will receive" is money flowing TO the user, NOT a payment
    //    obligation. Suppress the bill classification entirely so it never yields
    //    a bill tier or a past_due.
    //
    //    PRECEDENCE: when BOTH refund phrasing AND a genuine payment-obligation
    //    phrase ("amount due", "past due", "credit card payment due", …) appear,
    //    we prefer the BILL reading (conservative for the user's missed-bill
    //    fear) and do NOT suppress. This keeps genuine bills that merely mention
    //    "credit" alive while dropping refund emails.
    if has_inbound_money_phrasing(&hay) && !has_payment_obligation_phrasing(&hay) {
        return None;
    }

    // 0b. PAST-TRANSACTION EXCLUSION. A receipt / payment confirmation / order
    //     confirmation ("payment received", "thank you for your payment", "order
    //     confirmation") records money that has ALREADY moved. It is a RECORD,
    //     not a deadline — a stray amount or shipping date must NOT let it fire
    //     as a bill on rung 1 before the rung-5 receipt-noise rung is consulted.
    //     Suppress the bill classification entirely.
    //
    //     PRECEDENCE (same rule as the refund exclusion above, same
    //     `has_payment_obligation_phrasing` guard): when BOTH receipt phrasing
    //     AND a genuine payment-obligation phrase ("amount due", "past due",
    //     "next payment ... due", "pay by", …) appear, we prefer the BILL reading
    //     (conservative for the user's missed-bill fear) and do NOT suppress. So
    //     a statement that says "payment received — next payment due Aug 1" is
    //     still a bill.
    if has_past_transaction_phrasing(&hay) && !has_payment_obligation_phrasing(&hay) {
        return None;
    }

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
    let due = extract_due_at(&hay, now);

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

    fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_common_date_formats() {
        let expect = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        for s in [
            "2026-01-05",
            "01/05/2026",
            "January 5, 2026",
            "Jan 5 2026",
            "5 January 2026",
            "January 5th, 2026",
        ] {
            assert_eq!(parse_date_fragment(s, now()), Some(expect), "failed: {s:?}");
        }
    }

    #[test]
    fn rejects_relative_dates() {
        assert_eq!(parse_date_fragment("Friday", now()), None);
        assert_eq!(parse_date_fragment("next week", now()), None);
    }

    // ---- year-less date resolution: nearest occurrence vs received_at ------

    #[test]
    fn yearless_future_date_uses_receipt_year() {
        // "July 13" received 2026-07-09 => 2026-07-13 (same year, future).
        let d = parse_date_fragment("July 13", at(2026, 7, 9)).unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2026, 7, 13).unwrap());
    }

    #[test]
    fn yearless_recently_passed_date_stays_in_receipt_year() {
        // "July 1" received 2026-07-09 => 2026-07-01 (past by 8 days, within the
        // 14-day grace, so NOT rolled forward — a genuine just-missed bill).
        let d = parse_date_fragment("July 1", at(2026, 7, 9)).unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2026, 7, 1).unwrap());
    }

    #[test]
    fn yearless_long_past_date_rolls_forward() {
        // "January 3" received 2026-12-28 => 2027-01-03 (>14 days before receipt
        // in the same year, so roll forward — December->January rollover).
        let d = parse_date_fragment("January 3", at(2026, 12, 28)).unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2027, 1, 3).unwrap());
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

    // ---- inbound-money exclusion (refund/return/credit-to-you/...) --------

    #[test]
    fn ebay_return_refund_is_not_a_bill() {
        // The real failing case: an eBay RETURN REFUND "by July 13th" must NOT be
        // detected as a bill at all — money flows TO the user.
        let hit = detect_bill(
            "Your eBay return refund",
            "Your eBay return refund of $23.99 will be issued by July 13th.",
            at(2026, 7, 9),
        );
        assert!(hit.is_none(), "refund must not be a bill: {hit:?}");
    }

    #[test]
    fn reimbursement_and_cashback_are_not_bills() {
        assert!(detect_bill(
            "Reimbursement processed",
            "Your reimbursement of $40 will be deposited by August 1.",
            now()
        )
        .is_none());
        assert!(detect_bill(
            "Cash back reward",
            "You will receive $15 cash back on your next statement.",
            now()
        )
        .is_none());
    }

    #[test]
    fn genuine_credit_card_bill_survives_refund_exclusion() {
        // "credit card payment due" carries a genuine obligation phrase, so the
        // bare word "credit" must NOT trip the refund exclusion.
        let hit = detect_bill(
            "Credit card statement",
            "Your credit card payment is due. Amount due $312.00, due date July 20, 2026.",
            at(2026, 7, 9),
        )
        .expect("genuine credit-card bill must survive");
        assert_eq!(hit.amount, Some(312.00));
    }

    #[test]
    fn refund_plus_obligation_prefers_the_bill() {
        // Both a refund phrase AND a payment obligation appear: prefer the BILL
        // reading (conservative for the user's missed-bill fear).
        let hit = detect_bill(
            "Account update",
            "A refund was applied, but your amount due is $88.00, past due.",
            now(),
        )
        .expect("obligation wins the tie");
        assert!(hit.past_due);
    }

    // ---- past-transaction exclusion (receipts are records, not deadlines) --

    #[test]
    fn payment_received_receipt_is_not_a_bill() {
        // Fixture (a): a payment-received confirmation carries money words (and
        // could carry a date) but the money has ALREADY moved. It is a receipt,
        // not a deadline — must NOT be a bill.
        let hit = detect_bill(
            "Payment received",
            "Your payment of $54.99 to PG&E was received.",
            now(),
        );
        assert!(hit.is_none(), "receipt must not be a bill: {hit:?}");
    }

    #[test]
    fn order_receipt_with_shipping_date_is_not_a_bill() {
        // Fixture (b): an order receipt with a total and a shipping date. The
        // "receipt for your order" phrasing suppresses the bill classification
        // even though a date is present.
        let hit = detect_bill(
            "Receipt for your order #12345",
            "Receipt for your order #12345 — total $89.20. Ships by August 15, 2026.",
            now(),
        );
        assert!(hit.is_none(), "order receipt must not be a bill: {hit:?}");
    }

    #[test]
    fn receipt_plus_next_payment_due_prefers_the_bill() {
        // Fixture (c): PRECEDENCE. A statement that confirms a received payment
        // AND states a genuine upcoming obligation ("next payment ... is due
        // August 1") is still a BILL/Deadline — conservative for the user's
        // missed-bill fear.
        let hit = detect_bill(
            "Payment confirmation",
            "Payment received. Your next payment of $54.99 is due August 1.",
            at(2026, 7, 9),
        )
        .expect("obligation wins: still a bill");
        assert!(!hit.past_due, "due Aug 1 is future");
        assert_eq!(hit.amount, Some(54.99));
    }

    #[test]
    fn amazon_delivered_order_update_is_not_a_bill() {
        // Fixture (d): an Amazon "Delivered:" order update. No obligation phrase,
        // so even with an order total present it must NOT be a bill.
        let hit = detect_bill(
            "Delivered: Your Amazon.com order",
            "Your order has been delivered. Order confirmation #114-555. Total $27.30.",
            now(),
        );
        assert!(hit.is_none(), "delivered order update must not be a bill: {hit:?}");
    }

    #[test]
    fn thank_you_for_your_payment_is_not_a_bill() {
        // A classic auto-pay receipt: completed-tense, no obligation.
        let hit = detect_bill(
            "Thank you for your payment",
            "Thank you for your payment of $120.00. Your autopay was processed on July 1, 2026.",
            now(),
        );
        assert!(hit.is_none(), "payment thank-you must not be a bill: {hit:?}");
    }

    // ---- extraction-level sanity guard on absurd dates --------------------

    #[test]
    fn absurd_past_year_is_dropped_no_deadline() {
        // A due date resolved to an absurd year (>365 days before receipt) is a
        // parser bug: no deadline emitted.
        assert_eq!(extract_due_at("payment due date: January 5, 2020", now()), None);
    }

    #[test]
    fn absurd_future_year_is_dropped_no_deadline() {
        // >3 years after receipt: also dropped.
        assert_eq!(extract_due_at("due by August 15, 2099", now()), None);
    }
}
