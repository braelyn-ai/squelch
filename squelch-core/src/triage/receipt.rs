//! Receipt / purchase-record detection for the ingest pipeline.
//!
//! This is a PURE detection/extraction module in the same spirit as
//! [`crate::triage::shipment`] and [`crate::triage::deadline`]: given a message's
//! surfaces (from-address, subject, body) it decides whether the message is a
//! RECEIPT — a record of money that has ALREADY been paid (past-tense: "payment
//! received", "your receipt", "order confirmation", "you were charged") — and, if
//! so, extracts a best-effort total ([`ReceiptInfo`]).
//!
//! A receipt is a RECORD, not an obligation and not a signal. It is noise-tier for
//! the ranked inbox, but it deserves its own desktop category (the "Receipts" zone)
//! rather than being mis-grabbed by the Newsletters heuristic (the live Bay Wheels
//! bug this module fixes). The ingest pipeline runs this classifier side by side
//! with the shipment/deadline classifiers for non-sealed mail.
//!
//! ## Precedence / independence (documented invariants)
//!
//!   * A receipt and a SHIPMENT can COEXIST. An order-confirmation that carries a
//!     total AND a tracking number is BOTH a receipt (the record of what you paid)
//!     and a shipment (the package to track). Receipt detection is INDEPENDENT of
//!     shipment detection — neither suppresses the other; the ingest pipeline emits
//!     both when both fire.
//!   * A receipt is NEVER a BILL (obligation). The two are mutually exclusive by
//!     phrasing: [`crate::triage::deadline::detect_bill`] already EXCLUDES
//!     past-transaction phrasing (it suppresses the bill classification when
//!     receipt-shaped language fires without a genuine payment-obligation phrase),
//!     so a "payment received" receipt never lands as a bill on rung 1. This module
//!     is the mirror: it classifies exactly those past-transaction records. We
//!     additionally verify (see [`detect_receipt`]) that a genuine
//!     payment-OBLIGATION email — "amount due", "payment due", "past due" — is NOT
//!     classified as a receipt, so the two classifiers never double-claim a
//!     message.
//!
//! ## Exclusions (return `None`)
//!
//!   * REFUNDS / inbound money: a refund is money flowing TO the user, not a
//!     purchase — it is not a receipt. Reuses the deadline module's inbound-money
//!     phrasing.
//!   * Pure marketing / newsletters with no transaction phrasing: nothing to
//!     record.
//!   * Bills / future obligations: kept separate (see above).
//!
//! SECURITY: this is NEVER run for sealed (auth/2FA) mail — the caller gates on
//! `sensitivity='normal'` exactly like the shipment/deadline paths, so a sealed
//! OTP can never become a receipt. No bodies/keys are logged.

use regex::Regex;
use std::sync::OnceLock;

/// A detected receipt. Mirrors the shape persisted in the `receipts` table minus
/// the DB identity/timestamp/sender columns (those are supplied by the ingest
/// pipeline from the message).
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiptInfo {
    /// The extracted total, or `None` when no amount parses. A receipt with no
    /// parseable total is STILL a receipt (it must still leave Newsletters) — the
    /// amount is best-effort, not a gate.
    pub amount: Option<f64>,
    /// Currency code for `amount` (always "USD" in v0, mirroring the deadline
    /// currency parser). `None` when `amount` is `None`.
    pub currency: Option<String>,
}

struct ReceiptDetector {
    /// Past-transaction / purchase-record phrasing. Any of these classifies the
    /// message as a receipt (subject to the exclusions below). This is the same
    /// family the deadline module uses to SUPPRESS the bill classification; here
    /// it POSITIVELY classifies. Bay-Wheels-style ride/service receipts are
    /// covered by "receipt for" / "your receipt" / "thank you for your ride".
    receipt_phrases: Vec<Regex>,
    /// Currency amount patterns (reused shape from the deadline module).
    amount: Vec<Regex>,
    /// "total|paid|charged|amount|order total|grand total <amount>" — an amount
    /// ADJACENT to a total-word is preferred over the largest amount in the body.
    /// Captures the amount in group 1.
    total_adjacent: Vec<Regex>,
    /// REFUND / inbound-money phrasing (reused semantics from the deadline
    /// module): a refund is money flowing TO the user, not a purchase — exclude.
    inbound_money: Vec<Regex>,
    /// Genuine payment-OBLIGATION phrasing: the user still OWES money. If any of
    /// these fire, the message is a BILL/deadline, not a receipt — exclude, so the
    /// receipt and bill classifiers never double-claim a message.
    obligation: Vec<Regex>,
}

