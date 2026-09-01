//! Receipt / purchase-record detection: pure classification of mail recording
//! money ALREADY paid ("payment received", "your receipt", "order confirmation",
//! "you were charged"), plus a best-effort total ([`ReceiptInfo`]). A receipt is
//! a RECORD — noise-tier for the ranked inbox, but its own client category.
//! Never run for sealed mail (see docs/SECURITY.md).
//!
//! Invariants: a receipt and a SHIPMENT can COEXIST — detection is independent,
//! neither suppresses the other. A receipt is NEVER a bill: obligation phrasing
//! excludes here and [`crate::triage::deadline::detect_bill`] excludes receipt
//! phrasing, so the two classifiers never double-claim a message. Refunds /
//! inbound money and pure marketing return `None`.

use crate::triage::text::rx;
use regex::Regex;
use std::sync::OnceLock;

/// A detected receipt: the `receipts` table shape minus the DB
/// identity/timestamp/sender columns, which ingest supplies from the message.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiptInfo {
    /// The extracted total. Best-effort, not a gate: a receipt with no parseable
    /// total is STILL a receipt.
    pub amount: Option<f64>,
    /// Currency code for `amount` (always "USD" in v0); `None` when `amount` is.
    pub currency: Option<String>,
}

struct ReceiptDetector {
    /// Past-transaction / purchase-record phrasing; any one classifies the
    /// message as a receipt, subject to the exclusions below.
    receipt_phrases: Vec<Regex>,
    /// Currency amount patterns.
    amount: Vec<Regex>,
    /// An amount ADJACENT to a total-word, preferred over the largest amount in
    /// the body. Captures the amount in group 1.
    total_adjacent: Vec<Regex>,
    /// REFUND / inbound-money phrasing: money flowing TO the user, not a
    /// purchase — exclude.
    inbound_money: Vec<Regex>,
    /// Genuine payment-OBLIGATION phrasing (the user still OWES): a BILL, not a
    /// receipt — exclude, so the two classifiers never double-claim a message.
    obligation: Vec<Regex>,
}

fn detector() -> &'static ReceiptDetector {
    static D: OnceLock<ReceiptDetector> = OnceLock::new();
    D.get_or_init(|| ReceiptDetector {
        receipt_phrases: vec![
            rx(r"\bpayment (was )?received\b"),
            rx(r"\breceipt for (your )?"),
            rx(r"\byour receipt\b"),
            rx(r"\bhere('| i)s your receipt\b"),
            rx(r"\bthis is (your )?receipt\b"),
            rx(r"\border confirmation\b"),
            rx(r"\byour order\b"),
            rx(r"\bthank you for your (payment|order|purchase|ride|trip)\b"),
            rx(r"\byou (were|have been) charged\b"),
            rx(r"\bwas charged\b"),
            rx(r"\bamount charged\b"),
            rx(r"\btransaction (receipt|complete|completed)\b"),
            rx(r"\binvoice paid\b"),
            rx(r"\border total\b"),
            rx(r"\bpayment (confirmation|successful|processed|complete|completed)\b"),
            rx(r"\bpayment posted\b"),
            rx(r"\bsuccessfully paid\b"),
            rx(r"\byour (payment|order|purchase) has been\b"),
            // Ride/service receipts.
            rx(r"\b(ride|trip) receipt\b"),
            rx(r"\breceipt for your (ride|trip)\b"),
            rx(r"\bthanks for (riding|your ride)\b"),
        ],
        amount: vec![
            // $1,234.56 or $42 or $42.10
            rx(r"\$\s?([0-9][0-9,]*(?:\.[0-9]+)?)"),
            // 1,234.56 USD / 42.00 usd
            rx(r"\b([0-9][0-9,]*(?:\.[0-9]+)?)\s?(?:USD|usd)\b"),
            // USD 1,234.56
            rx(r"\b(?:USD|usd)\s?([0-9][0-9,]*(?:\.[0-9]+)?)"),
        ],
        total_adjacent: vec![
            // "order total: $3.49" / "grand total $12" / "total $3.49" /
            // "amount charged: $3.49" / "you paid $3.49" / "charged $3.49".
            rx(r"\b(?:order total|grand total|total|amount(?:\s+charged)?|paid|charged)\b[:\s]*\$\s?([0-9][0-9,]*(?:\.[0-9]+)?)"),
            rx(r"\b(?:order total|grand total|total|amount(?:\s+charged)?|paid|charged)\b[:\s]*([0-9][0-9,]*(?:\.[0-9]+)?)\s?(?:USD|usd)\b"),
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
            rx(r"\bmoney back\b"),
        ],
        obligation: vec![
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
            rx(r"\bpay by\b"),
            rx(r"\bpayment\b.{0,40}?\bis (now )?due\b"),
            rx(r"\bdue (up)?on receipt\b"),
        ],
    })
}

