// Client-side 2FA code extraction, auth-mail copy, and the device-local
// "needs a decision" ledger.
//
// "Present, don't read": a login code's body is auto-revealed (audited
// server-side) and the code pulled out here, so the human never opens the mail.

import Foundation
import SwiftUI

// MARK: - code extraction

enum AuthCode {
    /// Kinds that warrant the code modal; the rest (resets/alerts/links) get the
    /// ring only. An unknown wire kind is never one of these.
    static let codeKinds: Set<SealedKind> = [.otp, .verification]

    static func isCodeKind(_ kind: SealedKind?) -> Bool {
        guard let kind else { return false }
        return codeKinds.contains(kind)
    }

    /// Words that tend to sit right next to the actual code.
    nonisolated(unsafe) private static let codeWordPattern =
        /(?i)\b(?:one[-\s]?time|verification|verify|passcode|pass[-\s]?code|security\s+code|access\s+code|auth(?:entication)?\s+code|login\s+code|sign[-\s]?in\s+code|confirmation\s+code|OTP|PIN|code)\b/

    /// A standalone 4-8 digit run, allowing one space/hyphen split ("123 456").
    /// Swift Regex has no lookbehind, so the left boundary is an explicit
    /// alternation and the run is capture group 1 — hence every read uses `m.1`.
    nonisolated(unsafe) private static let digitRunPattern =
        /(?:^|[^\w-])(\d{4,8}|\d{3}[\s-]\d{3}|\d{2}[\s-]\d{2}[\s-]\d{2})(?![\w-])/

    /// Normalize a matched run to bare digits, or nil if it isn't 4-8 digits.
    private static func cleanRun(_ raw: String) -> String? {
        let digits = raw.filter(\.isNumber)
        return (4...8).contains(digits.count) ? digits : nil
    }

    /// Char distance from `index` to the nearest code-word match, or .infinity.
    private static func nearestCodeWord(_ text: String, _ index: Int) -> Double {
        var best = Double.infinity
        for m in text.matches(of: codeWordPattern) {
            let wi = text.distance(from: text.startIndex, to: m.range.lowerBound)
            let wLen = text.distance(from: m.range.lowerBound, to: m.range.upperBound)
            let d = wi <= index ? index - (wi + wLen) : wi - index
            best = min(best, Double(max(0, d)))
        }
        return best
    }

    /// The most likely login code in a revealed body: a 4-8 digit run within ±80
    /// chars of a code word, closest first (proximity beats length), else the
    /// longest standalone run. nil when nothing plausible is found.
    static func extract(_ body: String?) -> String? {
        guard let body, !body.isEmpty else { return nil }
        // Codes live near the top; bound the work.
        let text = String(body.prefix(4000))

        struct Run {
            var code: String
            var index: Int
        }
        var runs: [Run] = []
        for m in text.matches(of: digitRunPattern) {
            guard let code = cleanRun(String(m.1)) else { continue }
            // Index the DIGITS, not the boundary char the pattern consumed.
            runs.append(
                Run(
                    code: code,
                    index: text.distance(from: text.startIndex, to: m.1.startIndex)))
        }
        guard !runs.isEmpty else { return nil }

        let near =
            runs
            .map { (run: $0, dist: nearestCodeWord(text, $0.index)) }
            .filter { $0.dist <= 80 }
        if !near.isEmpty {
            let best = near.sorted { a, b in
                a.dist != b.dist ? a.dist < b.dist : a.run.code.count > b.run.code.count
            }
            return best[0].run.code
        }

        let byLength = runs.sorted { a, b in
            a.code.count != b.code.count ? a.code.count > b.code.count : a.index < b.index
        }
        return byLength[0].code
    }
}

// MARK: - auth copy

/// User-facing copy for auth mail. "Sealed" is internal jargon and must never
/// reach the UI, so wire `sealed_kind` values map to auth-centric labels.
enum AuthCopy {
    static func label(_ kind: SealedKind?) -> String {
        switch kind {
        case .otp: "Login code"
        case .passwordReset: "Password reset"
        case .magicLink: "Sign-in link"
        case .loginAlert: "Sign-in alert"
        case .verification: "Verification"
        // A kind we don't know stays generic — its raw string is never shown.
        case .unknown, nil: "Auth message"
        }
    }

    static func symbol(_ kind: SealedKind?) -> String {
        switch kind {
        case .otp: "key.fill"
        case .passwordReset: "lock.rotation"
        case .magicLink: "envelope.badge.shield.half.filled"
        case .loginAlert: "exclamationmark.shield.fill"
        case .verification: "checkmark.seal.fill"
        case .unknown, nil: "key.fill"
        }
    }
}

// MARK: - decisions ledger

/// The human's verdict on a sign-in alert or password reset. Device-local by
/// design: there is no server field (/client/sealed is read-only metadata), so
/// decisions live in UserDefaults and do not follow the user to another machine.
/// They are also per ACCOUNT — the map is keyed by message id, which is one
/// daemon's SQLite int.
enum AuthVerdict: String, Sendable {
    case mine
    case notMine = "not-mine"
}

@MainActor
@Observable
final class AuthDecisions {
    static let shared = AuthDecisions()

    /// Kinds that ask the human a question rather than handing them a code.
    static let decisionKinds: Set<SealedKind> = [.loginAlert, .passwordReset, .magicLink]

    static func needsDecision(_ kind: SealedKind?) -> Bool {
        guard let kind else { return false }
        return decisionKinds.contains(kind)
    }

    /// Base name of the stored map; the live key is this scoped to the account.
    /// nil when there is no live account — nothing to read, nowhere to write.
    private static let keyBase = "passband.auth-decisions"
    private static var key: String? {
        guard let id = AccountManager.shared.activeId else { return nil }
        return AccountIndex.scopedKey(keyBase, id)
    }
    /// Cap the stored map so it cannot grow without bound.
    private static let cap = 300

    private var store: [String: String] = [:]

    private init() { reload() }

    /// Re-read the ledger for whatever account is live NOW. Called by the
    /// account switch, AFTER the new id is committed — this key is derived
    /// from it, so reloading any earlier would re-read the account that just
    /// went away.
    func reload() {
        store =
            Self.key.flatMap { UserDefaults.standard.dictionary(forKey: $0) as? [String: String] }
            ?? [:]
    }

    /// The recorded verdict for a message, or nil while it is still open.
    func decision(_ id: Int) -> AuthVerdict? {
        guard let raw = store[String(id)] else { return nil }
        return AuthVerdict(rawValue: raw)
    }

    /// Record a verdict. Asymmetric by design: `mine` just resolves the card,
    /// while `notMine` starts an investigation — the caller opens the message.
    func set(_ id: Int, _ verdict: AuthVerdict) {
        // No live account = no key to write under. Nothing can be showing a
        // decision card in that state either.
        guard let key = Self.key else { return }
        var next = store
        next[String(id)] = verdict.rawValue
        // Keys are message ids, so numeric order is arrival order: dropping the
        // lowest keys evicts the oldest decisions first.
        if next.count > Self.cap {
            let keys = next.keys.sorted { (Int($0) ?? 0) < (Int($1) ?? 0) }
            for k in keys.prefix(next.count - Self.cap) { next.removeValue(forKey: k) }
        }
        store = next
        UserDefaults.standard.set(next, forKey: key)
    }
}