/// Compile a case-insensitive static regex (panics on a bad pattern — these are
/// all compile-time-constant patterns).
fn rx(p: &str) -> Regex {
    Regex::new(&format!("(?i){p}")).expect("static receipt regex must compile")
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
            // Bay-Wheels-style ride/service receipts: "receipt for your ride",
            // "your trip receipt", "thanks for riding".
            rx(r"\b(ride|trip) receipt\b"),
            rx(r"\breceipt for your (ride|trip)\b"),
            rx(r"\bthanks for (riding|your ride)\b"),
        ],
        amount: vec![
            // $1,234.56 or $42 or $42.10
            rx(r"\$\s?([0-9][0-9,]*(?:\.[0-9]{2})?)"),
            // 1,234.56 USD / 42.00 usd
            rx(r"\b([0-9][0-9,]*(?:\.[0-9]{2})?)\s?(?:USD|usd)\b"),
            // USD 1,234.56
            rx(r"\b(?:USD|usd)\s?([0-9][0-9,]*(?:\.[0-9]{2})?)"),
        ],
        total_adjacent: vec![
            // "order total: $3.49" / "grand total $12" / "total $3.49" /
            // "amount charged: $3.49" / "you paid $3.49" / "charged $3.49".
            rx(r"\b(?:order total|grand total|total|amount(?:\s+charged)?|paid|charged)\b[:\s]*\$\s?([0-9][0-9,]*(?:\.[0-9]{2})?)"),
            rx(r"\b(?:order total|grand total|total|amount(?:\s+charged)?|paid|charged)\b[:\s]*([0-9][0-9,]*(?:\.[0-9]{2})?)\s?(?:USD|usd)\b"),
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

/// Does the text carry REFUND / inbound-money phrasing? A refund is money flowing
/// TO the user, not a purchase — never a receipt.
fn has_inbound_money(text: &str) -> bool {
    detector().inbound_money.iter().any(|re| re.is_match(text))
}

/// Does the text carry a genuine payment-OBLIGATION phrase (the user still owes)?
/// Such mail is a BILL/deadline, not a receipt — so the two classifiers never
/// double-claim a message.
fn has_obligation(text: &str) -> bool {
    detector().obligation.iter().any(|re| re.is_match(text))
}

/// Parse a single amount token ("1,234.56") to `f64`.
fn parse_amount(raw: &str) -> Option<f64> {
    raw.replace(',', "").parse::<f64>().ok()
}

/// Extract the receipt TOTAL. Prefers a value adjacent to a total-word
/// (total|paid|charged|amount|order total|grand total); else the LARGEST currency
/// amount in the text (the total is almost always the largest line on a receipt).
/// Returns `(amount, "USD")` or `None` when nothing parses.
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

/// Detect a receipt from a message's surfaces. Returns `None` when the message is
/// not a purchase record, is a refund/inbound-money notice, or is a bill/obligation.
///
/// A receipt WITHOUT a parseable total is STILL a receipt (`amount: None`) — the
/// classification is driven by past-transaction phrasing, not by the amount.
///
/// SECURITY: callers MUST NOT invoke this for sealed mail. It reads only the
/// provided text and never logs.
pub fn detect_receipt(from_addr: &str, subject: &str, body: &str) -> Option<ReceiptInfo> {
    let d = detector();
    let hay = format!("{from_addr}\n{subject}\n{body}");

    // 0. REFUND / INBOUND-MONEY EXCLUSION. A refund is money flowing TO the user —
    //    not a purchase receipt. Suppress entirely. (Mirrors the deadline module's
    //    inbound-money exclusion.)
    if has_inbound_money(&hay) {
        return None;
    }

    // 0b. OBLIGATION EXCLUSION. A genuine "amount due" / "payment due" / "past due"
    //     is a BILL, handled by detect_bill — never a receipt. This keeps the two
    //     classifiers from double-claiming a message (a receipt is never a bill).
    if has_obligation(&hay) {
        return None;
    }

    // 1. Is there past-transaction / purchase-record phrasing at all? Without one,
    //    a stray amount or a marketing blast is not a receipt.
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
        // The live bug: a Bay Wheels ride receipt. It must classify as a receipt
        // (so it leaves Newsletters) and pull its total.
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
        assert_eq!(r.amount, Some(12.00), "total-adjacent wins over the largest line");
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
        // No parseable total anywhere — STILL a receipt (amount None), so it must
        // still leave Newsletters.
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
        // A genuine bill (payment due) is an OBLIGATION handled by detect_bill —
        // it stays a bill and must NOT be double-claimed as a receipt.
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
        // Belt (the ingest pipeline never calls this for sealed mail) + suspenders:
        // an OTP carries no receipt phrasing, so it is not a receipt regardless.
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
        // An order confirmation with a total AND a tracking number is BOTH a
        // receipt and a shipment. Receipt detection is independent of shipment
        // detection — this fires as a receipt (the shipment classifier fires
        // separately in ingest).
        let r = detect_receipt(
            "auto@amazon.com",
            "Your Amazon.com order confirmation",
            "Thank you for your order. Order total: $42.10. Tracking TBA303392911000.",
        )
        .expect("order confirmation is a receipt even with tracking");
        assert_eq!(r.amount, Some(42.10));
    }
}
