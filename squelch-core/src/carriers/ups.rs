//! UPS Tracking API client. OAuth client-credentials with the pair in an
//! `Authorization: Basic` header; the token endpoint and the tracking endpoint
//! share one origin, so a single `base_url` override redirects both in tests.
//!
//! The wire types below are a SUBSET of UPS's `TrackApiResponse` (Tracking.yaml):
//! containers the poller never reads are dropped, and everything it does read is
//! optional, because UPS omits containers rather than emptying them. A body that
//! yields no status at all is a [`TrackError`], never a guessed rung.

use super::oauth::{self, TokenCache};
use super::{CarrierClient, TrackError};
use crate::config::UpsCarrierConfig;
use crate::triage::{CarrierTrack, ShipmentStatus};
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Deserializer};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const BASE_URL: &str = "https://onlinetools.ups.com";
const TOKEN_PATH: &str = "/security/v1/oauth/token";
const TRACK_PATH: &str = "/api/track/v1/details/";

/// UPS's `transactionSrc` header: "identifies the client/source application that
/// is calling". A UPS-side label for the daemon, carrying nothing about a user.
const TRANSACTION_SRC: &str = "squelchd";

/// Floor between UPS calls. UPS meters per second, not per day.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// UPS's own bound on an inquiry number: "between 7 and 34 characters".
const NUMBER_LEN: std::ops::RangeInclusive<usize> = 7..=34;

pub struct UpsClient {
    http: reqwest::Client,
    client_id: String,
    /// Secret material, NEVER logged or included in an error.
    client_secret: String,
    token: TokenCache,
    base_url: String,
}

impl UpsClient {
    /// `None` when credentials do not fully resolve — half a pair leaves UPS
    /// out of the registry rather than sending an empty string at UPS's auth
    /// endpoint.
    pub fn from_config(cfg: &UpsCarrierConfig, http: reqwest::Client) -> Option<Self> {
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

    /// A live access token via the shared client-credentials flow. UPS types
    /// every token-response field as a JSON string, `expires_in` included
    /// (`"14399"`); [`oauth::TokenResponse`] parses both spellings.
    async fn access_token(&self) -> Result<String, TrackError> {
        self.token
            .client_credentials(
                &self.http,
                &format!("{}{TOKEN_PATH}", self.base_url.trim_end_matches('/')),
                oauth::ClientAuth::Basic {
                    client_id: &self.client_id,
                    client_secret: &self.client_secret,
                },
                &[],
            )
            .await
    }
}

#[async_trait]
impl CarrierClient for UpsClient {
    fn carrier(&self) -> &'static str {
        "ups"
    }

    fn min_interval(&self) -> Duration {
        MIN_INTERVAL
    }

    async fn track(&self, tracking_number: &str) -> Result<CarrierTrack, TrackError> {
        // The number is spliced into the PATH, so a stored value that is not
        // the shape UPS issues is refused here rather than escaped and sent: a
        // `/` or a `?` in it would otherwise rewrite the request. UPS could
        // never resolve such a number either, which is what NotFound means.
        if !is_wellformed(tracking_number) {
            return Err(TrackError::NotFound);
        }
        let base = self.base_url.trim_end_matches('/');
        let token = self.access_token().await?;
        let req = self
            .http
            .get(format!("{base}{TRACK_PATH}{tracking_number}"))
            .bearer_auth(token)
            .header("transId", trans_id())
            .header("transactionSrc", TRANSACTION_SRC);
        let body: TrackApiResponse = super::send_json(req).await?;
        parse_track(&body, tracking_number)
    }
}

/// Is this the shape UPS issues — alphanumeric, within UPS's own length bound?
fn is_wellformed(tracking_number: &str) -> bool {
    NUMBER_LEN.contains(&tracking_number.len())
        && tracking_number.chars().all(|c| c.is_ascii_alphanumeric())
}

/// A per-request value for UPS's `transId` header ("an identifier unique to the
/// request"). Unique within a process by counter and across processes by a
/// random prefix, so a restart cannot replay an identifier UPS has already seen.
fn trans_id() -> String {
    static PREFIX: OnceLock<String> = OnceLock::new();
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let prefix = PREFIX.get_or_init(|| {
        let mut seed = [0u8; 8];
        // An entropy failure costs cross-process uniqueness, not correctness,
        // so it degrades to a fixed prefix rather than failing the poll.
        let _ = getrandom::fill(&mut seed);
        format!("{:016x}", u64::from_be_bytes(seed))
    });
    format!("{prefix}{:016x}", SEQ.fetch_add(1, Ordering::Relaxed))
}

// ---- wire types ----------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackApiResponse {
    #[serde(default)]
    track_response: Option<TrackResponse>,
    /// UPS's error envelope. Documented for the 4xx/5xx bodies, which
    /// [`super::check_status`] classifies before the body is ever read; it is
    /// modelled here for the 200s that carry it instead of a shipment.
    #[serde(default)]
    response: Option<ErrorEnvelope>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope {
    #[serde(default, deserialize_with = "nullable_vec")]
    errors: Vec<Diagnostic>,
}

