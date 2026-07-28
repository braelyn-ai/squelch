// Sender avatars + display names — deterministic and initials-based by default.
//
// PRIVACY MODEL: human correspondents are NEVER resolved over the network — no
// Gravatar, no favicon fetch — because the human correspondent graph must never
// leak off-device. The ONLY exception is ROBOT senders (no-reply@,
// notifications@, billing@, …), whose local-parts identify a service, not a
// person. For those we fetch the DOMAIN's favicon once (verdict cached), which
// leaks nothing about who a human talks to.
//
// Ported from squelch-desktop/src/lib/avatar.ts.

import Foundation

/// Palette slots — 10 theme-aware pairs (see Palette.avatarPalette).
let avatarSlots = 10

enum SenderID {
    struct Parsed {
        var name: String
        var addr: String
    }

    /// Extract a display name and address from a sender string:
    /// "Sarah Chen <sarah@acme.com>" -> ("Sarah Chen", "sarah@acme.com")
    static func parse(_ sender: String) -> Parsed {
        let s = sender.trimmingCharacters(in: .whitespacesAndNewlines)
        if let m = s.firstMatch(of: /^(.*?)[<\s]*([^<>\s@]+@[^<>\s]+)>?\s*$/) {
            let name = String(m.1)
                .replacingOccurrences(of: "\"", with: "")
                .replacingOccurrences(of: "'", with: "")
                .trimmingCharacters(in: .whitespaces)
            return Parsed(name: name, addr: String(m.2))
        }
        return Parsed(name: s, addr: s)
    }

    /// Bare address (lowercased) from a sender string — the grouping key.
    static func address(_ sender: String) -> String {
        parse(sender).addr.lowercased()
    }

    /// Up to two initials from a display name; fallback to the local-part.
    static func initials(_ sender: String) -> String {
        let p = parse(sender)
        let source = p.name.isEmpty ? p.addr : p.name
        let words = source
            .split(whereSeparator: { $0 == " " || $0 == "." || $0 == "_" || $0 == "-" })
            .filter { $0.contains(where: { $0.isLetter || $0.isNumber }) }
        if words.count >= 2 {
            return (String(words[0].prefix(1)) + String(words[1].prefix(1))).uppercased()
        }
        if words.count == 1, words[0].count >= 2 {
            return String(words[0].prefix(2)).uppercased()
        }
        let local = p.addr.split(separator: "@").first.map(String.init) ?? source
        return String(local.prefix(1)).uppercased().isEmpty
            ? "?" : String(local.prefix(1)).uppercased()
    }

    /// Deterministic djb2 hash of the address (stable across sessions).
    private static func hashAddr(_ addr: String) -> UInt32 {
        let a = parse(addr).addr.lowercased()
        var h: UInt32 = 5381
        for scalar in a.unicodeScalars {
            h = (h &<< 5) &+ h &+ UInt32(truncatingIfNeeded: scalar.value)
        }
        return h
    }

    /// Palette slot 0..avatarSlots-1 for a sender, deterministic by address.
    static func slot(_ sender: String) -> Int { Int(hashAddr(sender) % UInt32(avatarSlots)) }

    // MARK: - robot / brand senders

    /// Robot local-part shapes (segment BEFORE any "+tag"). These are automated
    /// service mailboxes, not people — safe to resolve a favicon for.
    private static let robotLocals: Set<String> = [
        "no-reply", "noreply", "do-not-reply", "donotreply", "notification", "notifications",
        "alert", "alerts", "update", "updates", "news", "newsletter", "marketing", "mailer",
        "billing", "receipt", "receipts", "order", "orders", "team", "hello", "info", "support",
        "account", "accounts", "security", "admin", "service", "contact", "help", "feedback",
    ]

    /// Mail-ish subdomain prefixes to peel so notifications.github.com resolves
    /// the github.com favicon.
    private static let mailSubdomains: Set<String> = [
        "mail", "email", "e", "em", "mg", "mta", "smtp", "news", "info", "mailer", "marketing",
        "notification", "notifications", "alert", "alerts", "sfmail", "bounce", "reply", "link",
        "click", "go", "m",
    ]

    /// True if the sender's local-part (pre-"+tag") is a known robot shape.
    static func isRobot(_ sender: String) -> Bool {
        let addr = parse(sender).addr
        let local = addr.split(separator: "@").first.map(String.init) ?? ""
        let base = local.split(separator: "+").first.map(String.init) ?? local
        if robotLocals.contains(base.lowercased()) { return true }
        return base.lowercased().hasPrefix("noreply-")
    }

