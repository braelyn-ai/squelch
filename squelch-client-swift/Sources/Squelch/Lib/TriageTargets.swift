// Correction targets for the triage-fix palette (`v`), and the prefix matcher
// that turns what you type into one of them.
//
// The values here MIRROR the pipeline exactly — TriageAxis::allowed in
// squelch-core/src/types.rs is the server-side gate, and it rejects anything
// else with a 400. That is deliberate: the whole point of the feedback dataset
// is "the model said X, the human said Y", and Y has to be a label the model
// could itself have produced or the pair means nothing.
//
// TYPING. You should be able to hit v, type "bill", and hit Enter. The aliases
// below are what make that work. Matches are RANKED rather than resolved to a
// single answer, because some words genuinely are ambiguous — "bill" fits both
// an invoice you owe and an autopay notice — and silently guessing between them
// would write a wrong label into the training set.
//
// Ported from squelch-desktop/src/lib/triageTargets.ts.

import Foundation

enum TriageAxis: String, Sendable, Hashable {
    case tier, category, sensitivity

    /// "sensitivity" is the column name, not a word anyone thinks in — the chip
    /// says what the axis MEANS.
    var chipLabel: String { self == .sensitivity ? "auth" : rawValue }
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

    var id: String { "\(axis.rawValue):\(value)" }
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
            axis: .category, value: "general", label: "General",
            hint: "none of the money categories",
            aliases: ["general", "none", "other", "plain"]),

        // --- auth: the sealed axis -------------------------------------------
        // Auth is NOT a category — it is `triage.sensitivity`, and it is the
        // axis with real consequences. Sealed mail is structurally absent from
        // the agent door, so moving a message here RESTRICTS what any agent can
        // ever see of it, and moving it out EXPOSES it.
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
        TriageTarget(
            axis: .tier, value: "past_due", label: "Past due",
            hint: "a deadline that has already passed",
            aliases: ["pastdue", "past", "overdue", "late"]),
        TriageTarget(
            axis: .tier, value: "deadline", label: "Deadline",
            hint: "has a date you must act by",
            aliases: ["deadline", "due", "date"]),
        TriageTarget(
            axis: .tier, value: "signal", label: "Signal",
            hint: "worth your attention, no deadline",
            aliases: ["signal", "important", "attention"]),
        TriageTarget(
            axis: .tier, value: "noise", label: "Noise",
            hint: "should not have surfaced at all",
            // The marketing words deliberately do NOT live here: "this is
            // marketing" is a statement about what the mail IS; "this is noise"
            // is about whether it should have surfaced. Conflating them would
            // teach the dataset that every promo is unwanted.
            aliases: ["noise", "junk", "ignore", "spam", "quiet"]),
    ]

    /// Normalize for matching: lowercase, and underscores/spaces are the same.
    private static func norm(_ s: String) -> String {
        s.lowercased().filter { $0 != " " && $0 != "_" && $0 != "-" }
    }

    /// Rank a target against what the user typed. Higher = better; 0 hides it.
    ///
    /// The ordering is deliberately boring: an exact hit beats a prefix, a
    /// prefix beats a mid-word substring, and a match on the real value beats a
    /// match on a convenience alias. Anything cleverer would make it harder to
    /// predict which label you are about to write, and writing the wrong label
    /// is the one failure mode this feature cannot afford.
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
