//! DHL Unified Tracking API client. No OAuth — a bare `DHL-API-Key` header —
//! which also makes DHL the strictest metered carrier here: the free tier is
//! one call per five seconds, 250 per day.
//!
//! DHL's `statusCode` vocabulary is FIVE values wide (`pre-transit`, `transit`,
//! `delivered`, `failure`, `unknown`) and has no out-for-delivery rung: DHL
//! lists that one as planned-but-unimplemented. So the OFD rung is recovered
//! from the human `description`, and only ever narrows `transit` — a description
//! can promote an in-transit shipment, never invent a stage DHL did not report.

use super::{CarrierClient, TrackError, send_json};
use crate::config::DhlCarrierConfig;
use crate::triage::{CarrierTrack, ShipmentStatus};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::Deserialize;
use std::time::Duration;

const BASE_URL: &str = "https://api-eu.dhl.com";

/// The whole credential, and the whole auth scheme.
const API_KEY_HEADER: &str = "DHL-API-Key";

/// DHL's free tier allows 1 call per 5 seconds; violating it burns the daily
/// budget on 429s.
const MIN_INTERVAL: Duration = Duration::from_secs(5);

/// The phrase that lifts `transit` to out-for-delivery, matched case-insensitively
/// against the status/event description.
const OUT_FOR_DELIVERY: &str = "out for delivery";

pub struct DhlClient {
    http: reqwest::Client,
    /// Secret material, NEVER logged or included in an error.
    api_key: String,
    base_url: String,
}

impl DhlClient {
    /// `None` when the key is absent or blank — a blank header is a guaranteed
    /// 401 at DHL, better spent as "not configured".
    pub fn from_config(cfg: &DhlCarrierConfig, http: reqwest::Client) -> Option<Self> {
        let api_key = cfg.api_key()?;
        Some(Self {
            http,
            api_key: api_key.to_string(),
            base_url: BASE_URL.to_string(),
        })
    }

    /// Point the client at a mock server. Test hook only.
    #[doc(hidden)]
    pub fn for_test(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            http,
            api_key: "test-api-key".to_string(),
            base_url: base_url.into(),
        }
    }
}

#[async_trait]
impl CarrierClient for DhlClient {
    fn carrier(&self) -> &'static str {
        "dhl"
    }

    fn min_interval(&self) -> Duration {
        MIN_INTERVAL
    }

    async fn track(&self, tracking_number: &str) -> Result<CarrierTrack, TrackError> {
        let req = self
            .http
            .get(format!("{}/track/shipments", self.base_url))
            .header(API_KEY_HEADER, &self.api_key)
            // Built as a query PAIR, never interpolated: a tracking number is
            // attacker-supplied (it arrives from an email body).
            .query(&[("trackingNumber", tracking_number)]);
        to_track(&send_json::<TrackingResponse>(req).await?)
    }
}

