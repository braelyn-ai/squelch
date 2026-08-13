//! USPS Tracking API (v3) client. OAuth client-credentials with the consumer
//! pair in a JSON body; token and tracking endpoints share one origin, so a
//! single `base_url` override redirects both in tests.
//!
//! TWO DEVIATIONS FROM THE OTHER CARRIERS, both forced by USPS's own examples
//! (<https://github.com/USPS/api-examples>, `Example-Postman.postman_collection.json`):
//!
//!   * The token request is a JSON body carrying `client_id` / `client_secret` /
//!     `grant_type`, not `Authorization: Basic` and not a form. Neither
//!     [`super::oauth::ClientAuth`] arm encodes that, so the request is built
//!     here and only the caching and the response shape are shared.
//!   * The track call asks for `expand=DETAIL`. `SUMMARY` (the endpoint default)
//!     returns `TrackingSummary`, which has NO `statusCategory` / `status` /
//!     `statusSummary` at all — only one line of prose per scan — so it cannot
//!     drive [`ShipmentStatus`]. `DETAIL` is a superset and returns both.
//!
//! Every USPS-shaped body is parsed by [`parse_track`], which never touches the
//! network: the mapping is the part that breaks when USPS extends its status
//! vocabulary, and it is checked against canned payloads.

use super::oauth::{TokenCache, TokenResponse};
use super::{CarrierClient, TrackError, classify_status, send_json};
use crate::config::UspsCarrierConfig;
use crate::triage::{CarrierTrack, ShipmentStatus};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const BASE_URL: &str = "https://apis.usps.com";

/// Floor between USPS calls. The default API product allows 60/hour, which the
/// poller's per-carrier budget enforces; this floor only keeps bursts polite.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// USPS's internal code for "this tracking number means nothing to us", spent
/// with an HTTP **400**, not a 404 (USPS Error Inventory, Tracking sheet). The
/// shared status map would call that 400 transient and retry a number that can
/// never resolve, so the body is consulted before the status is trusted.
const UNKNOWN_NUMBER_CODE: &str = "150002";

pub struct UspsClient {
    http: reqwest::Client,
    consumer_key: String,
    /// Secret material, NEVER logged or included in an error.
    consumer_secret: String,
    token: TokenCache,
    base_url: String,
}

impl UspsClient {
    /// `None` when credentials do not fully resolve — half a pair leaves USPS
    /// out of the registry rather than sending an empty string at USPS's auth
    /// endpoint.
    pub fn from_config(cfg: &UspsCarrierConfig, http: reqwest::Client) -> Option<Self> {
        let (consumer_key, consumer_secret) = cfg.credentials()?;
        Some(Self {
            http,
            consumer_key: consumer_key.to_string(),
            consumer_secret: consumer_secret.to_string(),
            token: TokenCache::new(),
            base_url: BASE_URL.to_string(),
        })
    }

    /// Point the client at a mock server. Test hook only.
    #[doc(hidden)]
    pub fn for_test(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            http,
            consumer_key: "test-consumer-key".to_string(),
            consumer_secret: "test-consumer-secret".to_string(),
            token: TokenCache::new(),
            base_url: base_url.into(),
        }
    }

    /// A live access token, minted at most once per expiry window by the shared
    /// cache. The body is USPS's own client-credentials example verbatim; `scope`
    /// is omitted exactly as that example omits it, which grants every scope the
    /// consumer key is subscribed to.
    async fn access_token(&self) -> Result<String, TrackError> {
        self.token
            .get_or_refresh(|| async {
                let body = json!({
                    "client_id": self.consumer_key,
                    "client_secret": self.consumer_secret,
                    "grant_type": "client_credentials",
                });
                let url = format!("{}/oauth2/v3/token", self.base_url);
                send_json::<TokenResponse>(self.http.post(url).json(&body)).await
            })
            .await
    }
}

