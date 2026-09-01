//! Core domain types. Types that cross the MCP boundary derive serde.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type AccountId = i64;

/// Declares a C-like enum together with its string mapping, so the enum, its
/// `as_str`, and its `parse` all come from ONE variant→literal table and can
/// never drift out of step.
///
/// Attributes and doc comments pass through verbatim onto the enum and each
/// variant, so derives and `#[serde(rename_all = …)]` keep producing exactly the
/// wire shape they did hand-written — these enums cross the doors, so the
/// serialized bytes are a contract.
///
/// `parse` returns `Option<Self>`. A trailing `fallback = Variant;` clause makes
/// it infallible instead, returning `Self` with every unrecognized string mapped
/// to that variant.
macro_rules! str_enum {
    // Internal: the `parse` half, infallible form (a `fallback` was declared).
    (@parse $name:ident { $($variant:ident => $lit:literal),+ } fallback = $fallback:ident;) => {
        /// Infallible: an unrecognized string parses to the fallback variant.
        pub fn parse(s: &str) -> $name {
            match s {
                $($lit => $name::$variant,)+
                _ => $name::$fallback,
            }
        }
    };
    // Internal: the `parse` half, fallible form (no `fallback` declared).
    (@parse $name:ident { $($variant:ident => $lit:literal),+ }) => {
        pub fn parse(s: &str) -> Option<$name> {
            match s {
                $($lit => Some($name::$variant),)+
                _ => None,
            }
        }
    };

    (
        $(#[$emeta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$vmeta:meta])* $variant:ident => $lit:literal),+ $(,)?
        }
        $($fallback:tt)*
    ) => {
        $(#[$emeta])*
        $vis enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $lit,)+
                }
            }

            $crate::types::str_enum!(@parse $name { $($variant => $lit),+ } $($fallback)*);
        }
    };
}

// Sibling modules reach the macro by path, so definition order never matters.
pub(crate) use str_enum;

str_enum! {
    /// MCP-visible triage tier. There is deliberately NO `Sealed` variant here:
    /// sealed messages are excluded structurally, never surfaced as a tier.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Tier {
        PastDue => "past_due",
        Deadline => "deadline",
        Signal => "signal",
        Noise => "noise",
    }
}

str_enum! {
    /// Internal-only classification. NEVER crosses the MCP boundary.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Sensitivity {
        Normal => "normal",
        Sealed => "sealed",
    }
    // Anything that is not explicitly sealed is normal — parse never fails.
    fallback = Normal;
}

str_enum! {
    /// The kind of auth-related content that caused a message to be sealed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SealedKind {
        Otp => "otp",
        PasswordReset => "password_reset",
        MagicLink => "magic_link",
        LoginAlert => "login_alert",
        Verification => "verification",
    }
}

str_enum! {
    /// What squelch decides to do with a message at the surfacing layer.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Disposition {
        Surface => "surface",
        Squelch => "squelch",
        Filtered => "filtered",
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
    /// The `From:` display name, so the reader can render "Dollar Car Rental"
    /// instead of reverse-engineering a label out of `sender`'s domain — which
    /// is how `marketing@emails.dollar.com` came to be shown as "Emails".
    ///
    /// HUMAN-DOOR ONLY, same discipline as the two above, and for a sharper
    /// reason than either: this is UNTRUSTED, attacker-chosen header text. A
    /// human reading mail expects display names and can weigh one against the
    /// address beside it; an agent making decisions should keep seeing only the
    /// address it can actually verify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_name: Option<String>,
}

