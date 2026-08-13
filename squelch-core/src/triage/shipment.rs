//! Shipment / package-tracking detection: pure extraction of carrier, tracking
//! number, best-effort item name, coarse status, and a carrier tracking URL
//! ([`ShipmentInfo`]). Independent of the Stage-1 tier — shipping mail is
//! noise-tier for the ranked inbox but still feeds the shipments tracker. Never
//! run for sealed mail (see docs/SECURITY.md).
//!
//! Carrier resolution takes two passes, because USPS / FedEx / DHL numbers are
//! all overlapping bare digit-runs: an EXPLICIT carrier-name mention in the
//! surfaces wins, else the number's SHAPE decides (UPS `1Z…` and Amazon `TBA…`
//! are unambiguous prefixes; the rest fall to length heuristics). A genuinely
//! ambiguous number is kept as `carrier="unknown"` with no URL, never guessed.

use crate::triage::text::rx;
use crate::types::str_enum;
use chrono::{DateTime, Utc};
use regex::Regex;
use std::sync::OnceLock;

/// A detected shipment: the `shipments` table shape minus the DB
/// identity/timestamp columns.
#[derive(Debug, Clone, PartialEq)]
pub struct ShipmentInfo {
    /// Lower-case carrier slug: "ups", "usps", "fedex", "dhl", "amazon", or
    /// "unknown" (a number we could not attribute).
    pub carrier: String,
    /// Always present — a shipment with no tracking number is never emitted,
    /// since it could not be deduped or tracked.
    pub tracking_number: String,
    /// Best-effort item phrase; empty when nothing survives the strip.
    pub item_name: String,
    /// Coarse lifecycle status.
    pub status: ShipmentStatus,
    /// Tracking URL with the number substituted; `None` for a carrier with no
    /// public tracking URL (Amazon) or an unknown one.
    pub tracking_url: Option<String>,
}

/// One carrier-API tracking result for an already-known shipment: the same
/// subject as [`ShipmentInfo`], sourced from the carrier instead of an email.
/// Carries no tracking number or carrier — the caller polled a specific row and
/// already knows both.
#[derive(Debug, Clone, PartialEq)]
pub struct CarrierTrack {
    /// The carrier's status mapped onto our ladder. `None` when the carrier's
    /// vocabulary maps to nothing we model: the raw string is still recorded,
    /// but the shipment's status is left alone rather than guessed at.
    pub status: Option<ShipmentStatus>,
    /// The carrier's status string, verbatim.
    pub carrier_status_raw: String,
    /// Carrier-estimated delivery; `None` when the carrier gives none.
    pub eta: Option<DateTime<Utc>>,
    /// Carrier-reported delivery time; `None` until the carrier reports one.
    pub delivered_at: Option<DateTime<Utc>>,
}

str_enum! {
    /// Coarse shipment lifecycle, ranked so the ingest state machine never regresses
    /// a delivered package back to shipped (see [`ShipmentStatus::rank`]).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ShipmentStatus {
        Ordered => "ordered",
        Shipped => "shipped",
        OutForDelivery => "out_for_delivery",
        Delivered => "delivered",
        /// A delivery problem. A FLAG, not a point on the ladder — it can apply at
        /// any stage, so it carries no monotonic rank.
        Exception => "exception",
    }
}

impl ShipmentStatus {
    /// Progress rank for the no-regress state machine: `ordered < shipped <
    /// out_for_delivery < delivered`. `Exception` has none — it is a flag.
    pub fn rank(self) -> Option<u8> {
        match self {
            ShipmentStatus::Ordered => Some(0),
            ShipmentStatus::Shipped => Some(1),
            ShipmentStatus::OutForDelivery => Some(2),
            ShipmentStatus::Delivered => Some(3),
            ShipmentStatus::Exception => None,
        }
    }

    /// Merge an incoming status onto an existing one, never regressing:
    /// delivered is terminal (a later "shipped" email is a stale duplicate); an
    /// incoming Exception is kept unless already delivered; else higher rank wins.
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

    /// Reconcile a CARRIER-API status onto the stored one. The carrier is ground
    /// truth, so unlike [`merge`](ShipmentStatus::merge) it REPLACES rather than
    /// ratchets: it may clear an exception the mail announced, and it may walk
    /// out_for_delivery back to shipped when a scan says the package missed its
    /// truck. Email statuses are inferences from marketing copy; carrier scans
    /// are the record.
    ///
    /// The one exception is Delivered, which stays terminal: carrier scan data
    /// can lag a delivery the user already got an email about, and a card that
    /// un-delivers itself reads as a bug.
    pub fn reconcile_carrier(existing: ShipmentStatus, carrier: ShipmentStatus) -> ShipmentStatus {
        if existing == ShipmentStatus::Delivered {
            ShipmentStatus::Delivered
        } else {
            carrier
        }
    }
}