/// The union of `TrackingSummary` and `TrackingDetails`, which the 200 is
/// declared as a `oneOf` over. Every field is optional because USPS drops the
/// ones that do not apply (`expectedDeliveryDate` disappears once a package is
/// delivered) and adds its own without notice.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackingBody {
    /// `TrackingDetails` only: the scan phrase, e.g. "Delivered, Front
    /// Door/Porch".
    #[serde(default)]
    status: Option<String>,
    /// `TrackingDetails` only: the bucket the scan rolls up to. THE ONLY FIELD
    /// [`map_status`] reads — `status` is a long tail of thousands of Pub-199
    /// scan events.
    #[serde(default)]
    status_category: Option<String>,
    /// `TrackingDetails` only: the sentence USPS shows a human.
    #[serde(default)]
    status_summary: Option<String>,
    /// `TrackingDetails` form of the ETA.
    #[serde(default)]
    expected_delivery_time_stamp: Option<String>,
    /// `TrackingSummary` form of the ETA, which `DETAIL` also carries.
    #[serde(default)]
    expected_delivery_date: Option<String>,
    /// Time-of-day half of [`Self::expected_delivery_date`]. Free-form text, not
    /// a typed time.
    #[serde(default)]
    expected_delivery_time: Option<String>,
    /// `TrackingDetails` scans, REVERSE CHRONOLOGICAL.
    #[serde(default)]
    tracking_events: Vec<TrackingEvent>,
    /// `TrackingSummary` scans: one prose line each, same ordering.
    #[serde(default)]
    event_summaries: Vec<String>,
    /// USPS sometimes answers 200 with an error envelope instead of a tracking
    /// body. Left untyped: `error.code` is documented as a string but arrives as
    /// a number often enough that a typed field would fail the WHOLE parse and
    /// turn a not-found into a retry loop.
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackingEvent {
    #[serde(default)]
    event_type: Option<String>,
    /// LOCAL wall time, no offset. Only the fallback.
    #[serde(default)]
    event_timestamp: Option<String>,
    /// Unambiguously UTC, and therefore preferred. Named outright: the
    /// camel-case rule would render this `gMTTimestamp`.
    #[serde(default, rename = "GMTTimestamp")]
    gmt_timestamp: Option<String>,
}

/// USPS's `statusCategory` onto our ladder. Case-insensitive; ANYTHING
/// unrecognized is `None` rather than a guess, per [`CarrierTrack::status`].
///
/// Deliberately unmapped, though USPS does spend them: "Available for Pickup"
/// and "Delivery Attempt" are neither a rung nor unambiguously a failure, and
/// inventing one would move a shipment card on a scan that means "come get it".
fn map_status(category: &str) -> Option<ShipmentStatus> {
    let c = category.trim().to_ascii_lowercase();
    // USPS varies the tail ("In Transit to Next Facility", "In-Transit"), so the
    // substring decides before any exact match.
    if c.contains("in transit") || c.contains("in-transit") {
        return Some(ShipmentStatus::Shipped);
    }
    match c.as_str() {
        "pre-shipment" | "pre shipment" | "preshipment" => Some(ShipmentStatus::Ordered),
        // Accepted is a real hand-off scan: the package is moving.
        "accepted" => Some(ShipmentStatus::Shipped),
        "out for delivery" => Some(ShipmentStatus::OutForDelivery),
        "delivered" => Some(ShipmentStatus::Delivered),
        "alert" | "return to sender" => Some(ShipmentStatus::Exception),
        _ => None,
    }
}

/// A USPS timestamp in any of the three shapes it ships in. An offset-less stamp
/// is read as UTC rather than assigned a zone we would have to invent; `None`
/// for anything else, since a wrong ETA is worse than no ETA.
fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(naive.and_utc());
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .map(|d| d.and_time(NaiveTime::MIN).and_utc())
}

/// `expectedDeliveryTime`, which USPS types as a free string and populates with
/// whatever its front end renders ("6:00 PM", "18:00:00", "" ).
fn parse_time_of_day(raw: &str) -> Option<NaiveTime> {
    let s = raw.trim().to_ascii_uppercase();
    if s.is_empty() {
        return None;
    }
    for fmt in [
        "%I:%M:%S %p",
        "%I:%M %p",
        "%I:%M%p",
        "%I %p",
        "%I%p",
        "%H:%M:%S",
        "%H:%M",
    ] {
        if let Ok(t) = NaiveTime::parse_from_str(&s, fmt) {
            return Some(t);
        }
    }
    None
}

