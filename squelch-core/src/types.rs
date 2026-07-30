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

/// Short (<= ~200 char) reasons for why importance / deadline / tier hold their
/// stored values.
///
/// WIRE CONTRACT (flattened into [`Update`] on GET /client/updates):
/// `{ "importance"?: string, "deadline"?: string, "tier"?: string }`; an absent
/// key means no reason was recorded. HUMAN-DOOR ONLY — the MCP path leaves
/// [`Update::field_reasons`] `None`, so no key ever reaches the agent door.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldReasons {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub importance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

impl FieldReasons {
    /// True when no property carries a reason.
    pub fn is_empty(&self) -> bool {
        self.importance.is_none() && self.deadline.is_none() && self.tier.is_none()
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
    /// HUMAN-DOOR ONLY: set by [`crate::store::Store::attention_updates`], left
    /// `None` by the MCP path so no key reaches the agent door.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_reasons: Option<FieldReasons>,
    /// Whether the message carries stored attachments (the paperclip glyph).
    /// HUMAN-DOOR ONLY, same discipline as `field_reasons`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_attachments: Option<bool>,
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

/// An [`Update`] plus its attention-lifecycle fields. HUMAN-DOOR ONLY
/// (`/client/updates`); the agent door serializes the leaner [`Update`]. Sealed
/// rows are excluded in SQL exactly as for [`Update`]. `surfaced_at` is the
/// PRE-stamp value: `None` means "new since anyone last looked".
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

/// A full thread as serialized DIRECTLY by the agent door (`get_thread`).
/// Sealed threads are NotFound, never this. Carries NO html by construction —
/// structural absence, not filtering; the html-bearing human-door sibling is
/// [`ClientThreadView`]. See docs/SECURITY.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadView {
    pub thread_id: String,
    pub subject: String,
    pub messages: Vec<SanitizedMessage>,
}

/// A single message body for the HUMAN DOOR: flattened text plus the optional
/// sanitized HTML body (`None` for plain-text-only mail, where the client falls
/// back to `content`). Deliberately a SEPARATE type from [`SanitizedMessage`],
/// never flattened into the MCP path, so `html` can never cross the agent door.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: i64,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub received_at: DateTime<Utc>,
    pub content: String,
    /// Sanitized HTML body; served ONLY here (GET /client/thread/{id}).
    pub html: Option<String>,
    /// Attachment metadata (real attachments + cid-inline parts); always
    /// present, `[]` when there are none. Bytes come from
    /// GET /client/attachments/{id}. HUMAN-DOOR ONLY, like `html`.
    #[serde(default)]
    pub attachments: Vec<ClientAttachment>,
}

/// One attachment's metadata on the HUMAN DOOR. Carries NO bytes.
///
/// WIRE CONTRACT: `{ id, filename, mime, size, downloadable }`.
/// `downloadable == false` means the bytes were never stored (the part exceeded
/// the ingest cap) and the byte endpoint returns 410 for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientAttachment {
    /// The `attachments.id` — the handle for GET /client/attachments/{id}.
    pub id: i64,
    pub filename: String,
    pub mime: String,
    /// Decoded size in bytes (the real part size, whether or not bytes were kept).
    pub size: i64,
    pub downloadable: bool,
}

/// A full thread for the HUMAN DOOR (`GET /client/thread/{id}`), carrying
/// per-message sanitized HTML. Sealed threads are still `NotFound`, never this.
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

/// A tracked shipment/package, extracted at ingest from NON-SEALED shipping
/// mail into the `shipments` table (keyed by tracking number). Served on both
/// doors (`GET /client/shipments`, `get_shipments`).
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

/// A record of money ALREADY PAID, extracted from NON-SEALED past-transaction
/// mail into the `receipts` table (keyed by message). HUMAN-DOOR ONLY
/// (`GET /client/receipts`). `amount`/`currency` are best-effort — a receipt
/// with no parseable total is still a receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub id: i64,
    pub account_id: AccountId,
    pub message_id: i64,
    /// The message's thread, so the client can open the email from this row.
    pub thread_id: String,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub amount: Option<f64>,
    pub currency: Option<String>,
    pub received_at: DateTime<Utc>,
}

