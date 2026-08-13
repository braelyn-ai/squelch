// Correction targets for the triage-fix palette (`v`), and the prefix matcher
// that turns what you type into one of them.
//
// Every `value` must exist in TriageAxis::allowed (squelch-core/src/types.rs) —
// the server 400s anything else, and a feedback pair only means something if the
// human's label is one the model could itself have produced. BANDS ARE NOT
// LABELS: "For your eyes" is a server query over tiers, so band words map onto
// the tiers that constitute it (see `lands`) rather than a value that would 400.

import Foundation

enum TriageAxis: String, Sendable, Hashable {
    case tier, category, sensitivity

    /// "sensitivity" is the column name, not a word anyone thinks in — the chip
    /// says what the axis MEANS.
    var chipLabel: String { self == .sensitivity ? "auth" : rawValue }
}

/// What a correction does to the row's current band membership, by the server's
/// own predicates. The palette applies exactly this and no more — modelling a
/// move the server does not make flickers when the forced refresh undoes it.
enum CorrectionExit: Sendable {
    /// Nothing moves: `/client/updates` buckets on tier, status and surfaced_at
    /// only, so a category is not a band.
    case stays
    /// Leaves For-your-eyes only — `new`/`open` are defined by surfaced_at and
    /// status, which a tier correction does not touch.
    case standing
    /// Leaves every band — the base predicate excludes `sensitivity = 'sealed'`.
    case allBands
}

struct TriageTarget: Identifiable, Hashable, Sendable {
    var axis: TriageAxis
    /// The wire value. Must be in TriageAxis::allowed server-side.
    var value: String
    /// What the human sees.
    var label: String
    /// One line on when this is the right answer.
    var hint: String
    /// Extra words that should match this target.
    var aliases: [String]
    /// Words that SURFACE this target without naming it. Ranked below aliases on
    /// purpose: "important" names `signal`, so Enter must keep writing `signal`
    /// while the band's real tiers still appear under it.
    var nudges: [String] = []

    var id: String { "\(axis.rawValue):\(value)" }

    /// Which sitrep surface this value PUTS the mail on, by server predicate
    /// rather than heuristic. nil = it places nothing, and the row stays silent
    /// rather than implying a move.
    var lands: String? {
        switch (axis, value) {
        // standing = a dated obligation OR live correspondence: tier IN
        // ('past_due','deadline'), OR a thread the user has written in, OR a
        // sender the user has written to (contacts.sent_count > 0) — all AND
        // status != 'done', and never sealed or the user's own sent mail. Only
        // the tier arm is reachable from a correction: the correspondence arms
        // are facts about the thread, not values this palette can write.
        case (.tier, "past_due"), (.tier, "deadline"): "For your eyes"
        // Every band excludes sealed mail, and the Auth page IS that set.
        case (.sensitivity, "sealed"): "Auth"
        default: nil
        }
    }

    /// See CorrectionExit — the inverse of `lands`, for the band it leaves.
    var exit: CorrectionExit {
        switch (axis, value) {
        // A signal/noise correction does NOT exit standing: the band's
        // correspondence arms are tier-independent, and nothing on the wire
        // says which arm admitted a row. Optimistically removing one that
        // stands on correspondence just makes it vanish and reappear a round
        // trip later, so .stays is the only truthful mapping; a tier-arm row
        // lingers one refresh instead.
        case (.sensitivity, "sealed"): .allBands
        default: .stays
        }
    }
}