/// Money flowing TO the user (a refund) is never a receipt.
fn has_inbound_money(text: &str) -> bool {
    detector().inbound_money.iter().any(|re| re.is_match(text))
}

/// A genuine payment-OBLIGATION phrase (the user still owes) makes this a BILL,
/// not a receipt.
fn has_obligation(text: &str) -> bool {
    detector().obligation.iter().any(|re| re.is_match(text))
}

/// Parse a single amount token ("1,234.56") to `f64`, ROUNDED TO CENTS.
///
/// The fraction the patterns above capture is deliberately unbounded (`[0-9]+`,
/// not `[0-9]{2}`), and this is the other half of that decision. A two-digit
/// fraction looks like the only thing a price can have right up until a real
/// sender does its arithmetic in binary floating point and prints the result
/// unrounded — Amazon's shipment mail says
///
/// ```text
/// Total
/// 67.28999999999999 USD
/// ```
///
/// A pattern that admits exactly two decimals cannot match that at all, so the
/// engine used to give up on the "67" and resume scanning INSIDE the number,
/// where `28999999999999 USD` matches beautifully. Every affected receipt was
/// stored as its own fractional tail: a $67.29 order became $28,999,999,999,999
/// on the card. Taking the whole number and rounding here is what keeps the
/// value and the cents both honest.
fn parse_amount(raw: &str) -> Option<f64> {
    let v = raw.replace(',', "").parse::<f64>().ok()?;
    if !v.is_finite() {
        return None;
    }
    Some((v * 100.0).round() / 100.0)
}

/// Extract the receipt TOTAL: an amount adjacent to a total-word if there is
/// one, else the LARGEST currency amount (the total is almost always the largest
/// line on a receipt). `None` when nothing parses.
fn extract_total(text: &str) -> Option<(f64, String)> {
    let d = detector();
    // 1. Prefer an amount adjacent to a total-word.
    for re in &d.total_adjacent {
        if let Some(cap) = re.captures(text)
            && let Some(m) = cap.get(1)
            && let Some(v) = parse_amount(m.as_str())
        {
            return Some((v, "USD".to_string()));
        }
    }
    // 2. Else the largest currency amount anywhere in the text.
    let mut best: Option<f64> = None;
    for re in &d.amount {
        for cap in re.captures_iter(text) {
            if let Some(m) = cap.get(1)
                && let Some(v) = parse_amount(m.as_str())
            {
                best = Some(best.map_or(v, |b| b.max(v)));
            }
        }
    }
    best.map(|v| (v, "USD".to_string()))
}

/// The text every rule here reads: the sender, the subject and the body, in that
/// order. ONE definition, because [`recompute_total`] has to see character for
/// character what [`detect_receipt`] saw or a re-parse would answer a different
/// question than the parse it is correcting.
fn haystack(from_addr: &str, subject: &str, body: &str) -> String {
    format!("{from_addr}\n{subject}\n{body}")
}

/// Re-run ONLY the total extraction over a message already known to be a
/// receipt, for the one-shot repair of rows a looser
/// [`parse_amount`] stored wrong. `None` when nothing parses, which is a legal
/// receipt state (the amount column is nullable).
///
/// Deliberately NOT [`detect_receipt`]: the row's existence already settles the
/// classification, and re-litigating it here would let an unrelated change to
/// the exclusion phrases silently delete somebody's purchase history during
/// what is supposed to be an arithmetic fix.
pub fn recompute_total(from_addr: &str, subject: &str, body: &str) -> Option<f64> {
    extract_total(&haystack(from_addr, subject, body)).map(|(amount, _)| amount)
}

