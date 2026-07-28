// Client-side 2FA code extraction, auth-mail copy, and the device-local
// "needs a decision" ledger.
//
// The "present, don't read" flow: when a login code arrives we auto-reveal the
// body (server-side audited) and pull the code out here so the human never has
// to open the email — the code just appears.
//
// Ported from squelch-desktop/src/lib/{authCode,authCopy,authDecisions}.ts.

import Foundation
import SwiftUI

// MARK: - code extraction

enum AuthCode {
    /// Kinds that warrant the code modal. Others (resets/alerts) get the ring only.
    static let codeKinds: Set<String> = ["otp", "login_code", "verification"]

    /// True if this sealed kind should pop the code modal (vs. ring-only).
    static func isCodeKind(_ kind: String?) -> Bool {
        guard let kind else { return false }
        return codeKinds.contains(kind)
    }

    /// Words that tend to sit right next to the actual code.
    nonisolated(unsafe) private static let codeWordPattern =
        /(?i)\b(?:one[-\s]?time|verification|verify|passcode|pass[-\s]?code|security\s+code|access\s+code|auth(?:entication)?\s+code|login\s+code|sign[-\s]?in\s+code|confirmation\s+code|OTP|PIN|code)\b/

    /// A standalone 4-8 digit run, allowing one space/hyphen split (some
    /// providers format longer codes as "123 456" for readability).
    ///
    /// The desktop version used a lookbehind for the left boundary; Swift Regex
    /// has no lookbehind, so the boundary is an explicit alternation and the
    /// run is capture group 1 — which is why every read below uses `m.1` rather
    /// than the whole match.
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

    /// Extract the most likely login code from a revealed body.
    ///
    /// Strategy, in order of confidence:
    ///   1. Prefer a 4-8 digit run within ±80 chars of a code word, ranked by
    ///      how CLOSE it sits — proximity beats length, so "login code is
    ///      55231" wins over a longer order number nearby.
    ///   2. Fallback: the longest standalone 4-8 digit run in the body.
    /// Returns nil when nothing plausible is found.
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
            // Index the DIGITS, not the boundary character the pattern consumed
            // (the capture's own startIndex is an index into `text`).
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

/// User-facing copy for auth-related mail. "Sealed" is internal jargon and must
/// never reach the UI — this maps wire-level `sealed_kind` strings to
/// auth-centric labels the user actually understands.
enum AuthCopy {
    static func label(_ kind: String?) -> String {
        switch kind {
        case "otp": "Login code"
        case "password_reset": "Password reset"
        case "magic_link": "Sign-in link"
        case "login_alert": "Sign-in alert"
        case "verification": "Verification"
        default: "Auth message"
        }
    }

    /// Per-kind SF Symbol, mirroring the desktop client's lucide choices.
    static func symbol(_ kind: String?) -> String {
        switch kind {
        case "otp": "key.fill"
        case "password_reset": "lock.rotation"
        case "magic_link": "envelope.badge.shield.half.filled"
        case "login_alert": "exclamationmark.shield.fill"
        case "verification": "checkmark.seal.fill"
        default: "key.fill"
        }
    }
}

// MARK: - decisions ledger

/// "Needs a decision" state — which sign-in alerts and password resets the
/// human has already ruled on.
///
/// DEVICE-LOCAL BY DESIGN. There is no server field for this: /client/sealed is
/// read-only metadata, and squelch cannot mark Gmail read either. So the
/// decision lives in UserDefaults, exactly like the arrival seen-set.
///
/// What that costs, stated plainly: decisions do not follow you to another
/// machine. If these ever need to be a RECORD (audited, cross-device), that is
/// the moment to promote them server-side rather than to grow this file.
enum AuthVerdict: String, Sendable {
    case mine
    case notMine = "not-mine"
}

@MainActor
@Observable
final class AuthDecisions {
    static let shared = AuthDecisions()

    /// Kinds that ask the human a question rather than handing them a code.
    static let decisionKinds: Set<String> = ["login_alert", "password_reset", "magic_link"]

    static func needsDecision(_ kind: String?) -> Bool {
        guard let kind else { return false }
        return decisionKinds.contains(kind)
    }

    private static let key = "squelch.auth-decisions"
    /// Cap the stored map so it cannot grow without bound.
    private static let cap = 300

    private var store: [String: String] = [:]

    private init() {
        store = (UserDefaults.standard.dictionary(forKey: Self.key) as? [String: String]) ?? [:]
    }

    /// The recorded verdict for a message, or nil while it is still open.
    func decision(_ id: Int) -> AuthVerdict? {
        guard let raw = store[String(id)] else { return nil }
        return AuthVerdict(rawValue: raw)
    }

    /// Record a verdict.
    ///
    /// Note the asymmetry in what the two answers mean. "That was me" is a
    /// dismissal — it resolves the card and nothing else happens. "Not me" is
    /// NOT a dismissal: it is the start of an investigation, so the caller
    /// opens the message so the human can read what happened and act on it.
    func set(_ id: Int, _ verdict: AuthVerdict) {
        var next = store
        next[String(id)] = verdict.rawValue
        // Keys are message ids, so numeric order is arrival order: dropping the
        // lowest keys evicts the oldest decisions first.
        if next.count > Self.cap {
            let keys = next.keys.sorted { (Int($0) ?? 0) < (Int($1) ?? 0) }
            for k in keys.prefix(next.count - Self.cap) { next.removeValue(forKey: k) }
        }
        store = next
        UserDefaults.standard.set(next, forKey: Self.key)
    }
}
