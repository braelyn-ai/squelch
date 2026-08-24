// Swift mirrors of the squelch-core / squelch-api JSON contracts under
// /client/*. THESE SHAPES ARE A WIRE CONTRACT: field names and enum string
// values match the Rust serde output exactly (rename_all = "snake_case"), so do
// not "improve" them. Every enum decodes leniently (an unknown wire value falls
// back) so a newer daemon can add a variant without bricking an older client.

import Foundation

// MARK: - lenient enum helper

/// A String-backed enum that never fails to decode: an unrecognised wire value
/// becomes `unknownFallback`. Without this, one new server-side tier value
/// would fail the whole page decode.
protocol LenientRawEnum: RawRepresentable, Codable, Sendable, Hashable
where RawValue == String {
    static var unknownFallback: Self { get }
}

extension LenientRawEnum {
    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = Self(rawValue: raw) ?? Self.unknownFallback
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        try c.encode(rawValue)
    }
}

// MARK: - sender pair

/// A wire type carrying the `from_name` / `from_addr` pair, so the one true
/// rendering of a sender lives in a single place.
protocol SenderStringConvertible {
    var from_addr: String { get }
    var from_name: String? { get }
}

extension SenderStringConvertible {
    /// "Name <addr>" when a display name exists, else the bare address — the
    /// shape `SenderID.parse` expects.
    var senderString: String {
        if let n = from_name, !n.isEmpty { return "\(n) <\(from_addr)>" }
        return from_addr
    }
}

// MARK: - core enums

enum Tier: String, LenientRawEnum {
    case pastDue = "past_due"
    case deadline
    case signal
    case noise
    static var unknownFallback: Tier { .signal }

    /// Display label used by chips/debug rows.
    var label: String { rawValue }
}

enum AttentionStatus: String, LenientRawEnum {
    case new, open, done
    static var unknownFallback: AttentionStatus { .new }
}

enum Disposition: String, LenientRawEnum, CaseIterable {
    case surface, squelch, filtered
    static var unknownFallback: Disposition { .squelch }

    /// User-facing label. The wire values are the daemon's, not UI verbs — the
    /// chips read allow/mute/filter while `surface`/`squelch` stay on the wire.
    var label: String {
        switch self {
        case .surface: "allow"
        case .squelch: "mute"
        case .filtered: "filter"
        }
    }

    var hint: String {
        switch self {
        case .surface: "allow: always surface this sender, never mute them"
        case .squelch: "mute: keep out of the sitrep unless it escalates"
        case .filtered: "filter: a plain english line decides what gets through, in either direction"
        }
    }
}

/// Server-side sitrep bucket (query param `band` on /client/updates).
enum Band: String, Sendable, CaseIterable {
    case standing, new, open
}

// MARK: - notification events

/// core::types::EventKind — why an event was emitted. Server-side precedence
/// when classifying a verdict is urgent > deadline > surfaced.
enum EventKind: String, LenientRawEnum {
    /// Tier is past_due/deadline — the dated-obligation tiers, immune to
    /// thresholds. A subset of the standing band, which also holds live
    /// correspondence; a row in the band on that footing earns no event here.
    case urgent
    /// A deadline on a message that is not itself urgent-tier.
    case deadline
    /// Importance landed at or above the notify threshold.
    case surfaced
    /// Somebody opened the user's OWN tracked outbound mail. Not a triage
    /// verdict at all: `sender` is the account's own address, `one_line` is the
    /// sent subject, and `importance` is a fixed placeholder rather than a
    /// score. One per message ever, however many times it is reopened.
    case opened
    /// An unheard-of kind still earns a banner; it only loses urgent styling.
    static var unknownFallback: EventKind { .surfaced }
}

/// core::types::Event — one row of the durable notification log, delivered as
/// SSE frames on GET /client/events. Every field is a snapshot taken at
/// emission time, so a client renders the whole notification from this row with
/// no second round trip. SEALED MAIL CAN NEVER APPEAR HERE (enforced
/// server-side; see docs/SECURITY.md §4).
struct Event: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var kind: EventKind
    var message_id: Int
    var thread_id: String
    var tier: Tier
    var importance: Int
    var sender: String
    var one_line: String
    /// Snapshotted RFC3339 text, or nil. Display copy only.
    var deadline: String?
    var created_at: String
}