enum TriageTargets {
    static let all: [TriageTarget] = [
        // --- categories: what KIND of mail this is ---------------------------
        TriageTarget(
            axis: .category, value: "invoice", label: "Invoice",
            hint: "a bill you owe and have to pay",
            aliases: ["bill", "billing", "invoice", "owe", "payment", "due"]),
        TriageTarget(
            axis: .category, value: "autopay_bill", label: "Autopay bill",
            hint: "a bill that pays itself; a record, not a task",
            aliases: ["autopay", "auto", "bill", "billing", "subscription", "recurring"]),
        TriageTarget(
            axis: .category, value: "banking_statement", label: "Bank statement",
            hint: "a periodic statement — a record",
            aliases: ["statement", "bank", "banking", "balance"]),
        TriageTarget(
            axis: .category, value: "transaction_alert", label: "Transaction alert",
            hint: "a charge or activity notice",
            aliases: ["transaction", "charge", "alert", "spend", "purchase"]),
        TriageTarget(
            axis: .category, value: "marketing", label: "Marketing",
            hint: "a sale, offer, newsletter or promo blast",
            aliases: [
                "marketing", "newsletter", "promo", "promotional", "ad", "advertising", "sale",
                "offer", "deal",
            ]),
        TriageTarget(
            axis: .category, value: "shipping", label: "Shipping",
            hint: "a shipment or delivery status notice",
            aliases: [
                "shipping", "shipment", "shipped", "package", "parcel", "delivery", "tracking",
            ]),
        TriageTarget(
            axis: .category, value: "general", label: "General",
            hint: "none of the money categories",
            aliases: ["general", "none", "other", "plain"]),

        // --- auth: the sealed axis -------------------------------------------
        // Not a category but `triage.sensitivity`, and the axis with real
        // consequences: sealed mail is structurally absent from the agent door,
        // so moving mail in RESTRICTS what an agent can see and moving it out
        // EXPOSES it. See docs/SECURITY.md §4.
        TriageTarget(
            axis: .sensitivity, value: "sealed", label: "Auth",
            hint: "a code, reset or sign-in alert; hides it from agents",
            aliases: [
                "auth", "sealed", "seal", "code", "otp", "2fa", "mfa", "login", "signin",
                "verification", "password", "reset",
            ]),
        TriageTarget(
            axis: .sensitivity, value: "normal", label: "Not auth",
            hint: "wrongly sealed; unhides it from agents",
            aliases: ["notauth", "unseal", "unsealed", "normal", "notsealed"]),

        // --- tiers: how much it should DEMAND of you -------------------------
        // Tier is what places mail: past_due and deadline ARE the For-your-eyes
        // band, so the band words live on those two and on neither of the
        // others — aliasing "for your eyes" onto `signal` would write a value
        // definitionally not in that band.
        TriageTarget(
            axis: .tier, value: "past_due", label: "Past due",
            hint: "a deadline that has already passed",
            aliases: ["pastdue", "past", "overdue", "late"],
            // Ranks below Deadline for the band words: both land in
            // For-your-eyes, but "past due" also asserts the date has PASSED.
            nudges: ["foryoureyes", "fye", "eyes", "important"]),
        TriageTarget(
            axis: .tier, value: "deadline", label: "Deadline",
            hint: "has a date you must act by",
            aliases: ["deadline", "due", "date", "foryoureyes", "fye", "eyes"],
            nudges: ["important"]),
        TriageTarget(
            axis: .tier, value: "signal", label: "Signal",
            hint: "worth your attention, no deadline",
            // "important" stays the name of THIS tier: plenty of important mail
            // has no date, and demoting it would make Enter write a deadline
            // claim onto mail that has none.
            aliases: ["signal", "important", "attention"]),
        TriageTarget(
            axis: .tier, value: "noise", label: "Noise",
            hint: "should not have surfaced at all",
            // The marketing words deliberately do NOT live here: conflating
            // "this IS marketing" with "this should not have surfaced" teaches
            // the dataset that every promo is unwanted.
            aliases: ["noise", "junk", "ignore", "spam", "quiet"]),
    ]

    /// Normalize for matching: lowercase, and underscores/spaces are the same.
    private static func norm(_ s: String) -> String {
        s.lowercased().filter { $0 != " " && $0 != "_" && $0 != "-" }
    }

    /// Rank a target against what the user typed. Higher = better; 0 hides it.
    /// Deliberately boring — exact beats prefix beats substring, and value beats
    /// alias beats nudge — so which label Enter writes stays predictable.
    static func score(_ target: TriageTarget, query: String) -> Int {
        let q = norm(query)
        if q.isEmpty { return 1 }  // empty query: everything, in declaration order

        let value = norm(target.value)
        let label = norm(target.label)
        if value == q || label == q { return 100 }
        if value.hasPrefix(q) || label.hasPrefix(q) { return 80 }

        var best = 0
        for alias in target.aliases {
            let a = norm(alias)
            if a == q {
                best = max(best, 60)
            } else if a.hasPrefix(q) {
                best = max(best, 40)
            }
        }
        if best > 0 { return best }

        // A nudge ranks under every alias, so it can only ADD an option below
        // the one the typed word names — never replace it.
        for nudge in target.nudges {
            let n = norm(nudge)
            if n == q || n.hasPrefix(q) { return 30 }
        }

        if value.contains(q) || label.contains(q) { return 20 }
        return 0
    }

    /// One target's ranking row: declaration order + its score for a query.
    private struct Ranked {
        var index: Int
        var target: TriageTarget
        var score: Int
    }

    /// The ranked, filtered target list for a query. Stable within equal scores.
    static func match(_ query: String) -> [TriageTarget] {
        var ranked: [Ranked] = []
        for (index, target) in all.enumerated() {
            let s = score(target, query: query)
            if s > 0 { ranked.append(Ranked(index: index, target: target, score: s)) }
        }
        ranked.sort { a, b in a.score != b.score ? a.score > b.score : a.index < b.index }
        return ranked.map(\.target)
    }

    /// Human-facing label for a raw wire value, for showing what it WAS.
    static func label(axis: TriageAxis, value: String?) -> String {
        guard let value, !value.isEmpty else { return "unset" }
        return all.first { $0.axis == axis && $0.value == value }?.label ?? value
    }
}
