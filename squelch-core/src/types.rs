//! Core domain types. Types that cross the MCP boundary derive serde.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type AccountId = i64;

/// MCP-visible triage tier. There is deliberately NO `Sealed` variant here:
/// sealed messages are excluded structurally, never surfaced as a tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    PastDue,
    Deadline,
    Signal,
    Noise,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::PastDue => "past_due",
            Tier::Deadline => "deadline",
            Tier::Signal => "signal",
            Tier::Noise => "noise",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        match s {
            "past_due" => Some(Tier::PastDue),
            "deadline" => Some(Tier::Deadline),
            "signal" => Some(Tier::Signal),
            "noise" => Some(Tier::Noise),
            _ => None,
        }
    }
}

/// Internal-only classification. NEVER crosses the MCP boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    Normal,
    Sealed,
}

impl Sensitivity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Sensitivity::Normal => "normal",
            Sensitivity::Sealed => "sealed",
        }
    }

    pub fn parse(s: &str) -> Sensitivity {
        match s {
            "sealed" => Sensitivity::Sealed,
            _ => Sensitivity::Normal,
        }
    }
}

/// The kind of auth-related content that caused a message to be sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealedKind {
    Otp,
    PasswordReset,
    MagicLink,
    LoginAlert,
    Verification,
}

impl SealedKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SealedKind::Otp => "otp",
            SealedKind::PasswordReset => "password_reset",
            SealedKind::MagicLink => "magic_link",
            SealedKind::LoginAlert => "login_alert",
            SealedKind::Verification => "verification",
        }
    }

    pub fn parse(s: &str) -> Option<SealedKind> {
        match s {
            "otp" => Some(SealedKind::Otp),
            "password_reset" => Some(SealedKind::PasswordReset),
            "magic_link" => Some(SealedKind::MagicLink),
            "login_alert" => Some(SealedKind::LoginAlert),
            "verification" => Some(SealedKind::Verification),
            _ => None,
        }
    }
}

/// What squelch decides to do with a message at the surfacing layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Surface,
    Squelch,
    Filtered,
}

impl Disposition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Disposition::Surface => "surface",
            Disposition::Squelch => "squelch",
            Disposition::Filtered => "filtered",
        }
    }

    pub fn parse(s: &str) -> Option<Disposition> {
        match s {
            "surface" => Some(Disposition::Surface),
            "squelch" => Some(Disposition::Squelch),
            "filtered" => Some(Disposition::Filtered),
            _ => None,
        }
    }
}

/// A ranked inbox update. MCP-visible; sealed rows are never represented here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Update {
    pub id: i64,
    pub thread_id: String,
    pub tier: Tier,
    pub importance: u8,
    pub sender: String,
    pub one_line: String,
    pub reason: String,
    pub deadline: Option<DateTime<Utc>>,
    pub matched_rule: Option<i64>,
}

/// The attention-lifecycle status of a triage row (sitrep seen-ledger).
/// `new` = never surfaced through any door; `open` = surfaced, still needs
/// attention; `done` = resolved (acted on or explicitly dismissed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionStatus {
    New,
    Open,
    Done,
}

impl AttentionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttentionStatus::New => "new",
            AttentionStatus::Open => "open",
            AttentionStatus::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<AttentionStatus> {
        match s {
            "new" => Some(AttentionStatus::New),
            "open" => Some(AttentionStatus::Open),
            "done" => Some(AttentionStatus::Done),
            _ => None,
        }
    }
}

/// A ranked inbox update PLUS its attention-lifecycle fields. HUMAN-DOOR-ONLY
/// (squelch-api `/client/updates`): the desktop client buckets on these; the
/// agent (MCP) never sees them (it serializes the leaner [`Update`]). Sealed
/// rows are excluded in SQL exactly like [`Update`], so this never represents a
/// sealed message. `surfaced_at` here is the PRE-stamp value: a row with
/// `surfaced_at == None` is "new since anyone last looked".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionUpdate {
    #[serde(flatten)]
    pub update: Update,
    pub status: AttentionStatus,
    pub surfaced_at: Option<DateTime<Utc>>,
    pub resolved_at: Option<DateTime<Utc>>,
}