// MARK: - updates

/// Per-property triage justifications. An absent key means no reason was
/// recorded for that property.
struct FieldReasons: Codable, Sendable, Hashable {
    var importance: String?
    var deadline: String?
    var tier: String?
}

/// core::types::AttentionUpdate. Rust `#[serde(flatten)]`s `Update` in, so the
/// wire shape is ONE flat object. `surfaced_at == nil` => NEW.
struct AttentionUpdate: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var thread_id: String
    var tier: Tier
    var importance: Int
    var sender: String
    var one_line: String
    var reason: String
    var deadline: String?
    var matched_rule: Int?
    var field_reasons: FieldReasons?
    /// ABSENT on rows served by a pre-attachment daemon — treat as false.
    var has_attachments: Bool?
    /// The `From:` display name. ABSENT on rows served by an older daemon, and
    /// on the agent door, which is why every read goes through `senderString`
    /// rather than this directly.
    var from_name: String?
    var status: AttentionStatus
    var surfaced_at: String?
    var resolved_at: String?
    /// A PENDING reminder: when the daemon will pull this back into the bands.
    /// Non-nil implies the row is done — setting a reminder resolves the thread
    /// — so a row carrying one is not "unfinished", it is parked. HUMAN DOOR
    /// ONLY: the agent door never serves either of these fields, which is why
    /// both are optional rather than merely nullable.
    var remind_at: String?
    /// A reminder that FIRED, cleared the next time the row is resolved. It is
    /// what re-enters the standing band regardless of tier: a reminder is the
    /// user declaring the mail owed attention, and that outranks a verdict.
    var reminded_at: String?

    var hasAttachments: Bool { has_attachments ?? false }

    /// Whether a reminder is waiting to fire on this row.
    var hasPendingReminder: Bool { !(remind_at ?? "").isEmpty }
}

/// The wire calls the address `sender` here and `from_addr` everywhere else, so
/// this is the one place that reconciles the two. With it, an update renders its
/// sender through exactly the same path as a message, a receipt or a search hit.
extension AttentionUpdate: SenderStringConvertible {
    var from_addr: String { sender }
}

// MARK: - thread / messages

/// core::types::Attachment. `downloadable` is false when the raw bytes were NOT
/// stored (over the ingest cap): metadata exists but the bytes route 410s.
struct Attachment: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var filename: String
    var mime: String
    var size: Int
    var downloadable: Bool
    /// The part's Content-ID with its angle brackets already off, which is what
    /// a body's `<img src="cid:…">` names — see CidImages. ABSENT on a daemon
    /// that predates the field and null for a part that declared none, so it
    /// stays optional: the client's answer to both is the same, which is to drop
    /// the reference rather than paint a broken image.
    var content_id: String?
}

/// core::types::ClientMessage — the HUMAN-door message shape. `html` is a
/// server-side-sanitized (ammonia) string, or nil for plain-text-only mail.
struct ClientMessage: Codable, Sendable, Identifiable, Hashable, SenderStringConvertible {
    var id: Int
    var from_addr: String
    var from_name: String?
    var received_at: String
    var content: String
    var html: String?
    var attachments: [Attachment]?
    /// This message's OWN triage verdict — ABSENT on a pre-highlight daemon.
    /// Drives the in-thread attention highlight: the bands show one row per
    /// thread, so the reader is where "which message is the reason" is answered.
    var tier: Tier?
    var deadline: String?
    /// Whether the attention row is still unresolved; a resolved obligation
    /// must not keep glowing.
    var attention_open: Bool?
    var one_line: String?
    /// True when `from_addr` is somebody the user has sent mail to (the daemon's
    /// Sent-derived contacts, computed per request). ABSENT on a pre-tracking
    /// daemon, which reads as unknown — the strict side. Governs the reader's
    /// tracker strip: see `allowsTrackers`.
    var sender_known: Bool?
    /// Whether the USER sent this one. ABSENT on a pre-sent-flag daemon, which
    /// reads as unknown rather than as inbound — a nil here must not make the
    /// reader treat a reply as somebody else's mail. Picks which message in a
    /// thread a reminder lands on: see ThreadViewer's `h`.
    var is_sent: Bool?

    var attachmentList: [Attachment] { attachments ?? [] }