/// UPS's `{code, message}` pair, the same shape for an error and a warning.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Diagnostic {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackResponse {
    #[serde(default, deserialize_with = "nullable_vec")]
    shipment: Vec<Shipment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Shipment {
    /// Absent on a number UPS cannot resolve: it answers 200 with a
    /// shipment-level warning and no packages at all.
    #[serde(default, deserialize_with = "nullable_vec")]
    package: Vec<Package>,
    #[serde(default, deserialize_with = "nullable_vec")]
    warnings: Vec<Diagnostic>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Package {
    #[serde(default)]
    tracking_number: Option<String>,
    #[serde(default)]
    current_status: Option<Status>,
    /// Newest activity FIRST, per UPS's schema.
    #[serde(default, deserialize_with = "nullable_vec")]
    activity: Vec<Activity>,
    /// `SDD` scheduled / `RDD` rescheduled / `DEL` delivered — one entry each.
    #[serde(default, deserialize_with = "nullable_vec")]
    delivery_date: Vec<DeliveryDate>,
    #[serde(default)]
    delivery_time: Option<DeliveryTime>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    /// The granular activity code (`MP`, `F4`, `SR`). Some responses carry the
    /// broad type here instead of in `kind`.
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    simplified_text_description: Option<String>,
    /// UPS's broad status type — the vocabulary the ladder is mapped from.
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Activity {
    #[serde(default)]
    status: Option<Status>,
    /// Local to the scan. Format `YYYYMMDD`.
    #[serde(default)]
    date: Option<String>,
    /// Local to the scan. Format `HHMMSS`.
    #[serde(default)]
    time: Option<String>,
    #[serde(default)]
    gmt_date: Option<String>,
    #[serde(default)]
    gmt_time: Option<String>,
    /// The local time's offset from GMT, `±HH:MM`.
    #[serde(default)]
    gmt_offset: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryDate {
    #[serde(default)]
    date: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryTime {
    /// `EOD`/`CMT`/`EDW`/`CDW`/`IDW` are estimates; `DEL` is the delivered time.
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// The close of the quoted window, or the delivered time when `DEL`.
    #[serde(default)]
    end_time: Option<String>,
}

/// `null` where UPS's schema says array. Absent and empty mean the same thing to
/// every reader below, and a `null` should not be the difference between a
/// parsed body and a transient failure.
fn nullable_vec<'de, D, T>(de: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(de)?.unwrap_or_default())
}

// ---- parsing -------------------------------------------------------------

/// UPS's status vocabulary onto our ladder. Unmapped vocabulary yields `None`:
/// a status we do not model is recorded raw and left alone, never rounded to
/// the nearest rung.
fn status_from_code(code: &str) -> Option<ShipmentStatus> {
    match code.trim().to_ascii_uppercase().as_str() {
        // Billing information received / label created — UPS does not hold the
        // package yet, which is our "ordered".
        "M" | "MP" => Some(ShipmentStatus::Ordered),
        // In transit, and the origin pickup scan: both mean moving.
        "I" | "P" => Some(ShipmentStatus::Shipped),
        "O" => Some(ShipmentStatus::OutForDelivery),
        "D" => Some(ShipmentStatus::Delivered),
        // `X` is UPS's whole exception family; `RS` is a return to sender.
        "X" | "RS" => Some(ShipmentStatus::Exception),
        _ => None,
    }
}

/// Case- and whitespace-insensitive compare for UPS's short discriminators,
/// which arrive space-padded in real responses.
fn is_kind(field: Option<&str>, kind: &str) -> bool {
    field.is_some_and(|f| f.trim().eq_ignore_ascii_case(kind))
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// The token the ladder is mapped from: the broad `type` when UPS sends one,
/// else `code`, which some responses use for the same letter.
fn status_token(s: &Status) -> Option<&str> {
    non_empty(s.kind.as_deref()).or_else(|| non_empty(s.code.as_deref()))
}

/// What the carrier said, for the client to show verbatim: the code and the
/// description as UPS worded them, joined. Only surrounding whitespace is
/// touched — UPS pads its descriptions ("DELIVERED ").
fn status_raw(s: &Status) -> String {
    let code = non_empty(s.code.as_deref()).or_else(|| non_empty(s.kind.as_deref()));
    let text = non_empty(s.description.as_deref())
        .or_else(|| non_empty(s.simplified_text_description.as_deref()));
    match (code, text) {
        (Some(code), Some(text)) => format!("{code} {text}"),
        (Some(only), None) | (None, Some(only)) => only.to_string(),
        (None, None) => String::new(),
    }
}

/// Every package in the body, across shipments.
fn packages(resp: &TrackApiResponse) -> impl Iterator<Item = &Package> {
    resp.track_response
        .iter()
        .flat_map(|t| t.shipment.iter())
        .flat_map(|s| s.package.iter())
}

/// The package this poll asked about. UPS answers a single-number inquiry with
/// one package, but a multi-piece shipment can hand back siblings, so the
/// requested number wins over position.
fn select_package<'a>(resp: &'a TrackApiResponse, tracking_number: &str) -> Option<&'a Package> {
    packages(resp)
        .find(|p| {
            p.tracking_number
                .as_deref()
                .is_some_and(|n| n.trim().eq_ignore_ascii_case(tracking_number))
        })
        .or_else(|| packages(resp).next())
}

/// The status container: the package's own `currentStatus`, else the newest
/// activity's — some responses carry only the activity trail.
fn current_status(pkg: &Package) -> Option<&Status> {
    pkg.current_status
        .as_ref()
        .or_else(|| pkg.activity.first().and_then(|a| a.status.as_ref()))
}

/// `YYYYMMDD` -> a date. Separators are tolerated; anything that is not exactly
/// eight digits after that is refused rather than guessed at.
fn parse_date(raw: &str) -> Option<NaiveDate> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 8 {
        return None;
    }
    NaiveDate::from_ymd_opt(
        digits[0..4].parse().ok()?,
        digits[4..6].parse().ok()?,
        digits[6..8].parse().ok()?,
    )
}

/// `HHMMSS` (24h) -> a time. UPS drops the leading zero on a single-digit hour
/// (`74700` is 07:47:00), so five digits are padded; any other length is refused,
/// since `1630` could be 16:30:00 or 00:16:30 and neither is worth inventing.
fn parse_time(raw: &str) -> Option<NaiveTime> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    let padded = match digits.len() {
        6 => digits,
        5 => format!("0{digits}"),
        _ => return None,
    };
    NaiveTime::from_hms_opt(
        padded[0..2].parse().ok()?,
        padded[2..4].parse().ok()?,
        padded[4..6].parse().ok()?,
    )
}

