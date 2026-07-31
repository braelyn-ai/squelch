// The Sitrep's newsletters zone — recurring noise-tier senders, and the
// rule-onboarding CTA when no rule governs them yet.
//
// Qualification prefers a real `marketing` classification (GET
// /client/marketing). The reason-string / recurring-robot fallback is a
// migration bridge: it applies ONLY while nothing has been categorized yet.

import Foundation

/// A newsletter card: one recurring noise sender for the window.
struct Newsletter: Identifiable, Hashable, Sendable {
    /// Grouping key = bare lowercased address.
    var address: String
    /// A representative raw sender string (for avatar + display name).
    var sender: String
    /// Count of qualifying noise messages in the window.
    var count: Int
    /// Latest one_line in the window (the summary line).
    var summary: String
    /// Latest message date — cards sort newest-first.
    var latest: Double
    /// The latest message's thread — clicking the card opens this email.
    var latestThreadId: String
    /// The window's updates, NEWEST FIRST — the viewer's horizontal queue (h/l
    /// between this sender's emails) and the bulk-done target.
    var items: [AttentionUpdate]
    /// The rule governing this sender, if any (drives the chip).
    var rule: SenderRule?

    var id: String { address }
}

enum Newsletters {
    /// Exact rung-5 reason literals we key off (substring, case-insensitive).
    private static let newsletterReason = "unsubscribe footer"
    private static let receiptReason = "order confirmation / receipt"

    private static func isNewsletterReason(_ reason: String) -> Bool {
        let r = reason.lowercased()
        if r.contains(newsletterReason) { return true }
        return r.firstMatch(
            of: /(?i)\b(unsubscribe|newsletter|bulk\/list|mailing list|marketing|promotional|digest)\b/
        ) != nil
    }

    private static func isReceiptReason(_ reason: String) -> Bool {
        let r = reason.lowercased()
        if r.contains(receiptReason) { return true }
        return r.firstMatch(
            of: /(?i)\b(order confirmation|receipt|your order|shipment|shipped|tracking)\b/) != nil
    }

    /// Date proxy for a noise update (no received_at on the wire model).
    private static func dateOf(_ u: AttentionUpdate) -> Double {
        guard let d = Fmt.date(u.surfaced_at ?? u.resolved_at) else { return 0 }
        return d.timeIntervalSince1970
    }

    /// Glob match for a rule's match_pattern ("*@acme.com") against a bare
    /// address. `*` matches any run; case-insensitive.
    static func ruleMatches(pattern: String, address: String) -> Bool {
        let pat = pattern.trimmingCharacters(in: .whitespaces).lowercased()
        guard !pat.isEmpty else { return false }
        var rx = "^"
        for ch in pat {
            if ch == "*" {
                rx += ".*"
            } else if ".+?^${}()|[]\\".contains(ch) {
                rx += "\\\(ch)"
            } else {
                rx.append(ch)
            }
        }
        rx += "$"
        if let re = try? Regex(rx) {
            return address.lowercased().wholeMatch(of: re) != nil
        }
        // Pragmatic fallback: does the pattern's domain appear in the address?
        let dom = pat.split(separator: "@").last.map(String.init) ?? pat
        return address.lowercased().contains(dom.replacingOccurrences(of: "*", with: ""))
    }

    /// Find the rule governing an address. Prefers the MOST SPECIFIC (fewest
    /// wildcards) match so the chip is stable.
    static func rule(for address: String, in rules: [SenderRule]) -> SenderRule? {
        let hits = rules.filter { ruleMatches(pattern: $0.match_pattern, address: address) }
        return hits.min {
            $0.match_pattern.filter { c in c == "*" }.count
                < $1.match_pattern.filter { c in c == "*" }.count
        }
    }

    private static let weekSeconds: Double = 7 * 86400

