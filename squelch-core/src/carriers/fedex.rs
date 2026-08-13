//! FedEx Track API client. OAuth client-credentials with the pair in the form
//! body; token and tracking endpoints share one origin, so a single `base_url`
//! override redirects both in tests.
//!
//! FedEx answers an unknown tracking number with a 200 whose `trackResults`
//! entry carries an `error` block instead of a status, so the HTTP status alone
//! cannot classify a failure here: [`parse_track_response`] has to read the body
//! to reach [`TrackError::NotFound`].

use super::oauth::{ClientAuth, TokenCache};
use super::{CarrierClient, TrackError, send_json};
use crate::config::FedexCarrierConfig;
use crate::triage::{CarrierTrack, ShipmentStatus};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use std::time::Duration;

const BASE_URL: &str = "https://apis.fedex.com";

/// Floor between FedEx calls. FedEx meters per second, not per day.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Pins the language of `description`, which is stored verbatim as
/// [`CarrierTrack::carrier_status_raw`]. Without it FedEx localizes by account,
/// and a shipment's raw status would drift language mid-life.
const LOCALE: &str = "en_US";

/// Every FedEx error code for "we have never heard of this number" ends here
/// (`TRACKING.TRACKINGNUMBER.NOTFOUND`). Matched as a suffix so a renamed
/// prefix does not silently turn a dead number back into a retry loop.
const NOT_FOUND_SUFFIX: &str = "NOTFOUND";

pub struct FedexClient {
    http: reqwest::Client,
    client_id: String,
    /// Secret material, NEVER logged or included in an error.
    client_secret: String,
    token: TokenCache,
    base_url: String,
}

impl FedexClient {
    /// `None` when credentials do not fully resolve — half a pair leaves FedEx
    /// out of the registry rather than sending an empty string at FedEx's auth
    /// endpoint.
    pub fn from_config(cfg: &FedexCarrierConfig, http: reqwest::Client) -> Option<Self> {
        let (client_id, client_secret) = cfg.credentials()?;
        Some(Self {
            http,
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            token: TokenCache::new(),
            base_url: BASE_URL.to_string(),
        })
    }

    /// Point the client at a mock server. Test hook only.
    #[doc(hidden)]
    pub fn for_test(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            http,
            client_id: "test-client-id".to_string(),
            client_secret: "test-client-secret".to_string(),
            token: TokenCache::new(),
            base_url: base_url.into(),
        }
    }

    /// `path` joined onto the configured origin, tolerating a `base_url` that
    /// already ends in a slash.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// A live access token, minted only when the cache has none.
    async fn access_token(&self) -> Result<String, TrackError> {
        self.token
            .client_credentials(
                &self.http,
                &self.url("/oauth/token"),
                ClientAuth::Form {
                    client_id: &self.client_id,
                    client_secret: &self.client_secret,
                },
                &[],
            )
            .await
    }
}

#[async_trait]
impl CarrierClient for FedexClient {
    fn carrier(&self) -> &'static str {
        "fedex"
    }

    fn min_interval(&self) -> Duration {
        MIN_INTERVAL
    }

    async fn track(&self, tracking_number: &str) -> Result<CarrierTrack, TrackError> {
        let token = self.access_token().await?;
        // `includeDetailedScans: false` is the whole scan history we do NOT
        // want: the ladder is decided by `latestStatusDetail`, and the detailed
        // form returns every scan with its street address.
        let body = serde_json::json!({
            "includeDetailedScans": false,
            "trackingInfo": [{
                "trackingNumberInfo": { "trackingNumber": tracking_number }
            }],
        });
        let resp: TrackResponse = send_json(
            self.http
                .post(self.url("/track/v1/trackingnumbers"))
                .bearer_auth(token)
                .header("X-locale", LOCALE)
                .json(&body),
        )
        .await?;
        parse_track_response(&resp)
    }
}