struct Detector {
    /// `(carrier_slug, regex, requires_signal)`, MOST-SPECIFIC FIRST. The
    /// unambiguous prefixes (UPS `1Z…`, Amazon `TBA…`) come first and need no
    /// signal; the bare digit-runs follow with `requires_signal=true`, since a
    /// lone digit-run with no carrier signal must NOT become that carrier.
    numbers: Vec<(&'static str, Regex, bool)>,
    /// Explicit carrier-name mentions, used to disambiguate the digit-run
    /// carriers and to attribute an otherwise-ambiguous number.
    carrier_names: Vec<(&'static str, Regex)>,
    /// Boilerplate phrases stripped from the subject to recover the item name.
    boilerplate: Vec<Regex>,
    /// `(status, regex)` in priority order — out_for_delivery / delivered /
    /// exception are checked before the weaker shipped / ordered.
    status: Vec<(ShipmentStatus, Regex)>,
    /// "Is this even a shipping email" signal, required before an ambiguous
    /// bare-digit number counts — otherwise an order total or phone number
    /// reads as a tracking number.
    shipping_signal: Vec<Regex>,
    /// OUTBOUND/RETURN phrasing (a return to the seller, a return label, a
    /// refund). Goods flow away from the user, so it is never a tracked shipment.
    return_signal: Vec<Regex>,
    /// GENUINE INBOUND-DELIVERY phrasing. Consulted only by the precedence test
    /// proving that a return notice excludes even when this also fires.
    #[cfg_attr(not(test), allow(dead_code))]
    inbound_delivery_signal: Vec<Regex>,
    /// Product/merchant phrases pulled from the BODY when the subject strips to
    /// a generic leftover ("Package", "Your order", ""). Best-effort, regex-only.
    body_item: Vec<Regex>,
}

fn detector() -> &'static Detector {
    static D: OnceLock<Detector> = OnceLock::new();
    D.get_or_init(|| Detector {
        numbers: vec![
            // UPS: 1Z + 16 alnum. Unambiguous prefix — trusted without a signal.
            ("ups", rx(r"\b1Z[0-9A-Z]{16}\b"), false),
            // Amazon logistics: TBA + >=9 digits. Unambiguous prefix.
            ("amazon", rx(r"\bTBA\d{9,}\b"), false),
            // USPS: the distinctive 9[234]-prefixed impb form is trusted; a bare
            // 20-22 digit run is ambiguous and needs a carrier signal.
            ("usps", rx(r"\b9[234]\d{18,24}\b"), false),
            ("usps", rx(r"\b\d{20,22}\b"), true),
            // FedEx: 12, 15, or 20 digits — all bare runs, all need a signal.
            ("fedex", rx(r"\b\d{20}\b"), true),
            ("fedex", rx(r"\b\d{15}\b"), true),
            ("fedex", rx(r"\b\d{12}\b"), true),
            // DHL: 10-11 digits, the loosest shape — a bare 10-digit order
            // number with no DHL signal must not read as a DHL shipment.
            ("dhl", rx(r"\b\d{10,11}\b"), true),
        ],
        carrier_names: vec![
            ("ups", rx(r"\bUPS\b|\bups\.com\b")),
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
            // A standalone word left behind by the strips above ("tracking
            // number update" -> "update").
            rx(r"\b(update|notification|confirmation|alert|info(rmation)?)\b"),
            rx(r"\border\s*#?\s*[\w-]+"),
            rx(r"#\s*[\w-]+"),
            rx(r"\bhas been\b"),
            rx(r"\bis\b"),
            rx(r"\bof\b"),
            rx(r"\bfor\b"),
        ],
        status: vec![
            // Exception FIRST: a delivery problem wins over any weaker
            // "shipped"/"on its way" text lingering in the same email.
            (ShipmentStatus::Exception, rx(r"\bdelay(ed|s)?\b|\bexception\b|\bfailed delivery\b|\bdelivery (failed|attempt|problem|issue)\b|\bcould not be delivered\b|\bundeliverable\b")),
            (ShipmentStatus::OutForDelivery, rx(r"\bout for delivery\b|\barriving today\b|\bwill be delivered today\b")),
            (ShipmentStatus::Delivered, rx(r"\b(was|has been|is)?\s*delivered\b|\bdelivery complete\b")),
            (ShipmentStatus::Shipped, rx(r"\bshipped\b|\bhas shipped\b|\bon its way\b|\bon the way\b|\bshipment (is )?on\b|\bin transit\b")),
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
            // "From <SELLER>", the UPS/USPS delivered-notice style.
            rx(r"\bfrom\s+([A-Za-z0-9][A-Za-z0-9 &'.,\-]{2,60})"),
            // "Your order of/for <X>".
            rx(r"\byour order (?:of|for)\s+([A-Za-z0-9][A-Za-z0-9 &'.,\-]{2,60})"),
            // The phrase PRECEDING a shipping verb: "<X> is on its way".
            rx(r"\b([A-Za-z0-9][A-Za-z0-9 &'.,\-]{2,60}?)\s+(?:is on its way|has shipped|have shipped|has been delivered|was delivered)\b"),
            // "shipment of <X>" / "order of <X>".
            rx(r"\b(?:shipment|order) of\s+([A-Za-z0-9][A-Za-z0-9 &'.,\-]{2,60})"),
        ],
    })
}

/// The tracking URL template for a carrier, with `{n}` substituted by the number.
/// Amazon and unknown carriers have no public URL -> `None`.
///
/// `pub` because the SHIPMENTS EXTRACTOR's store apply builds a [`ShipmentInfo`]
/// from model output rather than from [`detect_shipment`], and both paths must
/// mint the same URL from the same table — a second copy would drift.
pub fn tracking_url(carrier: &str, number: &str) -> Option<String> {
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
        // Amazon has no public URL keyed by the TBA number (the user tracks via
        // their order page); unknown carriers get nothing.
        _ => None,
    }
}