    /// Whether this message's tracking pixels may load. Trusted people are
    /// allowed to learn their mail was opened; everyone else is stripped as
    /// before. `nil` (old daemon) is NOT allowed — the default stays private.
    var allowsTrackers: Bool { sender_known ?? false }

    /// The highlight predicate: an UNRESOLVED row in a dated-obligation tier.
    /// Deliberately NARROWER than the for-your-eyes band, which also carries
    /// live correspondence: the mark answers "which message is the obligation",
    /// and a mark on every message from a known contact answers nothing.
    var needsAttention: Bool {
        (attention_open ?? false) && (tier == .pastDue || tier == .deadline)
    }
}

/// core::types::ClientThreadView (GET /client/thread/{id}).
struct ClientThreadView: Codable, Sendable, Hashable {
    var thread_id: String
    var subject: String
    var messages: [ClientMessage]
}

// MARK: - shipments

enum Carrier: String, LenientRawEnum {
    case ups, usps, fedex, dhl, amazon, unknown
    static var unknownFallback: Carrier { .unknown }

    var label: String {
        switch self {
        case .unknown: "carrier"
        case .ups, .usps, .dhl: rawValue.uppercased()
        case .fedex: "FedEx"
        case .amazon: "Amazon"
        }
    }

    /// Domain whose favicon stands in for the carrier; nil falls back to a glyph.
    var faviconDomain: String? {
        switch self {
        case .ups: "ups.com"
        case .usps: "usps.com"
        case .fedex: "fedex.com"
        case .dhl: "dhl.com"
        case .amazon, .unknown: nil
        }
    }
}

enum ShipmentStatus: String, LenientRawEnum {
    case ordered
    case shipped
    case outForDelivery = "out_for_delivery"
    case delivered
    case exception
    static var unknownFallback: ShipmentStatus { .ordered }

    var label: String {
        switch self {
        case .ordered: "ordered"
        case .shipped: "shipped"
        case .outForDelivery: "out for delivery"
        case .delivered: "delivered"
        case .exception: "exception"
        }
    }
}

struct Shipment: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var account_id: Int
    var tracking_number: String
    var carrier: Carrier
    var item_name: String
    var status: ShipmentStatus
    var tracking_url: String?
    /// Thread of the latest email whose status stuck; absent from an older
    /// daemon, and then the card simply isn't clickable.
    var thread_id: String?
    var first_seen: String
    var last_update: String
    /// THE CARRIER-POLL FIELDS. Every one of them is absent from an older
    /// daemon's rows, so every one is optional and every reader must render the
    /// pre-poll card when it is nil.
    ///
    /// Carrier-estimated delivery; nil when the carrier gives none. A POLL is
    /// the only thing that ever sets it — no email carries one.
    var eta: String?
    /// The carrier's own status words, verbatim, including the ones our five-rung
    /// `status` could not express. Nil until the first poll. SOMEBODY ELSE'S
    /// TEXT: flatten and cap it before drawing it.
    var carrier_status_raw: String?
    /// When it landed, from whichever path saw it first; nil until delivered.
    var delivered_at: String?
    /// Last carrier poll ATTEMPT; nil = never asked.
    var last_polled_at: String?
    /// Consecutive polls the carrier answered with "no record of this number".
    /// Rate limits and transport errors never count, which is what makes a
    /// nonzero value evidence about the NUMBER rather than about the network.
    var poll_failures: Int?
}

/// What `POST /client/shipments/poll` answers. `kicked` is false, with an empty
/// `carriers`, on a daemon holding no carrier credentials: polling is BYOK, so
/// that is the ordinary state and not an error, but it does mean nothing
/// happened and the UI must not claim otherwise.
struct ShipmentPollKick: Codable, Sendable {
    var kicked: Bool
    var carriers: [String]
}

// MARK: - receipts / calendar / banking

struct Receipt: Codable, Sendable, Identifiable, Hashable, SenderStringConvertible {
    var id: Int
    var account_id: Int
    var message_id: Int
    var thread_id: String?
    var from_addr: String
    var from_name: String?
    var amount: Double?
    var currency: String?
    var received_at: String
}

enum CalendarKind: String, LenientRawEnum {
    case invite, update, cancellation, response, reservation
    static var unknownFallback: CalendarKind { .invite }