str_enum! {
    /// The attention-lifecycle status of a triage row (sitrep seen-ledger).
    /// `new` = never surfaced through any door; `open` = surfaced, still needs
    /// attention; `done` = resolved (acted on or explicitly dismissed).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AttentionStatus {
        New => "new",
        Open => "open",
        Done => "done",
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
    /// A PENDING reminder: when the user asked to see this again, `None` when
    /// they never did or the reminder already fired. HUMAN-DOOR ONLY, like the
    /// two fields above — a reminder is a statement the user made about their
    /// own attention, and the agent door serializes the leaner [`Update`], which
    /// has no place to put it.
    ///
    /// Serialized even when `None` (no `skip_serializing_if`, same as
    /// `surfaced_at`): the client decodes these as optionals and "absent" and
    /// "null" must not become two different readings of "no reminder".
    pub remind_at: Option<DateTime<Utc>>,
    /// A reminder that ALREADY FIRED, `None` until one does. Exactly one of this
    /// and `remind_at` is ever set: the sweep moves the stamp across.
    pub reminded_at: Option<DateTime<Utc>>,
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
    /// THIS message's own `Subject`, not the thread's.
    ///
    /// [`ClientThreadView::subject`] is the OLDEST message's subject, which is
    /// the right title for the conversation and the WRONG title for one message
    /// inside it: a renamed thread ("Lunch?" -> "Re: Lunch? -> Thursday
    /// instead") makes the two diverge. The client forwards ONE message, and it
    /// titles and labels that forward from the message it actually forwards, so
    /// the per-message value has to be on the wire.
    ///
    /// An old client simply ignores the extra key.
    pub subject: String,
    pub content: String,
    /// Sanitized HTML body; served ONLY here (GET /client/thread/{id}).
    pub html: Option<String>,
    /// Attachment metadata (real attachments + cid-inline parts); always
    /// present, `[]` when there are none. Bytes come from
    /// GET /client/attachments/{id}. HUMAN-DOOR ONLY, like `html`.
    #[serde(default)]
    pub attachments: Vec<ClientAttachment>,
    /// AUTHORED BY THIS ACCOUNT: `true` when the account itself wrote this
    /// message — the stored outbound copy, OR a received copy whose `from_addr`
    /// is the account's own address — and `false` for mail from anyone else.
    /// The reader aligns every chat bubble on it, and an action aimed at "the
    /// message" — remind me about this, reply to this — must land on inbound
    /// mail, and a thread ends on the user's own reply often enough that aiming
    /// at the last message would aim at themselves. So it is ALWAYS on the wire
    /// as a plain bool — never skipped, defaulting to false for a row an older
    /// daemon serialized — and a client that never learns about it simply
    /// ignores the key.
    ///
    /// NOT the stored `messages.is_sent` column alone. That column is a
    /// VISIBILITY flag, sticky to 0 on upsert and deliberately lost to the INBOX
    /// copy for self-addressed mail (self-Cc, mail to oneself, a group echo), so
    /// a message the user demonstrably wrote can sit there at 0 forever. The
    /// human-door thread query therefore ORs it with a case-insensitive
    /// `from_addr` == account-email match; that OR is the whole difference
    /// between the column and this field.
    ///
    /// This is the ONLY type that carries it: the agent door's
    /// [`SanitizedMessage`] is untouched.
    #[serde(default)]
    pub is_sent: bool,
    /// THE PROVIDER'S SPAM VERDICT on this message, straight off
    /// `messages.is_spam`. On the wire because the reader has to be able to SAY
    /// SO: mail that arrives here having been filtered by Gmail rather than
    /// scored by squelch is the one case where the honest thing to show the user
    /// is who made the call, and the client cannot infer it — a spam row looks
    /// exactly like any other untriaged one (tier=noise, importance 0).
    ///
    /// Per MESSAGE and not per thread, because a thread can hold both: a real
    /// correspondent's mail and a spoof of them can share a Subject and land in
    /// the same conversation, and marking the whole thread would either libel
    /// the real message or excuse the forgery.
    ///
    /// Like `is_sent`, this is the human door only — the agent door's
    /// [`SanitizedMessage`] carries no spam at all rather than labelled spam.
    #[serde(default)]
    pub is_spam: bool,
    /// This message's OWN triage verdict, for in-thread attention highlighting:
    /// the band shows one row per thread, so the reader is where "which message
    /// is the reason" gets answered. All optional — absent on a pre-highlight
    /// daemon — and `None` never serializes, keeping old clients' decoders calm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    /// Whether this message's attention row is still unresolved (`status !=
    /// 'done'`). Highlighting keys on THIS, not tier alone, so a resolved
    /// obligation stops glowing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention_open: Option<bool>,
    /// The stage-1 one-line summary — the highlight's tooltip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_line: Option<String>,
    /// The stored `messages.auth_pass` (see [`NewMessage::auth_pass`]). NOT on
    /// the wire: it exists so `get_thread` can gate the read-time `sender_known`
    /// bit on it, and the client already receives that derived answer.
    #[serde(default, skip_serializing)]
    pub auth_pass: Option<bool>,
}