/// Is there a REAL carrier signal — a carrier-name mention, or a carrier
/// tracking URL/word? Required before an ambiguous bare-digit number counts: a
/// lone digit-run with no carrier signal is almost certainly an order number.
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

/// Does the text carry OUTBOUND/RETURN phrasing? PRECEDENCE: this wins even when
/// inbound-delivery language also appears — a return notice that mentions the
/// original shipment is still a return.
fn is_return_or_outbound(hay: &str) -> bool {
    detector().return_signal.iter().any(|re| re.is_match(hay))
}

/// LOOSE shipping signal: the `shipping_signal` battery minus the
/// tracking-number requirement, still excluding returns and outbound mail.
/// Drives the shipments-extractor trigger (`triage.ship_extract_model`), which
/// is why it is deliberately HIGH-RECALL — an order confirmation that names no
/// carrier and carries no number must still reach the model, and the model
/// makes the real call. Reuses [`detect_shipment`]'s own batteries, so the two
/// can never drift.
pub fn has_loose_shipping_signal(from_addr: &str, subject: &str, body: &str) -> bool {
    let hay = format!("{from_addr}\n{subject}\n{body}");
    // Same PRECEDENCE as `detect_shipment`: a return notice is excluded even
    // when it also talks about the original inbound shipment.
    if is_return_or_outbound(&hay) {
        return false;
    }
    detector()
        .shipping_signal
        .iter()
        .any(|re| re.is_match(&hay))
}

// ---- ambiguous digit-run gating ------------------------------------------
//
// The `requires_signal` shapes are bare digit runs, and a retailer's ITEM or
// ORDER id has exactly the same shape: one eBay order thread carries six
// 12-digit item ids, and an Amazon marketing mail ("review your upcoming
// delivery") carries a 10-digit run next to a DHL mention. A carrier signal
// somewhere in the mail is not enough to tell those apart, so an ambiguous
// candidate must ALSO sit in tracking-number CONTEXT and must not sit in
// retailer item/order context. The unambiguous shapes (1Z…, TBA…, 9[234]…) are
// exempt from both tests: their prefix already identifies them.

/// The largest byte index `<= idx` that is a char boundary, `n` bytes back.
fn window_start(hay: &str, idx: usize, n: usize) -> usize {
    let mut lo = idx.saturating_sub(n);
    while lo < idx && !hay.is_char_boundary(lo) {
        lo += 1;
    }
    lo
}

/// The smallest byte index `>= idx` that is a char boundary, `n` bytes on.
fn window_end(hay: &str, idx: usize, n: usize) -> usize {
    let mut hi = idx.saturating_add(n).min(hay.len());
    while hi > idx && !hay.is_char_boundary(hi) {
        hi -= 1;
    }
    hi
}

/// The whitespace-delimited token containing `hay[start..end]` — the URL or word
/// the number is embedded in.
fn containing_token(hay: &str, start: usize, end: usize) -> &str {
    let b = hay.as_bytes();
    // Both scans stop just past an ASCII whitespace byte (or at an edge), which
    // is always a char boundary: a UTF-8 continuation byte is never whitespace.
    let mut lo = start;
    while lo > 0 && !b[lo - 1].is_ascii_whitespace() {
        lo -= 1;
    }
    let mut hi = end.min(b.len());
    while hi < b.len() && !b[hi].is_ascii_whitespace() {
        hi += 1;
    }
    &hay[lo..hi]
}