    /// Row tag for the non-default kinds; a plain invite needs no label.
    var tag: String? {
        switch self {
        case .invite: nil
        case .update: "updated"
        case .cancellation: "canceled"
        case .response: "rsvp"
        case .reservation: "reservation"
        }
    }
}

struct CalendarUpdate: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var message_id: Int
    var thread_id: String?
    var kind: CalendarKind
    var event_title: String?
    var starts_at: String?
    var organizer: String?
    var received_at: String
}

enum BankingKind: String, LenientRawEnum {
    case statement
    case transactionAlert = "transaction_alert"
    case autopay
    static var unknownFallback: BankingKind { .statement }

    var tag: String {
        switch self {
        case .statement: "statement"
        case .transactionAlert: "alert"
        case .autopay: "autopay"
        }
    }
}

struct BankingRecord: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var message_id: Int
    var thread_id: String?
    var from_addr: String?
    var kind: BankingKind
    var institution: String?
    var amount: Double?
    var currency: String?
    var account_hint: String?
    var received_at: String
}

// MARK: - rules

struct SenderRule: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var account_id: Int
    var match_pattern: String
    var want_text: String
    var disposition: Disposition
    var updated_at: String
}

struct CreateRuleBody: Codable, Sendable {
    var match_pattern: String
    var want: String
    /// ABSENT asks the daemon to infer the outcome from `want` — the owner's own
    /// sentence, and nothing else, ever. Encodes as absent when nil, the same way
    /// `source_message_id` does. A caller that means one specific outcome (the
    /// block flow, an undo restoring a deleted rule, an explicit override in the
    /// editor's advanced section) still names it, and a named value wins.
    var disposition: Disposition?
    /// The message the block was invoked FROM, when there is one on screen: a
    /// squelch rule resolves it server-side (blocking is that email's
    /// disposition). Encodes as absent when nil, so the rule editor's plain
    /// creates are unchanged.
    var source_message_id: Int? = nil
    /// Asks the daemon to ALSO resolve every open message already on file from
    /// this sender, not just the one the rule was created from. Encodes as
    /// absent when nil and the server reads absent as false, so only a caller
    /// that means "clear this sender out of my inbox" sends it.
    ///
    /// Deliberately separate from an explicit `.squelch`: stating the outcome
    /// says what the rule IS, this says what to do about mail that already
    /// arrived. An undo recreating a deleted mute rule needs the first without
    /// the second, so it omits this and nothing gets swept.
    var sweep: Bool? = nil
}

/// POST /client/rules and PUT /client/rules/{id}.
struct CreatedRule: Codable, Sendable {
    var rule_id: Int
    /// What the rule ACTUALLY ended up as, which is the only place the client
    /// can learn the answer when it asked for inference. Optional because a
    /// pre-inference daemon answers with `rule_id` alone: absent means "the
    /// server did not say", never "no disposition".
    var disposition: Disposition?
}

// MARK: - unsubscribe

enum UnsubscribeMethod: String, LenientRawEnum {
    case browser
    case oneClick = "one_click"
    case mailto
    static var unknownFallback: UnsubscribeMethod { .browser }
}

enum UnsubResolution: String, LenientRawEnum {
    case blocked, dismissed
    static var unknownFallback: UnsubResolution { .dismissed }
}

struct UnsubscribeResult: Codable, Sendable {
    var method: String
    var sender: String
    var url: String
}

struct UnsubscribeRecord: Codable, Sendable, Hashable, Identifiable {
    var sender: String
    var requested_at: String
    var method: UnsubscribeMethod
    var violation_count: Int
    var last_violation_at: String?
    var resolution: UnsubResolution?

    var id: String { sender }
}

// MARK: - search / audit / stats

struct SearchHit: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var thread_id: String
    var from_addr: String
    var from_name: String?
    var subject: String
    var received_at: String
    var snippet: String
}

struct AuditEntry: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var account_id: Int
    var ts: String
    var actor: String
    var action: String
    var target: String?
    var detail: String?
    var target_sender: String?
    var target_subject: String?
}

struct BandCounts: Codable, Sendable, Hashable {
    var standing: Int
    var new: Int
    var open: Int
}

struct Stage2Stats: Codable, Sendable, Hashable {
    var est_cost_usd_today: Double?
}