/// FedEx's status vocabulary mapped onto our ladder.
///
/// An unmapped code is `None` ON PURPOSE. FedEx ships dozens of scan codes
/// (`HL` hold at location, `CD` customs delay, `DY` delay, `HP` ready for
/// pickup) whose lane on a four-rung ladder is a guess, and a guess here
/// overwrites an email-inferred status with fiction. The raw code is recorded
/// either way, so nothing is lost but the wrong answer.
fn map_status(derived_code: &str) -> Option<ShipmentStatus> {
    match derived_code {
        // Label created, nothing physical has happened yet.
        "OC" => Some(ShipmentStatus::Ordered),
        // Picked up / in transit / departed a facility / arrived at one: all
        // "moving", none of them the last leg.
        "PU" | "IT" | "DP" | "AR" => Some(ShipmentStatus::Shipped),
        "OD" => Some(ShipmentStatus::OutForDelivery),
        "DL" => Some(ShipmentStatus::Delivered),
        // Delivery exception / shipment exception / cancelled / return to
        // shipper — all four are "this is not arriving as planned".
        "DE" | "SE" | "CA" | "RS" => Some(ShipmentStatus::Exception),
        _ => None,
    }
}

/// The response body reduced to one [`CarrierTrack`]. Pure, so every rung of the
/// ladder and both in-200 failure shapes are testable on a canned body.
fn parse_track_response(resp: &TrackResponse) -> Result<CarrierTrack, TrackError> {
    let result = resp
        .output
        .as_ref()
        .and_then(|o| o.complete_track_results.as_deref())
        .and_then(<[CompleteTrackResult]>::first)
        .and_then(|c| c.track_results.as_deref())
        .and_then(<[TrackResult]>::first)
        // A 200 carrying no track result at all is a malformed answer, not a
        // verdict on the number: NotFound retires the shipment for good, so only
        // an explicit FedEx error gets to say it.
        .ok_or(TrackError::Transient)?;

    if let Some(err) = &result.error {
        let code = err
            .code
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_uppercase();
        return Err(if code.ends_with(NOT_FOUND_SUFFIX) {
            TrackError::NotFound
        } else {
            // Every other in-200 error (`…TRACKINGNUMBER.EMPTY`, an internal
            // fault) is our bug or theirs, and both are worth retrying.
            TrackError::Transient
        });
    }

    let latest = result.latest_status_detail.as_ref();
    // `derivedCode` is FedEx's normalized status and the one we map; `code`
    // shares the vocabulary and stands in when a result omits the derived form.
    let code = latest
        .and_then(|s| nonempty(&s.derived_code))
        .or_else(|| latest.and_then(|s| nonempty(&s.code)))
        .unwrap_or_default();
    let description = latest
        .and_then(|s| nonempty(&s.description))
        .unwrap_or_default();

    Ok(CarrierTrack {
        status: map_status(code),
        // Verbatim, both halves: the code alone is unreadable and the
        // description alone collapses distinct codes onto one string, which the
        // store reads as "nothing changed".
        carrier_status_raw: match (code, description) {
            ("", d) => d.to_string(),
            (c, "") => c.to_string(),
            (c, d) => format!("{c} {d}"),
        },
        eta: date_and_time(result, "ESTIMATED_DELIVERY").or_else(|| {
            // The window is the wider commitment; its END is the promise, so a
            // "between 9am and 1pm" window is not reported as arriving at 9.
            result
                .estimated_delivery_time_window
                .as_ref()
                .and_then(|w| w.window.as_ref())
                .and_then(|w| w.ends.as_deref())
                .and_then(parse_timestamp)
        }),
        delivered_at: date_and_time(result, "ACTUAL_DELIVERY"),
    })
}

/// The first `dateAndTimes` entry of `kind` whose stamp parses. FedEx repeats a
/// type across entries when a commitment is revised, and an entry whose stamp is
/// unreadable must not mask a later usable one.
fn date_and_time(result: &TrackResult, kind: &str) -> Option<DateTime<Utc>> {
    result
        .date_and_times
        .as_deref()?
        .iter()
        .filter(|e| e.kind.as_deref().map(str::trim) == Some(kind))
        .find_map(|e| e.date_time.as_deref().and_then(parse_timestamp))
}