/// Detect a receipt from a message's surfaces. `None` when the message is not a
/// purchase record, is a refund, or is a bill. Classification is driven by
/// past-transaction phrasing, so a receipt with no parseable total still counts.
///
/// SECURITY: callers MUST NOT invoke this for sealed mail (docs/SECURITY.md).
pub fn detect_receipt(from_addr: &str, subject: &str, body: &str) -> Option<ReceiptInfo> {
    let d = detector();
    let hay = haystack(from_addr, subject, body);

    // 0. Refund / inbound money: flowing TO the user, so not a purchase.
    if has_inbound_money(&hay) {
        return None;
    }

    // 0b. A genuine obligation is a BILL (detect_bill's job), never a receipt.
    if has_obligation(&hay) {
        return None;
    }

    // 1. Without past-transaction phrasing, a stray amount is not a receipt.
    if !d.receipt_phrases.iter().any(|re| re.is_match(&hay)) {
        return None;
    }

    // 2. Best-effort total. `None` is fine — still a receipt.
    let (amount, currency) = match extract_total(&hay) {
        Some((a, c)) => (Some(a), Some(c)),
        None => (None, None),
    };

    Some(ReceiptInfo { amount, currency })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- classification: past-transaction phrasing -----------------------

    #[test]
    fn bay_wheels_ride_receipt_is_a_receipt_with_amount() {
        // A ride receipt must classify and pull its total.
        let r = detect_receipt(
            "no-reply@baywheels.com",
            "Your Bay Wheels ride receipt",
            "Thanks for riding! Receipt for your ride. Total: $3.49. Ride time 12 min.",
        )
        .expect("bay wheels ride receipt");
        assert_eq!(r.amount, Some(3.49));
        assert_eq!(r.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn order_confirmation_total_extracts_amount() {
        let r = detect_receipt(
            "orders@shop.com",
            "Order confirmation #12345",
            "Thank you for your order. Order total $3.49.",
        )
        .expect("order confirmation");
        assert_eq!(r.amount, Some(3.49));
    }

    #[test]
    fn total_adjacent_preferred_over_larger_line_items() {
        // The body has larger per-line amounts, but the TOTAL word anchors 12.00.
        let r = detect_receipt(
            "receipts@shop.com",
            "Your receipt",
            "Widget $9.99. Gadget $8.50. Order total: $12.00. Thanks for your purchase.",
        )
        .expect("receipt");
        assert_eq!(
            r.amount,
            Some(12.00),
            "total-adjacent wins over the largest line"
        );
    }

    #[test]
    fn largest_amount_used_when_no_total_word() {
        // No total-word: fall back to the LARGEST currency amount (the total).
        let r = detect_receipt(
            "receipts@shop.com",
            "Receipt for your purchase",
            "You were charged for: coffee $4.50, muffin $3.00, tax $0.60. $8.10.",
        )
        .expect("receipt");
        assert_eq!(r.amount, Some(8.10));
    }

    #[test]
    fn receipt_with_no_amount_is_still_a_receipt() {
        // No parseable total anywhere — still a receipt, with amount None.
        let r = detect_receipt(
            "no-reply@service.com",
            "Your receipt",
            "Thank you for your payment. Your receipt is attached as a PDF.",
        )
        .expect("receipt without amount is still a receipt");
        assert_eq!(r.amount, None);
        assert_eq!(r.currency, None);
    }

    #[test]
    fn you_were_charged_is_a_receipt() {
        let r = detect_receipt(
            "billing@saas.com",
            "Payment successful",
            "You were charged $29.00 for your monthly subscription.",
        )
        .expect("charged receipt");
        assert_eq!(r.amount, Some(29.00));
    }

    // ---- exclusions ------------------------------------------------------

    #[test]
    fn an_unrounded_float_total_parses_to_cents_not_to_its_tail() {
        // AMAZON'S OWN ARITHMETIC, verbatim from a shipment email: their total
        // is summed in binary floating point and printed unrounded. The old
        // patterns admitted exactly two decimals, so they could not match at the
        // "67" at all and the engine resumed scanning INSIDE the number, where
        // "28999999999999 USD" matched perfectly. A $67.29 order was stored, and
        // rendered on the receipts card, as $28,999,999,999,999.
        let body = "* Gap Filler Syringe\r\n  Quantity: 1\r\n  7.97 USD\r\n\
                    \r\n* Goat Ram Horn Devil Mask\r\n  Quantity: 2\r\n  21.99 USD\r\n\
                    \r\nTotal\r\n67.28999999999999 USD\r\n\
                    \r\nIf your order contains one or more items from a third party.\r\n";
        let r = detect_receipt(
            "shipment-tracking@amazon.com",
            "Shipped: 2 \"Goat Ram Horn Devil Mask...\" and 2 more items",
            body,
        )
        .expect("order confirmation is a receipt");
        assert_eq!(r.amount, Some(67.29), "total must round to cents");
    }

    #[test]
    fn every_float_artifact_seen_in_production_rounds_correctly() {
        // The five that actually landed, with the totals they should have had.
        for (printed, want) in [
            ("14.530000000000001", 14.53),
            ("40.099999999999994", 40.10),
            ("96.63999999999999", 96.64),
            ("77.41999999999999", 77.42),
            ("67.28999999999999", 67.29),
        ] {
            let body = format!("Your order\r\nTotal\r\n{printed} USD\r\n");
            let r = detect_receipt("orders@amazon.com", "Your order", &body)
                .expect("receipt classified");
            assert_eq!(r.amount, Some(want), "{printed} should store as {want}");
        }
    }

    #[test]
    fn a_long_fraction_after_a_dollar_sign_rounds_too() {
        let r = detect_receipt("a@b.com", "Your receipt", "Total: $14.530000000000001")
            .expect("receipt");
        assert_eq!(r.amount, Some(14.53));
    }

    #[test]
    fn ordinary_two_decimal_totals_are_unchanged() {
        // The fix widens what the patterns accept; it must not move what they
        // already got right.
        let r =
            detect_receipt("a@b.com", "Your receipt", "Order total: $1,234.56").expect("receipt");
        assert_eq!(r.amount, Some(1234.56));
        let r = detect_receipt("a@b.com", "Your order", "Total\r\n42 USD").expect("receipt");
        assert_eq!(r.amount, Some(42.0));
    }

    #[test]
    fn refund_is_not_a_receipt() {
        // A refund is money flowing TO the user — NOT a purchase receipt.
        let r = detect_receipt(
            "ebay@ebay.com",
            "Your refund receipt",
            "Your refund of $23.99 has been issued to your original payment method.",
        );
        assert!(r.is_none(), "refund must not be a receipt: {r:?}");
    }

    #[test]
    fn bill_payment_due_is_not_a_receipt() {
        // An obligation stays a bill; the two classifiers never double-claim.
        let r = detect_receipt(
            "billing@pge.com",
            "Your PG&E statement",
            "Your amount due is $84.20. Payment due by August 1, 2026.",
        );
        assert!(r.is_none(), "a bill must not be a receipt: {r:?}");
    }

    #[test]
    fn pure_marketing_is_not_a_receipt() {
        // No transaction phrasing at all — a marketing blast, not a receipt.
        let r = detect_receipt(
            "news@shop.com",
            "50% off everything this weekend!",
            "Our biggest sale of the year. Shop now and save. Unsubscribe here.",
        );
        assert!(r.is_none(), "marketing must not be a receipt: {r:?}");
    }

    #[test]
    fn otp_shaped_email_is_not_a_receipt() {
        // Ingest never calls this for sealed mail; an OTP also carries no receipt
        // phrasing, so it cannot classify either way.
        let r = detect_receipt(
            "noreply@bank.com",
            "Your verification code",
            "Your one-time passcode is 483920. Enter this code to continue.",
        );
        assert!(r.is_none(), "OTP must not be a receipt: {r:?}");
    }

    // ---- precedence: receipt + shipment coexist --------------------------

    #[test]
    fn order_confirmation_with_tracking_is_still_a_receipt() {
        // A total AND a tracking number is both; the shipment classifier fires
        // separately in ingest.
        let r = detect_receipt(
            "auto@amazon.com",
            "Your Amazon.com order confirmation",
            "Thank you for your order. Order total: $42.10. Tracking TBA303392911000.",
        )
        .expect("order confirmation is a receipt even with tracking");
        assert_eq!(r.amount, Some(42.10));
    }
}