/// Gmail's own INBOX unread counters, as the daemon last saw them. NOT a
/// passband number: it is what the mailbox says before triage has an opinion,
/// which is the only honest way to say how much mail was waiting.
struct InboxUnread: Codable, Sendable, Hashable {
    var messages: Int
    var threads: Int
}

struct StoreStats: Codable, Sendable, Hashable {
    var tier_counts: [String: Int]
    var total: Int
    var sealed: Int
    var last_history_id: Int?
    var bands: BandCounts
    var last_surfaced_at: String?
    var stage2: Stage2Stats?
    /// ABSENT on a daemon too old to fetch it, and absent for as long as the
    /// first fetch has not landed — so nil means "we do not know", never zero.
    /// Callers must have copy for both.
    var inbox_unread: InboxUnread?
    /// Whether this daemon can relay ⌘K assistant calls to a hosted gateway
    /// (POST /client/assistant/messages). ABSENT on a daemon too old to say —
    /// and nil reads exactly like false: BYOK is the only assistant door.
    var assistant_relay: Bool?
    /// Whether this daemon can mint and send invites (POST /client/invites).
    /// ABSENT on a daemon too old to say, and on every self-host, and nil reads
    /// exactly like false: no button, no nudge, nothing offered.
    var invite_sharing: Bool?
}

// MARK: - invites

/// What GET /client/invites answers: whether sharing is possible at all, and
/// the one number the invite mail would be able to say about this mailbox.
///
/// `open_percent` is NIL far more often than not, and that is by design on the
/// daemon's side (too new a mailbox, too little mail, or a rate not worth
/// quoting). The sheet must have copy for both, and it must never invent one.
struct InviteAvailability: Codable, Sendable, Hashable {
    var can_share: Bool
    /// WHY not, when `can_share` is false. `no_control_plane` is a property of
    /// the deployment and nothing the reader can act on; `no_write_credential`
    /// names a command that fixes it. Absent when sharing is possible, and on a
    /// daemon too old to say — which the sheet reads as the first, the more
    /// conservative of the two, because telling somebody to run a command that
    /// will not help is worse than telling them nothing.
    var reason: String?
    var open_percent: Int?
    /// The mail as it will go out, rendered by the DAEMON so the preview on
    /// screen cannot drift from what is sent. Absent when this daemon cannot
    /// share (there is no mail to preview) and on one too old to render one.
    var preview: String?
}

/// One friend's outcome. `error` is copy the daemon wrote for a human; it never
/// carries a status code or anything about the invite code itself.
struct InviteResult: Codable, Sendable, Hashable, Identifiable {
    var email: String
    var sent: Bool
    var error: String?
    var id: String { email }
}

/// What POST /client/invites answers. `remaining` is nil when nothing was
/// minted at all, so it is never rendered as "0 left" for a press that failed
/// before it reached the control plane.
struct InviteSendResponse: Codable, Sendable, Hashable {
    var results: [InviteResult]
    var remaining: Int?
}

// MARK: - usage

struct UsageRow: Codable, Sendable, Hashable, Identifiable {
    var day: String
    var calls: Int
    var input_tokens: Int
    var output_tokens: Int
    var id: String { day }
}

struct UsageTotals: Codable, Sendable, Hashable {
    var calls: Int
    var input_tokens: Int
    var output_tokens: Int
    var est_cost_usd: Double
}

struct UsageCategory: Codable, Sendable, Hashable {
    var rows: [UsageRow]
    var totals: UsageTotals
    var model: String
    var provider: String?
}

struct UsageResponse: Codable, Sendable, Hashable {
    var rows: [UsageRow]
    var totals: UsageTotals
    var provider: String?
    var model: String
    var categories: [String: UsageCategory]?
}

// MARK: - triage config

enum TriageConfigSource: String, LenientRawEnum {
    case `default`, config, override
    static var unknownFallback: TriageConfigSource { .default }

    var note: String {
        switch self {
        case .config: "(from config file)"
        case .override: "(app override)"
        case .default: "(default)"
        }
    }
}

struct TriageConfigSources: Codable, Sendable, Hashable {
    var thread_daily_cap: TriageConfigSource
    var sender_daily_cap: TriageConfigSource
    var global_daily_cap: TriageConfigSource
}