/// A FedEx timestamp, or `None` — a shipment with no ETA is worth strictly more
/// than one with an invented ETA.
///
/// The stamps are ISO-8601 but inconsistently zoned even within one response:
/// FedEx's own documented sample pairs `2021-10-15T00:00:00-06:00` with a bare
/// `2007-09-27T00:00:00`, and window bounds are sometimes date-only. An unzoned
/// stamp is read as UTC, which is at worst a few hours off on a value the UI
/// renders as a day.
fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(raw) {
        return Some(at.with_timezone(&Utc));
    }
    if let Ok(at) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(Utc.from_utc_datetime(&at));
    }
    let day = NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    Some(Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0)?))
}

/// A trimmed field, or `None` when it is absent or blank — FedEx sends both
/// forms for "no value".
fn nonempty(field: &Option<String>) -> Option<&str> {
    field.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// The slice of FedEx's track response this client reads. Every field is
/// optional and every string may arrive as `null`: the response is carrier
/// input, and a missing sub-object must cost an ETA, not the whole poll.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TrackResponse {
    output: Option<TrackOutput>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TrackOutput {
    complete_track_results: Option<Vec<CompleteTrackResult>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CompleteTrackResult {
    track_results: Option<Vec<TrackResult>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TrackResult {
    /// Present INSTEAD of a status when FedEx rejects the number, inside a 200.
    error: Option<TrackResultError>,
    latest_status_detail: Option<LatestStatusDetail>,
    date_and_times: Option<Vec<DateAndTime>>,
    estimated_delivery_time_window: Option<TimeWindow>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TrackResultError {
    code: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LatestStatusDetail {
    code: Option<String>,
    derived_code: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct DateAndTime {
    #[serde(rename = "type")]
    kind: Option<String>,
    date_time: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TimeWindow {
    window: Option<Window>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Window {
    ends: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// FedEx's envelope around a single `trackResults` entry. Every fixture
    /// below is that entry, in the shape of FedEx's own published Track sample
    /// (`output.completeTrackResults[].trackResults[]`), trimmed to the fields
    /// this client reads.
    fn envelope(result: &str) -> String {
        format!(
            r#"{{
              "transactionId": "624deea6-b709-470c-8c39-4b5511281492",
              "output": {{
                "completeTrackResults": [
                  {{
                    "trackingNumber": "123456789012",
                    "trackResults": [{result}]
                  }}
                ]
              }}
            }}"#
        )
    }

    fn parse(result: &str) -> Result<CarrierTrack, TrackError> {
        let resp: TrackResponse =
            serde_json::from_str(&envelope(result)).expect("fixture is valid json");
        parse_track_response(&resp)
    }

    /// A result carrying nothing but a status, for the mapping table.
    fn status_only(code: &str, description: &str) -> String {
        format!(
            r#"{{
              "trackingNumberInfo": {{
                "trackingNumber": "123456789012",
                "carrierCode": "FDXE"
              }},
              "latestStatusDetail": {{
                "scanLocation": {{ "city": "SEATTLE", "countryCode": "US" }},
                "code": "{code}",
                "derivedCode": "{code}",
                "statusByLocale": "{description}",
                "description": "{description}"
              }}
            }}"#
        )
    }

    // ---- status vocabulary ------------------------------------------------

    #[test]
    fn documented_codes_map_to_their_rung() {
        let cases = [
            (
                "OC",
                "Shipment information sent to FedEx",
                Some(ShipmentStatus::Ordered),
            ),
            ("PU", "Picked up", Some(ShipmentStatus::Shipped)),
            ("IT", "In transit", Some(ShipmentStatus::Shipped)),
            (
                "DP",
                "Departed FedEx location",
                Some(ShipmentStatus::Shipped),
            ),
            (
                "AR",
                "Arrived at FedEx location",
                Some(ShipmentStatus::Shipped),
            ),
            (
                "OD",
                "On FedEx vehicle for delivery",
                Some(ShipmentStatus::OutForDelivery),
            ),
            ("DL", "Delivered", Some(ShipmentStatus::Delivered)),
            ("DE", "Delivery exception", Some(ShipmentStatus::Exception)),
            ("SE", "Shipment exception", Some(ShipmentStatus::Exception)),
            ("CA", "Shipment cancelled", Some(ShipmentStatus::Exception)),
            ("RS", "Return to shipper", Some(ShipmentStatus::Exception)),
        ];
        for (code, description, want) in cases {
            let track = parse(&status_only(code, description)).expect("fixture tracks");
            assert_eq!(track.status, want, "code {code}");
            assert_eq!(track.carrier_status_raw, format!("{code} {description}"));
        }
    }

    #[test]
    fn unmapped_code_leaves_the_status_alone_but_keeps_the_raw() {
        // `HL` (hold at location) is real FedEx vocabulary with no rung on our
        // ladder: the store keeps the email-inferred status and still records
        // what the carrier said.
        let track = parse(&status_only("HL", "Ready for recipient pickup")).expect("tracks");
        assert_eq!(track.status, None);
        assert_eq!(track.carrier_status_raw, "HL Ready for recipient pickup");
    }

    #[test]
    fn derived_code_wins_over_code() {
        // The two disagree while a scan is being reclassified; `derivedCode` is
        // the normalized one, so it decides both the rung and the raw string.
        let track = parse(
            r#"{
              "latestStatusDetail": {
                "code": "IT",
                "derivedCode": "OD",
                "description": "On FedEx vehicle for delivery"
              }
            }"#,
        )
        .expect("tracks");
        assert_eq!(track.status, Some(ShipmentStatus::OutForDelivery));
        assert_eq!(track.carrier_status_raw, "OD On FedEx vehicle for delivery");
    }

    #[test]
    fn code_stands_in_when_derived_is_absent() {
        let track =
            parse(r#"{ "latestStatusDetail": { "code": "DL", "description": "Delivered" } }"#)
                .expect("tracks");
        assert_eq!(track.status, Some(ShipmentStatus::Delivered));
        assert_eq!(track.carrier_status_raw, "DL Delivered");
    }

    #[test]
    fn a_result_without_any_status_is_still_a_track() {
        // No status detail at all: nothing to map, nothing to record, but the
        // poll succeeded — it must not be counted as a failure.
        let track = parse(r#"{ "trackingNumberInfo": { "trackingNumber": "123456789012" } }"#)
            .expect("tracks");
        assert_eq!(track.status, None);
        assert_eq!(track.carrier_status_raw, "");
        assert_eq!(track.eta, None);
        assert_eq!(track.delivered_at, None);
    }

    // ---- failures that ride inside a 200 -----------------------------------

    #[test]
    fn not_found_inside_a_200_is_not_found() {
        let err = parse(
            r#"{
              "trackingNumberInfo": { "trackingNumber": "123456789012" },
              "error": {
                "code": "TRACKING.TRACKINGNUMBER.NOTFOUND",
                "message": "Tracking number cannot be found. Please correct the tracking number and try again.",
                "parameterList": [{ "key": "trackingNumber", "value": "123456789012" }]
              }
            }"#,
        )
        .unwrap_err();
        assert_eq!(err, TrackError::NotFound);
    }

    #[test]
    fn other_in_200_errors_stay_transient() {
        // An empty-number rejection is our bug, not a retired shipment: it must
        // not age the row out.
        let err = parse(
            r#"{
              "error": {
                "code": "TRACKING.TRACKINGNUMBER.EMPTY",
                "message": "Please provide tracking number."
              }
            }"#,
        )
        .unwrap_err();
        assert_eq!(err, TrackError::Transient);
    }

    #[test]
    fn an_empty_result_set_is_transient_not_not_found() {
        let empty = r#"{ "transactionId": "x", "output": { "completeTrackResults": [] } }"#;
        let resp: TrackResponse = serde_json::from_str(empty).unwrap();
        assert_eq!(
            parse_track_response(&resp).unwrap_err(),
            TrackError::Transient
        );
        // Same for a body with no `output` at all.
        let resp: TrackResponse = serde_json::from_str(r#"{ "transactionId": "x" }"#).unwrap();
        assert_eq!(
            parse_track_response(&resp).unwrap_err(),
            TrackError::Transient
        );
    }

    // ---- timestamps --------------------------------------------------------

    #[test]
    fn estimated_delivery_comes_from_date_and_times() {
        let track = parse(
            r#"{
              "latestStatusDetail": { "derivedCode": "IT", "description": "In transit" },
              "dateAndTimes": [
                { "type": "ACTUAL_PICKUP", "dateTime": "2021-10-11T09:04:00-06:00" },
                { "type": "ESTIMATED_DELIVERY", "dateTime": "2021-10-15T18:00:00-06:00" }
              ],
              "estimatedDeliveryTimeWindow": {
                "window": { "begins": "2021-10-16T08:00:00-06:00", "ends": "2021-10-16T20:00:00-06:00" }
              }
            }"#,
        )
        .expect("tracks");
        // The explicit ESTIMATED_DELIVERY stamp wins over the window.
        assert_eq!(
            track.eta,
            Some("2021-10-16T00:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(track.delivered_at, None);
    }

    #[test]
    fn estimated_delivery_falls_back_to_the_window_end() {
        let track = parse(
            r#"{
              "latestStatusDetail": { "derivedCode": "IT", "description": "In transit" },
              "dateAndTimes": [{ "type": "SHIP", "dateTime": "2021-10-11T00:00:00" }],
              "estimatedDeliveryTimeWindow": {
                "description": "Description field",
                "window": { "begins": "2021-10-15T08:00:00", "ends": "2021-10-15T00:00:00-06:00" },
                "type": "ESTIMATED_DELIVERY"
              }
            }"#,
        )
        .expect("tracks");
        assert_eq!(
            track.eta,
            Some("2021-10-15T06:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn delivered_carries_the_actual_delivery_stamp() {
        let track = parse(
            r#"{
              "latestStatusDetail": { "code": "DL", "derivedCode": "DL", "description": "Delivered" },
              "dateAndTimes": [
                { "type": "ACTUAL_DELIVERY", "dateTime": "2007-09-27T00:00:00" },
                { "type": "ACTUAL_PICKUP", "dateTime": "2007-09-25T08:12:00" }
              ],
              "deliveryDetails": { "receivedByName": "Reciever" }
            }"#,
        )
        .expect("tracks");
        assert_eq!(track.status, Some(ShipmentStatus::Delivered));
        // Unzoned in FedEx's own sample; read as UTC rather than dropped.
        assert_eq!(
            track.delivered_at,
            Some("2007-09-27T00:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(track.eta, None);
    }

    #[test]
    fn unreadable_stamps_cost_the_field_not_the_poll() {
        let track = parse(
            r#"{
              "latestStatusDetail": { "derivedCode": "DL", "description": "Delivered" },
              "dateAndTimes": [
                { "type": "ACTUAL_DELIVERY", "dateTime": "yesterday" },
                { "type": "ACTUAL_DELIVERY", "dateTime": "2024-03-04T11:30:00Z" },
                { "type": "ESTIMATED_DELIVERY", "dateTime": "" }
              ]
            }"#,
        )
        .expect("tracks");
        assert_eq!(track.status, Some(ShipmentStatus::Delivered));
        // The unparseable first entry does not mask the usable second one.
        assert_eq!(
            track.delivered_at,
            Some("2024-03-04T11:30:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(track.eta, None);
    }

    #[test]
    fn timestamp_forms_fedex_actually_sends() {
        let utc = |s: &str| Some(s.parse::<DateTime<Utc>>().unwrap());
        assert_eq!(
            parse_timestamp("2021-10-15T00:00:00-06:00"),
            utc("2021-10-15T06:00:00Z")
        );
        assert_eq!(
            parse_timestamp("2007-09-27T00:00:00"),
            utc("2007-09-27T00:00:00Z")
        );
        assert_eq!(
            parse_timestamp("2024-03-04T11:30:00.250Z"),
            utc("2024-03-04T11:30:00.250Z")
        );
        // Date-only window bounds.
        assert_eq!(parse_timestamp("2025-01-18"), utc("2025-01-18T00:00:00Z"));
        assert_eq!(parse_timestamp(""), None);
        assert_eq!(parse_timestamp("   "), None);
        assert_eq!(parse_timestamp("2025-13-45"), None);
        assert_eq!(parse_timestamp("tomorrow"), None);
    }

    // ---- the wire ----------------------------------------------------------

    /// Serve `bodies` in order, one request per connection, handing back every
    /// raw request. `Connection: close` keeps each call on its own socket, so
    /// the sequence is deterministic.
    async fn mock_fedex(bodies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for body in bodies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 16384];
                let n = sock.read(&mut buf).await.unwrap();
                seen.push(String::from_utf8_lossy(&buf[..n]).to_string());
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn track_mints_one_token_and_reuses_it() {
        let token = r#"{"access_token":"tok-123","token_type":"bearer","expires_in":3600}"#;
        let delivered = envelope(&status_only("DL", "Delivered"));
        let (base, handle) = mock_fedex(vec![
            token.to_string(),
            delivered.clone(),
            delivered.clone(),
        ])
        .await;

        let client = FedexClient::for_test(&base, reqwest::Client::new());
        for _ in 0..2 {
            let track = client.track("123456789012").await.expect("tracked");
            assert_eq!(track.status, Some(ShipmentStatus::Delivered));
            assert_eq!(track.carrier_status_raw, "DL Delivered");
        }

        let seen = handle.await.unwrap();
        assert_eq!(seen.len(), 3, "one token call, two track calls");

        let token_req = seen[0].to_lowercase();
        assert!(token_req.starts_with("post /oauth/token "), "{token_req}");
        assert!(token_req.contains("grant_type=client_credentials"));
        assert!(token_req.contains("client_id=test-client-id"));
        assert!(token_req.contains("client_secret=test-client-secret"));

        for req in &seen[1..] {
            let lower = req.to_lowercase();
            assert!(
                lower.starts_with("post /track/v1/trackingnumbers "),
                "{lower}"
            );
            assert!(lower.contains("authorization: bearer tok-123"), "{lower}");
            assert!(req.contains(r#""trackingNumber":"123456789012""#), "{req}");
            assert!(
                req.contains(r#""includeDetailedScans":false"#),
                "detailed scans are never requested: {req}"
            );
        }
    }

    #[tokio::test]
    async fn a_rejected_credential_never_reaches_the_track_call() {
        // The token call is the only request the mock serves; a second one would
        // hang, proving `track` stops at the auth failure.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            // The token request's bytes are not the subject here; only that the
            // 401 stops the flow before any track call.
            let _len = sock.read(&mut buf).await.unwrap();
            let body = r#"{"errors":[{"code":"NOT.AUTHORIZED.ERROR"}]}"#;
            let resp = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });

        let client = FedexClient::for_test(format!("http://{addr}"), reqwest::Client::new());
        assert_eq!(
            client.track("123456789012").await.unwrap_err(),
            TrackError::Auth
        );
    }

    #[test]
    fn errors_carry_no_credential_or_url() {
        // The poller logs these; a FedEx 401 body echoes the client id back, so
        // nothing from the response may ride along.
        for err in [
            TrackError::NotFound,
            TrackError::Auth,
            TrackError::Transient,
        ] {
            let rendered = err.to_string();
            assert!(!rendered.contains("test-client"), "{rendered}");
            assert!(!rendered.contains("fedex.com"), "{rendered}");
        }
    }
}