/// Is `from_addr` one of the carriers themselves? The carrier mailing you a
/// number IS the context — their notices routinely print it with no label at all.
/// Subdomains count (`ship-confirm@email.ups.com`); look-alikes do not
/// (`ups.com.example.net` fails the suffix test).
fn is_carrier_sender(from_addr: &str) -> bool {
    let Some(domain) = from_addr.rsplit('@').next() else {
        return false;
    };
    let domain = domain.trim().trim_end_matches('>').to_ascii_lowercase();
    ["ups.com", "usps.com", "fedex.com", "dhl.com"]
        .iter()
        .any(|c| domain == *c || domain.ends_with(&format!(".{c}")))
}

/// Positive context for an ambiguous digit-run at `hay[start..end]`: any of
///  (a) a tracking-ish label within ~80 chars before or ~40 after the match,
///  (b) the match lies inside a known carrier tracking URL (the containing
///      non-whitespace token carries one of the carriers' number parameters),
///  (c) CARRIER-SENDER EXEMPTION: the mail came FROM a carrier's domain.
fn ambiguous_number_in_context(from_addr: &str, hay: &str, start: usize, end: usize) -> bool {
    // (c) first: it is the cheapest and needs no windowing.
    if is_carrier_sender(from_addr) {
        return true;
    }
    static LABEL: OnceLock<Regex> = OnceLock::new();
    let label = LABEL.get_or_init(|| rx(r"tracking\s*(number|no\.?|#|:|id)?|\btrack\b"));
    let before = &hay[window_start(hay, start, 80)..start];
    let after = &hay[end..window_end(hay, end, 40)];
    if label.is_match(before) || label.is_match(after) {
        return true;
    }
    static URL_PARAM: OnceLock<Regex> = OnceLock::new();
    URL_PARAM
        .get_or_init(|| rx(r"tracknum=|trknbr=|tLabels=|tracking-id=|TrackConfirmAction"))
        .is_match(containing_token(hay, start, end))
}

/// Hard negative for an ambiguous digit-run at `hay[start..end]`, with
/// PRECEDENCE OVER the positive context — a labelled-looking item id is still an
/// item id:
///  (a) the match sits inside a retailer item/order URL, or
///  (b) the match is immediately preceded by an item/order label.
fn ambiguous_number_is_hard_negative(hay: &str, start: usize, end: usize) -> bool {
    static ITEM_URL: OnceLock<Regex> = OnceLock::new();
    let token = containing_token(hay, start, end);
    if ITEM_URL
        .get_or_init(|| rx(r"/itm/|/ord/|ebay\."))
        .is_match(token)
    {
        return true;
    }
    static ITEM_LABEL: OnceLock<Regex> = OnceLock::new();
    ITEM_LABEL
        .get_or_init(|| rx(r"\b(item|order)\b\s*(#|no\.?|number|:)?\s*$"))
        .is_match(&hay[window_start(hay, start, 20)..start])
}

/// Shape-only ambiguity test for an ALREADY-STORED tracking number, the read-side
/// sibling of the gating above: `true` for the bare digit runs a retailer item or
/// order id can impersonate (10-15 digits, or 20-22 digits without the IMpb
/// `9[234]` prefix), `false` for the self-identifying shapes — UPS `1Z…`, Amazon
/// `TBA…`, and `9[234]`-prefixed IMpb numbers.
///
/// ONE definition of "ambiguous", shared by the listing suppression and the
/// repair passes, so a number can never be ambiguous to one and not the other.
pub fn is_ambiguous_tracking_shape(number: &str) -> bool {
    let n = number.trim();
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        // Anything carrying letters is a prefixed shape (1Z…, TBA…), not a run.
        return false;
    }
    match n.len() {
        10..=15 => true,
        20..=22 => !matches!(&n[..2], "92" | "93" | "94"),
        _ => false,
    }
}

/// Does the text carry GENUINE INBOUND-DELIVERY phrasing? Advisory only, and it
/// never overrides the return exclusion — see [`is_return_or_outbound`].
#[cfg(test)]
fn has_inbound_delivery_signal(hay: &str) -> bool {
    detector()
        .inbound_delivery_signal
        .iter()
        .any(|re| re.is_match(hay))
}