struct TriageStage1: Codable, Sendable, Hashable {
    var model: String
    var global_daily_cap: Int
    var source: TriageConfigSource
    var avg_calls_per_day: Double
    var avg_tokens_in_per_call: Double?
    var avg_tokens_out_per_call: Double?
    var price_in_per_mtok: Double
    var price_out_per_mtok: Double
}

struct TriageConfig: Codable, Sendable, Hashable {
    var thread_daily_cap: Int
    var sender_daily_cap: Int
    var global_daily_cap: Int
    var sources: TriageConfigSources
    var avg_inbound_per_day: Double
    var avg_stage2_calls_per_day: Double
    var avg_tokens_in_per_call: Double?
    var avg_tokens_out_per_call: Double?
    var price_in_per_mtok: Double
    var price_out_per_mtok: Double
    var stage1: TriageStage1
    var stage2_model: String
}

struct TriageConfigPatch: Codable, Sendable {
    var thread_daily_cap: Int?
    var sender_daily_cap: Int?
    var global_daily_cap: Int?
    var stage1_global_daily_cap: Int?
}

// MARK: - sealed

/// core::types::SealedKind — why a message was sealed out of the sitrep.
/// Lenient like the rest, but it must NOT collapse an unknown value onto a known
/// case: otp/verification auto-reveal, so a kind this build has never heard of
/// keeps its raw string in `.unknown`, stays inert, and re-encodes verbatim.
enum SealedKind: LenientRawEnum {
    case otp
    case passwordReset
    case magicLink
    case loginAlert
    case verification
    case unknown(String)

    var rawValue: String {
        switch self {
        case .otp: "otp"
        case .passwordReset: "password_reset"
        case .magicLink: "magic_link"
        case .loginAlert: "login_alert"
        case .verification: "verification"
        case .unknown(let raw): raw
        }
    }

    /// Total, so the LenientRawEnum decode never has to fall back — and so no
    /// wire string is ever dropped on the way in.
    init(rawValue: String) {
        switch rawValue {
        case "otp": self = .otp
        case "password_reset": self = .passwordReset
        case "magic_link": self = .magicLink
        case "login_alert": self = .loginAlert
        case "verification": self = .verification
        default: self = .unknown(rawValue)
        }
    }

    /// Unreachable — `init(rawValue:)` never fails; the protocol still wants it.
    static var unknownFallback: SealedKind { .unknown("") }

    /// Identity IS the wire string, so `.unknown("otp")` can never become a
    /// second spelling of `.otp` that set membership would miss.
    static func == (a: SealedKind, b: SealedKind) -> Bool { a.rawValue == b.rawValue }
    func hash(into hasher: inout Hasher) { hasher.combine(rawValue) }
}

struct SealedMeta: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var thread_id: String
    var sender: String
    var subject: String
    var kind: SealedKind?
    var received_at: String
}

/// POST /client/sealed/{id}/reveal. `body` is a sensitive one-time reveal:
/// hold in view state only, never persist.
struct RevealedSealed: Codable, Sendable {
    var id: Int
    var thread_id: String
    var sender: String
    var from_name: String?
    var subject: String
    var kind: SealedKind?
    var received_at: String
    var body: String
    var html: String?
}

// MARK: - envelopes / action bodies

struct Page<T: Codable & Sendable>: Codable, Sendable {
    var items: [T]
    var next_cursor: String?
}

struct ArchiveBody: Codable, Sendable {
    var message_id: Int
    var confirm: Bool
}

struct LabelBody: Codable, Sendable {
    var message_id: Int
    var add: [String]?
    var remove: [String]?
    var confirm: Bool
}