/// One attachment's metadata on the HUMAN DOOR. Carries NO bytes.
///
/// WIRE CONTRACT: `{ id, filename, mime, size, downloadable, content_id? }`.
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
    /// The part's normalized Content-ID (see [`AttachmentInfo::content_id`]) —
    /// what an `<img src="cid:...">` in the sanitized HTML resolves against, so
    /// the client can point the tag at this attachment's bytes instead of
    /// painting a broken image. `None` when the part declared none, and
    /// structurally ABSENT then: a pre-`content_id` daemon omits the key too, so
    /// the client cannot tell the two apart and must not try.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
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

str_enum! {
    /// HOW A SEND GROUP ADDRESSES ITS MEMBERS. A property of the AUDIENCE, not
    /// of any one message: an investor list is individually-addressed every
    /// time or it is not one, so this is chosen when the group is created and
    /// the composer obeys it rather than asking again per send.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum GroupMode {
        /// One message, every member in `To` — they see each other.
        To => "to",
        /// One message, every member in `Bcc` — they do not.
        Bcc => "bcc",
        /// One message PER member, sent separately. Replies come back as
        /// private threads and read receipts are per-recipient.
        Individual => "individual",
    }
    fallback = To;
}

/// One mailbox in a [`SendGroup`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    /// Lowercased bare address — the identity half of the membership key.
    pub addr: String,
    /// A convenience copy for rendering the list without joining `contacts`,
    /// which stays the source of truth for who anyone is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// A named audience the user can address as one. HUMAN DOOR ONLY.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendGroup {
    pub id: i64,
    pub name: String,
    /// Lowercased, whitespace-collapsed `name`. The uniqueness key, and what
    /// composer autocomplete matches.
    pub slug: String,
    pub mode: GroupMode,
    pub note: String,
    pub member_count: i64,
    /// EMPTY ON THE LIST READ, populated on the single-group read: a sidebar
    /// listing every group's full membership is a contacts dump nobody asked
    /// for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<GroupMember>,
    /// When this group was last addressed, recorded or derived. `None` for a
    /// group that has never been written to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sent_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// ONE ENTRY IN A GROUP'S SEND HISTORY, from either of the two sources that feed
/// it — a recorded [`crate::types::SendGroup`] send, or a message DERIVED by
/// matching its stored recipients against the current membership.
///
/// The derived half is what makes the feature work on the day it ships: a group
/// is created for people the user has already been emailing for a year, so a
/// history that only knew about sends made through the group would open empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupHistoryEntry {
    /// The recorded send this row is, or `None` when it was derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_send_id: Option<i64>,
    /// The message to open. For a fan-out this is the first recipient's copy —
    /// the batch has no single message — and `None` when no echo ever landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub subject: String,
    pub snippet: String,
    pub sent_at: DateTime<Utc>,
    pub mode: GroupMode,
    /// How many of the group this reached. A derived row counts CURRENT members
    /// the message named; a recorded row counts its own recipients.
    pub reached: i64,
    /// The denominator `reached` is out of: the snapshot taken at send time for
    /// a recorded row, the current membership for a derived one. The two differ
    /// on purpose — "9 of 12" about last quarter must keep meaning what it
    /// meant, while a derived row can only ever speak about the group as it is
    /// now.
    pub group_size: i64,
    /// Recipients a fan-out failed to reach. Always 0 for a derived row and for
    /// a single-message send, which either went or did not.
    pub failed: i64,
    /// Recipients a fan-out has not got to YET. Non-zero means this send is
    /// still in flight, which is what lets the history row be its own progress
    /// indicator: a plain re-read watches `reached` climb. Always 0 for a
    /// derived row.
    pub pending: i64,
    pub opens: i64,
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
    /// Thread of the latest message whose status the merge accepted, so the
    /// client can open the email from the row. `None` only for rows written
    /// before `last_message_id` existed (an older daemon); those list fine,
    /// they just aren't clickable.
    pub thread_id: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_update: DateTime<Utc>,
    /// The carrier's own latest status string, verbatim; `None` until the first
    /// poll. Present even when it mapped to no `status`, so the client can show
    /// what the carrier said.
    pub carrier_status_raw: Option<String>,
    /// Carrier-estimated delivery; `None` when the carrier gives none.
    pub eta: Option<DateTime<Utc>>,
    /// When the package landed, from whichever path saw it first (a delivered
    /// email or a poll); `None` until it is delivered.
    pub delivered_at: Option<DateTime<Utc>>,
    /// Last carrier-API poll ATTEMPT; `None` = never polled.
    pub last_polled_at: Option<DateTime<Utc>>,
    /// Consecutive PERMANENT carrier-poll failures (0 until one happens, reset by
    /// any successful poll). Surfaced because it is EVIDENCE ABOUT THE NUMBER: a
    /// bare digit-run the carrier keeps rejecting is very likely a retailer item
    /// or order id that was never a tracking number at all.
    pub poll_failures: u32,
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