/// Best-effort item name from the BODY, for when the subject stripped to an
/// empty or generic leftover. Empty when nothing meaningful is found.
fn extract_item_name_from_body(body: &str) -> String {
    let d = detector();
    for re in &d.body_item {
        if let Some(cap) = re.captures(body)
            && let Some(m) = cap.get(1)
        {
            let raw = m.as_str();
            // Cut at a sentence/line terminator, so a whole paragraph after the
            // phrase is not slurped in.
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
    let track =
        TRACK.get_or_init(|| rx(r"\b1Z[0-9A-Z]{16}\b|\bTBA\d{9,}\b|\b\d{10,}\b|https?://\S+"));
    let mut out = track.replace_all(s, " ").to_string();
    for (_, re) in &detector().carrier_names {
        out = re.replace_all(&out, " ").to_string();
    }
    let joined = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined
        .trim_matches(|c: char| c == ',' || c == '.' || c == '-' || c == ':' || c.is_whitespace());
    // Cap length (defensive against a runaway capture).
    trimmed
        .chars()
        .take(60)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Is `s` a generic placeholder ("Package", "Your order", …) rather than a real
/// item? Such a leftover is no better than the client's own fallback label.
///
/// Shared with the LLM extractor, whose model volunteers a wider set of
/// placeholders than the subject-lifting detector ever produces; one list keeps
/// the two paths from disagreeing about what counts as a name.
pub(crate) fn is_generic_item(s: &str) -> bool {
    let l = s.trim().to_lowercase();
    matches!(
        l.as_str(),
        "" | "package"
            | "your package"
            | "the package"
            | "order"
            | "your order"
            | "order update"
            | "shipping update"
            | "delivery update"
            | "shipment"
            | "your shipment"
            | "parcel"
            | "item"
            | "your item"
            | "unknown"
            | "n/a"
            | "none"
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

/// The status from the surfaces, defaulting to [`ShipmentStatus::Shipped`]: a
/// shipping email with no status keyword ("tracking number: X") is in transit.
fn extract_status(hay: &str) -> ShipmentStatus {
    for (status, re) in &detector().status {
        if re.is_match(hay) {
            return *status;
        }
    }
    ShipmentStatus::Shipped
}

/// Best-effort item name from the subject: strip shipping boilerplate and
/// leftover punctuation. Empty when nothing meaningful survives.
pub fn extract_item_name(subject: &str) -> String {
    let mut s = subject.to_string();
    for re in &detector().boilerplate {
        s = re.replace_all(&s, " ").to_string();
    }
    // So "UPS" and friends can't masquerade as the item.
    for (_, re) in &detector().carrier_names {
        s = re.replace_all(&s, " ").to_string();
    }
    // Strip stray separators left behind by the removals.
    let cleaned: String = s
        .chars()
        .map(|c| if "|:–—-•·".contains(c) { ' ' } else { c })
        .collect();
    let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim_matches(|c: char| c == ',' || c == '.' || c.is_whitespace());
    // A one-or-two-char residue ("s", "of") is noise, not an item.
    if trimmed.chars().count() < 3 {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// Attribute a tracking-number match to a carrier, disambiguating the
/// overlapping digit-run carriers with any explicit carrier mention.
/// `shape_carrier` is what the number's SHAPE suggested.
fn attribute_carrier(shape_carrier: &str, mentioned: &[&'static str]) -> String {
    // The UPS/Amazon prefixes are distinctive enough to trust outright.
    if shape_carrier == "ups" || shape_carrier == "amazon" {
        return shape_carrier.to_string();
    }
    // For a digit-run number, only a mention of another DIGIT-RUN carrier
    // disambiguates; a ups/amazon mention alongside one likely refers to
    // something else, so the shape guess stands.
    for m in mentioned {
        if matches!(*m, "usps" | "fedex" | "dhl") {
            return (*m).to_string();
        }
    }
    shape_carrier.to_string()
}

/// Detect a shipment from a message's surfaces. `None` when the message is not a
/// shipping notification or carries no tracking number.
///
/// SECURITY: callers MUST NOT invoke this for sealed mail (docs/SECURITY.md).
pub fn detect_shipment(from_addr: &str, subject: &str, body: &str) -> Option<ShipmentInfo> {
    let d = detector();
    let hay = format!("{from_addr}\n{subject}\n{body}");

    // 0. Outbound/return exclusion: nothing inbound to track. PRECEDENCE — this
    //    wins even when inbound-delivery language ALSO appears, since a return
    //    notice that references the original shipment is still a return.
    if is_return_or_outbound(&hay) {
        return None;
    }

    // Without any shipping signal, a long digit-run is almost certainly an order
    // total, an amount, or a phone number rather than a tracking number.
    let has_signal = d.shipping_signal.iter().any(|re| re.is_match(&hay));
    if !has_signal {
        return None;
    }

    let mentioned = mentioned_carriers(&hay);

    // First ACCEPTED match by most-specific-first shape wins its number; the
    // carrier is then disambiguated.
    for (shape_carrier, re, requires_signal) in &d.numbers {
        // CARRIER SIGNAL GATE: the ambiguous bare-digit shapes need a real
        // carrier signal, so skip to the next shape without one. There is no
        // other shape for a plain 10-digit number, so that falls to `None`.
        if *requires_signal && !has_carrier_signal(&mentioned, &hay) {
            continue;
        }
        // EVERY match of the shape, not just the first: an eBay order mail leads
        // with six 12-digit ITEM ids and may carry the real number after them, so
        // first-match-only would pin the row to an item id forever.
        for m in re.find_iter(&hay) {
            // CONTEXT GATE, ambiguous shapes only. The hard negative has
            // precedence: an item id under a "tracking" heading is still an item
            // id.
            if *requires_signal
                && (ambiguous_number_is_hard_negative(&hay, m.start(), m.end())
                    || !ambiguous_number_in_context(from_addr, &hay, m.start(), m.end()))
            {
                continue;
            }
            let number = m.as_str().to_string();
            let carrier = attribute_carrier(shape_carrier, &mentioned);
            let status = extract_status(&hay);
            let mut item_name = extract_item_name(subject);
            if is_generic_item(&item_name) {
                // The subject stripped to a generic leftover: recover a real
                // phrase from the body, and store nothing if there is none, so
                // the client shows its own fallback rather than a bare "Package".
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
        assert!(
            s.tracking_url
                .as_deref()
                .unwrap()
                .contains("tools.usps.com")
        );
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
        assert!(
            s.tracking_url.is_none(),
            "amazon has no public tracking url"
        );
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
        // A bare 12-digit number with a shipping signal but no carrier name/URL
        // is too ambiguous to name a carrier: precision over a guess.
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

    // ---- carrier reconciliation: replace, not ratchet ---------------------

    #[test]
    fn carrier_replaces_upward() {
        assert_eq!(
            ShipmentStatus::reconcile_carrier(
                ShipmentStatus::Shipped,
                ShipmentStatus::OutForDelivery
            ),
            ShipmentStatus::OutForDelivery
        );
        assert_eq!(
            ShipmentStatus::reconcile_carrier(ShipmentStatus::Ordered, ShipmentStatus::Shipped),
            ShipmentStatus::Shipped
        );
    }

    #[test]
    fn carrier_replaces_downward() {
        // Where `merge` would refuse the regression, the carrier's scan wins:
        // a package that missed its truck really is back to shipped.
        assert_eq!(
            ShipmentStatus::merge(ShipmentStatus::OutForDelivery, ShipmentStatus::Shipped),
            ShipmentStatus::OutForDelivery
        );
        assert_eq!(
            ShipmentStatus::reconcile_carrier(
                ShipmentStatus::OutForDelivery,
                ShipmentStatus::Shipped
            ),
            ShipmentStatus::Shipped
        );
    }

    #[test]
    fn carrier_clears_an_exception() {
        // A resolved delay is the carrier's to report, at any stage — including
        // one `merge` would keep the exception flag through.
        assert_eq!(
            ShipmentStatus::reconcile_carrier(ShipmentStatus::Exception, ShipmentStatus::Shipped),
            ShipmentStatus::Shipped
        );
        assert_eq!(
            ShipmentStatus::reconcile_carrier(ShipmentStatus::Exception, ShipmentStatus::Ordered),
            ShipmentStatus::Ordered
        );
    }

    #[test]
    fn carrier_raises_an_exception() {
        assert_eq!(
            ShipmentStatus::reconcile_carrier(ShipmentStatus::Shipped, ShipmentStatus::Exception),
            ShipmentStatus::Exception
        );
    }

    #[test]
    fn delivered_is_terminal_against_any_carrier_status() {
        // Carrier scan data lags an email-announced delivery, so nothing the
        // carrier says walks a delivered row back.
        for carrier in [
            ShipmentStatus::Ordered,
            ShipmentStatus::Shipped,
            ShipmentStatus::OutForDelivery,
            ShipmentStatus::Delivered,
            ShipmentStatus::Exception,
        ] {
            assert_eq!(
                ShipmentStatus::reconcile_carrier(ShipmentStatus::Delivered, carrier),
                ShipmentStatus::Delivered,
                "carrier: {carrier:?}"
            );
        }
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
        // Ingest never calls this for sealed mail, and an OTP's long digit-run
        // carries no shipping signal, so it cannot classify either way.
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

    // ---- return / outbound exclusion -------------------------------------

    #[test]
    fn ebay_return_seller_received_is_not_a_shipment() {
        // A return carrying a bare 10-digit number that loosely matches DHL's
        // \d{10,11}: excluded twice over, by the carrier-signal gate and by the
        // return language.
        let s = detect_shipment(
            "ebay@ebay.com",
            "Return 5322397648: Seller received item",
            "The seller received the item you returned. Your refund is being processed.",
        );
        assert!(s.is_none(), "eBay return must not be a shipment: {s:?}");
    }

    #[test]
    fn return_label_and_rma_are_excluded() {
        assert!(
            detect_shipment(
                "returns@shop.com",
                "Your return label is ready",
                "Print your return label and drop off the package. Tracking 1Z999AA10123456784.",
            )
            .is_none(),
            "a return label is outbound-from-user, not a tracked delivery"
        );
        assert!(
            detect_shipment(
                "support@shop.com",
                "RMA 4471 approved",
                "Your RMA has been approved; ship the item back to us.",
            )
            .is_none()
        );
    }

    #[test]
    fn return_precedence_wins_over_inbound_delivery_language() {
        // A return that references the original shipment is still a return.
        let subject = "Your return received";
        let body = "We have received your return. Your package was delivered on July 1. Tracking 1Z999AA10123456784.";
        let hay = format!("x@y.com\n{subject}\n{body}");
        assert!(is_return_or_outbound(&hay));
        assert!(
            has_inbound_delivery_signal(&hay),
            "inbound language is present"
        );
        assert!(
            detect_shipment("x@y.com", subject, body).is_none(),
            "return exclusion must win over the inbound-delivery language"
        );
    }

    // ---- the LOOSE trigger signal ----------------------------------------

    #[test]
    fn loose_signal_fires_on_an_order_confirmation_with_no_tracking_number() {
        // THE CASE THE TRIGGER EXISTS FOR: a real purchase whose confirmation
        // names no carrier and carries no number, so `detect_shipment` can do
        // nothing with it — but the model can.
        let from = "auto-confirm@shop.example";
        let subject = "Order #A-1042 confirmed";
        let body = "Thanks for your order! We'll email you when it ships. \
                    Shipping to 100 Main St.";
        assert!(
            detect_shipment(from, subject, body).is_none(),
            "no tracking number: the regex extractor has nothing to key on"
        );
        assert!(
            has_loose_shipping_signal(from, subject, body),
            "but the loose trigger must still queue it for the extractor"
        );
    }

    #[test]
    fn loose_signal_excludes_returns_and_rmas() {
        // Same precedence as detection: goods flowing AWAY are never queued, so
        // the extractor never spends a call arguing with a return label.
        assert!(!has_loose_shipping_signal(
            "support@shop.example",
            "Your return label is ready",
            "Print the label and drop off your package at any UPS location. \
             Your refund posts once we receive it."
        ));
        assert!(!has_loose_shipping_signal(
            "support@shop.example",
            "RMA 4471 approved",
            "Your RMA has been approved; ship the item back to us."
        ));
    }

    #[test]
    fn loose_signal_ignores_unrelated_mail() {
        assert!(!has_loose_shipping_signal(
            "alice@example.com",
            "Lunch tomorrow?",
            "Want to grab a sandwich around noon? My treat."
        ));
        // A bare invoice with an amount and a long digit-run, and no shipping
        // vocabulary anywhere: no signal, so no model spend.
        assert!(!has_loose_shipping_signal(
            "billing@saas.example",
            "Invoice 998877665544 is ready",
            "Your monthly invoice for $42.00 is available in your dashboard."
        ));
    }

    // ---- carrier signal required for bare digit-runs ---------------------

    #[test]
    fn bare_digit_run_without_carrier_signal_is_not_a_shipment() {
        // A shipping signal ("package") plus a bare 10-digit number but no
        // carrier name/URL: not enough to name a carrier or a shipment.
        let s = detect_shipment(
            "orders@shop.example",
            "Your package update",
            "Reference 1234567890 for your package.",
        );
        assert!(
            s.is_none(),
            "bare digit-run, no carrier signal => no shipment: {s:?}"
        );
    }

    #[test]
    fn dhl_bare_digit_with_signal_is_a_shipment() {
        // Same shape, but with a real DHL signal (name + domain).
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

    // ---- item name recovered from the body -------------------------------

    #[test]
    fn ups_delivered_pulls_item_name_from_body_from_seller() {
        // The subject strips to the generic "Package"; the body's "From DOUBLE
        // TAKE MIRROR" supplies a real merchant phrase.
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
        // Generic subject, nothing usable in the body: item stays empty and the
        // client supplies its own fallback label.
        let s = detect_shipment(
            "ship@ups.com",
            "Your package has shipped",
            "Your package has shipped. Tracking 1Z999AA10123456784.",
        )
        .expect("ups shipment");
        assert_eq!(
            s.item_name, "",
            "no real phrase => empty, desktop fills the fallback"
        );
    }

    // ---- ambiguous digit-run context gating -------------------------------

    #[test]
    fn ebay_item_id_with_usps_mention_is_not_a_shipment() {
        // THE live bug: an eBay order mail carries 12-digit ITEM ids, shipping
        // language and a USPS mention, which cleared the old carrier-signal gate
        // and minted a phantom row per item id. Neither id sits in tracking
        // context, and both sit in item context.
        let s = detect_shipment(
            "ebay@ebay.com",
            "Your package is now with its carrier!",
            "Your package is now with its carrier! It is shipping via USPS. \
             See https://www.ebay.com/itm/123456789012 for the listing, item 234567890123.",
        );
        assert!(s.is_none(), "eBay item ids are not tracking numbers: {s:?}");
    }

    #[test]
    fn ebay_thread_with_item_ids_and_real_impb_yields_only_the_impb() {
        // The same mail once the carrier's REAL number lands in it: the IMpb is
        // self-identifying, so it wins outright and the item ids stay phantoms.
        let s = detect_shipment(
            "ebay@ebay.com",
            "Your package is now with its carrier!",
            "Item 234567890123 (https://www.ebay.com/itm/123456789012) shipped via USPS. \
             Tracking number 9400111899223817428490.",
        )
        .expect("the real IMpb is a shipment");
        assert_eq!(s.tracking_number, "9400111899223817428490");
        assert_eq!(s.carrier, "usps");
    }

    #[test]
    fn amazon_marketing_ten_digit_run_with_dhl_mention_is_not_a_shipment() {
        // "Price changes: review your upcoming delivery" — delivery language, a
        // DHL mention and a bare 10-digit run, none of it a tracking number.
        // Amazon is NOT a carrier-sender domain, so the exemption does not fire.
        let s = detect_shipment(
            "store-news@amazon.com",
            "Price changes: review your upcoming delivery",
            "We noticed a price change on an item in your upcoming delivery. \
             Reference 3051234567. Fulfilled by DHL eCommerce.",
        );
        assert!(s.is_none(), "marketing digit-run is not a shipment: {s:?}");
    }

    #[test]
    fn ambiguous_number_in_later_position_is_still_found() {
        // First-match-only would pin this row to the item id; the scan has to
        // walk past it to the labelled number behind it.
        let s = detect_shipment(
            "orders@shop.example",
            "Your order has shipped",
            "Item 234567890123 from the store. Tracking number 123456789012 via FedEx.",
        )
        .expect("the labelled number is a shipment");
        assert_eq!(s.tracking_number, "123456789012");
        assert_eq!(s.carrier, "fedex");
    }

    #[test]
    fn carrier_sender_domain_is_sufficient_context() {
        // A carrier's own notice often prints the number with no label at all —
        // the sender IS the context. Subdomains count.
        let s = detect_shipment(
            "auto@ship.fedex.com",
            "Your FedEx shipment",
            "Shipment 123456789012 is on its way.",
        )
        .expect("carrier-sent shipment");
        assert_eq!(s.tracking_number, "123456789012");
        assert_eq!(s.carrier, "fedex");
        // Same body from a retailer, with no label anywhere: not enough.
        assert!(
            detect_shipment(
                "orders@shop.example",
                "Your FedEx shipment",
                "Shipment 123456789012 is on its way.",
            )
            .is_none(),
            "no label, no carrier sender => no shipment"
        );
    }

    #[test]
    fn tracking_url_around_the_number_is_context() {
        // The number inside a carrier tracking URL needs no prose label.
        let s = detect_shipment(
            "orders@shop.example",
            "Your FedEx shipment is on its way",
            "See https://www.fedex.com/fedextrack/?trknbr=123456789012 for details.",
        )
        .expect("url-embedded number is a shipment");
        assert_eq!(s.tracking_number, "123456789012");
    }

    #[test]
    fn item_label_beats_a_nearby_tracking_label() {
        // Hard negative has PRECEDENCE: a "tracking" heading a few words away
        // does not launder an id introduced as "item".
        let s = detect_shipment(
            "orders@shop.example",
            "Tracking details for your USPS shipment",
            "Tracking details below. Item 234567890123 was packed.",
        );
        assert!(s.is_none(), "item label wins over the heading: {s:?}");
    }

    #[test]
    fn ambiguous_shape_test_covers_bare_runs_only() {
        // Bare runs an item/order id can impersonate.
        assert!(is_ambiguous_tracking_shape("1234567890"), "10 digits");
        assert!(is_ambiguous_tracking_shape("123456789012"), "12 digits");
        assert!(is_ambiguous_tracking_shape("123456789012345"), "15 digits");
        assert!(
            is_ambiguous_tracking_shape("12345678901234567890"),
            "20 digits, no IMpb prefix"
        );
        assert!(is_ambiguous_tracking_shape(" 1234567890 "), "trimmed");

        // Self-identifying shapes.
        assert!(!is_ambiguous_tracking_shape("1Z999AA10123456784"), "UPS");
        assert!(!is_ambiguous_tracking_shape("TBA303392911000"), "Amazon");
        assert!(
            !is_ambiguous_tracking_shape("9400111899223817428490"),
            "IMpb 94"
        );
        assert!(
            !is_ambiguous_tracking_shape("9205590164917312751089"),
            "IMpb 92"
        );
        assert!(
            !is_ambiguous_tracking_shape("9300120111410471677883"),
            "IMpb 93"
        );

        // Out-of-range runs and junk.
        assert!(!is_ambiguous_tracking_shape("123456789"), "9 digits");
        assert!(
            !is_ambiguous_tracking_shape("1234567890123456"),
            "16 digits"
        );
        assert!(!is_ambiguous_tracking_shape(""), "empty");
        assert!(!is_ambiguous_tracking_shape("abc"), "not digits");
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
