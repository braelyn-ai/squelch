// Sender avatars + display names — deterministic and initials-based by default.
//
// PRIVACY: human correspondents are NEVER resolved over the network (no
// Gravatar, no favicon) — the human correspondent graph must not leave the
// device. The only exception is ROBOT/BRAND senders, whose local-parts identify
// a service rather than a person; those fetch the domain's favicon once.

import Foundation

/// Palette slots — 10 theme-aware pairs (see Palette.avatarPalette).
let avatarSlots = 10

enum SenderID {
    struct Parsed {
        var name: String
        var addr: String
    }

    /// "Sarah Chen <sarah@acme.com>" -> ("Sarah Chen", "sarah@acme.com").
    ///
    /// Plain string scanning, not regex, deliberately: a row parses its sender
    /// several times over (name, initials, slot, robot verdict), so a long list
    /// ran thousands of `Regex` matches per frame and scrolling crawled.
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

        // A bare address has no display name: callers treat a non-empty `name`
        // as a real one, and `initials` would render the domain's letters
        // ("bboynton97@gmail.com" -> "BC", the C of ".com").
        if s.contains("@") { return Parsed(name: "", addr: s) }
        return Parsed(name: s, addr: s)
    }

    /// Everything a row needs about a sender. Rows re-evaluate constantly
    /// (hover, selection, poll) and the derivation is pure, so `SenderCache`
    /// memoizes this by raw sender string.
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

    /// Split a comma-joined recipient list into its individual entries, each
    /// still in the "Name <addr>" shape `parse` expects.
    ///
    /// QUOTE- AND BRACKET-AWARE, because a display name is allowed to hold a
    /// comma: a plain `split(separator: ",")` turns `"Doe, John" <j@x.com>`
    /// into two nonsense recipients, and the sent page would name a person
    /// after the punctuation in their own address book entry.
    static func recipients(_ list: String) -> [String] {
        var out: [String] = []
        var current = ""
        var inQuotes = false
        var inAngle = false
        for ch in list {
            switch ch {
            case "\"" where !inAngle: inQuotes.toggle()
            case "<" where !inQuotes: inAngle = true
            case ">" where !inQuotes: inAngle = false
            case "," where !inQuotes && !inAngle:
                let token = current.trimmingCharacters(in: .whitespacesAndNewlines)
                if !token.isEmpty { out.append(token) }
                current = ""
                continue
            default: break
            }
            current.append(ch)
        }
        let tail = current.trimmingCharacters(in: .whitespacesAndNewlines)
        if !tail.isEmpty { out.append(tail) }
        return out
    }

    /// Up to two initials for a sender. THE SOURCE IS NEVER THE FULL ADDRESS —
    /// the domain leaks in otherwise ("bboynton97@gmail.com" -> "BC"). Order: a
    /// real display name, then the brand/robot label (so a row labelled
    /// "Corpnet" shows CO, not the IC of "info@corpnet.com"), then the
    /// local-part alone.
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
        "mail", "email", "invoice", "invoices", "statement", "statements", "confirmation",
        "confirmations", "tracking", "delivery", "digest", "bulletin",
    ]

    /// Two-part public suffixes, so `foo.co.uk` is not read as the site "co".
    /// NOT the full Public Suffix List — that is thousands of entries and a
    /// standing maintenance debt for what is, here, a favicon lookup and a
    /// fallback label. These are the ones a reader actually receives mail from;
    /// anything missed degrades to a slightly wrong label, never a crash.
    private static let compoundSuffixes: Set<String> = [
        "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "ltd.uk", "plc.uk",
        "com.au", "net.au", "org.au", "edu.au", "gov.au",
        "co.jp", "or.jp", "ne.jp", "ac.jp",
        "co.nz", "co.za", "co.in", "co.kr", "co.il", "co.th",
        "com.br", "com.mx", "com.sg", "com.tr", "com.cn", "com.hk", "com.tw",
        "com.ar", "com.co", "com.pl", "com.ua",
    ]

    /// Unambiguous automation markers, matched against the local-part with its
    /// separators SQUASHED ("no.reply.alerts", "billing-noreply"), which
    /// `robotLocals`' whole-local-part match misses. Narrow on purpose:
    /// segment-matching robotLocals' human-capable words (hello, info, support)
    /// would favicon-fetch a domain a HUMAN corresponds with.
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
        let labels = host.split(separator: ".").map(String.init)
        guard labels.count >= 2 else { return nil }

        // THE REGISTRABLE DOMAIN, not a whitelist of prefixes to peel. A list
        // of mail-ish words falls behind: `emails.dollar.com`, plural and
        // therefore not on it, would keep its subdomain, name Dollar Car
        // Rental "Emails", and ask an icon service about a host that has no
        // icon. Taking the last two labels has no list to fall behind: every
        // `<anything>.dollar.com` resolves to dollar.com, and the apex is also
        // far likelier to actually serve a favicon than a bulk-mail subdomain.
        let lastTwo = labels.suffix(2).joined(separator: ".")
        if labels.count >= 3, compoundSuffixes.contains(lastTwo) {
            return labels.suffix(3).joined(separator: ".")
        }
        return lastTwo
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
            return Fmt.capitalizingFirst(base)
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

/// Per-domain verdict, persisted in UserDefaults. "ok" = the image loaded;
/// "failed" = error / blank / tiny.
///
/// A FAILURE IS NOT PERMANENT: one fetch cannot tell "this domain has no icon"
/// from an offline moment or a rate-limit, so failures carry their timestamp
/// and are retried after `failedRetry`.
@MainActor
final class FaviconCache {
    static let shared = FaviconCache()
    private static let key = "passband.favicons"
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
            // A bare "failed" carries no timestamp, so there is no honest way
            // to age it: drop it, retry the domain once, re-record it properly.
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


/// The memo table for `SenderID.resolve`. Main-actor isolated because that is
/// the only place rows render; `SenderID` itself stays nonisolated so pure
/// helpers can run off the main actor.
@MainActor
enum SenderCache {
    private static var cache = LRUMap<String, SenderID.Resolved>(limit: cap)
    /// Bound the table — a long session can see a lot of distinct senders.
    private static let cap = 4000

    static func resolved(_ sender: String) -> SenderID.Resolved {
        if let hit = cache.get(sender) { return hit }
        let value = SenderID.resolve(sender)
        cache.set(sender, value)
        return value
    }
}