/// The carrier ETA. The `DETAIL` timestamp wins; the `SUMMARY` date is the
/// fallback, refined by `expectedDeliveryTime` when that parses. A date whose
/// time half is unreadable still yields the DATE — the day is the part the card
/// shows, and dropping it over an unparsed "6ish" would be a worse answer.
fn parse_eta(body: &TrackingBody) -> Option<DateTime<Utc>> {
    if let Some(at) = body
        .expected_delivery_time_stamp
        .as_deref()
        .and_then(parse_timestamp)
    {
        return Some(at);
    }
    let date =
        NaiveDate::parse_from_str(body.expected_delivery_date.as_deref()?.trim(), "%Y-%m-%d")
            .ok()?;
    let time = body
        .expected_delivery_time
        .as_deref()
        .and_then(parse_time_of_day)
        .unwrap_or(NaiveTime::MIN);
    Some(date.and_time(time).and_utc())
}

/// The delivery scan's time, or `None` while the package is still moving.
///
/// `trackingEvents` is REVERSE CHRONOLOGICAL, so the first match is the latest
/// delivery scan. Matched on a `starts_with` rather than a substring: USPS
/// suffixes the placement ("Delivered, In/At Mailbox") but also ships
/// "Undeliverable as Addressed" and "Delivery Attempted", neither of which is a
/// delivery.
fn delivered_at(events: &[TrackingEvent]) -> Option<DateTime<Utc>> {
    let event = events.iter().find(|e| {
        e.event_type
            .as_deref()
            .is_some_and(|t| t.trim().to_ascii_lowercase().starts_with("delivered"))
    })?;
    event
        .gmt_timestamp
        .as_deref()
        .and_then(parse_timestamp)
        .or_else(|| event.event_timestamp.as_deref().and_then(parse_timestamp))
}

/// Flatten every string and number leaf of an error envelope. USPS spreads the
/// reason across `error.message`, `error.errors[].title` and
/// `error.errors[].detail`, and which one carries it varies by endpoint.
fn collect_error_text(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Number(n) => {
            out.push_str(&n.to_string());
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|i| collect_error_text(i, out)),
        Value::Object(map) => map.values().for_each(|i| collect_error_text(i, out)),
        _ => {}
    }
}

/// Does an error envelope say the NUMBER is unknown, as opposed to anything else
/// having gone wrong?
///
/// Scoped to the `error` object on purpose: a live shipment's `statusSummary`
/// can legitimately read "Addressee not found", and matching that would age out
/// a package that is merely having a bad day.
fn error_reports_unknown_number(err: &Value) -> bool {
    let mut text = String::new();
    collect_error_text(err, &mut text);
    let text = text.to_ascii_lowercase();
    text.contains("not found") || text.contains("no record") || text.contains(UNKNOWN_NUMBER_CODE)
}

/// The same question against a raw body, for the NON-2xx path where there is no
/// parsed struct yet. A body that is not JSON is not an answer about the number.
fn body_reports_unknown_number(body: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    v.get("error").is_some_and(error_reports_unknown_number)
}

/// One USPS 200 body onto a [`CarrierTrack`]. Pure: everything USPS-specific
/// about a tracking answer is decided here, without a socket.
fn parse_track(body: &str) -> Result<CarrierTrack, TrackError> {
    let parsed: TrackingBody = serde_json::from_str(body).map_err(|_| TrackError::Transient)?;
    if let Some(err) = parsed.error.as_ref() {
        return Err(if error_reports_unknown_number(err) {
            TrackError::NotFound
        } else {
            TrackError::Transient
        });
    }

    let status = parsed.status_category.as_deref().and_then(map_status);
    // The sentence USPS shows a human first, then the scan phrase, then the
    // bucket, then a SUMMARY-shaped body's newest prose line. Something is
    // recorded verbatim whichever half of the `oneOf` came back.
    let carrier_status_raw = [
        parsed.status_summary.as_deref(),
        parsed.status.as_deref(),
        parsed.status_category.as_deref(),
        parsed.event_summaries.first().map(String::as_str),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|s| !s.is_empty())
    .unwrap_or_default()
    .to_string();

    Ok(CarrierTrack {
        status,
        carrier_status_raw,
        eta: parse_eta(&parsed),
        delivered_at: delivered_at(&parsed.tracking_events),
    })
}

