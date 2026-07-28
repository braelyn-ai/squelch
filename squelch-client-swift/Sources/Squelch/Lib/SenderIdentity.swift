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
    ///
    /// PLAIN STRING SCANNING, NOT REGEX, and deliberately so. Every row asks for
    /// a display name, initials, a palette slot and a robot/brand verdict, and
    /// each of those parses the sender — so a 500-row list was running thousands
    /// of Swift `Regex` matches per frame, which is what made scrolling crawl.
    /// This does the same job by walking the string once.
    static func parse(_ sender: String) -> Parsed {
        let s = sender.trimmingCharacters(in: .whitespacesAndNewlines)

        // "Name <addr>" — take the LAST angle-bracket pair, which is what a
        // display name containing brackets would leave intact.
        if let open = s.lastIndex(of: "<"), let close = s.lastIndex(of: ">"), open < close {
            let addr = String(s[s.index(after: open)..<close])
                .trimmingCharacters(in: .whitespaces)
            if addr.contains("@") {
                let name = String(s[s.startIndex..<open])
                    .replacingOccurrences(of: "\"", with: "")
                    .replacingOccurrences(of: "'", with: "")
                    .trimmingCharacters(in: .whitespaces)
                return Parsed(name: name, addr: addr)
            }
        }

        // "Name addr@host" (no brackets): split on the last space when the tail
        // looks like an address.
        if let space = s.lastIndex(of: " ") {
            let tail = String(s[s.index(after: space)...])
            if tail.contains("@"), !tail.contains(" ") {
                let name = String(s[s.startIndex..<space])
                    .replacingOccurrences(of: "\"", with: "")
                    .replacingOccurrences(of: "'", with: "")
                    .trimmingCharacters(in: .whitespaces)
                return Parsed(name: name, addr: tail)
            }
        }

        // A BARE ADDRESS HAS NO DISPLAY NAME. This used to return the address in
        // `name` as well, which is a landmine for every caller that treats a
        // non-empty `name` as a real one: `initials` did, and rendered the
        // DOMAIN's letters ("bboynton97@gmail.com" -> "BC", the C of ".com").
        // The desktop's regex yields "" here, so this is also the faithful port.
        if s.contains("@") { return Parsed(name: "", addr: s) }
        return Parsed(name: s, addr: s)
    }

    /// Everything a row needs about a sender, resolved once and memoized.
    ///
    /// Rows are re-evaluated constantly (hover, selection, poll), and the
    /// derivation is pure, so caching by the raw sender string turns a
    /// per-frame cost into a one-time one.
    struct Resolved: Sendable {
        var displayName: String
        var initials: String
        var slot: Int
        var faviconDomain: String?
    }

    static func resolve(_ sender: String) -> Resolved {
        Resolved(
            displayName: displayName(sender),
            initials: initials(sender),
            slot: slot(sender),
            faviconDomain: eligibleFaviconDomain(sender))
    }

    /// Bare address (lowercased) from a sender string — the grouping key.
    static func address(_ sender: String) -> String {
        parse(sender).addr.lowercased()
    }

    /// Up to two initials from a display name; fallback to the local-part.
    /// Up to two initials for a sender.
    ///
    /// THE SOURCE IS NEVER THE FULL ADDRESS. It used to be, and the domain leaked
    /// into the result: "bboynton97@gmail.com" split to ["bboynton97@gmail","com"]
    /// and rendered "BC" — that second letter is the C of ".com". Almost every
    /// bare address produced a "?C" monogram, which is why a column of avatars
    /// read RC, IC, BC, MC, SC.
    ///
    /// Order: a real display name, then the resolved brand/robot label (so a row
    /// labelled "Corpnet" shows CO rather than the IC of "info@corpnet.com"), then
    /// the local-part alone.
    static func initials(_ sender: String) -> String {
        let p = parse(sender)
        let local = (p.addr.split(separator: "@").first.map(String.init) ?? "")
            .split(separator: "+").first.map(String.init) ?? ""

        var source = p.name
        if source.isEmpty {
            let shown = displayName(sender)
            source = (!shown.isEmpty && shown.lowercased() != p.addr.lowercased()) ? shown : local
        }

        let words = source
            .split(whereSeparator: { $0 == " " || $0 == "." || $0 == "_" || $0 == "-" })
            .filter { $0.contains(where: { $0.isLetter || $0.isNumber }) }
        if words.count >= 2 {
            return (String(words[0].prefix(1)) + String(words[1].prefix(1))).uppercased()
        }
        if words.count == 1, words[0].count >= 2 {
            return String(words[0].prefix(2)).uppercased()
        }
        let fallback = local.first.map(String.init) ?? source.first.map(String.init) ?? "?"
        return fallback.uppercased()
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
        // Parity with the desktop's ROBOT_LOCAL, which had drifted ahead.
        "mail", "email", "invoice", "invoices", "statement", "statements", "confirmation",
        "confirmations", "tracking", "delivery", "digest", "bulletin",
    ]

    /// Mail-ish subdomain prefixes to peel so notifications.github.com resolves
    /// the github.com favicon.
    private static let mailSubdomains: Set<String> = [
        "mail", "email", "e", "em", "mg", "mta", "smtp", "news", "info", "mailer", "marketing",
        "notification", "notifications", "alert", "alerts", "sfmail", "bounce", "reply", "link",
        "click", "go", "m",
    ]

    /// Unambiguous automation markers, matched against the local-part with its
    /// separators SQUASHED.
    ///
    /// `robotLocals` above must match the WHOLE local-part, which real senders
    /// very often fail: "no.reply.alerts@chase.com", "no_reply@discord.com",
    /// "billing-noreply@stripe.com" and "no-reply-aws@amazon.com" are all
    /// obviously machines, and all fell through to initials.
    ///
    /// Deliberately narrow: only markers no human is ever behind. The
    /// human-capable words in `robotLocals` (hello, info, support, team, contact)
    /// stay WHOLE-local-part matches only — segment-matching those would classify
    /// "jane.support@acme.com" as a robot and fetch a favicon for a domain a HUMAN
    /// corresponds with, which is exactly the leak the privacy model forbids.
    private static let robotMarkers = [
        "noreply", "donotreply", "mailerdaemon", "automailer", "automated", "autoconfirm",
    ]

    /// True if the sender's local-part (pre-"+tag") is a known robot shape.
    static func isRobot(_ sender: String) -> Bool {
        let addr = parse(sender).addr
        let local = addr.split(separator: "@").first.map(String.init) ?? ""
        let base = (local.split(separator: "+").first.map(String.init) ?? local).lowercased()
        if robotLocals.contains(base) { return true }
        // "no.reply.alerts" / "no_reply" / "billing-noreply" -> "...noreply..."
        let squashed = base.filter { $0.isLetter || $0.isNumber }
        return robotMarkers.contains { squashed.contains($0) }
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
        let addr = parse(sender).addr
        if let at = addr.lastIndex(of: "@") {
            return "*@" + String(addr[addr.index(after: at)...])
        }
        return addr
    }
}