str_enum! {
    /// Why a notification-worthy event was emitted. Precedence when classifying a
    /// verdict is `Urgent` > `Deadline` > `Surfaced` (see
    /// [`crate::triage::events::worthy_kind`]).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum EventKind {
        /// Tier is `past_due`/`deadline` — the standing band, immune to thresholds.
        Urgent => "urgent",
        /// A deadline was detected on a message that is not itself urgent-tier.
        Deadline => "deadline",
        /// Importance landed at or above the notify threshold (above the line).
        Surfaced => "surfaced",
        /// The recipient of the user's OWN tracked outbound mail opened it. Not
        /// produced by triage at all — it is written by
        /// [`crate::tracking::OpensPoller`], so `sender` is the account's own
        /// address, `one_line` is the sent message's subject, and `importance`
        /// is a fixed placeholder rather than a score.
        Opened => "opened",
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
    /// Count of non-sealed messages per tier (past_due/deadline/signal/noise),
    /// over RECEIVED, NON-SPAM mail only. Both exclusions are corrections: sent
    /// mail and provider spam both land tier=noise without ever being triaged,
    /// so counting them made the header's "noise" number — which is the door to
    /// the noise page — describe a list several times larger than the page it
    /// opens.
    pub tier_counts: std::collections::BTreeMap<String, i64>,
    /// Total non-sealed, triaged messages.
    pub total: i64,
    /// Count of sealed messages (metadata only).
    pub sealed: i64,
    /// How many messages the mail provider filed as spam. Its own count rather
    /// than a `tier_counts` entry because spam is not a tier: these rows carry
    /// tier=noise like everything untriaged, and folding them in would put them
    /// back in the number they were just taken out of.
    pub spam: i64,
    /// The persisted Gmail history cursor (mailbox='history'), if any.
    pub last_history_id: Option<u64>,
    /// Sitrep per-band counts over non-sealed rows: `standing` (past_due/
    /// deadline, not done), `new` (never surfaced), `open` (status='open').
    pub bands: BandCounts,
    /// The most recent `surfaced_at` across non-sealed rows; `None` if nothing
    /// has ever been surfaced.
    pub last_surfaced_at: Option<DateTime<Utc>>,
}

/// How much of a window's incoming mail the user had to open, as two raw
/// counts and the reach of the evidence behind them.
///
/// RAW COUNTS, NOT A PERCENTAGE, on purpose: whether the sample is big enough
/// to quote and how coarsely to round it are the CALLER's policy, and a store
/// that handed back a single tidy number would have made both decisions
/// silently. The one caller (the invite mail) has a floor and a rounding rule,
/// and both are written down where a person reviewing the copy can see them.
///
/// `opened` is what THIS DAEMON served the body of. See
/// `Store::share_open_rate` for what that misses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRate {
    /// Received, triaged messages in the window. Sealed mail is included.
    pub received: u64,
    /// How many of them the human door has ever served the body of.
    pub opened: u64,
    /// The oldest received row in the window, or `None` when there are none.
    /// How much history the pair rests on, which is not the same as how long
    /// the window is: a mailbox synced yesterday has one day of evidence
    /// whatever window it is asked about.
    pub oldest_received_at: Option<DateTime<Utc>>,
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
    /// `true` when Gmail had this message under its SPAM label. The provider's
    /// verdict, never ours: it keeps the row out of every band, queue and search
    /// (see `messages.is_spam`) and off the LLM path entirely.
    pub is_spam: bool,
    /// Display recipients (To + Cc) of SENT mail, comma-joined `Name <addr>`
    /// (bare addr when the header carried no display name). `None` for received
    /// mail, and `None` from any caller that did not parse the headers — the
    /// upsert treats `None` as "no opinion" and keeps whatever is stored, so a
    /// re-fetch can fill a row in without a later one blanking it.
    pub to_addrs: Option<String>,
    /// Raw `List-Unsubscribe` header value (comma-separated `<mailto:…>` /
    /// `<https:…>` entries). Consumed ONLY by the human door's unsubscribe
    /// endpoint; never crosses /mcp.
    pub list_unsubscribe: Option<String>,
    /// `true` when the mail advertised RFC 8058 one-click unsubscribe
    /// (`List-Unsubscribe-Post: List-Unsubscribe=One-Click`).
    pub list_unsub_one_click: bool,
    /// Email-authentication verdict read at ingest from the TOPMOST
    /// `Authentication-Results` header Gmail itself wrote: `Some(true)` when
    /// DMARC passed or an aligned DKIM signature did, `Some(false)` when Gmail
    /// evaluated it and neither held, `None` when nothing trustworthy was there
    /// to evaluate. `None` is NEVER an assertion of pass — see
    /// `sync::ingest::extract_auth_pass`.
    pub auth_pass: Option<bool>,
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
    /// The part's Content-ID, NORMALIZED: whitespace trimmed and one surrounding
    /// `<...>` pair stripped, so it compares directly against the token in an
    /// `<img src="cid:...">`. `None` when the part declares none (every real
    /// attachment) or declares an empty one.
    pub content_id: Option<String>,
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