/// A bank/credit-card STATEMENT or TRANSACTION ALERT ("you spent" / deposit /
/// low-balance), extracted from NON-SEALED mail by `triage::extract::banking`
/// into the `banking` table (keyed by message). HUMAN-DOOR ONLY.
///
/// SERIALIZED SHAPE IS A WIRE CONTRACT — no `account_id` on purpose (the
/// endpoint is already account-scoped). `kind` is "statement" |
/// "transaction_alert"; `amount` is the total statement balance for a statement
/// and the transaction amount for an alert. `account_hint` is only ever a masked
/// last-4 tail ("…1234") or null — a full account number is never emitted. Every
/// extracted field is nullable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Banking {
    pub id: i64,
    pub message_id: i64,
    /// The message's thread, so the client can open the email from this row.
    pub thread_id: String,
    /// Sender address — the client's display fallback when no institution was
    /// extracted.
    pub from_addr: String,
    pub kind: String,
    pub institution: Option<String>,
    pub amount: Option<f64>,
    pub currency: Option<String>,
    pub account_hint: Option<String>,
    pub received_at: DateTime<Utc>,
}

/// An invite / updated invitation / cancellation / RSVP response, extracted
/// from NON-SEALED calendar mail (Google Calendar, Outlook, ics-bearing) into
/// the `calendar_updates` table (keyed by message). HUMAN-DOOR ONLY.
///
/// SERIALIZED SHAPE IS A WIRE CONTRACT — no `account_id` on purpose (the
/// endpoint is already account-scoped). `kind` is "invite" | "update" |
/// "cancellation" | "response" (see [`crate::triage::CalendarKind`]); every
/// extracted field is best-effort and nullable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarUpdate {
    pub id: i64,
    pub message_id: i64,
    /// The message's thread, so the client can open the email from this row.
    pub thread_id: String,
    pub kind: String,
    pub event_title: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub organizer: Option<String>,
    pub received_at: DateTime<Utc>,
}

/// A keyword-search hit over the FTS index. HUMAN-DOOR-facing; sealed rows are
/// excluded by the query.
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
    /// Sender of the targeted message (`from_name` else `from_addr`) when
    /// `target` parses as a message id that exists for the account, else `None`.
    /// Deliberately includes sealed messages' sender, never sealed CONTENT.
    #[serde(default)]
    pub target_sender: Option<String>,
    /// Subject of the targeted message, same rules as
    /// [`AuditEntry::target_sender`].
    #[serde(default)]
    pub target_subject: Option<String>,
}

/// Why a notification-worthy event was emitted. Precedence when classifying a
/// verdict is `Urgent` > `Deadline` > `Surfaced` (see
/// [`crate::triage::events::worthy_kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Tier is `past_due`/`deadline` — the standing band, immune to thresholds.
    Urgent,
    /// A deadline was detected on a message that is not itself urgent-tier.
    Deadline,
    /// Importance landed at or above the notify threshold (above the line).
    Surfaced,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Urgent => "urgent",
            EventKind::Deadline => "deadline",
            EventKind::Surfaced => "surfaced",
        }
    }

    pub fn parse(s: &str) -> Option<EventKind> {
        match s {
            "urgent" => Some(EventKind::Urgent),
            "deadline" => Some(EventKind::Deadline),
            "surfaced" => Some(EventKind::Surfaced),
            _ => None,
        }
    }
}

/// One durable notification event from the monotonic `events` log the delivery
/// adapters read (SSE for the Mac app, APNs for iOS). Every field is a
/// DENORMALIZED SNAPSHOT taken at emission time: a client must render the whole
/// notification from this row alone (the iOS Notification Service Extension
/// fetches one by id after an opaque push and has no second round-trip to
/// spend). Sealed mail can never be represented here — emission requires
/// `Sensitivity::Normal`; see docs/SECURITY.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Monotonic id — also the per-channel cursor clients page with (`after`).
    pub id: i64,
    pub kind: EventKind,
    pub message_id: i64,
    pub thread_id: String,
    pub tier: Tier,
    pub importance: u8,
    pub sender: String,
    pub one_line: String,
    /// Snapshotted deadline as stored RFC3339 text, passed through verbatim:
    /// display copy only, never something the delivery path computes with.
    pub deadline: Option<String>,
    pub created_at: DateTime<Utc>,
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
    /// Sitrep per-band counts over non-sealed rows: `standing` (past_due/
    /// deadline, not done), `new` (never surfaced), `open` (status='open').
    pub bands: BandCounts,
    /// The most recent `surfaced_at` across non-sealed rows; `None` if nothing
    /// has ever been surfaced.
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
    /// Ammonia-sanitized HTML body, `None` for plain-text-only mail. Stored in
    /// `messages.body_html` and served ONLY through the human door — the agent
    /// door serves flattened `body` text (see docs/SECURITY.md).
    pub body_html: Option<String>,
    pub is_sent: bool,
    /// Raw `List-Unsubscribe` header value (comma-separated `<mailto:…>` /
    /// `<https:…>` entries). Consumed ONLY by the human door's unsubscribe
    /// endpoint; never crosses /mcp.
    pub list_unsubscribe: Option<String>,
    /// `true` when the mail advertised RFC 8058 one-click unsubscribe
    /// (`List-Unsubscribe-Post: List-Unsubscribe=One-Click`).
    pub list_unsub_one_click: bool,
}