/// A local date/time as an instant. The offset is what makes it exact; without
/// one the local time is taken AS UTC, which is within a day of the truth and
/// is all UPS gave us.
fn local_to_utc(date: NaiveDate, time: NaiveTime, offset: Option<&str>) -> DateTime<Utc> {
    let local = date.and_time(time);
    offset
        .and_then(|o| o.trim().parse::<FixedOffset>().ok())
        .and_then(|o| local.and_local_timezone(o).single())
        .map_or_else(|| local.and_utc(), |at| at.with_timezone(&Utc))
}

/// One scan's instant. UPS reports the same event three ways: `gmtDate`/`gmtTime`
/// (already GMT), the local `date`/`time`, and `gmtOffset` to convert between
/// them. GMT is preferred, then local plus offset, then local alone.
fn activity_at(a: &Activity) -> Option<DateTime<Utc>> {
    if let (Some(date), Some(time)) = (
        non_empty(a.gmt_date.as_deref()).and_then(parse_date),
        non_empty(a.gmt_time.as_deref()).and_then(parse_time),
    ) {
        return Some(date.and_time(time).and_utc());
    }
    let date = non_empty(a.date.as_deref()).and_then(parse_date)?;
    // A scan with no time still pins a day; midnight is the marker, not a claim.
    let time = non_empty(a.time.as_deref())
        .and_then(parse_time)
        .unwrap_or(NaiveTime::MIN);
    Some(local_to_utc(date, time, non_empty(a.gmt_offset.as_deref())))
}

/// The `date` of the first delivery-date entry of this kind that parses.
fn delivery_date(pkg: &Package, kind: &str) -> Option<NaiveDate> {
    pkg.delivery_date
        .iter()
        .filter(|d| is_kind(d.kind.as_deref(), kind))
        .filter_map(|d| non_empty(d.date.as_deref()))
        .find_map(parse_date)
}

/// The carrier's estimate: the rescheduled date when UPS moved it, else the
/// scheduled one. `DEL` is deliberately NOT an estimate — it is the date the
/// package landed, and it belongs to [`delivered_at`].
fn eta(pkg: &Package) -> Option<DateTime<Utc>> {
    let date = delivery_date(pkg, "RDD").or_else(|| delivery_date(pkg, "SDD"))?;
    // An estimate is read at its LATEST instant: the close of the window UPS
    // quoted, or end of day when it quoted only a date (the convention
    // `triage::deadline` already uses for a date-only commitment).
    let time = pkg
        .delivery_time
        .as_ref()
        .filter(|dt| !is_kind(dt.kind.as_deref(), "DEL"))
        .and_then(|dt| non_empty(dt.end_time.as_deref()))
        .and_then(parse_time)
        .unwrap_or_else(|| NaiveTime::from_hms_opt(23, 59, 59).expect("valid end-of-day"));
    Some(date.and_time(time).and_utc())
}