/// The slice of DHL's response we read. Every other field — addresses, piece
/// ids, proof of delivery, reroute urls — is dropped at the boundary rather than
/// modelled: none of it reaches [`CarrierTrack`].
#[derive(Debug, Default, Deserialize)]
struct TrackingResponse {
    #[serde(default)]
    shipments: Vec<Shipment>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Shipment {
    #[serde(default)]
    status: Option<StatusBlock>,
    #[serde(default)]
    estimated_time_of_delivery: Option<String>,
    #[serde(default)]
    events: Vec<StatusBlock>,
}

/// Shared by `status` and each `events[]` entry: DHL gives both the same four
/// fields, so one struct reads both and the event fallback needs no conversion.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusBlock {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    status_code: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// The human string for a block: `description`, falling back to the terser
/// `status` text some DHL divisions send instead. Both the OFD probe and
/// [`CarrierTrack::carrier_status_raw`] read this one resolution, so the text a
/// status is judged on is the text the user is shown.
fn detail(block: &StatusBlock) -> &str {
    let description = block.description.as_deref().unwrap_or("").trim();
    if !description.is_empty() {
        return description;
    }
    block.status.as_deref().unwrap_or("").trim()
}

/// `"statusCode description"` verbatim, degrading to whichever half exists.
fn raw_status(block: &StatusBlock) -> String {
    let code = block.status_code.as_deref().unwrap_or("").trim();
    let text = detail(block);
    match (code.is_empty(), text.is_empty()) {
        (false, false) => format!("{code} {text}"),
        (false, true) => code.to_string(),
        _ => text.to_string(),
    }
}

/// DHL's `statusCode` onto our ladder, pure. Anything outside the documented
/// five — including DHL's own `unknown`, and any code a division invents — is
/// `None`: the raw string is still recorded, but no stage is guessed.
///
/// `detail_hay` is the status and latest-event description text; it can only
/// promote `transit` to out-for-delivery.
fn map_status_code(code: &str, detail_hay: &str) -> Option<ShipmentStatus> {
    match code.trim().to_ascii_lowercase().as_str() {
        "pre-transit" => Some(ShipmentStatus::Ordered),
        "transit" => {
            if detail_hay.to_ascii_lowercase().contains(OUT_FOR_DELIVERY) {
                Some(ShipmentStatus::OutForDelivery)
            } else {
                Some(ShipmentStatus::Shipped)
            }
        }
        "delivered" => Some(ShipmentStatus::Delivered),
        "failure" => Some(ShipmentStatus::Exception),
        _ => None,
    }
}

/// The newest event by PARSED timestamp, not by array position. DHL documents
/// no ordering and its divisions disagree, so ordering is derived rather than
/// trusted; `events[0]` is the fallback only when nothing parses.
fn latest_event(events: &[StatusBlock]) -> Option<&StatusBlock> {
    events
        .iter()
        .filter_map(|e| Some((parse_timestamp(e.timestamp.as_deref()?)?, e)))
        .max_by_key(|(at, _)| *at)
        .map(|(_, e)| e)
        .or_else(|| events.first())
}

/// One DHL timestamp to UTC, or `None`.
///
/// Deliberately wider than RFC 3339, because DHL is: its own docs print
/// `2017-06-21T14:07:17+2:00` (a one-digit offset hour, which RFC 3339 forbids),
/// its OpenAPI examples print offsetless local times, and parcel divisions send
/// bare dates for an ETA. An unparseable stamp is dropped, never defaulted to
/// now — a wrong ETA is worse than no ETA.
fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(raw) {
        return Some(at.with_timezone(&Utc));
    }
    if let Some(fixed) = canonical_offset(raw)
        && let Ok(at) = DateTime::parse_from_rfc3339(&fixed)
    {
        return Some(at.with_timezone(&Utc));
    }
    // Offsetless: DHL means local time at the scan, which we have no zone for.
    // UTC is the only defensible read, and the error is bounded by a day.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(at) = NaiveDateTime::parse_from_str(raw, fmt) {
            return Some(at.and_utc());
        }
    }
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .ok()
        .map(|d| d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc())
}

/// Rewrite a loose numeric offset (`+2:00`, `+0200`, `+02`) into the `+02:00`
/// form RFC 3339 requires. `None` when the tail is not an offset at all — the
/// `-` inside the date is excluded by position, so an offsetless stamp is not
/// mistaken for one.
fn canonical_offset(raw: &str) -> Option<String> {
    let at = raw.rfind(['+', '-']).filter(|i| *i > 10)?;
    let (head, tail) = raw.split_at(at);
    let (sign, digits) = tail.split_at(1);
    let (hh, mm) = match digits.split_once(':') {
        Some(halves) => halves,
        None => match digits.len() {
            1 | 2 => (digits, "00"),
            4 => digits.split_at(2),
            _ => return None,
        },
    };
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !numeric(hh) || hh.len() > 2 || mm.len() != 2 || !numeric(mm) {
        return None;
    }
    Some(format!("{head}{sign}{hh:0>2}:{mm}"))
}