    /// Base domain for a favicon lookup: strip ONE leading mail-ish subdomain
    /// label, keeping a two-label minimum.
    static func faviconDomain(_ sender: String) -> String? {
        let addr = parse(sender).addr
        let parts = addr.split(separator: "@")
        guard parts.count >= 2 else { return nil }
        var host = String(parts[parts.count - 1]).lowercased()
        while host.hasSuffix(".") { host.removeLast() }
        guard host.contains(".") else { return nil }
        var labels = host.split(separator: ".").map(String.init)
        if labels.count > 2, mailSubdomains.contains(labels[0]) { labels.removeFirst() }
        return labels.count >= 2 ? labels.joined(separator: ".") : nil
    }

    /// The brand's base label — the first label of the favicon domain.
    static func baseLabel(_ sender: String) -> String? {
        guard let domain = faviconDomain(sender) else { return nil }
        let first = domain.split(separator: ".").first.map(String.init) ?? ""
        return first.isEmpty ? nil : first
    }

    /// True if the local-part equals the domain's base label ("eBay@eBay.com").
    /// These are brand mailboxes naming a service, not a person.
    static func isBrand(_ sender: String) -> Bool {
        let addr = parse(sender).addr
        let local = (addr.split(separator: "@").first.map(String.init) ?? "")
            .split(separator: "+").first.map(String.init) ?? ""
        guard !local.isEmpty, let base = baseLabel(sender) else { return false }
        return local.lowercased() == base.lowercased()
    }

    /// The name to SHOW for a sender:
    ///  1. A display name that differs from the raw address wins.
    ///  2. A BRAND sender shows the local-part as given ("eBay").
    ///  3. A ROBOT sender shows the capitalized base domain label ("Stripe").
    ///  4. Otherwise the address as-is.
    /// Never emits "x@x.com"-style redundancy.
    static func displayName(_ sender: String) -> String {
        let p = parse(sender)
        if !p.name.isEmpty, p.name.lowercased() != p.addr.lowercased() { return p.name }
        if isBrand(sender) {
            let local = (p.addr.split(separator: "@").first.map(String.init) ?? "")
                .split(separator: "+").first.map(String.init) ?? ""
            if !local.isEmpty { return local }
        }
        if isRobot(sender), let base = baseLabel(sender) {
            return base.prefix(1).uppercased() + base.dropFirst()
        }
        return p.addr
    }

    /// DuckDuckGo icon service URL for a base domain.
    static func faviconURL(_ domain: String) -> URL? {
        URL(string: "https://icons.duckduckgo.com/ip3/\(domain).ico")
    }

    /// The favicon domain to use for a sender, or nil when it must stay local
    /// (a human correspondent — never resolved over the network).
    static func eligibleFaviconDomain(_ sender: String) -> String? {
        (isRobot(sender) || isBrand(sender)) ? faviconDomain(sender) : nil
    }

    /// Turn a sender into a "*@domain" rule pattern.
    static func patternFromSender(_ sender: String) -> String {
        let addr: String
        if let m = sender.firstMatch(of: /[<\s]([^<>\s@]+@[^<>\s]+)>?\s*$/) {
            addr = String(m.1)
        } else if let m = sender.firstMatch(of: /([^<>\s@]+@[^<>\s]+)/) {
            addr = String(m.1)
        } else {
            addr = sender.trimmingCharacters(in: .whitespaces)
        }
        if let at = addr.lastIndex(of: "@") {
            return "*@" + String(addr[addr.index(after: at)...])
        }
        return addr
    }
}

// MARK: - favicon verdict cache

/// Per-domain verdict: each domain resolves at most once. "ok" = the image
/// loaded; "failed" = error / blank / tiny — fall back to initials forever.
/// Persisted in UserDefaults, mirroring the desktop client's localStorage map.
@MainActor
final class FaviconCache {
    static let shared = FaviconCache()
    private static let key = "squelch.favicons"

    enum Verdict: String { case ok, failed }

    private var mem: [String: Verdict] = [:]

    private init() {
        if let raw = UserDefaults.standard.dictionary(forKey: Self.key) as? [String: String] {
            for (d, v) in raw { if let verdict = Verdict(rawValue: v) { mem[d] = verdict } }
        }
    }

    func verdict(_ domain: String) -> Verdict? { mem[domain] }

    func record(_ domain: String, _ verdict: Verdict) {
        guard mem[domain] != verdict else { return }
        mem[domain] = verdict
        UserDefaults.standard.set(mem.mapValues(\.rawValue), forKey: Self.key)
    }
}