/// When the package landed: the delivery SCAN's own timestamp, the only place
/// UPS reports a time of day for it. A response carrying the summary but not
/// the trail falls back to the `DEL` date (and `DEL` time, when there is one).
fn delivered_at(pkg: &Package) -> Option<DateTime<Utc>> {
    let scan = pkg
        .activity
        .iter()
        .find(|a| {
            a.status
                .as_ref()
                .is_some_and(|s| is_kind(s.kind.as_deref(), "D"))
        })
        .and_then(activity_at);
    if scan.is_some() {
        return scan;
    }
    let date = delivery_date(pkg, "DEL")?;
    let time = pkg
        .delivery_time
        .as_ref()
        .filter(|dt| is_kind(dt.kind.as_deref(), "DEL"))
        .and_then(|dt| non_empty(dt.end_time.as_deref()))
        .and_then(parse_time)
        .unwrap_or(NaiveTime::MIN);
    Some(date.and_time(time).and_utc())
}

/// Does this diagnostic say UPS has no such number? UPS answers an unknown
/// inquiry with HTTP 200 and the warning `TW0001` "Tracking Information Not
/// Found", which is the one case worth aging a shipment out over.
fn says_not_found(d: &Diagnostic) -> bool {
    if is_kind(d.code.as_deref(), "TW0001") {
        return true;
    }
    d.message.as_deref().is_some_and(|m| {
        let m = m.to_ascii_lowercase();
        m.contains("not found") || m.contains("no tracking information")
    })
}

/// Why a 200 carried no package. An unresolvable number is UPS's usual reason
/// (a shipment-level warning, or an empty shipment array saying it more
/// quietly), and it is the only one the poller should stop retrying: a body
/// that declines for some OTHER stated reason is counted as a failure instead.
/// Every HTTP-level failure was already classified by [`super::check_status`],
/// so only UPS's soft refusals reach here.
fn no_package_reason(resp: &TrackApiResponse) -> TrackError {
    let diagnostics: Vec<&Diagnostic> = resp
        .response
        .iter()
        .flat_map(|r| r.errors.iter())
        .chain(
            resp.track_response
                .iter()
                .flat_map(|t| t.shipment.iter())
                .flat_map(|s| s.warnings.iter()),
        )
        .collect();
    if diagnostics.is_empty() || diagnostics.iter().copied().any(says_not_found) {
        TrackError::NotFound
    } else {
        TrackError::Transient
    }
}