/// The whole response-to-[`CarrierTrack`] mapping, pure and HTTP-free.
///
/// An empty `shipments` array is [`TrackError::NotFound`]: DHL answers an
/// unknown number with either a 404 or a 200 carrying nothing, and the poller
/// must age the row out the same way for both.
fn to_track(resp: &TrackingResponse) -> Result<CarrierTrack, TrackError> {
    let shipment = resp.shipments.first().ok_or(TrackError::NotFound)?;
    let newest = latest_event(&shipment.events);
    // A shipment with no `status` block still has its scans; the newest event is
    // the same four fields.
    let head = shipment.status.as_ref().or(newest);

    let code = head.and_then(|b| b.status_code.as_deref()).unwrap_or("");
    let hay = format!(
        "{} {}",
        head.map(detail).unwrap_or(""),
        newest.map(detail).unwrap_or("")
    );
    let status = map_status_code(code, &hay);

    let timestamp = |block: Option<&StatusBlock>| {
        block
            .and_then(|b| b.timestamp.as_deref())
            .and_then(parse_timestamp)
    };
    let delivered_at = if status == Some(ShipmentStatus::Delivered) {
        timestamp(head).or_else(|| timestamp(newest))
    } else {
        None
    };

    Ok(CarrierTrack {
        status,
        carrier_status_raw: head.map(raw_status).unwrap_or_default(),
        eta: shipment
            .estimated_time_of_delivery
            .as_deref()
            .and_then(parse_timestamp),
        delivered_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn track(json: &str) -> Result<CarrierTrack, TrackError> {
        to_track(&serde_json::from_str::<TrackingResponse>(json).expect("fixture parses"))
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    // ---- status rungs ----------------------------------------------------

    /// Shape and field spelling taken from DHL's own Unified Tracking example
    /// payload (shipment 7777777770), extra fields left in to prove they are
    /// ignored rather than rejected.
    const PRE_TRANSIT: &str = r#"{
      "shipments": [{
        "id": "7777777770",
        "service": "express",
        "origin": { "address": { "countryCode": "NL", "addressLocality": "AMSTERDAM" } },
        "status": {
          "timestamp": "2018-03-02T07:53:47Z",
          "location": { "address": { "countryCode": "NL" } },
          "statusCode": "pre-transit",
          "status": "PRE_TRANSIT",
          "description": "Shipment information received",
          "pieceIds": ["JD014600006281230701"],
          "remark": "The shipment is pending pickup."
        },
        "estimatedTimeOfDelivery": "2018-03-05T00:00:00Z",
        "events": [{
          "timestamp": "2018-03-02T07:53:47Z",
          "statusCode": "pre-transit",
          "description": "Shipment information received"
        }]
      }]
    }"#;

    #[test]
    fn pre_transit_is_ordered() {
        let t = track(PRE_TRANSIT).unwrap();
        assert_eq!(t.status, Some(ShipmentStatus::Ordered));
        assert_eq!(
            t.carrier_status_raw,
            "pre-transit Shipment information received"
        );
        assert_eq!(t.eta, Some(at("2018-03-05T00:00:00Z")));
        assert_eq!(t.delivered_at, None);
    }

    #[test]
    fn transit_is_shipped() {
        let t = track(
            r#"{"shipments":[{"status":{
                 "timestamp":"2023-06-13T09:42:00+02:00",
                 "statusCode":"transit",
                 "status":"transit",
                 "description":"Processed at LEIPZIG - GERMANY"}}]}"#,
        )
        .unwrap();
        assert_eq!(t.status, Some(ShipmentStatus::Shipped));
        assert_eq!(
            t.carrier_status_raw,
            "transit Processed at LEIPZIG - GERMANY"
        );
        assert_eq!(
            t.delivered_at, None,
            "only delivered carries a delivery time"
        );
    }

    #[test]
    fn out_for_delivery_is_recovered_from_the_status_description() {
        // DHL has no out-for-delivery statusCode; the description is the only
        // place the rung appears.
        let t = track(
            r#"{"shipments":[{"status":{
                 "timestamp":"2023-06-14T07:05:00+02:00",
                 "statusCode":"transit",
                 "description":"Out for delivery"}}]}"#,
        )
        .unwrap();
        assert_eq!(t.status, Some(ShipmentStatus::OutForDelivery));
        assert_eq!(t.carrier_status_raw, "transit Out for delivery");
    }

    #[test]
    fn out_for_delivery_is_recovered_from_the_latest_event_description() {
        // The status block lags the scan feed, which is where the phrase lands
        // first. Mixed case, to pin the case-insensitive match.
        let t = track(
            r#"{"shipments":[{
                 "status":{"statusCode":"transit","description":"In transit"},
                 "events":[
                   {"timestamp":"2023-06-14T07:05:00+02:00","statusCode":"transit",
                    "description":"OUT FOR DELIVERY"},
                   {"timestamp":"2023-06-13T09:42:00+02:00","statusCode":"transit",
                    "description":"Processed at LEIPZIG"}]}]}"#,
        )
        .unwrap();
        assert_eq!(t.status, Some(ShipmentStatus::OutForDelivery));
        // The RAW string stays the status block's, not the event's.
        assert_eq!(t.carrier_status_raw, "transit In transit");
    }

    #[test]
    fn delivered_carries_a_delivery_time() {
        let t = track(
            r#"{"shipments":[{
                 "status":{"timestamp":"2023-06-14T11:18:00+02:00","statusCode":"delivered",
                           "status":"DELIVERED","description":"Delivered - Signed for by JESSICA"},
                 "estimatedTimeOfDelivery":"2023-06-14T18:00:00+02:00",
                 "events":[{"timestamp":"2023-06-14T11:18:00+02:00","statusCode":"delivered",
                            "description":"Delivered"}]}]}"#,
        )
        .unwrap();
        assert_eq!(t.status, Some(ShipmentStatus::Delivered));
        assert_eq!(t.delivered_at, Some(at("2023-06-14T09:18:00Z")));
        assert_eq!(t.eta, Some(at("2023-06-14T16:00:00Z")));
        assert_eq!(
            t.carrier_status_raw,
            "delivered Delivered - Signed for by JESSICA"
        );
    }

    #[test]
    fn delivered_falls_back_to_the_latest_event_timestamp() {
        // A status block with no timestamp of its own still delivers a time.
        let t = track(
            r#"{"shipments":[{
                 "status":{"statusCode":"delivered","description":"Delivered"},
                 "events":[
                   {"timestamp":"2023-06-13T09:42:00Z","statusCode":"transit","description":"In transit"},
                   {"timestamp":"2023-06-14T11:18:00Z","statusCode":"delivered","description":"Delivered"}]}]}"#,
        )
        .unwrap();
        assert_eq!(t.delivered_at, Some(at("2023-06-14T11:18:00Z")));
    }

    #[test]
    fn failure_is_an_exception() {
        let t = track(
            r#"{"shipments":[{"status":{
                 "timestamp":"2023-06-14T15:00:00Z",
                 "statusCode":"failure",
                 "description":"Delivery attempt unsuccessful - recipient not at home"}}]}"#,
        )
        .unwrap();
        assert_eq!(t.status, Some(ShipmentStatus::Exception));
        assert_eq!(t.delivered_at, None);
    }

    // ---- unmapped vocabulary ---------------------------------------------

    #[test]
    fn unknown_status_code_keeps_the_raw_string_and_maps_to_nothing() {
        let t = track(
            r#"{"shipments":[{"status":{
                 "timestamp":"2023-06-14T15:00:00Z",
                 "statusCode":"unknown",
                 "description":"Held at customs"}}]}"#,
        )
        .unwrap();
        assert_eq!(t.status, None, "DHL's own `unknown` is not a rung");
        assert_eq!(t.carrier_status_raw, "unknown Held at customs");
    }

    #[test]
    fn a_division_specific_code_maps_to_nothing() {
        let t = track(
            r#"{"shipments":[{"status":{
                 "statusCode":"ready-for-pickup","description":"Ready for pick-up"}}]}"#,
        )
        .unwrap();
        assert_eq!(t.status, None);
        assert_eq!(t.carrier_status_raw, "ready-for-pickup Ready for pick-up");
    }

    #[test]
    fn a_missing_description_falls_back_to_the_status_text() {
        let t =
            track(r#"{"shipments":[{"status":{"statusCode":"transit","status":"IN TRANSIT"}}]}"#)
                .unwrap();
        assert_eq!(t.status, Some(ShipmentStatus::Shipped));
        assert_eq!(t.carrier_status_raw, "transit IN TRANSIT");
    }

    #[test]
    fn a_shipment_with_no_status_block_reads_its_newest_event() {
        let t = track(
            r#"{"shipments":[{"events":[
                 {"timestamp":"2023-06-13T09:42:00Z","statusCode":"transit","description":"In transit"},
                 {"timestamp":"2023-06-14T11:18:00Z","statusCode":"delivered","description":"Delivered"}]}]}"#,
        )
        .unwrap();
        assert_eq!(t.status, Some(ShipmentStatus::Delivered));
        assert_eq!(t.carrier_status_raw, "delivered Delivered");
        assert_eq!(t.delivered_at, Some(at("2023-06-14T11:18:00Z")));
    }

    #[test]
    fn an_empty_shipment_list_is_not_found() {
        assert_eq!(track(r#"{"shipments":[]}"#), Err(TrackError::NotFound));
        assert_eq!(track("{}"), Err(TrackError::NotFound));
    }

    // ---- timestamps ------------------------------------------------------

    #[test]
    fn timestamps_survive_dhls_offset_spellings() {
        // RFC 3339, the common case.
        assert_eq!(
            parse_timestamp("2023-06-13T09:42:00+02:00"),
            Some(at("2023-06-13T07:42:00Z"))
        );
        assert_eq!(
            parse_timestamp("2018-03-02T07:53:47Z"),
            Some(at("2018-03-02T07:53:47Z"))
        );
        // A one-digit offset hour, as printed in DHL's own docs.
        assert_eq!(
            parse_timestamp("2017-06-21T14:07:17+2:00"),
            Some(at("2017-06-21T12:07:17Z"))
        );
        // Compact and hour-only offsets.
        assert_eq!(
            parse_timestamp("2023-06-13T09:42:00-0500"),
            Some(at("2023-06-13T14:42:00Z"))
        );
        assert_eq!(
            parse_timestamp("2023-06-13T09:42:00+02"),
            Some(at("2023-06-13T07:42:00Z"))
        );
        // Offsetless and date-only, both read as UTC.
        assert_eq!(
            parse_timestamp("2021-01-01T00:00:00"),
            Some(at("2021-01-01T00:00:00Z"))
        );
        assert_eq!(
            parse_timestamp("2023-06-13T09:42:00.123"),
            Some(at("2023-06-13T09:42:00.123Z"))
        );
        assert_eq!(
            parse_timestamp("2018-08-03"),
            Some(at("2018-08-03T00:00:00Z"))
        );
    }

    #[test]
    fn an_unparseable_timestamp_is_dropped_not_defaulted() {
        for raw in ["", "   ", "tomorrow", "13/06/2023", "2023-13-45T99:99:99Z"] {
            assert_eq!(parse_timestamp(raw), None, "raw: {raw:?}");
        }
        // …and a shipment carrying one still tracks, just without an ETA.
        let t = track(
            r#"{"shipments":[{"status":{"statusCode":"transit","description":"In transit"},
                 "estimatedTimeOfDelivery":"soon"}]}"#,
        )
        .unwrap();
        assert_eq!(t.eta, None);
        assert_eq!(t.status, Some(ShipmentStatus::Shipped));
    }

    #[test]
    fn the_newest_event_wins_whichever_way_the_array_is_ordered() {
        let newest = r#"{"timestamp":"2023-06-14T11:18:00Z","statusCode":"delivered","description":"Delivered"}"#;
        let older = r#"{"timestamp":"2023-06-13T09:42:00Z","statusCode":"transit","description":"In transit"}"#;
        for events in [format!("[{newest},{older}]"), format!("[{older},{newest}]")] {
            let t = track(&format!(r#"{{"shipments":[{{"events":{events}}}]}}"#)).unwrap();
            assert_eq!(
                t.status,
                Some(ShipmentStatus::Delivered),
                "events: {events}"
            );
        }
    }

    // ---- the request itself ----------------------------------------------

    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<HashMap<String, String>>>);

    async fn mock_shipments(
        State(seen): State<Seen>,
        headers: HeaderMap,
        Query(query): Query<HashMap<String, String>>,
    ) -> impl IntoResponse {
        let mut seen = seen.0.lock().unwrap();
        seen.insert(
            "api-key".into(),
            headers
                .get(API_KEY_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string(),
        );
        seen.insert(
            "trackingNumber".into(),
            query.get("trackingNumber").cloned().unwrap_or_default(),
        );
        Json(serde_json::json!({
            "shipments": [{
                "status": { "statusCode": "transit", "description": "Out for delivery" }
            }]
        }))
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn the_request_carries_the_key_header_and_the_tracking_number() {
        let seen = Seen::default();
        let base = serve(
            Router::new()
                .route("/track/shipments", get(mock_shipments))
                .with_state(seen.clone()),
        )
        .await;
        let client = DhlClient::for_test(base, reqwest::Client::new());

        let t = client.track("00340434292135100186").await.unwrap();
        assert_eq!(t.status, Some(ShipmentStatus::OutForDelivery));
        let seen = seen.0.lock().unwrap();
        assert_eq!(seen["api-key"], "test-api-key");
        assert_eq!(seen["trackingNumber"], "00340434292135100186");
    }

    #[tokio::test]
    async fn a_404_problem_json_body_is_not_found() {
        let base = serve(Router::new().route(
            "/track/shipments",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    [("content-type", "application/problem+json")],
                    r#"{"status":404,"title":"No result found","detail":"No shipment with given id"}"#,
                )
            }),
        ))
        .await;
        let client = DhlClient::for_test(base, reqwest::Client::new());
        assert_eq!(client.track("0000000000").await, Err(TrackError::NotFound));
    }
}