/// One attachment extracted from a message's RFC822 at ingest — BOTH real
/// attachments and cid-inline parts — committed to the `attachments` table in
/// the same transaction as the message. `data` is `None` when the part exceeded
/// the ingest cap (10 MB per attachment, 25 MB per message): metadata is kept,
/// bytes dropped, and `size_bytes` is the real decoded size either way.
#[derive(Debug, Clone)]
pub struct AttachmentInfo {
    /// Filename; ingest substitutes `attachment-<n>` when the part declares none.
    pub filename: String,
    /// The MIME type the part DECLARED (else `application/octet-stream`), stored
    /// verbatim — the serving endpoint applies its own render-safety whitelist.
    pub mime: String,
    /// Real decoded size in bytes, kept even when `data` is `None`.
    pub size_bytes: i64,
    /// The decoded bytes, or `None` when the part was over the ingest cap.
    pub data: Option<Vec<u8>>,
}

/// One row of the human door's unsubscribe ledger (`GET /client/unsubscribes`).
///
/// WIRE CONTRACT: `{ sender, requested_at, method, violation_count,
/// last_violation_at, resolution }`, newest `requested_at` first. `method` is
/// "one_click" | "mailto" | "browser"; `resolution` is "blocked" | "dismissed"
/// | null.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsubscribeRecord {
    /// Lowercased sender address the unsubscribe was requested against.
    pub sender: String,
    pub requested_at: DateTime<Utc>,
    pub method: String,
    /// Post-grace inbound messages from this sender since the request — 0 until
    /// the sender re-offends past the 72h grace window.
    pub violation_count: i64,
    /// Timestamp of the most recent counted violation, or `None`.
    pub last_violation_at: Option<DateTime<Utc>>,
    /// User's resolution of a repeat offender: "blocked" | "dismissed", or
    /// `None` while the request is still outstanding.
    pub resolution: Option<String>,
}

/// Which axis of a triage verdict a human overruled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriageAxis {
    /// `triage.tier` — past_due | deadline | signal | noise.
    Tier,
    /// `triage.category` — general | marketing | invoice | autopay_bill |
    /// banking_statement | transaction_alert.
    Category,
    /// `triage.sensitivity` — normal | sealed. Corrections matter in BOTH
    /// directions: under-sealing let a code into the normal inbox, over-sealing
    /// locked ordinary mail away.
    Sensitivity,
}

impl TriageAxis {
    pub fn as_str(&self) -> &'static str {
        match self {
            TriageAxis::Tier => "tier",
            TriageAxis::Category => "category",
            TriageAxis::Sensitivity => "sensitivity",
        }
    }

    pub fn parse(s: &str) -> Option<TriageAxis> {
        match s {
            "tier" => Some(TriageAxis::Tier),
            "category" => Some(TriageAxis::Category),
            "sensitivity" => Some(TriageAxis::Sensitivity),
            _ => None,
        }
    }

    /// The values this axis accepts. Corrections are validated against it so a
    /// typo can never write a label the pipeline would not itself produce.
    pub fn allowed(&self) -> &'static [&'static str] {
        match self {
            TriageAxis::Tier => &["past_due", "deadline", "signal", "noise"],
            TriageAxis::Category => &[
                "general",
                "marketing",
                "invoice",
                "autopay_bill",
                "banking_statement",
                "transaction_alert",
            ],
            TriageAxis::Sensitivity => &["normal", "sealed"],
        }
    }
}

/// One recorded human correction of a triage verdict. See the `triage_feedback`
/// block in schema.sql for why `original` and the denormalized sender/subject
/// are kept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageFeedback {
    pub id: i64,
    pub message_id: i64,
    pub corrected_at: DateTime<Utc>,
    pub dimension: String,
    pub from_value: Option<String>,
    pub to_value: String,
    /// JSON snapshot of the triage row as it stood before the correction.
    pub original: serde_json::Value,
    pub sender: String,
    pub subject: String,
    pub note: Option<String>,
}