/// A single sanitized message body (HTML flattened to text).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SanitizedMessage {
    pub id: i64,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub received_at: DateTime<Utc>,
    pub content: String,
}

/// A full thread as exposed over MCP. Sealed threads are NotFound, never this.
///
/// SECURITY: this type is serialized DIRECTLY by the agent door
/// (`squelch-mcp get_thread`). It carries NO HTML — by construction (structural
/// absence, matching the sealed philosophy), not by filtering. The HTML-bearing
/// view for the human door is the separate [`ClientThreadView`] below, which the
/// MCP layer never touches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadView {
    pub thread_id: String,
    pub subject: String,
    pub messages: Vec<SanitizedMessage>,
}

/// A single message body for the HUMAN DOOR: flattened text PLUS the optional
/// server-side-sanitized HTML body. `html` is `None` for plain-text-only mail;
/// the client falls back to rendering `content` (text) in that case.
///
/// This is the html-bearing sibling of [`SanitizedMessage`]. It exists as a
/// SEPARATE type (never `#[serde(flatten)]`-ed into the MCP path) so `html` can
/// NEVER cross the agent door — structural absence over runtime filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: i64,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub received_at: DateTime<Utc>,
    pub content: String,
    /// Server-side-sanitized HTML body, or `None` when the email was
    /// plain-text-only. Served ONLY here (GET /client/thread/{id}).
    pub html: Option<String>,
}

/// A full thread for the HUMAN DOOR (squelch-api `GET /client/thread/{id}`),
/// carrying per-message sanitized HTML. Sealed threads are still `NotFound`,
/// never this. The MCP surface uses [`ThreadView`] instead and never sees `html`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientThreadView {
    pub thread_id: String,
    pub subject: String,
    pub messages: Vec<ClientMessage>,
}

/// A local rule that biases how a sender is dispositioned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderRule {
    pub id: i64,
    pub account_id: AccountId,
    pub match_pattern: String,
    pub want_text: String,
    pub disposition: Disposition,
    pub updated_at: DateTime<Utc>,
}

/// An extracted bill/deadline. Bypasses the squelch threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deadline {
    pub id: i64,
    pub account_id: AccountId,
    pub message_id: i64,
    pub kind: String,
    pub amount: Option<f64>,
    pub currency: Option<String>,
    pub due_at: DateTime<Utc>,
    pub past_due: bool,
    pub source: String,
}

/// A tracked shipment/package. Produced from NON-SEALED shipping mail by the
/// ingest pipeline and stored in the `shipments` table (keyed by tracking
/// number). Surfaced by both the human door (`GET /client/shipments`) and the
/// agent door (`get_shipments`). Sealed mail never produces a shipment, so this
/// type is structurally incapable of representing sealed content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shipment {
    pub id: i64,
    pub account_id: AccountId,
    pub tracking_number: String,
    /// Carrier slug: "ups" | "usps" | "fedex" | "dhl" | "amazon" | "unknown".
    pub carrier: String,
    pub item_name: String,
    /// One of: ordered | shipped | out_for_delivery | delivered | exception.
    pub status: String,
    /// Carrier tracking URL, or `None` (Amazon / unknown carrier).
    pub tracking_url: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_update: DateTime<Utc>,
}

/// A receipt: a record of money ALREADY PAID, produced from NON-SEALED
/// past-transaction mail by the ingest pipeline and stored in the `receipts`
/// table (keyed by message). Surfaced by the human door (`GET /client/receipts`);
/// deliberately NOT exposed on the agent door (the agent doesn't need receipts).
/// Sealed mail never produces a receipt, so this type is structurally incapable
/// of representing sealed content. `amount`/`currency` are best-effort — a receipt
/// with no parseable total is still a receipt (`amount == None`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub id: i64,
    pub account_id: AccountId,
    pub message_id: i64,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub amount: Option<f64>,
    pub currency: Option<String>,
    pub received_at: DateTime<Utc>,
}