    /// Derive newsletter cards from a batch of noise-tier updates.
    static func derive(
        updates: [AttentionUpdate],
        rules: [SenderRule],
        marketingIds: Set<Int> = [],
        since: Double? = nil,
        limit: Int = 24,
        now: Date = Date()
    ) -> [Newsletter] {
        let cutoff = since ?? (now.timeIntervalSince1970 - weekSeconds)

        struct Bucket {
            var sender: String
            var total = 0
            var newsletterHits = 0
            var receiptHits = 0
            /// Messages of this sender the pipeline categorized `marketing`.
            var marketingHits = 0
            var robot: Bool
            var latest: Double = 0
            var summary = ""
            var latestThreadId = ""
            var items: [AttentionUpdate] = []
        }
        var byAddr: [String: Bucket] = [:]
        var order: [String] = []

        for u in updates {
            // Excludes receipts: the server auto-resolves receipt-classified
            // mail to status='done' at ingest, and a settled record is not
            // recurring noise to onboard a rule for.
            if u.status == .done { continue }
            if dateOf(u) < cutoff { continue }
            let address = SenderID.address(u.sender)
            guard address.contains("@") else { continue }

            if byAddr[address] == nil {
                byAddr[address] = Bucket(
                    sender: u.sender,
                    robot: SenderID.isRobot(u.sender) || SenderID.isBrand(u.sender))
                order.append(address)
            }
            byAddr[address]!.total += 1
            byAddr[address]!.items.append(u)
            if marketingIds.contains(u.id) { byAddr[address]!.marketingHits += 1 }
            if isNewsletterReason(u.reason) { byAddr[address]!.newsletterHits += 1 }
            if isReceiptReason(u.reason) { byAddr[address]!.receiptHits += 1 }
            let d = dateOf(u)
            if d >= byAddr[address]!.latest {
                byAddr[address]!.latest = d
                if !u.one_line.isEmpty { byAddr[address]!.summary = u.one_line }
                byAddr[address]!.latestThreadId = u.thread_id
            }
        }

        var out: [Newsletter] = []
        for address in order {
            guard let b = byAddr[address] else { continue }
            // Exclude senders whose window is entirely receipts (order updates,
            // not a newsletter) with no newsletter signal at all.
            let allReceipts = b.receiptHits > 0 && b.newsletterHits == 0 && b.marketingHits == 0
            if allReceipts { continue }

            let qualifies =
                marketingIds.isEmpty
                ? (b.newsletterHits > 0 || (b.robot && b.total >= 2))
                : b.marketingHits > 0
            guard qualifies else { continue }

            out.append(
                Newsletter(
                    address: address,
                    sender: b.sender,
                    count: b.total,
                    summary: b.summary,
                    latest: b.latest,
                    latestThreadId: b.latestThreadId,
                    items: b.items.sorted { dateOf($0) > dateOf($1) },
                    rule: rule(for: address, in: rules)))
        }

        // Newest activity first; ties break on higher volume.
        out.sort { a, b in a.latest != b.latest ? a.latest > b.latest : a.count > b.count }
        return Array(out.prefix(limit))
    }

    /// The `*@domain` pattern a newsletter CTA prefills into the rule editor.
    /// Drop already-resolved messages from a derived window, recomputing the
    /// fields taken from the newest survivor and removing any sender left with
    /// nothing at all.
    ///
    /// A READ-SIDE FILTER, not a mutation of the store. `derive` skips
    /// `status == .done`, so the server's next poll produces this same answer —
    /// this only covers the ten seconds in between, which is exactly the window
    /// in which marking mail done looked like it had not worked. `resolvedIds`
    /// is already the app's record of "resolved, poll has not caught up", and
    /// undo clears it, so a restored message brings its card straight back.
    static func prune(_ newsletters: [Newsletter], resolved: Set<Int>) -> [Newsletter] {
        guard !resolved.isEmpty else { return newsletters }
        return newsletters.compactMap { nl in
            let live = nl.items.filter { !resolved.contains($0.id) }
            if live.count == nl.items.count { return nl }
            // Nothing left in the window: the card goes, rather than sitting
            // there at zero until the poll agrees.
            guard let newest = live.first else { return nil }
            var out = nl
            out.items = live
            out.count = live.count
            // `derive` sorts items newest-first and takes these three from the
            // newest message, so the head of the surviving list carries them.
            out.latest = dateOf(newest)
            out.latestThreadId = newest.thread_id
            if !newest.one_line.isEmpty { out.summary = newest.one_line }
            return out
        }
    }

    static func domainPattern(_ address: String) -> String {
        let domain =
            SenderID.faviconDomain(address) ?? address.split(separator: "@").last.map(String.init)
            ?? address
        return "*@\(domain)"
    }

    /// Strip redundant genre labels ("Promotional email from X:", "Newsletter:
    /// …") — the section already says what these are. Only recognized leading
    /// shapes are removed; anything else passes through unchanged.
    static func cleanSummary(_ summary: String) -> String {
        var out = summary.trimmingCharacters(in: .whitespaces)
        out = out.replacing(
            /(?i)^(promotional email|marketing email|newsletter|promo(?:tion)?)\s*(from\s+[^:,-]+)?[:,-]?\s+/,
            with: "")
        out = out.replacing(/(?i)^\w[\w ]{0,24}?\bpromotion\s+(for|from|of)\s+/, with: "")
        out = out.trimmingCharacters(in: .whitespaces)
        if out.isEmpty { return summary.trimmingCharacters(in: .whitespaces) }
        return out.prefix(1).uppercased() + out.dropFirst()
    }

    /// Truncate with an ellipsis, matching the card's copy budget.
    static func truncate(_ s: String, _ n: Int) -> String {
        guard s.count > n else { return s }
        return s.prefix(n - 1).trimmingCharacters(in: .whitespaces) + "…"
    }
}