struct SendBody: Codable, Sendable {
    var reply_to_message_id: Int?
    /// The message being PASSED ON, and the whole of a forward on this wire:
    /// the daemon quotes the original, carries its attachments over and starts
    /// a NEW thread from it. MUTUALLY EXCLUSIVE with `reply_to_message_id` —
    /// the daemon rejects a body naming both — and it requires a non-empty
    /// `to`, because a forward has nobody to derive a recipient from. An empty
    /// `body` is fine here and only here: passing mail on without a word of
    /// your own is the ordinary case.
    var forward_of_message_id: Int?
    var to: String?
    /// Omitted (not "") on a reply: the daemon derives `Re: <parent subject>`
    /// only when the field is absent — `Some("")` is an explicit empty subject.
    var subject: String?
    var body: String
    /// `"markdown"`: `body` is markdown source — it goes out verbatim as the
    /// text/plain part and the daemon renders the HTML alternative beside it.
    /// Absent = single-part plain text, exactly the pre-markdown wire.
    var body_format: String?
    var confirm: Bool
    var override_guard: Bool?
    /// Server-side draft to delete once the send succeeds.
    var draft_id: Int?
    /// Mint a read-tracking pixel for this send. OMITTED when false, the same
    /// way `subject` is: absent reads as false server-side. `true` on a daemon
    /// with no tracking base_url is NOT an error — the mail goes out untracked.
    var include_tracker: Bool?
    /// Answer EVERYONE on the parent, not just its sender. Omitted when false,
    /// like the two above. The daemon derives the recipient set itself at send
    /// time — the client never enumerates it into `to`, so a stale or failed
    /// preview cannot change who the mail actually reaches.
    var reply_all: Bool?
}

/// GET /client/messages/{id}/reply_recipients?all=true — the addresses a reply
/// to this message would carry, as the daemon derives them. Asked for so the
/// review pane can state the REAL set rather than guess at it; the send derives
/// it again server-side, so this answer is display-only and never rides back.
/// `cc` is absent or empty when the parent has no other participants.
struct ReplyRecipients: Codable, Sendable, Hashable {
    var to: String
    var cc: String?
}

// MARK: - read tracking

/// GET /client/messages/{id}/opens — one recorded fetch of a sent message's
/// pixel. `{id}` is the LOCAL message id, i.e. the `echo_message_id` a send
/// handed back; an untracked or unknown id answers with an empty list, so
/// asking about somebody else's message is a legal question with a dull answer.
struct MessageOpen: Codable, Sendable, Hashable {
    /// Unix SECONDS — the one stamp on this wire that is not RFC3339 text.
    var opened_at: Int
    var user_agent: String?
    /// "proxied" or "unknown". Kept as a String because the daemon's set is
    /// open-ended: an unrecognised value must read as the weaker claim rather
    /// than be collapsed onto the stronger one.
    var classification: String

    var date: Date { Date(timeIntervalSince1970: TimeInterval(opened_at)) }

    /// The fetch came from Gmail's image proxy, which may be warming a cache
    /// rather than showing a human the mail. The copy says so.
    var viaProxy: Bool { classification == "proxied" }
}

struct MessageOpensResult: Codable, Sendable {
    var opens: [MessageOpen]
}

/// GET/POST /client/tracking-config. `configured` false means the daemon has
/// nowhere to point a pixel, so every send goes out untracked whatever the
/// client asks — hide the affordance rather than offer a dead switch.
/// `default_enabled` is remembered CLIENT preference: the daemon never reads it
/// when sending, every send states its own `include_tracker`.
struct TrackingConfig: Codable, Sendable, Hashable {
    var default_enabled: Bool
    var configured: Bool
}

/// POST /client/tracking-config. An omitted field leaves the stored value alone.
struct TrackingConfigBody: Codable, Sendable {
    var default_enabled: Bool?
}

/// GET /client/contacts — one recipient-autocomplete hit, ranked server-side
/// (prefix > substring, then frequency and recency).
struct ContactHit: Codable, Sendable, Equatable, Identifiable {
    var addr: String
    var display_name: String?
    var sent_count: Int
    var last_sent_at: String?

    var id: String { addr }
}

struct StatusResult: Codable, Sendable {
    var status: String
    var message_id: Int?
}

struct SendResult: Codable, Sendable {
    var status: String
    /// The sent copy as it landed in the local store, and its thread — both null
    /// when the send succeeded but the echo has not been ingested yet.
    var echo_message_id: Int?
    var thread_id: String?
}
struct RefreshResult: Codable, Sendable { var triggered: Bool }
struct RetriageResult: Codable, Sendable { var reset: Int }
struct UnsubResolutionResult: Codable, Sendable {
    var sender: String
    var resolution: String
}

// MARK: - drafts

/// One saved draft. LOCAL and human-door only: never a Gmail draft, never
/// synced, never reachable from the agent door. `to` is the send endpoint's own
/// spelling of the field, not the store's `to_addr`.
struct DraftView: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    /// The message this answers. nil = the account's single new-message draft.
    var reply_to_message_id: Int?
    var to: String
    var subject: String
    var body: String
    var created_at: String
    var updated_at: String
}