/// A keyword-search hit over the FTS index. HUMAN-DOOR-facing (squelch-api).
/// Sealed rows are excluded by the query, so a `SearchHit` never represents a
/// sealed message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: i64,
    pub thread_id: String,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub subject: String,
    pub received_at: DateTime<Utc>,
    pub snippet: String,
}

/// One row of the human-door audit log. Human-door-only; never crosses MCP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub account_id: AccountId,
    pub ts: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<String>,
}

/// Per-tier / sealed / sync summary counts. Human-door-facing (squelch-api).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreStats {
    /// Count of non-sealed messages per tier (past_due/deadline/signal/noise).
    pub tier_counts: std::collections::BTreeMap<String, i64>,
    /// Total non-sealed, triaged messages.
    pub total: i64,
    /// Count of sealed messages (metadata only).
    pub sealed: i64,
    /// The persisted Gmail history cursor (mailbox='history'), if any.
    pub last_history_id: Option<u64>,
    /// Sitrep per-band counts over non-sealed rows (the desktop chassis header):
    /// `standing` (past_due/deadline, not done), `new` (never surfaced),
    /// `open` (status='open'). Mirrors the `band` query on `/client/updates`.
    pub bands: BandCounts,
    /// The most recent `surfaced_at` across non-sealed rows — powers the
    /// "last checked: 4h ago" header. `None` if nothing has ever been surfaced.
    pub last_surfaced_at: Option<DateTime<Utc>>,
}

/// Per-band counts for the sitrep header. See [`StoreStats::bands`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BandCounts {
    pub standing: i64,
    pub new: i64,
    pub open: i64,
}

/// Input record for upserting a fetched message into the store.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub account_id: AccountId,
    pub gmail_msg_id: String,
    pub thread_id: String,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub subject: String,
    pub received_at: DateTime<Utc>,
    pub snippet: String,
    pub body: String,
    /// Server-side-sanitized (ammonia) HTML body. `None` for plain-text-only
    /// mail. Stored in `messages.body_html`; served ONLY through the human door.
    /// Never crosses /mcp — the agent door serves flattened `body` text only.
    pub body_html: Option<String>,
    pub is_sent: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// COMPILE + SERDE PROOF that the AGENT-DOOR type carries NO html: the
    /// MCP-serialized `ThreadView`/`SanitizedMessage` JSON has no `html` key —
    /// structural absence, not runtime filtering. (Also a compile-level proof:
    /// `SanitizedMessage` has no `html` field, so `get_thread` physically cannot
    /// serialize one.)
    #[test]
    fn mcp_thread_view_json_has_no_html_key() {
        let tv = ThreadView {
            thread_id: "t1".into(),
            subject: "s".into(),
            messages: vec![SanitizedMessage {
                id: 1,
                from_addr: "a@b.com".into(),
                from_name: None,
                received_at: Utc::now(),
                content: "text".into(),
            }],
        };
        let v = serde_json::to_value(&tv).unwrap();
        let msg = &v["messages"][0];
        assert!(msg.get("html").is_none(), "MCP thread view must not carry html");
        assert!(msg.get("content").is_some());
    }

    /// The HUMAN-DOOR type DOES carry html (null when absent).
    #[test]
    fn client_thread_view_json_carries_html() {
        let ctv = ClientThreadView {
            thread_id: "t1".into(),
            subject: "s".into(),
            messages: vec![
                ClientMessage {
                    id: 1,
                    from_addr: "a@b.com".into(),
                    from_name: None,
                    received_at: Utc::now(),
                    content: "text".into(),
                    html: Some("<p>hi</p>".into()),
                },
                ClientMessage {
                    id: 2,
                    from_addr: "a@b.com".into(),
                    from_name: None,
                    received_at: Utc::now(),
                    content: "plain".into(),
                    html: None,
                },
            ],
        };
        let v = serde_json::to_value(&ctv).unwrap();
        assert_eq!(v["messages"][0]["html"], serde_json::json!("<p>hi</p>"));
        // Absent html serializes as JSON null (the client falls back to text).
        assert_eq!(v["messages"][1]["html"], serde_json::Value::Null);
    }
}