str_enum! {
    /// Which axis of a triage verdict a human overruled.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum TriageAxis {
        /// `triage.tier` — past_due | deadline | signal | noise.
        Tier => "tier",
        /// `triage.category` — general | marketing | invoice | autopay_bill |
        /// banking_statement | transaction_alert.
        Category => "category",
        /// `triage.sensitivity` — normal | sealed. Corrections matter in BOTH
        /// directions: under-sealing let a code into the normal inbox, over-sealing
        /// locked ordinary mail away.
        Sensitivity => "sensitivity",
    }
}

impl TriageAxis {
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

/// How far a dev re-triage has got. The "run" is every row carrying a LIVE
/// `retriage_at` stamp — the same [`crate::triage::retriage_forced`] window the
/// passes themselves read, so this counts exactly the rows the force still
/// covers. Two kicks inside the window are ONE run here, which is the honest
/// answer: they are one pile of work to the queues.
///
/// `done` is derived from the queue predicates rather than from a marker of its
/// own, so it can never disagree with what the passes will actually pick up: a
/// row is finished when neither the Stage-1 queue nor the Stage-2 queue would
/// still hand it out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetriageProgress {
    /// Rows in the live window. Zero means no run is in flight — nothing was
    /// ever asked for, or every stamp has aged out.
    pub total: i64,
    /// Rows of `total` that have left both queues.
    pub done: i64,
    /// The OLDEST live stamp: when the run being counted began. `None` exactly
    /// when `total` is 0.
    pub started_at: Option<DateTime<Utc>>,
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
        assert!(
            msg.get("html").is_none(),
            "MCP thread view must not carry html"
        );
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
                    // Renamed mid-thread: the message's own subject is NOT the
                    // thread's, and the forward composer titles from this one.
                    subject: "Re: s -> Thursday instead".into(),
                    content: "text".into(),
                    html: Some("<p>hi</p>".into()),
                    attachments: vec![
                        ClientAttachment {
                            id: 7,
                            filename: "doc.pdf".into(),
                            mime: "application/pdf".into(),
                            size: 1234,
                            downloadable: true,
                            content_id: None,
                        },
                        ClientAttachment {
                            id: 8,
                            filename: "logo.png".into(),
                            mime: "image/png".into(),
                            size: 99,
                            downloadable: true,
                            content_id: Some("logo@squelch".into()),
                        },
                    ],
                    tier: Some(Tier::PastDue),
                    deadline: None,
                    attention_open: Some(true),
                    one_line: Some("bill is 12 days past due".into()),
                    auth_pass: Some(true),
                    is_sent: false,
                    is_spam: false,
                },
                ClientMessage {
                    id: 2,
                    from_addr: "a@b.com".into(),
                    from_name: None,
                    received_at: Utc::now(),
                    subject: "s".into(),
                    content: "plain".into(),
                    html: None,
                    attachments: vec![],
                    tier: None,
                    deadline: None,
                    attention_open: None,
                    one_line: None,
                    auth_pass: None,
                    is_sent: true,
                    is_spam: false,
                },
            ],
        };
        let v = serde_json::to_value(&ctv).unwrap();
        assert_eq!(v["messages"][0]["html"], serde_json::json!("<p>hi</p>"));
        // Absent html serializes as JSON null (the client falls back to text).
        assert_eq!(v["messages"][1]["html"], serde_json::Value::Null);
        // Attachments always present; [] when none.
        assert_eq!(
            v["messages"][0]["attachments"][0]["filename"],
            serde_json::json!("doc.pdf")
        );
        assert_eq!(
            v["messages"][0]["attachments"][0]["downloadable"],
            serde_json::json!(true)
        );
        // content_id rides along on a cid-inline part and is structurally ABSENT
        // on a part that declared none — the same shape a pre-content_id daemon
        // sends, which is what lets the client treat both as "no cid".
        assert_eq!(
            v["messages"][0]["attachments"][1]["content_id"],
            serde_json::json!("logo@squelch")
        );
        assert!(
            v["messages"][0]["attachments"][0]
                .get("content_id")
                .is_none(),
            "None content_id must not serialize"
        );
        assert_eq!(v["messages"][1]["attachments"], serde_json::json!([]));
        // PER-MESSAGE subject, which a renamed thread makes differ from the
        // thread's own: the forward composer titles from the message it
        // forwards, so both values have to be on the wire, at their own levels.
        assert_eq!(v["subject"], serde_json::json!("s"));
        assert_eq!(
            v["messages"][0]["subject"],
            serde_json::json!("Re: s -> Thursday instead")
        );
        assert_eq!(v["messages"][1]["subject"], serde_json::json!("s"));
        // Triage highlight fields: present when set, structurally ABSENT when
        // None (not null) — an old client's strict decoder never sees the keys.
        assert_eq!(v["messages"][0]["tier"], serde_json::json!("past_due"));
        assert_eq!(v["messages"][0]["attention_open"], serde_json::json!(true));
        assert!(
            v["messages"][1].get("tier").is_none(),
            "None tier must not serialize"
        );
        assert!(v["messages"][1].get("attention_open").is_none());
        // is_sent is NEVER skipped: the reader aligns every bubble on it, so
        // both polarities are on the wire as plain bools.
        assert_eq!(v["messages"][0]["is_sent"], serde_json::json!(false));
        assert_eq!(v["messages"][1]["is_sent"], serde_json::json!(true));
        // auth_pass is an internal gate for the human door's `sender_known`
        // bit, never a wire field, even when it holds a verdict.
        assert!(v["messages"][0].get("auth_pass").is_none());
        // `is_sent` is the opposite: ALWAYS on the wire, both ways, because the
        // reader has to aim its actions at inbound mail on every message.
        assert_eq!(v["messages"][0]["is_sent"], serde_json::json!(false));
        assert_eq!(v["messages"][1]["is_sent"], serde_json::json!(true));

        // ...and the key is OPTIONAL inbound too, so a payload minted by a
        // pre-content_id daemon still decodes.
        let old: ClientAttachment = serde_json::from_value(serde_json::json!({
            "id": 7, "filename": "doc.pdf", "mime": "application/pdf",
            "size": 1234, "downloadable": true
        }))
        .expect("attachment without content_id must decode");
        assert!(old.content_id.is_none());
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
            from_name: None,
        }
    }

    /// The agent door leaves `from_name` unset, and an absent display name must
    /// be structurally ABSENT rather than null — same contract the two fields
    /// above already hold, and the reason the MCP path can stay strict.
    #[test]
    fn update_without_from_name_omits_the_key() {
        let v = serde_json::to_value(base_update()).unwrap();
        assert!(
            v.get("from_name").is_none(),
            "None from_name must not serialize"
        );

        let named = Update {
            from_name: Some("Dollar Car Rental".into()),
            ..base_update()
        };
        let v = serde_json::to_value(named).unwrap();
        assert_eq!(v["from_name"], serde_json::json!("Dollar Car Rental"));
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
        assert_eq!(
            fr["importance"],
            serde_json::json!("known contact + appears in Sent mail")
        );
        assert_eq!(fr["tier"], serde_json::json!("known contact -> signal"));
        assert!(
            fr.get("deadline").is_none(),
            "absent deadline reason must be omitted, not null"
        );

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