/// PUT /client/drafts — upsert keyed on `reply_to_message_id`. Every text field
/// is optional server-side (a half-composed draft is the normal case), but the
/// composer always knows all three, so all three go.
struct DraftBody: Codable, Sendable {
    var reply_to_message_id: Int?
    var to: String
    var subject: String
    var body: String
}

// MARK: - triage debug / shredder / feedback / marketing

struct TriageDebug: Codable, Sendable, Hashable {
    var message_id: Int
    var subject: String
    var importance: Int
    var tier: String
    var category: String?
    var one_line: String
    var reason: String
    var field_reasons: FieldReasons?
    var deadline: String?
    var matched_rule_id: Int?
    var status: String
    var surfaced_at: String?
    var resolved_at: String?
    var stage1_model_used: String?
    var model_used: String?
    var needs_stage2: Bool
    var extractor_model_used: String?
    var created_at: String
    /// Optional so a client pointed at a daemon older than the field still
    /// decodes the page rather than failing the whole read.
    var thread_id: String?
}

struct ShredStats: Codable, Sendable, Hashable {
    var enabled: Bool
    var after_days: Int
    var shredded_recent: Int
    var shredded_total: Int
    var last_shredded_at: String?
    var pending: Int
    var write_ready: Bool
}

struct ShredderPatch: Codable, Sendable {
    var enabled: Bool?
    var after_days: Int?
}

struct ShredderRunResult: Codable, Sendable {
    var shredded: Int
    var stats: ShredStats
}

struct TriageFeedback: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var message_id: Int
    var corrected_at: String
    var dimension: String
    var from_value: String?
    var to_value: String
    var sender: String
    var subject: String
    var note: String?
}

/// store::MarketingOffer (GET /client/marketing). Deliberately carries NO url —
/// a model-emitted, email-derived link rendered as clickable is a
/// prompt-injection lever.
struct MarketingOffer: Codable, Sendable, Hashable {
    var message_id: Int
    var thread_id: String
    var sender: String
    var subject: String
    var brand: String?
    var offer: String?
    var discount: String?
    var code: String?
    var expires_at: String?
    var received_at: String
}

// MARK: - sent

/// One row of GET /client/sent — mail the user WROTE, newest first (the daemon
/// orders on received_at DESC, id DESC and the page renders that order as-is).
///
/// A deliberately different shape from `AttentionUpdate`: outbound mail has no
/// tier, no importance and no triage status, because nothing triaged it. What it
/// has instead is `to` (the display recipient list, comma-joined "Name <addr>",
/// EMPTY when the header carried nothing usable) and `opens` — the count of
/// recorded read receipts, which is 0 both for a send nobody opened and for a
/// send that never armed the pixel. Those two are the same silence by design;
/// see ReadReceipt.swift.
struct SentItem: Codable, Sendable, Identifiable, Hashable {
    /// The local message id, so a row keys and prefetches like any other.
    var id: Int
    var thread_id: String
    var to: String
    var subject: String
    var snippet: String
    var sent_at: String
    var opens: Int
}

// MARK: - query params

struct UpdatesParams: Sendable {
    var since: String?
    var min_importance: Int?
    var tier: Tier?
    var status: AttentionStatus?
    var band: Band?
    var limit: Int?
    var cursor: String?
    /// Narrow to rows with a PENDING reminder, soonest first. Named for the
    /// flag rather than the query value (`reminders=pending`) because unlike
    /// every field above it, it is a filter with its own ORDER: the server
    /// sorts these by remind_at, not by arrival, and it serves done rows —
    /// which every other read here would have excluded.
    var remindersPending: Bool

    init(
        since: String? = nil,
        min_importance: Int? = nil,
        tier: Tier? = nil,
        status: AttentionStatus? = nil,
        band: Band? = nil,
        limit: Int? = nil,
        cursor: String? = nil,
        remindersPending: Bool = false
    ) {
        self.since = since
        self.min_importance = min_importance
        self.tier = tier
        self.status = status
        self.band = band
        self.limit = limit
        self.cursor = cursor
        self.remindersPending = remindersPending
    }
}
