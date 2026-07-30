// Swift mirrors of the squelch-core / squelch-api JSON contracts served under
// /client/*. Field names and enum string values match the Rust serde output
// EXACTLY (serde rename_all = "snake_case"). Do not "improve" these.
//
// Ported 1:1 from squelch-desktop/src/api/types.ts. Every enum decodes
// leniently (an unknown wire value becomes `.unknown` rather than throwing) so
// a newer daemon can add a variant without bricking an older client.

import Foundation

// MARK: - lenient enum helper

/// A String-backed enum that never fails to decode: unrecognised wire values
/// fall into the type's `unknownFallback`. The desktop client got this for free
/// (TS unions are erased at runtime); Swift needs it spelled out or one new
/// server-side tier value would fail the whole page decode.
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

    /// User-facing label. "squelch" is the product name, not a UI verb — the
    /// chip reads "mute" while the wire value is unchanged.
    var label: String {
        switch self {
        case .surface: "surface"
        case .squelch: "mute"
        case .filtered: "filter"
        }
    }

    var hint: String {
        switch self {
        case .surface: "always surface — never mute this sender"
        case .squelch: "mute — keep out of the sitrep unless it escalates"
        case .filtered: "filter — drop before triage entirely"
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
    /// Tier is past_due/deadline — the standing band, immune to thresholds.
    case urgent
    /// A deadline on a message that is not itself urgent-tier.
    case deadline
    /// Importance landed at or above the notify threshold.
    case surfaced
    /// A kind this build has never heard of is still worth a banner — it only
    /// loses the urgent styling.
    static var unknownFallback: EventKind { .surfaced }
}

/// core::types::Event — one row of the durable notification log, delivered as
/// SSE frames on GET /client/events.
///
/// Every field is a snapshot taken at emission time, deliberately: a client
/// renders the whole notification from this row alone, with no second round
/// trip to spend. SEALED MAIL CAN NEVER APPEAR HERE (enforced server-side).
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

/// core::types::AttentionUpdate. Rust `#[serde(flatten)]`s `Update` into this
/// struct, so on the wire it is ONE flat object (Update fields + the three
/// lifecycle fields). `surfaced_at == nil` => NEW.
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
    var status: AttentionStatus
    var surfaced_at: String?
    var resolved_at: String?

    var hasAttachments: Bool { has_attachments ?? false }
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
}

/// core::types::ClientMessage — the HUMAN-door message shape. `html` is a
/// server-side-sanitized (ammonia) string, or nil for plain-text-only mail.
struct ClientMessage: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var from_addr: String
    var from_name: String?
    var received_at: String
    var content: String
    var html: String?
    var attachments: [Attachment]?

    var attachmentList: [Attachment] { attachments ?? [] }

    /// "Name <addr>" when a display name exists, else the bare address — the
    /// string senderDisplayName() expects.
    var senderString: String {
        if let n = from_name, !n.isEmpty { return "\(n) <\(from_addr)>" }
        return from_addr
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
    var first_seen: String
    var last_update: String
}

// MARK: - receipts / calendar / banking

struct Receipt: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var account_id: Int
    var message_id: Int
    var thread_id: String?
    var from_addr: String
    var from_name: String?
    var amount: Double?
    var currency: String?
    var received_at: String

    var senderString: String {
        if let n = from_name, !n.isEmpty { return "\(n) <\(from_addr)>" }
        return from_addr
    }
}

enum CalendarKind: String, LenientRawEnum {
    case invite, update, cancellation, response
    static var unknownFallback: CalendarKind { .invite }

    /// Row tag for the non-default kinds; a plain invite needs no label.
    var tag: String? {
        switch self {
        case .invite: nil
        case .update: "updated"
        case .cancellation: "canceled"
        case .response: "rsvp"
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
    var disposition: Disposition
}

struct CreatedRule: Codable, Sendable { var rule_id: Int }

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

struct StoreStats: Codable, Sendable, Hashable {
    var tier_counts: [String: Int]
    var total: Int
    var sealed: Int
    var last_history_id: Int?
    var bands: BandCounts
    var last_surfaced_at: String?
    var stage2: Stage2Stats?
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

struct SealedMeta: Codable, Sendable, Identifiable, Hashable {
    var id: Int
    var thread_id: String
    var sender: String
    var subject: String
    var kind: String?
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
    var kind: String?
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
    var to: String?
    var subject: String?
    var body: String
    var confirm: Bool
    var override_guard: Bool?
}

struct StatusResult: Codable, Sendable {
    var status: String
    var message_id: Int?
}

struct SendResult: Codable, Sendable { var status: String }
struct RefreshResult: Codable, Sendable { var triggered: Bool }
struct RetriageResult: Codable, Sendable { var reset: Int }
struct UnsubResolutionResult: Codable, Sendable {
    var sender: String
    var resolution: String
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

/// store::MarketingOffer (GET /client/marketing). Deliberately carries NO url:
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

// MARK: - query params

struct UpdatesParams: Sendable {
    var since: String?
    var min_importance: Int?
    var tier: Tier?
    var status: AttentionStatus?
    var band: Band?
    var limit: Int?
    var cursor: String?

    init(
        since: String? = nil,
        min_importance: Int? = nil,
        tier: Tier? = nil,
        status: AttentionStatus? = nil,
        band: Band? = nil,
        limit: Int? = nil,
        cursor: String? = nil
    ) {
        self.since = since
        self.min_importance = min_importance
        self.tier = tier
        self.status = status
        self.band = band
        self.limit = limit
        self.cursor = cursor
    }
}
