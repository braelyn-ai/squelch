//! Shipment / package-tracking detection for the ingest pipeline.
//!
//! This is a PURE detection/extraction module in the same spirit as
//! [`crate::triage::deadline`]: given a message's surfaces (from-address,
//! subject, body) it decides whether the message is a shipping/delivery
//! notification and, if so, extracts a [`ShipmentInfo`] — carrier, tracking
//! number, best-effort item name, coarse status, and a carrier tracking URL.
//!
//! It is INDEPENDENT of the Stage-1 triage tier: shipping mail is noise-tier for
//! the ranked inbox (a "your order shipped" email is not a bill and not a
//! signal), but the same email still feeds the shipments tracker so the desktop
//! "In Transit" zone can list every package currently en route. The ingest
//! pipeline runs both classifiers side by side for non-sealed mail.
//!
//! SECURITY: this is NEVER run for sealed (auth/2FA) mail — the caller gates on
//! `sensitivity='normal'` exactly like every other non-sealed-only path, so a
//! sealed OTP can never become a shipment. There is nothing to leak here (no
//! bodies/keys are logged), and by construction the `shipments` table only ever
//! holds rows produced from non-sealed mail.
//!
//! ## Carrier disambiguation
//!
//! The hard part is that USPS / FedEx / DHL tracking numbers are all bare
//! digit-runs and overlap heavily. We resolve carrier in two passes:
//!   1. an EXPLICIT carrier-name mention in the sender/subject/body (e.g. "UPS",
//!      "USPS", "fedex.com", "DHL") is the strongest signal — prefer it;
//!   2. otherwise fall back to the tracking-number SHAPE (UPS `1Z…` and Amazon
//!      `TBA…` are unambiguous prefixes; USPS/FedEx/DHL fall to length
//!      heuristics).
//!
//! If a number is found but the carrier is genuinely ambiguous, we keep the
//! number and store `carrier="unknown"` (no tracking URL) rather than guess.

use regex::Regex;
use std::sync::OnceLock;

/// A detected shipment. Mirrors the shape persisted in the `shipments` table
/// minus the DB identity/timestamp columns.
#[derive(Debug, Clone, PartialEq)]
pub struct ShipmentInfo {
    /// Lower-case carrier slug: "ups", "usps", "fedex", "dhl", "amazon", or
    /// "unknown" (a number we could not attribute to a carrier).
    pub carrier: String,
    /// The extracted tracking number. Always present — a shipment with no
    /// tracking number is not emitted (we can't dedupe/track it).
    pub tracking_number: String,
    /// Best-effort product/item phrase pulled from the subject; empty when
    /// nothing meaningful survives the boilerplate strip.
    pub item_name: String,
    /// Coarse lifecycle status.
    pub status: ShipmentStatus,
    /// Carrier tracking URL with the number substituted, or `None` when the
    /// carrier has no public tracking URL (Amazon) or is unknown.
    pub tracking_url: Option<String>,
}

/// Coarse shipment lifecycle. Ranked (except [`ShipmentStatus::Exception`],
/// which is a flag) so the ingest state machine never regresses a delivered
/// package back to shipped. See [`ShipmentStatus::rank`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShipmentStatus {
    Ordered,
    Shipped,
    OutForDelivery,
    Delivered,
    /// A delivery problem (delay / failed delivery / exception). Treated as a
    /// FLAG rather than a point on the ordered<shipped<... ladder: it can apply
    /// at any stage, so it does not have a monotonic rank.
    Exception,
}

impl ShipmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ShipmentStatus::Ordered => "ordered",
            ShipmentStatus::Shipped => "shipped",
            ShipmentStatus::OutForDelivery => "out_for_delivery",
            ShipmentStatus::Delivered => "delivered",
            ShipmentStatus::Exception => "exception",
        }
    }

    pub fn parse(s: &str) -> Option<ShipmentStatus> {
        match s {
            "ordered" => Some(ShipmentStatus::Ordered),
            "shipped" => Some(ShipmentStatus::Shipped),
            "out_for_delivery" => Some(ShipmentStatus::OutForDelivery),
            "delivered" => Some(ShipmentStatus::Delivered),
            "exception" => Some(ShipmentStatus::Exception),
            _ => None,
        }
    }

    /// Progress rank for the no-regress state machine. `ordered < shipped <
    /// out_for_delivery < delivered`. `Exception` returns `None` — it is a flag,
    /// not a monotonic stage, so it never forces a regress and is never regressed
    /// away by a later stage update.
    pub fn rank(self) -> Option<u8> {
        match self {
            ShipmentStatus::Ordered => Some(0),
            ShipmentStatus::Shipped => Some(1),
            ShipmentStatus::OutForDelivery => Some(2),
            ShipmentStatus::Delivered => Some(3),
            ShipmentStatus::Exception => None,
        }
    }

    /// Merge an incoming status onto an existing one WITHOUT ever regressing a
    /// delivered package. Rules:
    ///   * a delivered shipment stays delivered (a later "shipped" email is a
    ///     stale/duplicate notification, not a real regress);
    ///   * an incoming Exception is preserved (a delivery problem is worth
    ///     surfacing) UNLESS the shipment is already delivered;
    ///   * otherwise the HIGHER rank wins.
    pub fn merge(existing: ShipmentStatus, incoming: ShipmentStatus) -> ShipmentStatus {
        // Delivered is terminal — never walk it back.
        if existing == ShipmentStatus::Delivered {
            return ShipmentStatus::Delivered;
        }
        if incoming == ShipmentStatus::Delivered {
            return ShipmentStatus::Delivered;
        }
        // An exception on a not-yet-delivered shipment is worth keeping.
        if incoming == ShipmentStatus::Exception {
            return ShipmentStatus::Exception;
        }
        if existing == ShipmentStatus::Exception {
            // Keep the exception flag unless the incoming stage is a strict
            // forward move that resolves it (out_for_delivery / delivered).
            return match incoming.rank() {
                Some(r) if r >= ShipmentStatus::OutForDelivery.rank().unwrap() => incoming,
                _ => ShipmentStatus::Exception,
            };
        }
        // Both are ordered/shipped/out_for_delivery: higher rank wins.
        match (existing.rank(), incoming.rank()) {
            (Some(e), Some(i)) if i > e => incoming,
            _ => existing,
        }
    }
}