/// One auth message eligible for shredding: past the retention policy and not
/// already in `shred_log`. Human-door only.
#[derive(Debug, Clone)]
pub struct ShredCandidate {
    /// Local `messages.id`.
    pub message_id: i64,
    /// Gmail's own message id — what the trash API is keyed by.
    pub gmail_msg_id: String,
    pub sender: String,
    /// `triage.sealed_kind` (otp / password_reset / …), when classified.
    pub kind: Option<String>,
    pub received_at: DateTime<Utc>,
}

/// Shredder state for the Auth page: the policy plus the real counts from
/// `shred_log`. `write_ready == false` (no write credential) means the pass can
/// never run no matter what `enabled` says.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShredStats {
    pub enabled: bool,
    pub after_days: i64,
    /// Messages trashed in the trailing 30 days (the headline figure).
    pub shredded_recent: i64,
    /// Messages trashed since the ledger began.
    pub shredded_total: i64,
    /// When the most recent shred happened, or `None` if nothing ever has.
    pub last_shredded_at: Option<DateTime<Utc>>,
    /// How many messages are eligible RIGHT NOW under the current policy.
    pub pending: i64,
    /// Whether a write credential exists for the trash call.
    pub write_ready: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The agent-door thread view carries no `html` and no `attachments` key —
    /// structural absence, not runtime filtering.
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
        assert!(
            msg.get("attachments").is_none(),
            "MCP thread view must not carry attachments"
        );
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
                    attachments: vec![ClientAttachment {
                        id: 7,
                        filename: "doc.pdf".into(),
                        mime: "application/pdf".into(),
                        size: 1234,
                        downloadable: true,
                    }],
                },
                ClientMessage {
                    id: 2,
                    from_addr: "a@b.com".into(),
                    from_name: None,
                    received_at: Utc::now(),
                    content: "plain".into(),
                    html: None,
                    attachments: vec![],
                },
            ],
        };
        let v = serde_json::to_value(&ctv).unwrap();
        assert_eq!(v["messages"][0]["html"], serde_json::json!("<p>hi</p>"));
        // Absent html serializes as JSON null (the client falls back to text).
        assert_eq!(v["messages"][1]["html"], serde_json::Value::Null);
        // Attachments always present; [] when none.
        assert_eq!(v["messages"][0]["attachments"][0]["filename"], serde_json::json!("doc.pdf"));
        assert_eq!(v["messages"][0]["attachments"][0]["downloadable"], serde_json::json!(true));
        assert_eq!(v["messages"][1]["attachments"], serde_json::json!([]));
    }

    fn base_update() -> Update {
        Update {
            id: 1,
            thread_id: "t1".into(),
            tier: Tier::Signal,
            importance: 70,
            sender: "a@b.com".into(),
            one_line: "hi".into(),
            reason: "known contact".into(),
            deadline: None,
            matched_rule: None,
            field_reasons: None,
            has_attachments: None,
        }
    }

    /// An `Update` with `field_reasons == None` serializes with no such key.
    #[test]
    fn update_without_field_reasons_omits_the_key() {
        let v = serde_json::to_value(base_update()).unwrap();
        assert!(
            v.get("field_reasons").is_none(),
            "agent-door Update must not carry a field_reasons key: {v}"
        );
    }

    /// Only properties that carry a reason appear; absent keys are omitted, not
    /// null.
    #[test]
    fn field_reasons_serializes_only_present_keys() {
        let mut u = base_update();
        u.field_reasons = Some(FieldReasons {
            importance: Some("known contact + appears in Sent mail".into()),
            deadline: None,
            tier: Some("known contact -> signal".into()),
        });
        let v = serde_json::to_value(&u).unwrap();
        let fr = &v["field_reasons"];
        assert_eq!(fr["importance"], serde_json::json!("known contact + appears in Sent mail"));
        assert_eq!(fr["tier"], serde_json::json!("known contact -> signal"));
        assert!(fr.get("deadline").is_none(), "absent deadline reason must be omitted, not null");

        // Round-trips back through Deserialize.
        let again: Update = serde_json::from_value(v).unwrap();
        assert_eq!(again.field_reasons, u.field_reasons);
    }

    /// JSON with no `field_reasons` key deserializes cleanly.
    #[test]
    fn update_deserializes_without_field_reasons_key() {
        let json = r#"{"id":1,"thread_id":"t","tier":"noise","importance":10,
            "sender":"a@b.com","one_line":"x","reason":"y","deadline":null,"matched_rule":null}"#;
        let u: Update = serde_json::from_str(json).unwrap();
        assert!(u.field_reasons.is_none());
    }
}