#[async_trait]
impl CarrierClient for UspsClient {
    fn carrier(&self) -> &'static str {
        "usps"
    }

    fn min_interval(&self) -> Duration {
        MIN_INTERVAL
    }

    async fn track(&self, tracking_number: &str) -> Result<CarrierTrack, TrackError> {
        // The number lands in the URL PATH. It reaches us from mail text, so a
        // `/` or `?` in one would reshape the request into a different endpoint;
        // USPS has no non-alphanumeric number, making rejection free.
        if tracking_number.is_empty() || !tracking_number.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return Err(TrackError::NotFound);
        }
        let token = self.access_token().await?;
        let url = format!(
            "{}/tracking/v3/tracking/{tracking_number}?expand=DETAIL",
            self.base_url
        );
        let resp = self
            .http
            .get(url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| TrackError::Transient)?;

        // Deliberately NOT `send_json`: USPS answers an unknown number with a
        // 400, so the body has to be read BEFORE the status is classified.
        // Errors still come from the shared map; nothing read here is logged.
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.text().await.map_err(|_| TrackError::Transient)?;
        if !status.is_success() {
            return Err(if body_reports_unknown_number(&body) {
                TrackError::NotFound
            } else {
                classify_status(status, &headers)
            });
        }
        parse_track(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures follow the Tracking 3.0 OpenAPI spec USPS publishes alongside its
    // examples repo (`developers.usps.com/.../apidoc_specs/tracking_21.yaml`,
    // schemas `TrackingDetails` / `TrackingSummary` / `ErrorMessage`), with the
    // spec's own `statusSummary` example text.

    /// A `DETAIL` body with the fields the mapping actually reads. The extra key
    /// is there on purpose: `TrackingDetails` is `additionalProperties: true`,
    /// and an unknown field must not fail the parse.
    fn detail(category: &str, summary: &str) -> String {
        json!({
            "trackingNumber": "9400111899223817428490",
            "statusCategory": category,
            "status": category,
            "statusSummary": summary,
            "mailClass": "PRIORITY_MAIL",
            "trackingEvents": []
        })
        .to_string()
    }

    fn track(body: &str) -> CarrierTrack {
        parse_track(body).expect("fixture parses")
    }

    // ---- one fixture per rung --------------------------------------------

    #[test]
    fn pre_shipment_is_ordered() {
        let t = track(&detail(
            "Pre-Shipment",
            "A shipping label has been prepared for your item.",
        ));
        assert_eq!(t.status, Some(ShipmentStatus::Ordered));
        assert_eq!(
            t.carrier_status_raw,
            "A shipping label has been prepared for your item."
        );
        assert_eq!(t.delivered_at, None);
    }

    #[test]
    fn accepted_and_every_in_transit_variant_are_shipped() {
        for category in [
            "Accepted",
            "In Transit",
            "in transit",
            "In-Transit",
            "In Transit to Next Facility",
        ] {
            let t = track(&detail(category, "Your item is on its way."));
            assert_eq!(
                t.status,
                Some(ShipmentStatus::Shipped),
                "category: {category:?}"
            );
        }
    }

    #[test]
    fn out_for_delivery_is_its_own_rung() {
        let t = track(&detail(
            "Out for Delivery",
            "Out for Delivery, 06/17/2022, 6:10 am, FAIRFAX, VA 22032",
        ));
        assert_eq!(t.status, Some(ShipmentStatus::OutForDelivery));
    }

    #[test]
    fn delivered_is_delivered() {
        let t = track(&detail(
            "Delivered",
            "Your item was delivered at 12:55 pm on April 05, 2010 in FALMOUTH, MA 02540",
        ));
        assert_eq!(t.status, Some(ShipmentStatus::Delivered));
    }

    #[test]
    fn alert_and_return_to_sender_are_exceptions() {
        for category in ["Alert", "Return to Sender", "RETURN TO SENDER"] {
            let t = track(&detail(category, "We attempted to deliver your item."));
            assert_eq!(
                t.status,
                Some(ShipmentStatus::Exception),
                "category: {category:?}"
            );
        }
    }

    // ---- vocabulary we do not model --------------------------------------

    #[test]
    fn unmapped_category_yields_no_status_but_keeps_the_raw_text() {
        // Both are real USPS categories. Neither is a rung, so the shipment's
        // status is left alone and only the carrier's own words are recorded.
        for category in ["Available for Pickup", "Delivery Attempt"] {
            let t = track(&detail(category, "Your item arrived at a Post Office."));
            assert_eq!(t.status, None, "category: {category:?}");
            assert_eq!(t.carrier_status_raw, "Your item arrived at a Post Office.");
        }
    }

    #[test]
    fn a_body_with_no_status_fields_still_records_its_prose() {
        // The `oneOf` can answer `TrackingSummary`, which has no status fields at
        // all — the newest event line is the only verbatim text available.
        let body = json!({
            "trackingNumber": "9400111899223817428490",
            "expectedDeliveryDate": "2022-06-22",
            "eventSummaries": [
                "Out for Delivery, 06/17/2022, 6:10 am, FAIRFAX, VA 22032",
                "Arrived at Post Office, 06/16/2022, 2:15 pm, FAIRFAX, VA 22031"
            ]
        })
        .to_string();
        let t = track(&body);
        assert_eq!(t.status, None, "prose is never mapped onto the ladder");
        assert_eq!(
            t.carrier_status_raw,
            "Out for Delivery, 06/17/2022, 6:10 am, FAIRFAX, VA 22032"
        );
    }

    // ---- unknown numbers --------------------------------------------------

    #[test]
    fn an_error_envelope_naming_the_number_is_not_found() {
        // USPS's Tracking error 150002, verbatim from its Error Inventory.
        let body = json!({
            "apiVersion": "3.0",
            "error": {
                "code": "400",
                "message": "Bad Request",
                "errors": [{
                    "status": "400",
                    "code": "150002",
                    "title": "Invalid Tracking Number",
                    "detail": "7: The tracking number may be incorrect or the status update is not yet available. Please verify your tracking number and try again later. - "
                }]
            }
        })
        .to_string();
        assert_eq!(parse_track(&body), Err(TrackError::NotFound));
        assert!(body_reports_unknown_number(&body));
    }

    #[test]
    fn a_not_found_phrase_anywhere_in_the_envelope_counts() {
        let body = json!({
            "error": { "code": 404, "message": "Tracking number not found for a mailing date within the last 45 days" }
        })
        .to_string();
        assert_eq!(parse_track(&body), Err(TrackError::NotFound));
    }

    #[test]
    fn any_other_error_envelope_stays_transient() {
        // A carrier-side fault must be retried, not aged out.
        let body = json!({
            "error": { "code": "503", "message": "Service temporarily unavailable" }
        })
        .to_string();
        assert_eq!(parse_track(&body), Err(TrackError::Transient));
        assert!(!body_reports_unknown_number(&body));
    }

    #[test]
    fn a_live_shipment_is_never_mistaken_for_a_missing_one() {
        // "not found" in a STATUS line is a delivery problem, not an unknown
        // number: the phrase only counts inside an `error` envelope.
        let t = track(&detail(
            "Alert",
            "Addressee not found. Item being returned.",
        ));
        assert_eq!(t.status, Some(ShipmentStatus::Exception));
        assert!(!body_reports_unknown_number(&detail("Alert", "not found")));
    }

    #[test]
    fn a_body_that_is_not_json_is_transient() {
        assert_eq!(
            parse_track("<html>gateway timeout</html>"),
            Err(TrackError::Transient)
        );
        assert!(!body_reports_unknown_number("<html>gateway timeout</html>"));
    }

    // ---- ETA --------------------------------------------------------------

    #[test]
    fn eta_prefers_the_detail_timestamp() {
        let body = json!({
            "statusCategory": "In Transit",
            "statusSummary": "In transit to next facility",
            "expectedDeliveryTimeStamp": "2024-04-08T18:00:00Z",
            // Present and DIFFERENT, to prove the timestamp wins.
            "expectedDeliveryDate": "2024-04-09",
            "expectedDeliveryTime": "6:00 PM"
        })
        .to_string();
        assert_eq!(
            track(&body).eta,
            Some("2024-04-08T18:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn eta_falls_back_to_the_date_and_time_pair() {
        let body = json!({
            "statusCategory": "Accepted",
            "statusSummary": "Accepted at USPS Origin Facility",
            "expectedDeliveryDate": "2024-04-09",
            "expectedDeliveryTime": "6:00 PM"
        })
        .to_string();
        assert_eq!(
            track(&body).eta,
            Some("2024-04-09T18:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn an_unreadable_time_keeps_the_date() {
        let body = json!({
            "statusCategory": "Accepted",
            "expectedDeliveryDate": "2024-04-09",
            "expectedDeliveryTime": "by end of day"
        })
        .to_string();
        assert_eq!(
            track(&body).eta,
            Some("2024-04-09T00:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn an_unreadable_eta_is_none_rather_than_a_guess() {
        for body in [
            json!({ "statusCategory": "Accepted", "expectedDeliveryTimeStamp": "soon" }),
            json!({ "statusCategory": "Accepted", "expectedDeliveryDate": "April 9, 2024" }),
            json!({ "statusCategory": "Accepted", "expectedDeliveryDate": "" }),
            json!({ "statusCategory": "Delivered", "statusSummary": "delivered" }),
        ] {
            assert_eq!(track(&body.to_string()).eta, None, "body: {body}");
        }
    }

    #[test]
    fn expected_delivery_time_accepts_the_shapes_usps_ships() {
        assert_eq!(
            parse_time_of_day("6:00 PM"),
            NaiveTime::from_hms_opt(18, 0, 0)
        );
        assert_eq!(
            parse_time_of_day("6:00pm"),
            NaiveTime::from_hms_opt(18, 0, 0)
        );
        assert_eq!(
            parse_time_of_day("18:00:00"),
            NaiveTime::from_hms_opt(18, 0, 0)
        );
        assert_eq!(
            parse_time_of_day("09:30"),
            NaiveTime::from_hms_opt(9, 30, 0)
        );
        assert_eq!(parse_time_of_day(""), None);
        assert_eq!(parse_time_of_day("end of day"), None);
    }

    // ---- delivered_at -----------------------------------------------------

    #[test]
    fn delivered_at_comes_from_the_delivery_scan() {
        // Reverse-chronological, and the GMT stamp is the one to trust.
        let body = json!({
            "statusCategory": "Delivered",
            "statusSummary": "Your item was delivered at 12:55 pm on April 05, 2024 in FALMOUTH, MA 02540",
            "trackingEvents": [
                {
                    "eventType": "Delivered, In/At Mailbox",
                    "eventTimestamp": "2024-04-05T12:55:00",
                    "GMTTimestamp": "2024-04-05T16:55:00.000Z",
                    "eventCity": "FALMOUTH"
                },
                {
                    "eventType": "Out for Delivery",
                    "eventTimestamp": "2024-04-05T06:10:00",
                    "GMTTimestamp": "2024-04-05T10:10:00.000Z"
                }
            ]
        })
        .to_string();
        let t = track(&body);
        assert_eq!(t.status, Some(ShipmentStatus::Delivered));
        assert_eq!(
            t.delivered_at,
            Some("2024-04-05T16:55:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn delivered_at_falls_back_to_the_local_stamp() {
        // No GMT half: the offset-less stamp is read as UTC rather than dropped.
        let body = json!({
            "statusCategory": "Delivered",
            "trackingEvents": [
                { "eventType": "Delivered, Front Door/Porch", "eventTimestamp": "2024-04-05T12:55:00" }
            ]
        })
        .to_string();
        assert_eq!(
            track(&body).delivered_at,
            Some("2024-04-05T12:55:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn a_near_miss_scan_is_not_a_delivery() {
        // "Delivery Attempted" and "Undeliverable as Addressed" both contain
        // "deliver"; neither delivered anything.
        let body = json!({
            "statusCategory": "Alert",
            "statusSummary": "We attempted to deliver your item.",
            "trackingEvents": [
                { "eventType": "Delivery Attempted - No Access to Delivery Location", "GMTTimestamp": "2024-04-05T16:55:00Z" },
                { "eventType": "Undeliverable as Addressed", "GMTTimestamp": "2024-04-04T16:55:00Z" }
            ]
        })
        .to_string();
        let t = track(&body);
        assert_eq!(t.status, Some(ShipmentStatus::Exception));
        assert_eq!(t.delivered_at, None);
    }

    // ---- on the wire ------------------------------------------------------
    //
    // The token BODY SHAPE, the bearer hand-off and the `expand=DETAIL` query
    // are the parts a fixture cannot check, and each is a silent 400 in
    // production if it is wrong.

    use axum::extract::{Path, Query, RawQuery};
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// What the mock saw, so the assertions are on bytes rather than intent.
    #[derive(Default)]
    struct Seen {
        token_bodies: Vec<Value>,
        paths: Vec<String>,
        queries: Vec<String>,
        authorizations: Vec<String>,
    }

    async fn usps_mock(seen: Arc<Mutex<Seen>>) -> String {
        let token_state = seen.clone();
        let app = Router::new()
            .route(
                "/oauth2/v3/token",
                post(move |Json(body): Json<Value>| {
                    let seen = token_state.clone();
                    async move {
                        seen.lock().unwrap().token_bodies.push(body);
                        Json(json!({ "access_token": "usps-test-token", "expires_in": 3600 }))
                    }
                }),
            )
            .route(
                "/tracking/v3/tracking/{number}",
                get(
                    move |Path(number): Path<String>,
                          RawQuery(query): RawQuery,
                          Query(_q): Query<HashMap<String, String>>,
                          headers: HeaderMap| {
                        let seen = seen.clone();
                        async move {
                            {
                                let mut s = seen.lock().unwrap();
                                s.paths.push(number);
                                s.queries.push(query.unwrap_or_default());
                                s.authorizations.push(
                                    headers
                                        .get("authorization")
                                        .and_then(|v| v.to_str().ok())
                                        .unwrap_or_default()
                                        .to_string(),
                                );
                            }
                            Json(json!({
                                "trackingNumber": "9400111899223817428490",
                                "statusCategory": "Delivered",
                                "statusSummary": "Your item was delivered at 12:55 pm on April 05, 2024 in FALMOUTH, MA 02540",
                                "trackingEvents": [
                                    { "eventType": "Delivered, In/At Mailbox", "GMTTimestamp": "2024-04-05T16:55:00.000Z" }
                                ]
                            }))
                        }
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn track_mints_one_token_and_reads_the_detail_endpoint() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let base = usps_mock(seen.clone()).await;
        let client = UspsClient::for_test(base, reqwest::Client::new());

        for _ in 0..2 {
            let t = client.track("9400111899223817428490").await.unwrap();
            assert_eq!(t.status, Some(ShipmentStatus::Delivered));
            assert_eq!(
                t.delivered_at,
                Some("2024-04-05T16:55:00Z".parse::<DateTime<Utc>>().unwrap())
            );
        }

        let s = seen.lock().unwrap();
        // Cached: two tracks, ONE token call.
        assert_eq!(s.token_bodies.len(), 1);
        assert_eq!(
            s.token_bodies[0],
            json!({
                "client_id": "test-consumer-key",
                "client_secret": "test-consumer-secret",
                "grant_type": "client_credentials"
            }),
            "USPS's client-credentials example is a JSON body, not Basic auth"
        );
        assert_eq!(s.paths, ["9400111899223817428490"; 2]);
        assert_eq!(s.queries, ["expand=DETAIL"; 2]);
        assert_eq!(s.authorizations, ["Bearer usps-test-token"; 2]);
    }

    #[tokio::test]
    async fn a_number_that_could_reshape_the_url_never_leaves_the_process() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let base = usps_mock(seen.clone()).await;
        let client = UspsClient::for_test(base, reqwest::Client::new());
        for number in ["", "../../oauth2/v3/token", "94001 11899", "9400?expand=x"] {
            assert_eq!(
                client.track(number).await,
                Err(TrackError::NotFound),
                "number: {number:?}"
            );
        }
        let s = seen.lock().unwrap();
        assert!(s.token_bodies.is_empty(), "no credential is spent either");
        assert!(s.paths.is_empty());
    }
}