struct Detector {
    /// Carrier tracking-number patterns, MOST-SPECIFIC FIRST. Each entry is
    /// `(carrier_slug, regex, requires_signal)`. UPS `1Z…` and Amazon `TBA…` are
    /// unambiguous prefixes (`requires_signal=false`) and come first; the bare
    /// digit-runs (USPS/FedEx/DHL) follow with `requires_signal=true` — a lone
    /// digit-run with NO real carrier signal (carrier name in the surfaces, or a
    /// carrier tracking-URL/word) must NOT be classified as that carrier.
    numbers: Vec<(&'static str, Regex, bool)>,
    /// Explicit carrier-name mentions (sender/subject/body). Used to disambiguate
    /// the digit-run carriers and to attribute an otherwise-ambiguous number.
    carrier_names: Vec<(&'static str, Regex)>,
    /// Boilerplate phrases stripped from the subject to recover the item name.
    boilerplate: Vec<Regex>,
    /// Status keyword sets, checked in the order that resolves priority
    /// (out_for_delivery / delivered / exception before the weaker shipped /
    /// ordered). Each entry is `(status, regex)`.
    status: Vec<(ShipmentStatus, Regex)>,
    /// Generic "is this even a shipping email" signal — a delivery/tracking
    /// signal that must be present for detection to fire when only an ambiguous
    /// bare-digit number is found (guards against random long digit-runs like
    /// order totals or phone numbers being mistaken for tracking numbers).
    shipping_signal: Vec<Regex>,
    /// OUTBOUND/RETURN phrasing: a return going FROM the user TO the seller, or a
    /// return-label/refund notice. These are NOT inbound deliveries — money/goods
    /// flow away from the user — so they must never become a tracked shipment.
    /// Same spirit as the deadline module's inbound-money/receipt exclusions.
    return_signal: Vec<Regex>,
    /// GENUINE INBOUND-DELIVERY phrasing: "your package/order shipped / delivered
    /// / out for delivery / on its way / arriving". Used to document the return
    /// precedence: even when inbound-delivery language ALSO appears, a return
    /// notice still excludes (a return that mentions the original shipment is a
    /// return). This set exists for clarity/testing, not to override the return
    /// exclusion — it is consulted only by the precedence test.
    #[cfg_attr(not(test), allow(dead_code))]
    inbound_delivery_signal: Vec<Regex>,
    /// "From <SELLER/ITEM>", "Your order of <X>", "<X> is on its way", product/
    /// merchant phrases pulled from the BODY when the subject strips to a generic
    /// leftover ("Package", "Your order", "") — best-effort, regex-only.
    body_item: Vec<Regex>,
}

/// Compile a case-insensitive static regex (panics on a bad pattern — these are
/// all compile-time-constant patterns).
fn rx(p: &str) -> Regex {
    Regex::new(&format!("(?i){p}")).expect("static shipment regex must compile")
}

fn detector() -> &'static Detector {
    static D: OnceLock<Detector> = OnceLock::new();
    D.get_or_init(|| Detector {
        numbers: vec![
            // UPS: 1Z + 16 alnum. Unambiguous prefix — trusted without a signal.
            ("ups", rx(r"\b1Z[0-9A-Z]{16}\b"), false),
            // Amazon logistics: TBA + >=9 digits. Unambiguous prefix. No public
            // tracking URL -> we link to nothing / the order page.
            ("amazon", rx(r"\bTBA\d{9,}\b"), false),
            // USPS: 9[234] then 18-24 more digits (20-22 total common), OR a bare
            // 20-22 digit run. Most-specific USPS shape first. The distinctive
            // 9[234]-prefixed impb form is trusted; the bare 20-22 digit run is
            // ambiguous and REQUIRES a carrier signal.
            ("usps", rx(r"\b9[234]\d{18,24}\b"), false),
            ("usps", rx(r"\b\d{20,22}\b"), true),
            // FedEx: 12, 15, or 20 digits — all bare digit-runs, REQUIRE a signal.
            ("fedex", rx(r"\b\d{20}\b"), true),
            ("fedex", rx(r"\b\d{15}\b"), true),
            ("fedex", rx(r"\b\d{12}\b"), true),
            // DHL: 10-11 digits — bare digit-run, REQUIRES a signal (this is the
            // exact false-positive class: a bare 10-digit eBay order number with
            // no DHL signal must NOT read as a DHL shipment).
            ("dhl", rx(r"\b\d{10,11}\b"), true),
        ],
        carrier_names: vec![
            ("ups", rx(r"\bUPS\b|\bups\.com\b")),
            // USPS: the acronym, "postal service", or usps.com.
            ("usps", rx(r"\bUSPS\b|\bu\.s\.p\.s\b|usps\.com|postal service")),
            ("fedex", rx(r"\bfedex\b|fedex\.com")),
            ("dhl", rx(r"\bDHL\b|dhl\.com")),
            ("amazon", rx(r"\bamazon\b|amazon\.com")),
        ],
        boilerplate: vec![
            rx(r"^\s*(re|fwd|fw)\s*:\s*"),
            rx(r"\byour order\s+(of|for)?\b"),
            rx(r"\byour\b"),
            rx(r"\bhas shipped\b"),
            rx(r"\bhave shipped\b"),
            rx(r"\bhas been shipped\b"),
            rx(r"\bis on its way\b"),
            rx(r"\bon the way\b"),
            rx(r"\bout for delivery\b"),
            rx(r"\barriving today\b"),
            rx(r"\bhas been delivered\b"),
            rx(r"\bwas delivered\b"),
            rx(r"\bhas been delivered\b"),
            rx(r"\bdelivered\b"),
            rx(r"\bshipment of\b"),
            rx(r"\bshipment\b"),
            rx(r"\bshipping (confirmation|update|notification)\b"),
            rx(r"\bdelivery (confirmation|update|notification)\b"),
            rx(r"\btracking (info(rmation)?|number|update|details?)\b"),
            rx(r"\btracking\b"),
            rx(r"\border confirmation\b"),
            // A trailing/standalone update|notification|confirmation|alert word
            // left behind after the phrase strips above (e.g. "tracking number
            // update" -> "update").
            rx(r"\b(update|notification|confirmation|alert|info(rmation)?)\b"),
            rx(r"\border\s*#?\s*[\w-]+"),
            rx(r"#\s*[\w-]+"),
            rx(r"\bhas been\b"),
            rx(r"\bis\b"),
            rx(r"\bof\b"),
            rx(r"\bfor\b"),
        ],
        status: vec![
            // Exception FIRST: a delivery problem should win over any weaker
            // "shipped"/"on its way" text that lingers in the same email.
            (ShipmentStatus::Exception, rx(r"\bdelay(ed|s)?\b|\bexception\b|\bfailed delivery\b|\bdelivery (failed|attempt|problem|issue)\b|\bcould not be delivered\b|\bundeliverable\b")),
            // Out for delivery / arriving today.
            (ShipmentStatus::OutForDelivery, rx(r"\bout for delivery\b|\barriving today\b|\bwill be delivered today\b")),
            // Delivered.
            (ShipmentStatus::Delivered, rx(r"\b(was|has been|is)?\s*delivered\b|\bdelivery complete\b")),
            // Shipped / on its way.
            (ShipmentStatus::Shipped, rx(r"\bshipped\b|\bhas shipped\b|\bon its way\b|\bon the way\b|\bshipment (is )?on\b|\bin transit\b")),
            // Ordered / order confirmed.
            (ShipmentStatus::Ordered, rx(r"\border confirmed\b|\border placed\b|\border received\b|\bthank you for your order\b|\bwe('| ha)ve received your order\b")),
        ],
        shipping_signal: vec![
            rx(r"\btrack(ing)?\b"),
            rx(r"\bshipp?(ed|ing|ment)\b"),
            rx(r"\bdeliver(y|ed|ing)\b"),
            rx(r"\bout for delivery\b"),
            rx(r"\bon its way\b"),
            rx(r"\bpackage\b"),
            rx(r"\bparcel\b"),
            rx(r"\bcarrier\b"),
            rx(r"\bin transit\b"),
        ],
        return_signal: vec![
            rx(r"\breturn\s+(label|received|initiated|shipped|confirmation)\b"),
            rx(r"\byour return\b"),
            rx(r"\bseller received\b"),
            rx(r"\brefund(ed|s)?\b"),
            rx(r"\bwe('| ha)ve received your (return|item)\b"),
            rx(r"\bdrop.?off\b"),
            rx(r"\breturn to sender\b"),
            rx(r"\bRMA\b"),
        ],
        inbound_delivery_signal: vec![
            rx(r"\byour (package|order|parcel|item|shipment)\b.{0,40}?\b(shipped|has shipped|delivered|out for delivery|on its way|on the way|arriving)\b"),
            rx(r"\bout for delivery\b"),
            rx(r"\barriving today\b"),
        ],
        body_item: vec![
            // "From <SELLER/ITEM>" (UPS/USPS delivered-notice style: "From DOUBLE
            // TAKE MIRROR"). Capture the trailing phrase up to a line/sentence end.
            rx(r"\bfrom\s+([A-Za-z0-9][A-Za-z0-9 &'.,\-]{2,60})"),
            // "Your order of <X>" / "Your order for <X>".
            rx(r"\byour order (?:of|for)\s+([A-Za-z0-9][A-Za-z0-9 &'.,\-]{2,60})"),
            // "<X> is on its way / has shipped / has been delivered" — capture the
            // product/merchant phrase that PRECEDES the shipping verb.
            rx(r"\b([A-Za-z0-9][A-Za-z0-9 &'.,\-]{2,60}?)\s+(?:is on its way|has shipped|have shipped|has been delivered|was delivered)\b"),
            // "shipment of <X>" / "order of <X>".
            rx(r"\b(?:shipment|order) of\s+([A-Za-z0-9][A-Za-z0-9 &'.,\-]{2,60})"),
        ],
    })
}

/// The tracking URL template for a carrier, with `{n}` substituted by the number.
/// Amazon and unknown carriers have no public URL -> `None`.
fn tracking_url(carrier: &str, number: &str) -> Option<String> {
    let enc = number.trim();
    match carrier {
        "usps" => Some(format!(
            "https://tools.usps.com/go/TrackConfirmAction?tLabels={enc}"
        )),
        "ups" => Some(format!("https://www.ups.com/track?tracknum={enc}")),
        "fedex" => Some(format!("https://www.fedex.com/fedextrack/?trknbr={enc}")),
        "dhl" => Some(format!(
            "https://www.dhl.com/us-en/home/tracking.html?tracking-id={enc}"
        )),
        // Amazon has no public tracking URL keyed by the TBA number; the user
        // tracks via their order page. Unknown carriers get nothing.
        _ => None,
    }
}

/// Is there a REAL carrier signal in the surfaces — ANY carrier name mention, or
/// a carrier tracking URL/word ("united parcel", a `*.com` carrier domain, a
/// carrier tracking-URL fragment)? Required before an ambiguous bare-digit number
/// is classified as a shipment: a lone digit-run with no carrier signal is almost
/// certainly an order number, phone number, or amount — not a tracking number.
fn has_carrier_signal(mentioned: &[&'static str], hay: &str) -> bool {
    if !mentioned.is_empty() {
        return true;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        rx(r"united parcel|postal service|ups\.com|fedex\.com|dhl\.com|usps\.com|fedextrack|tracking-id|tracking\.html")
    })
    .is_match(hay)
}

/// Does the text carry OUTBOUND/RETURN phrasing (a return going FROM the user TO
/// the seller, a return-label/refund notice)? Such mail is never an inbound
/// delivery. PRECEDENCE: this wins even when inbound-delivery language also
/// appears — a return notice that mentions the original shipment is still a
/// return. Same spirit as the deadline module's inbound-money/receipt exclusions.
fn is_return_or_outbound(hay: &str) -> bool {
    detector().return_signal.iter().any(|re| re.is_match(hay))
}

/// Does the text carry GENUINE INBOUND-DELIVERY phrasing ("your package shipped /
/// delivered / out for delivery / arriving")? Advisory only: a true inbound
/// delivery is NOT excluded, but this NEVER overrides the return exclusion — see
/// the precedence documented on [`is_return_or_outbound`]. Exposed for the
/// precedence test that proves a return notice mentioning the original shipment
/// still excludes.
#[cfg(test)]
fn has_inbound_delivery_signal(hay: &str) -> bool {
    detector()
        .inbound_delivery_signal
        .iter()
        .any(|re| re.is_match(hay))
}

/// Best-effort item name pulled from the BODY when the subject stripped to an
/// empty/generic leftover. Looks for "From <SELLER>", "Your order of <X>", "<X>
/// is on its way", "shipment of <X>", caps length, strips tracking numbers/urls
/// and carrier names. Returns empty when nothing meaningful is found.
fn extract_item_name_from_body(body: &str) -> String {
    let d = detector();
    for re in &d.body_item {
        if let Some(cap) = re.captures(body)
            && let Some(m) = cap.get(1)
        {
            let raw = m.as_str();
            // Cut at a sentence/line terminator so we don't slurp a whole
            // paragraph after the phrase.
            let raw = raw
                .split(['.', '\n', '\r', '!', '?', ';'])
                .next()
                .unwrap_or(raw);
            let cleaned = clean_item_phrase(raw);
            if cleaned.chars().count() >= 3 && !is_generic_item(&cleaned) {
                return cleaned;
            }
        }
    }
    String::new()
}

/// Strip tracking-number/url noise and carrier names from a candidate item
/// phrase, collapse whitespace, and cap length.
fn clean_item_phrase(s: &str) -> String {
    static TRACK: OnceLock<Regex> = OnceLock::new();
    let track = TRACK.get_or_init(|| rx(r"\b1Z[0-9A-Z]{16}\b|\bTBA\d{9,}\b|\b\d{10,}\b|https?://\S+"));
    let mut out = track.replace_all(s, " ").to_string();
    for (_, re) in &detector().carrier_names {
        out = re.replace_all(&out, " ").to_string();
    }
    let joined = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim_matches(|c: char| {
        c == ',' || c == '.' || c == '-' || c == ':' || c.is_whitespace()
    });
    // Cap length (defensive against a runaway capture).
    trimmed.chars().take(60).collect::<String>().trim().to_string()
}

/// Is `s` a generic placeholder that is no better than the desktop's
/// "Package via {carrier}" fallback?
fn is_generic_item(s: &str) -> bool {
    let l = s.trim().to_lowercase();
    matches!(
        l.as_str(),
        "" | "package" | "your order" | "order" | "shipment" | "parcel" | "item" | "your package"
    )
}

/// Which explicit carrier names are mentioned across the surfaces, in the fixed
/// priority order of [`Detector::carrier_names`].
fn mentioned_carriers(hay: &str) -> Vec<&'static str> {
    detector()
        .carrier_names
        .iter()
        .filter(|(_, re)| re.is_match(hay))
        .map(|(c, _)| *c)
        .collect()
}