// MARK: - favicon verdict cache

/// Per-domain verdict. "ok" = the image loaded; "failed" = error / blank / tiny.
/// Persisted in UserDefaults, mirroring the desktop client's localStorage map.
///
/// A FAILURE IS NOT PERMANENT. It used to be: one fetch returns one
/// undifferentiated failure, so being offline for a moment, a rate-limit, a DNS
/// blip and "this domain has no icon" all looked identical — and every one was
/// recorded forever, no expiry, no retry. A live cache held 50 domains marked
/// failed, github.com, paypal.com, google.com, ebay.com, venmo.com and
/// schwab.com among them, every one of which serves a valid icon when re-tested.
///
/// Failures now carry the time they happened and are retried after
/// `failedRetry`. A domain that genuinely has no icon costs one request a week;
/// a domain that failed transiently heals itself.
@MainActor
final class FaviconCache {
    static let shared = FaviconCache()
    private static let key = "squelch.favicons"
    /// How long a failure is trusted before the domain is worth another attempt.
    private static let failedRetry: TimeInterval = 7 * 24 * 60 * 60

    enum Verdict: String { case ok, failed }

    private enum Entry {
        case ok
        case failed(at: Date)
    }

    private var mem: [String: Entry] = [:]

    private init() {
        guard let raw = UserDefaults.standard.dictionary(forKey: Self.key) else { return }
        for (domain, value) in raw {
            if let s = value as? String, s == Verdict.ok.rawValue {
                mem[domain] = .ok
            } else if let dict = value as? [String: Any],
                dict["v"] as? String == Verdict.failed.rawValue,
                let t = dict["t"] as? Double
            {
                mem[domain] = .failed(at: Date(timeIntervalSince1970: t))
            }
            // A LEGACY bare "failed" (written before failures expired) is
            // deliberately NOT loaded: it carries no timestamp, so there is no
            // honest way to age it, and the odds are high it was a transient
            // failure recorded as permanent. Dropping it retries the domain once
            // and then re-records it properly. This is what un-poisons an
            // existing install.
        }
    }

    /// Cached verdict for a domain. nil when the domain is unresolved OR when its
    /// recorded failure has aged out — both mean "try again".
    func verdict(_ domain: String, now: Date = Date()) -> Verdict? {
        switch mem[domain] {
        case .none: return nil
        case .ok: return .ok
        case .failed(let at):
            return now.timeIntervalSince(at) < Self.failedRetry ? .failed : nil
        }
    }

    func record(_ domain: String, _ verdict: Verdict, now: Date = Date()) {
        switch verdict {
        case .ok:
            if case .ok = mem[domain] { return }
            mem[domain] = .ok
        case .failed:
            mem[domain] = .failed(at: now)
        }
        persist()
    }

    private func persist() {
        var out: [String: Any] = [:]
        for (domain, entry) in mem {
            switch entry {
            case .ok:
                out[domain] = Verdict.ok.rawValue
            case .failed(let at):
                out[domain] = ["v": Verdict.failed.rawValue, "t": at.timeIntervalSince1970]
            }
        }
        UserDefaults.standard.set(out, forKey: Self.key)
    }
}


/// The memo table for `SenderID.resolve`. Isolated to the main actor because
/// that is the only place rows render; `SenderID` itself stays nonisolated so
/// pure helpers (Newsletters derivation) can still run off it.
@MainActor
enum SenderCache {
    private static var cache: [String: SenderID.Resolved] = [:]
    /// Bound the table — a long session can see a lot of distinct senders.
    private static let cap = 4000

    static func resolved(_ sender: String) -> SenderID.Resolved {
        if let hit = cache[sender] { return hit }
        let value = SenderID.resolve(sender)
        if cache.count >= cap { cache.removeAll(keepingCapacity: true) }
        cache[sender] = value
        return value
    }
}