/// One tracking body onto a [`CarrierTrack`]. Pure, so every status rung and
/// every malformed shape is a fixture rather than a live UPS call.
fn parse_track(resp: &TrackApiResponse, tracking_number: &str) -> Result<CarrierTrack, TrackError> {
    let Some(pkg) = select_package(resp, tracking_number) else {
        return Err(no_package_reason(resp));
    };
    let carrier_status_raw = current_status(pkg).map(status_raw).unwrap_or_default();
    // A package with no status text at all is a body we do not understand.
    // Reporting it would overwrite the stored `carrier_status_raw` with an
    // empty string, so it counts as a failure and the poller retries.
    if carrier_status_raw.is_empty() {
        return Err(TrackError::Transient);
    }
    Ok(CarrierTrack {
        status: current_status(pkg)
            .and_then(status_token)
            .and_then(status_from_code),
        carrier_status_raw,
        eta: eta(pkg),
        delivered_at: delivered_at(pkg),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// One package, wrapped in the envelope UPS answers a single-number inquiry
    /// with. Every fixture below is that envelope around a real package body.
    fn envelope(package: Value) -> Value {
        json!({
            "trackResponse": {
                "shipment": [{
                    "inquiryNumber": "1Z1442YY7229014688",
                    "package": [package]
                }]
            }
        })
    }

    fn track(body: &Value) -> Result<CarrierTrack, TrackError> {
        let resp: TrackApiResponse =
            serde_json::from_value(body.clone()).expect("fixture is a UPS body");
        parse_track(&resp, "1Z1442YY7229014688")
    }

    fn ok(body: &Value) -> CarrierTrack {
        track(body).expect("fixture yields a track")
    }

    // ---- status rungs -----------------------------------------------------

    #[test]
    fn label_created_is_ordered() {
        // Shape and wording from UPS's own tracking sample: type `M` with the
        // granular `MP` code, before UPS has the package.
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": {
                "type": "M",
                "code": "MP",
                "description": "Shipper created a label, UPS has not received the package yet. ",
                "statusCode": "003"
            },
            "activity": [{
                "status": {
                    "type": "M",
                    "code": "MP",
                    "description": "Shipper created a label, UPS has not received the package yet. "
                },
                "date": "20220126",
                "time": "151641"
            }]
        })));
        assert_eq!(t.status, Some(ShipmentStatus::Ordered));
        assert_eq!(
            t.carrier_status_raw,
            "MP Shipper created a label, UPS has not received the package yet."
        );
        assert_eq!(t.delivered_at, None);
    }

    #[test]
    fn in_transit_is_shipped() {
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": {
                "type": "I",
                "code": "DP",
                "description": "Departed from Facility",
                "simplifiedTextDescription": "On the Way"
            }
        })));
        assert_eq!(t.status, Some(ShipmentStatus::Shipped));
        assert_eq!(t.carrier_status_raw, "DP Departed from Facility");
    }

    #[test]
    fn out_for_delivery_is_its_own_rung() {
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": {
                "type": "O",
                "description": "Out For Delivery Today",
                "simplifiedTextDescription": "Out for Delivery"
            }
        })));
        assert_eq!(t.status, Some(ShipmentStatus::OutForDelivery));
        // No `code`: the type stands in, so the client still shows a token.
        assert_eq!(t.carrier_status_raw, "O Out For Delivery Today");
    }

    #[test]
    fn delivered_is_delivered() {
        let t = ok(&delivered_fixture());
        assert_eq!(t.status, Some(ShipmentStatus::Delivered));
        assert_eq!(t.carrier_status_raw, "011 DELIVERED");
    }

    #[test]
    fn exception_type_is_the_exception_rung() {
        // UPS's documented Status example. `X` covers its whole exception
        // family, customs events included.
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": {
                "type": "X",
                "code": "SR",
                "description": "Your package was released by the customs agency.",
                "statusCode": "003"
            }
        })));
        assert_eq!(t.status, Some(ShipmentStatus::Exception));
        assert_eq!(
            t.carrier_status_raw,
            "SR Your package was released by the customs agency."
        );
    }

    #[test]
    fn every_mapped_token_and_nothing_else() {
        for (token, want) in [
            ("M", ShipmentStatus::Ordered),
            ("MP", ShipmentStatus::Ordered),
            ("I", ShipmentStatus::Shipped),
            ("P", ShipmentStatus::Shipped),
            ("O", ShipmentStatus::OutForDelivery),
            ("D", ShipmentStatus::Delivered),
            ("X", ShipmentStatus::Exception),
            ("RS", ShipmentStatus::Exception),
        ] {
            assert_eq!(status_from_code(token), Some(want), "token: {token}");
            // Real bodies pad and lower-case these.
            assert_eq!(
                status_from_code(&format!(" {} ", token.to_lowercase())),
                Some(want)
            );
        }
        for token in ["", "MV", "NA", "W", "011", "OF", "?"] {
            assert_eq!(status_from_code(token), None, "token: {token}");
        }
    }

    // ---- vocabulary we do not model ---------------------------------------

    #[test]
    fn unmapped_status_keeps_the_raw_and_maps_to_nothing() {
        // A voided label: real UPS vocabulary, no rung of ours. The status is
        // left alone and the carrier's own words are still recorded.
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": {
                "type": "MV",
                "code": "VD",
                "description": "The shipper voided this label."
            }
        })));
        assert_eq!(t.status, None, "unmodelled vocabulary is never guessed");
        assert_eq!(t.carrier_status_raw, "VD The shipper voided this label.");
    }

    #[test]
    fn the_type_wins_over_the_code_for_the_ladder() {
        // `code` carries a granular out-for-delivery scan, `type` the broad
        // "in transit". The broad type is the vocabulary we map.
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": { "type": "I", "code": "OF", "description": "Out For Delivery" }
        })));
        assert_eq!(t.status, Some(ShipmentStatus::Shipped));
    }

    #[test]
    fn a_code_only_status_still_maps() {
        // Some responses carry the letter in `code` and send no `type`.
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z999AA10123456784",
            "currentStatus": { "code": "D", "description": "Delivered", "statusCode": "011" }
        })));
        assert_eq!(t.status, Some(ShipmentStatus::Delivered));
        assert_eq!(t.carrier_status_raw, "D Delivered");
    }

    #[test]
    fn the_newest_activity_stands_in_for_a_missing_current_status() {
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "activity": [
                { "status": { "type": "I", "code": "AR", "description": "Arrived at Facility" },
                  "date": "20220125", "time": "031500" },
                { "status": { "type": "M", "code": "MP", "description": "Label created" },
                  "date": "20220124", "time": "151641" }
            ]
        })));
        assert_eq!(t.status, Some(ShipmentStatus::Shipped));
        assert_eq!(t.carrier_status_raw, "AR Arrived at Facility");
    }

    // ---- eta ---------------------------------------------------------------

    #[test]
    fn eta_is_the_scheduled_date_at_the_close_of_the_quoted_window() {
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": { "type": "I", "code": "DP", "description": "Departed from Facility" },
            "deliveryDate": [{ "type": "SDD", "date": "20260515" }],
            "deliveryTime": { "type": "EDW", "startTime": "090000", "endTime": "210000" }
        })));
        assert_eq!(
            t.eta,
            Some("2026-05-15T21:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(t.delivered_at, None);
    }

    #[test]
    fn a_rescheduled_date_supersedes_the_scheduled_one() {
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": { "type": "X", "code": "EX", "description": "Delivery rescheduled" },
            "deliveryDate": [
                { "type": "SDD", "date": "20260515" },
                { "type": "RDD", "date": "20260518" }
            ]
        })));
        // No window quoted: end of day, the latest instant the estimate promises.
        assert_eq!(
            t.eta,
            Some("2026-05-18T23:59:59Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn a_delivered_date_is_not_an_eta() {
        let t = ok(&delivered_fixture());
        assert_eq!(t.eta, None, "DEL is when it landed, not an estimate");
    }

    #[test]
    fn an_unparseable_date_yields_no_eta() {
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": { "type": "I", "description": "In Transit" },
            "deliveryDate": [{ "type": "SDD", "date": "soon" }]
        })));
        assert_eq!(t.eta, None);
    }

    // ---- delivered_at ------------------------------------------------------

    #[test]
    fn delivered_at_comes_from_the_delivery_scan() {
        let t = ok(&delivered_fixture());
        assert_eq!(
            t.delivered_at,
            Some("2022-01-26T16:30:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn a_scan_offset_moves_the_delivery_to_utc() {
        // Local 16:30 at -05:00 is 21:30Z. UPS sends the offset, so the
        // instant is exact rather than approximate.
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": { "type": "D", "code": "011", "description": "Delivered" },
            "activity": [{
                "status": { "type": "D", "code": "F4", "description": "DELIVERED " },
                "date": "20220126", "time": "163000", "gmtOffset": "-05:00"
            }]
        })));
        assert_eq!(
            t.delivered_at,
            Some("2022-01-26T21:30:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn a_gmt_stamp_is_taken_as_gmt_and_survives_a_dropped_leading_zero() {
        // `gmtTime` "74700" is 07:47:00 GMT — UPS's own documented example.
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": { "type": "D", "code": "011", "description": "Delivered" },
            "activity": [{
                "status": { "type": "D", "code": "F4", "description": "DELIVERED " },
                "date": "20210210", "time": "024700",
                "gmtDate": "20210210", "gmtTime": "74700", "gmtOffset": "-05:00"
            }]
        })));
        assert_eq!(
            t.delivered_at,
            Some("2021-02-10T07:47:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn delivered_at_falls_back_to_the_del_date_and_time() {
        // The summary without the trail: still enough to say when it landed.
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": { "type": "D", "code": "011", "description": "Delivered" },
            "deliveryDate": [{ "type": "DEL", "date": "20220126" }],
            "deliveryTime": { "type": "DEL", "endTime": "163000" }
        })));
        assert_eq!(
            t.delivered_at,
            Some("2022-01-26T16:30:00Z".parse::<DateTime<Utc>>().unwrap())
        );
    }

    #[test]
    fn an_undelivered_package_has_no_delivery_time() {
        let t = ok(&envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": { "type": "O", "description": "Out For Delivery Today" },
            "activity": [{
                "status": { "type": "I", "code": "OF", "description": "Out For Delivery" },
                "date": "20260515", "time": "083200"
            }]
        })));
        assert_eq!(t.delivered_at, None);
    }

    // ---- numbers UPS does not know ----------------------------------------

    #[test]
    fn a_200_with_a_not_found_warning_is_not_found() {
        // Verbatim shape of what UPS answers for an unknown number: 200, a
        // shipment-level warning, and no package at all.
        let body = json!({
            "trackResponse": {
                "shipment": [{
                    "inquiryNumber": "1Z5629230309413682xxxx",
                    "warnings": [{ "code": "TW0001", "message": "Tracking Information Not Found" }]
                }]
            }
        });
        assert_eq!(track(&body), Err(TrackError::NotFound));
    }

    #[test]
    fn an_errors_envelope_is_not_found() {
        // The `Response`/`ErrorResponse` envelope, carrying no tracking data.
        let body = json!({
            "response": {
                "errors": [{ "code": "TJ001", "message": "No tracking information available" }]
            }
        });
        assert_eq!(track(&body), Err(TrackError::NotFound));
    }

    #[test]
    fn a_200_declining_for_another_reason_stays_transient() {
        // A package-less 200 that does NOT say "unknown number" is a failure to
        // retry, not a shipment to age out.
        let body = json!({
            "trackResponse": {
                "shipment": [{
                    "inquiryNumber": "1Z1442YY7229014688",
                    "warnings": [{ "code": "TW9999",
                                   "message": "Tracking is temporarily unavailable" }]
                }]
            }
        });
        assert_eq!(track(&body), Err(TrackError::Transient));
    }

    #[test]
    fn an_empty_shipment_array_is_not_found() {
        assert_eq!(
            track(&json!({ "trackResponse": { "shipment": [] } })),
            Err(TrackError::NotFound)
        );
    }

    #[test]
    fn a_null_array_parses_as_an_empty_one() {
        // A `null` where the schema says array must not cost a whole poll.
        let body = json!({ "trackResponse": { "shipment": null } });
        assert_eq!(track(&body), Err(TrackError::NotFound));
    }

    #[test]
    fn a_package_with_no_status_is_transient_not_a_blank_status() {
        // Reporting this would overwrite the stored raw status with "".
        let body = envelope(json!({ "trackingNumber": "1Z1442YY7229014688" }));
        assert_eq!(track(&body), Err(TrackError::Transient));
    }

    // ---- the requested package ---------------------------------------------

    #[test]
    fn the_requested_number_wins_over_position() {
        let body = json!({
            "trackResponse": {
                "shipment": [{
                    "inquiryNumber": "1Z1442YY7229014688",
                    "package": [
                        { "trackingNumber": "1Z1442YY7229099999",
                          "currentStatus": { "type": "M", "code": "MP", "description": "Label created" } },
                        { "trackingNumber": "1Z1442YY7229014688",
                          "currentStatus": { "type": "D", "code": "011", "description": "Delivered" } }
                    ]
                }]
            }
        });
        let t = track(&body).expect("the asked-for package");
        assert_eq!(t.status, Some(ShipmentStatus::Delivered));
    }

    #[test]
    fn a_package_without_a_number_is_still_the_answer() {
        // A single-package inquiry answered without echoing the number back.
        let t = ok(&envelope(json!({
            "currentStatus": { "type": "D", "code": "011", "description": "Delivered" }
        })));
        assert_eq!(t.status, Some(ShipmentStatus::Delivered));
    }

    // ---- date/time parsing -------------------------------------------------

    #[test]
    fn compact_dates_and_times_parse_or_refuse() {
        assert_eq!(parse_date("20220126"), NaiveDate::from_ymd_opt(2022, 1, 26));
        assert_eq!(
            parse_date("2022-01-26"),
            NaiveDate::from_ymd_opt(2022, 1, 26)
        );
        for bad in ["", "2022", "20221301", "20220132", "tomorrow", "202201260"] {
            assert_eq!(parse_date(bad), None, "date: {bad:?}");
        }
        assert_eq!(parse_time("163000"), NaiveTime::from_hms_opt(16, 30, 0));
        assert_eq!(parse_time("74700"), NaiveTime::from_hms_opt(7, 47, 0));
        assert_eq!(parse_time("03:53:12"), NaiveTime::from_hms_opt(3, 53, 12));
        // 4 digits is ambiguous, 256000 is not a time, and neither is a word.
        for bad in ["", "1630", "256000", "noon", "1234567"] {
            assert_eq!(parse_time(bad), None, "time: {bad:?}");
        }
    }

    // ---- the number never reaches the path unchecked ------------------------

    #[tokio::test]
    async fn a_number_that_could_rewrite_the_path_is_refused_before_the_call() {
        // No server is running: reaching the network at all would be the bug.
        let client = UpsClient::for_test("http://127.0.0.1:1", reqwest::Client::new());
        for number in [
            "../../security/v1/oauth/token",
            "1Z999AA1?locale=xx",
            "1Z999AA1/details",
            "short",
            &"1".repeat(35),
        ] {
            assert_eq!(
                client.track(number).await,
                Err(TrackError::NotFound),
                "{number}"
            );
        }
    }

    // ---- the token response ------------------------------------------------

    #[test]
    fn a_string_expiry_is_read_as_seconds() {
        // UPS's documented shape: every value a string, `expires_in` included.
        // The shared token type must read it, or every UPS mint fails.
        let ups = json!({
            "token_type": "Bearer",
            "issued_at": "1662523536426",
            "client_id": "test-client-id",
            "access_token": "ups-test-token",
            "scope": "",
            "expires_in": "14399",
            "refresh_count": "0",
            "status": "approved"
        });
        let parsed: oauth::TokenResponse = serde_json::from_value(ups).expect("UPS token body");
        assert_eq!(parsed.access_token, "ups-test-token");
        assert_eq!(parsed.expires_in, Some(14399));

        // A numeric or missing expiry costs nothing either way.
        let numeric: oauth::TokenResponse =
            serde_json::from_value(json!({ "access_token": "t", "expires_in": 3600 })).unwrap();
        assert_eq!(numeric.expires_in, Some(3600));
        let bare: oauth::TokenResponse =
            serde_json::from_value(json!({ "access_token": "t" })).unwrap();
        assert!(bare.expires_in.is_none());
        let junk: oauth::TokenResponse =
            serde_json::from_value(json!({ "access_token": "t", "expires_in": "soon" })).unwrap();
        assert_eq!(junk.expires_in, None);
    }

    #[test]
    fn trans_ids_do_not_repeat() {
        let ids: Vec<String> = (0..64).map(|_| trans_id()).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "transId must be per-request");
    }

    /// A delivered package as UPS returns one: `DEL` delivery date, a `DEL`
    /// delivery time, and the delivery scan at the head of the activity trail.
    fn delivered_fixture() -> Value {
        envelope(json!({
            "trackingNumber": "1Z1442YY7229014688",
            "currentStatus": {
                "code": "011",
                "description": "DELIVERED ",
                "simplifiedTextDescription": "Delivered",
                "statusCode": "011",
                "type": "D"
            },
            "activity": [
                {
                    "location": { "address": { "city": "PARAMUS", "stateProvince": "NJ",
                                               "postalCode": "07652", "countryCode": "US" },
                                  "slic": "0761" },
                    "status": { "type": "D", "description": "DELIVERED ", "code": "F4",
                                "statusCode": "011", "simplifiedTextDescription": "" },
                    "date": "20220126", "time": "163000"
                },
                {
                    "status": { "type": "M", "description": "Label created", "code": "MP" },
                    "date": "20220124", "time": "151641"
                }
            ],
            "deliveryDate": [{ "type": "DEL", "date": "20220126" }],
            "deliveryTime": { "type": "DEL", "endTime": "163000", "startTime": "163000" }
        }))
    }

    // ---- on the wire --------------------------------------------------------
    //
    // The Basic-vs-Bearer hand-off, the two required headers and the path the
    // number lands in are the parts a fixture cannot check, and each is a
    // silent 401 or 400 in production if it is wrong.

    use axum::extract::Path;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use std::sync::{Arc, Mutex};

    /// What the mock saw, so the assertions are on bytes rather than intent.
    #[derive(Default)]
    struct Seen {
        token_auth: Vec<String>,
        token_bodies: Vec<String>,
        numbers: Vec<String>,
        track_auth: Vec<String>,
        trans_ids: Vec<String>,
        trans_srcs: Vec<String>,
    }

    fn header(headers: &HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    async fn ups_mock(seen: Arc<Mutex<Seen>>) -> String {
        let token_state = seen.clone();
        let app = Router::new()
            .route(
                TOKEN_PATH,
                post(move |headers: HeaderMap, body: String| {
                    let seen = token_state.clone();
                    async move {
                        {
                            let mut s = seen.lock().unwrap();
                            s.token_auth.push(header(&headers, "authorization"));
                            s.token_bodies.push(body);
                        }
                        Json(json!({
                            "access_token": "ups-test-token",
                            "token_type": "Bearer",
                            "expires_in": "14399"
                        }))
                    }
                }),
            )
            .route(
                &format!("{TRACK_PATH}{{number}}"),
                get(move |Path(number): Path<String>, headers: HeaderMap| {
                    let seen = seen.clone();
                    async move {
                        {
                            let mut s = seen.lock().unwrap();
                            s.numbers.push(number);
                            s.track_auth.push(header(&headers, "authorization"));
                            s.trans_ids.push(header(&headers, "transId"));
                            s.trans_srcs.push(header(&headers, "transactionSrc"));
                        }
                        Json(delivered_fixture())
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn track_mints_one_token_and_reads_the_details_endpoint() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let base = ups_mock(seen.clone()).await;
        let client = UpsClient::for_test(base, reqwest::Client::new());
        assert_eq!(client.carrier(), "ups");

        for _ in 0..2 {
            let t = client.track("1Z1442YY7229014688").await.unwrap();
            assert_eq!(t.status, Some(ShipmentStatus::Delivered));
            assert_eq!(
                t.delivered_at,
                Some("2022-01-26T16:30:00Z".parse::<DateTime<Utc>>().unwrap())
            );
        }

        let s = seen.lock().unwrap();
        // One token for two polls: the cache holds it across the pass.
        assert_eq!(s.token_auth.len(), 1);
        let expect = STANDARD.encode("test-client-id:test-client-secret");
        assert_eq!(s.token_auth[0], format!("Basic {expect}"));
        assert_eq!(s.token_bodies[0], "grant_type=client_credentials");

        assert_eq!(s.numbers, ["1Z1442YY7229014688", "1Z1442YY7229014688"]);
        assert!(s.track_auth.iter().all(|a| a == "Bearer ups-test-token"));
        assert!(s.trans_srcs.iter().all(|src| src == TRANSACTION_SRC));
        assert!(s.trans_ids.iter().all(|id| !id.is_empty()));
        assert_ne!(s.trans_ids[0], s.trans_ids[1], "transId is per-request");
    }

    #[tokio::test]
    async fn a_rejected_credential_is_an_auth_failure_carrying_nothing() {
        let app = Router::new().route(
            TOKEN_PATH,
            post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    // UPS echoes the client id back in this body.
                    Json(json!({ "response": { "errors": [{
                        "code": "10401",
                        "message": "Invalid Client Id: test-client-id"
                    }]}})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = UpsClient::for_test(format!("http://{addr}"), reqwest::Client::new());
        let err = client.track("1Z1442YY7229014688").await.unwrap_err();
        assert_eq!(err, TrackError::Auth);
        assert!(
            !err.to_string().contains("test-client-id"),
            "no credential in an error string"
        );
    }
}