/// Extract the status from the surfaces, defaulting to [`ShipmentStatus::Shipped`]
/// when a shipping email carries no explicit status keyword (a bare "tracking
/// number: X" notification means the thing is in transit).
fn extract_status(hay: &str) -> ShipmentStatus {
    for (status, re) in &detector().status {
        if re.is_match(hay) {
            return *status;
        }
    }
    ShipmentStatus::Shipped
}

/// Best-effort item name from the subject: strip shipping boilerplate and
/// leftover punctuation, collapse whitespace. Returns an empty string when
/// nothing meaningful survives.
pub fn extract_item_name(subject: &str) -> String {
    let mut s = subject.to_string();
    for re in &detector().boilerplate {
        s = re.replace_all(&s, " ").to_string();
    }
    // Drop carrier names from the residual so "UPS" etc. don't masquerade as the
    // item.
    for (_, re) in &detector().carrier_names {
        s = re.replace_all(&s, " ").to_string();
    }
    // Strip stray punctuation/separators left behind by the removals.
    let cleaned: String = s
        .chars()
        .map(|c| if "|:–—-•·".contains(c) { ' ' } else { c })
        .collect();
    // Collapse whitespace and trim leftover separators.
    let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim_matches(|c: char| c == ',' || c == '.' || c.is_whitespace());
    // A one-or-two-char residue ("s", "of") is noise, not an item.
    if trimmed.chars().count() < 3 {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// Attribute a bare tracking-number match to a carrier, disambiguating the
/// overlapping digit-run carriers (USPS/FedEx/DHL) with any explicit carrier
/// mention. `shape_carrier` is the carrier the number's SHAPE suggested.
fn attribute_carrier(shape_carrier: &str, mentioned: &[&'static str]) -> String {
    // Unambiguous prefixes (UPS 1Z, Amazon TBA) trust their shape outright —
    // unless a DIFFERENT explicit carrier is mentioned AND the shape is a bare
    // digit-run carrier. UPS/Amazon prefixes are distinctive enough to keep.
    if shape_carrier == "ups" || shape_carrier == "amazon" {
        return shape_carrier.to_string();
    }
    // Digit-run carriers: an explicit mention wins if it is itself a digit-run
    // carrier (usps/fedex/dhl) — that is the disambiguation. If the only mention
    // is a prefix carrier (ups/amazon) but the number is a digit-run, keep the
    // shape guess (the mention likely refers to something else).
    for m in mentioned {
        if matches!(*m, "usps" | "fedex" | "dhl") {
            return (*m).to_string();
        }
    }
    // No disambiguating mention: fall back to the shape's length heuristic guess.
    shape_carrier.to_string()
}

/// Detect a shipment from a message's surfaces. Returns `None` when the message
/// is not a shipping notification or carries no tracking number.
///
/// SECURITY: callers MUST NOT invoke this for sealed mail. It reads only the
/// provided text and never logs.
pub fn detect_shipment(from_addr: &str, subject: &str, body: &str) -> Option<ShipmentInfo> {
    let d = detector();
    let hay = format!("{from_addr}\n{subject}\n{body}");

    // 0. OUTBOUND/RETURN EXCLUSION. A return (going FROM the user TO the seller),
    //    a return-label/refund notice, or "seller received item" is NOT an inbound
    //    delivery — nothing to track for the user's In-Transit zone. Suppress
    //    entirely. PRECEDENCE: this wins even when inbound-delivery language ALSO
    //    appears (a return notice that references the original shipment is still a
    //    return). Same spirit as deadline.rs's inbound-money/receipt exclusions.
    //    (This is what stops the eBay "Return 5322397648: Seller received item"
    //    false positive — both a bare-digit number AND "seller received item".)
    if is_return_or_outbound(&hay) {
        return None;
    }

    // Is there any shipping/tracking/delivery signal at all? Without one, a long
    // digit-run is almost certainly an order total, an amount, or a phone number
    // — not a tracking number. This guards recall/precision both.
    let has_signal = d.shipping_signal.iter().any(|re| re.is_match(&hay));
    if !has_signal {
        return None;
    }

    let mentioned = mentioned_carriers(&hay);

    // Find the first tracking number by most-specific-first shape. The first
    // match wins its number; carrier is then disambiguated.
    for (shape_carrier, re, requires_signal) in &d.numbers {
        if let Some(m) = re.find(&hay) {
            // CARRIER SIGNAL GATE: for the ambiguous bare-digit carriers
            // (DHL/FedEx/USPS-bare), a real carrier signal (name mention or a
            // carrier tracking-URL/word) MUST be present. A lone digit-run with no
            // carrier signal is not a shipment — skip this shape and try the next
            // (there is no other shape for a plain 10-digit number, so we fall
            // through to `None`). UPS 1Z / Amazon TBA prefixes are unambiguous and
            // skip this gate.
            if *requires_signal && !has_carrier_signal(&mentioned, &hay) {
                continue;
            }
            let number = m.as_str().to_string();
            let carrier = attribute_carrier(shape_carrier, &mentioned);
            let status = extract_status(&hay);
            // Item name: subject first; if it strips to empty/generic, recover a
            // real product/merchant phrase from the body (BUG 2).
            let mut item_name = extract_item_name(subject);
            if is_generic_item(&item_name) {
                // Subject stripped to empty/generic ("Package", "Your order", …):
                // try to recover a real product/merchant phrase from the body. If
                // the body yields nothing, blank the generic leftover so the
                // desktop uses its own "Package via {carrier}" fallback rather than
                // storing a bare "Package".
                item_name = extract_item_name_from_body(body);
            }
            let tracking_url = tracking_url(&carrier, &number);
            return Some(ShipmentInfo {
                carrier,
                tracking_number: number,
                item_name,
                status,
                tracking_url,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- carrier + tracking extraction (real formats) --------------------

    #[test]
    fn ups_1z_format() {
        let s = detect_shipment(
            "ship-confirm@ups.com",
            "Your UPS package has shipped",
            "Tracking number: 1Z999AA10123456784. Track your package.",
        )
        .expect("ups shipment");
        assert_eq!(s.carrier, "ups");
        assert_eq!(s.tracking_number, "1Z999AA10123456784");
        assert_eq!(
            s.tracking_url.as_deref(),
            Some("https://www.ups.com/track?tracknum=1Z999AA10123456784")
        );
        assert_eq!(s.status, ShipmentStatus::Shipped);
    }

    #[test]
    fn usps_9400_format() {
        let s = detect_shipment(
            "auto-reply@usps.com",
            "USPS Tracking update",
            "Your item 9400111899223817428490 is in transit.",
        )
        .expect("usps shipment");
        assert_eq!(s.carrier, "usps");
        assert_eq!(s.tracking_number, "9400111899223817428490");
        assert!(s
            .tracking_url
            .as_deref()
            .unwrap()
            .contains("tools.usps.com"));
    }

    #[test]
    fn fedex_12_digit_with_explicit_name() {
        let s = detect_shipment(
            "TrackingUpdates@fedex.com",
            "Your FedEx shipment is on its way",
            "Tracking number 123456789012 shipped today.",
        )
        .expect("fedex shipment");
        assert_eq!(s.carrier, "fedex");
        assert_eq!(s.tracking_number, "123456789012");
        assert!(s.tracking_url.as_deref().unwrap().contains("fedex.com"));
    }

    #[test]
    fn dhl_10_digit() {
        let s = detect_shipment(
            "noreply@dhl.com",
            "DHL delivery notification",
            "Your DHL parcel 1234567890 is out for delivery.",
        )
        .expect("dhl shipment");
        assert_eq!(s.carrier, "dhl");
        assert_eq!(s.tracking_number, "1234567890");
        assert_eq!(s.status, ShipmentStatus::OutForDelivery);
    }

    #[test]
    fn amazon_tba_has_no_tracking_url() {
        let s = detect_shipment(
            "shipment-tracking@amazon.com",
            "Your Amazon package has shipped",
            "Carrier: Amazon. Tracking ID TBA303392911000.",
        )
        .expect("amazon shipment");
        assert_eq!(s.carrier, "amazon");
        assert_eq!(s.tracking_number, "TBA303392911000");
        assert!(s.tracking_url.is_none(), "amazon has no public tracking url");
    }

    // ---- carrier disambiguation ------------------------------------------

    #[test]
    fn digit_run_disambiguated_by_explicit_usps_mention() {
        // A 12-digit number could be FedEx by shape, but the body says USPS.
        // FedEx shape (12 digits) is checked before DHL; the explicit USPS
        // mention pulls it to USPS.
        let s = detect_shipment(
            "info@shop.com",
            "Shipping confirmation",
            "Shipped via USPS. Tracking: 123456789012",
        )
        .expect("shipment");
        assert_eq!(s.carrier, "usps", "explicit USPS mention disambiguates");
    }

    #[test]
    fn ambiguous_digit_run_without_carrier_signal_is_not_a_shipment() {
        // UPDATED for BUG 1's carrier-signal gate: a bare 12-digit number with a
        // shipping signal but NO carrier name/URL is no longer classified as a
        // FedEx shipment — a lone digit-run with no real carrier signal is too
        // ambiguous (order number / amount / phone). Precision over a guessed
        // carrier.
        let s = detect_shipment(
            "orders@shop.example",
            "Your shipment is on its way",
            "Tracking number 123456789012.",
        );
        assert!(s.is_none(), "no carrier signal => no shipment: {s:?}");
    }

    #[test]
    fn ambiguous_digit_run_with_carrier_signal_keeps_number() {
        // WITH a carrier signal (explicit FedEx mention) the same digit-run is a
        // real shipment and the number survives.
        let s = detect_shipment(
            "orders@shop.example",
            "Your FedEx shipment is on its way",
            "Shipped via FedEx. Tracking number 123456789012.",
        )
        .expect("shipment with kept number");
        assert_eq!(s.tracking_number, "123456789012");
        assert_eq!(s.carrier, "fedex");
    }

    // ---- status keyword mapping ------------------------------------------

    #[test]
    fn status_keyword_mapping() {
        let cases = [
            ("out for delivery today", ShipmentStatus::OutForDelivery),
            ("arriving today", ShipmentStatus::OutForDelivery),
            ("your package was delivered", ShipmentStatus::Delivered),
            ("has shipped", ShipmentStatus::Shipped),
            ("on its way", ShipmentStatus::Shipped),
            ("delivery failed", ShipmentStatus::Exception),
            ("shipment delayed", ShipmentStatus::Exception),
            ("order confirmed", ShipmentStatus::Ordered),
        ];
        for (body, want) in cases {
            let full = format!("tracking 1Z999AA10123456784 {body}");
            assert_eq!(extract_status(&full), want, "body: {body:?}");
        }
    }

    // ---- state machine: no regress on delivered --------------------------

    #[test]
    fn delivered_never_regresses() {
        assert_eq!(
            ShipmentStatus::merge(ShipmentStatus::Delivered, ShipmentStatus::Shipped),
            ShipmentStatus::Delivered
        );
        assert_eq!(
            ShipmentStatus::merge(ShipmentStatus::Delivered, ShipmentStatus::OutForDelivery),
            ShipmentStatus::Delivered
        );
        assert_eq!(
            ShipmentStatus::merge(ShipmentStatus::Delivered, ShipmentStatus::Exception),
            ShipmentStatus::Delivered
        );
    }

    #[test]
    fn forward_progress_wins() {
        assert_eq!(
            ShipmentStatus::merge(ShipmentStatus::Ordered, ShipmentStatus::Shipped),
            ShipmentStatus::Shipped
        );
        assert_eq!(
            ShipmentStatus::merge(ShipmentStatus::Shipped, ShipmentStatus::OutForDelivery),
            ShipmentStatus::OutForDelivery
        );
        assert_eq!(
            ShipmentStatus::merge(ShipmentStatus::OutForDelivery, ShipmentStatus::Delivered),
            ShipmentStatus::Delivered
        );
    }

    #[test]
    fn stale_backward_update_is_ignored() {
        // A late "shipped" email after out_for_delivery does not walk it back.
        assert_eq!(
            ShipmentStatus::merge(ShipmentStatus::OutForDelivery, ShipmentStatus::Shipped),
            ShipmentStatus::OutForDelivery
        );
    }

    #[test]
    fn exception_flag_persists_until_forward_resolution() {
        assert_eq!(
            ShipmentStatus::merge(ShipmentStatus::Exception, ShipmentStatus::Shipped),
            ShipmentStatus::Exception,
            "a stale shipped does not clear the exception"
        );
        assert_eq!(
            ShipmentStatus::merge(ShipmentStatus::Exception, ShipmentStatus::OutForDelivery),
            ShipmentStatus::OutForDelivery,
            "out-for-delivery resolves the exception forward"
        );
    }

    // ---- item-name stripping ---------------------------------------------

    #[test]
    fn item_name_strips_boilerplate() {
        assert_eq!(
            extract_item_name("Your order of Wireless Headphones has shipped"),
            "Wireless Headphones"
        );
        assert_eq!(
            extract_item_name("Re: Your Espresso Machine is on its way"),
            "Espresso Machine"
        );
    }

    #[test]
    fn item_name_empty_when_only_boilerplate() {
        assert_eq!(extract_item_name("Your order has shipped"), "");
        assert_eq!(extract_item_name("Shipping confirmation"), "");
        assert_eq!(extract_item_name("Tracking number update"), "");
    }

    // ---- sealed-shaped mail never produces a shipment --------------------

    #[test]
    fn otp_shaped_email_is_not_a_shipment() {
        // An OTP body carries a long digit-run (the code) but NO shipping signal,
        // so it must never be mistaken for a tracking number. (Belt: the ingest
        // pipeline never even calls this for sealed mail; suspenders: no signal
        // => no shipment regardless.)
        let s = detect_shipment(
            "noreply@bank.com",
            "Your verification code",
            "Your one-time passcode is 483920123456. Enter this code to continue.",
        );
        assert!(s.is_none(), "OTP must not be a shipment: {s:?}");
    }

    #[test]
    fn plain_non_shipping_email_is_not_a_shipment() {
        assert!(detect_shipment("a@b.com", "lunch?", "grab food at 12").is_none());
    }

    #[test]
    fn order_total_without_shipping_signal_is_not_a_shipment() {
        // A receipt with a bare order number but no shipping/tracking language
        // must not be mistaken for a shipment.
        let s = detect_shipment(
            "receipts@shop.com",
            "Your receipt",
            "Order #123456789012 total $42.10. Thank you.",
        );
        // "order" alone is not a shipping signal; no tracking/ship/delivery words.
        assert!(s.is_none(), "receipt must not be a shipment: {s:?}");
    }

    // ---- full extraction end-to-end --------------------------------------

    // ---- BUG 1: return / outbound exclusion ------------------------------

    #[test]
    fn ebay_return_seller_received_is_not_a_shipment() {
        // The exact live false positive: an eBay RETURN ("Seller received item")
        // carrying a bare 10-digit return number that loosely matched DHL's
        // \d{10,11}. Two failures fixed: no DHL signal (carrier-signal gate) AND
        // return language (outbound exclusion). Must produce NO shipment.
        let s = detect_shipment(
            "ebay@ebay.com",
            "Return 5322397648: Seller received item",
            "The seller received the item you returned. Your refund is being processed.",
        );
        assert!(s.is_none(), "eBay return must not be a shipment: {s:?}");
    }

    #[test]
    fn return_label_and_rma_are_excluded() {
        assert!(detect_shipment(
            "returns@shop.com",
            "Your return label is ready",
            "Print your return label and drop off the package. Tracking 1Z999AA10123456784.",
        )
        .is_none(), "a return label is outbound-from-user, not a tracked delivery");
        assert!(detect_shipment(
            "support@shop.com",
            "RMA 4471 approved",
            "Your RMA has been approved; ship the item back to us.",
        )
        .is_none());
    }

    #[test]
    fn return_precedence_wins_over_inbound_delivery_language() {
        // PRECEDENCE: even when genuine inbound-delivery language ("your package
        // ... delivered") ALSO appears, a return notice still excludes — a return
        // that references the original shipment is still a return.
        let subject = "Your return received";
        let body = "We have received your return. Your package was delivered on July 1. Tracking 1Z999AA10123456784.";
        let hay = format!("x@y.com\n{subject}\n{body}");
        assert!(is_return_or_outbound(&hay));
        assert!(has_inbound_delivery_signal(&hay), "inbound language is present");
        assert!(
            detect_shipment("x@y.com", subject, body).is_none(),
            "return exclusion must win over the inbound-delivery language"
        );
    }

    // ---- BUG 1: carrier-signal required for bare digit-runs --------------

    #[test]
    fn bare_digit_run_without_carrier_signal_is_not_a_shipment() {
        // A shipping signal ("package") plus a bare 10-digit number but NO carrier
        // name / URL: not enough to name a carrier or a shipment.
        let s = detect_shipment(
            "orders@shop.example",
            "Your package update",
            "Reference 1234567890 for your package.",
        );
        assert!(s.is_none(), "bare digit-run, no carrier signal => no shipment: {s:?}");
    }

    #[test]
    fn dhl_bare_digit_with_signal_is_a_shipment() {
        // Same shape as above but WITH a real DHL signal (name + domain): the
        // genuine inbound DHL delivery is detected.
        let s = detect_shipment(
            "noreply@dhl.com",
            "Your DHL package is out for delivery",
            "Track your DHL shipment 1234567890 at dhl.com.",
        )
        .expect("dhl shipment with signal");
        assert_eq!(s.carrier, "dhl");
        assert_eq!(s.tracking_number, "1234567890");
        assert_eq!(s.status, ShipmentStatus::OutForDelivery);
    }

    // ---- BUG 2: item name recovered from body ----------------------------

    #[test]
    fn ups_delivered_pulls_item_name_from_body_from_seller() {
        // Subject strips to the generic "Package"; the body's "From DOUBLE TAKE
        // MIRROR" supplies a real merchant/product phrase.
        let s = detect_shipment(
            "mcinfo@ups.com",
            "Your UPS Package was delivered",
            "Your package was delivered. From DOUBLE TAKE MIRROR. Tracking 1Z999AA10123456784.",
        )
        .expect("ups shipment");
        assert_eq!(s.carrier, "ups");
        assert_eq!(s.item_name, "DOUBLE TAKE MIRROR");
        assert_ne!(s.item_name.to_lowercase(), "package");
    }

    #[test]
    fn body_item_falls_back_to_empty_when_nothing_useful() {
        // Generic subject, no product/merchant phrase in the body: item stays
        // empty (the desktop falls back to "Package via {carrier}").
        let s = detect_shipment(
            "ship@ups.com",
            "Your package has shipped",
            "Your package has shipped. Tracking 1Z999AA10123456784.",
        )
        .expect("ups shipment");
        assert_eq!(s.item_name, "", "no real phrase => empty, desktop fills the fallback");
    }

    #[test]
    fn full_extraction_ups_with_item() {
        let s = detect_shipment(
            "ship@ups.com",
            "Your order of Mechanical Keyboard has shipped",
            "UPS tracking 1Z12345E0205271688. On its way!",
        )
        .expect("shipment");
        assert_eq!(s.carrier, "ups");
        assert_eq!(s.tracking_number, "1Z12345E0205271688");
        assert_eq!(s.item_name, "Mechanical Keyboard");
        assert_eq!(s.status, ShipmentStatus::Shipped);
        assert!(s.tracking_url.is_some());
    }
}
