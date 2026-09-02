// How the rules page is ORDERED and NESTED, as pure functions over the wire
// type — no SwiftUI, so the shape is unit-testable without a window.
//
// The page reads as three sections (allow, filter, mute), and inside each one
// the rules gather by REGISTRABLE DOMAIN. A domain holding a single rule is one
// line; a domain holding several gets a header line and its rules indented
// under it, labelled by what actually distinguishes them. That structure is
// invisible on a mailbox whose rules happen to name eleven different domains,
// which is exactly why it is asserted here rather than eyeballed.

import Foundation

enum RuleGrouping {
    // MARK: - pattern anatomy

    /// The host half of a match pattern: `*@billing.garmin.com` ->
    /// `billing.garmin.com`. `nil` when the pattern names no host at all.
    static func host(_ pattern: String) -> String? {
        guard let at = pattern.lastIndex(of: "@") else { return nil }
        var host = String(pattern[pattern.index(after: at)...])
            .trimmingCharacters(in: .whitespaces)
            .lowercased()
        while host.hasSuffix(".") { host.removeLast() }
        return host.isEmpty ? nil : host
    }

    /// The local half, or `nil` when the pattern is a whole-domain wildcard.
    /// `nil` is the interesting case: it is what lets the page drop the `*@`
    /// the owner never needs to read.
    static func local(_ pattern: String) -> String? {
        guard let at = pattern.lastIndex(of: "@") else { return nil }
        let local = String(pattern[pattern.startIndex..<at]).trimmingCharacters(in: .whitespaces)
        return (local.isEmpty || local == "*") ? nil : local
    }

    /// True when the pattern covers a whole domain rather than one mailbox.
    static func isWildcard(_ pattern: String) -> Bool {
        host(pattern) != nil && local(pattern) == nil
    }

    /// The grouping key: the registrable domain, so `billing.garmin.com` and
    /// `garmin.com` land together.
    ///
    /// Reuses `SenderID.faviconDomain` rather than re-deriving it. That function
    /// already owns the compound-suffix table (`co.uk` is not the site "co"),
    /// and a second copy of that rule here would drift from the one the avatars
    /// use — the page would then group by one definition of a domain and draw
    /// icons for another.
    static func domainKey(_ pattern: String) -> String {
        SenderID.faviconDomain(pattern) ?? host(pattern) ?? pattern.lowercased()
    }

    /// The subdomain labels between the mailbox and the registrable domain:
    /// `billing.garmin.com` under `garmin.com` -> `billing`. Empty at the apex.
    static func subdomain(_ pattern: String) -> String {
        guard let host = host(pattern) else { return "" }
        let domain = domainKey(pattern)
        guard host != domain, host.hasSuffix("." + domain) else { return "" }
        return String(host.dropLast(domain.count + 1))
    }

    // MARK: - labels

    /// What a rule reads as when it stands ALONE under its domain: the pattern
    /// with a bare `*@` dropped, because "the owner wrote a rule for
    /// futureme.org" is the whole of what `*@futureme.org` says.
    static func standaloneLabel(_ pattern: String) -> String {
        guard let host = host(pattern) else { return pattern }
        guard let local = local(pattern) else { return host }
        return "\(local)@\(host)"
    }

    /// What a rule reads as when it sits UNDER a domain header, which already
    /// carries the domain: only the part that distinguishes it from its
    /// siblings.
    ///
    /// The apex wildcard is the one that needs words rather than a symbol -
    /// under a `garmin.com` header, `*@garmin.com` is "every sender at this
    /// domain", and showing a bare `*` for it is exactly the noise the owner
    /// asked to stop seeing.
    static func nestedLabel(_ pattern: String) -> String {
        let sub = subdomain(pattern)
        switch (local(pattern), sub.isEmpty) {
        case (nil, true): return "all senders"
        case (nil, false): return sub
        case (let local?, true): return local
        case (let local?, false): return "\(local) · \(sub)"
        }
    }

    // MARK: - the tree

    struct Row: Identifiable, Equatable {
        var rule: SenderRule
        /// Display text, already stripped of the `*@` and of any domain the
        /// header above it repeats.
        var label: String
        /// Sits under a domain header, so it draws indented and without its own
        /// icon.
        var nested: Bool
        var id: Int { rule.id }
    }

    struct DomainGroup: Identifiable, Equatable {
        /// The registrable domain, and the id, since a domain appears at most
        /// once per section.
        var domain: String
        /// Drawn as its own line above the rules, which happens only when there
        /// is more than one rule to gather.
        var showsHeader: Bool
        /// May this domain's icon be fetched? True only when some rule here
        /// names the whole domain rather than a person at it — see
        /// `iconPattern`.
        var iconEligible: Bool
        var rows: [Row]
        var id: String { domain }
    }

    struct Section: Identifiable, Equatable {
        var disposition: Disposition
        var groups: [DomainGroup]
        var id: String { disposition.rawValue }
        var ruleCount: Int { groups.reduce(0) { $0 + $1.rows.count } }
    }

    /// Section order, and it is the owner's: allow first, then filter, then
    /// mute. Reading order is strongest-to-weakest claim on the inbox.
    static let sectionOrder: [Disposition] = [.surface, .filtered, .squelch]

    /// Group `rules` into the page's display tree.
    static func sections(_ rules: [SenderRule]) -> [Section] {
        sectionOrder.compactMap { disposition in
            let mine = rules.filter { $0.disposition == disposition }
            if mine.isEmpty { return nil }

            var byDomain: [String: [SenderRule]] = [:]
            for rule in mine { byDomain[domainKey(rule.match_pattern), default: []].append(rule) }

            let groups = byDomain.keys.sorted().map { domain -> DomainGroup in
                let sorted = byDomain[domain]!.sorted(by: rulePrecedes)
                let multiple = sorted.count > 1
                return DomainGroup(
                    domain: domain,
                    showsHeader: multiple,
                    // A DOMAIN rule names a service; a mailbox rule names a
                    // person at one. Only the first earns a network lookup —
                    // see `iconPattern` for why that line is drawn here.
                    iconEligible: sorted.contains { isWildcard($0.match_pattern) },
                    rows: sorted.map { rule in
                        Row(
                            rule: rule,
                            label: multiple
                                ? nestedLabel(rule.match_pattern)
                                : standaloneLabel(rule.match_pattern),
                            nested: multiple)
                    })
            }
            return Section(disposition: disposition, groups: groups)
        }
    }

    /// Rules in the order the page draws them. THE SELECTION INDEXES THIS, not
    /// the fetch order: j/k must walk the list the reader can see, or the
    /// highlight jumps around the page.
    static func ordered(_ sections: [Section]) -> [SenderRule] {
        sections.flatMap { $0.groups.flatMap { $0.rows.map(\.rule) } }
    }

    /// Apex before subdomains, then alphabetical; a whole-domain rule before a
    /// single mailbox at the same host, since it is the broader claim.
    private static func rulePrecedes(_ a: SenderRule, _ b: SenderRule) -> Bool {
        let (sa, sb) = (subdomain(a.match_pattern), subdomain(b.match_pattern))
        if sa != sb { return sa < sb }
        let (la, lb) = (local(a.match_pattern), local(b.match_pattern))
        switch (la, lb) {
        case (nil, nil): return a.match_pattern < b.match_pattern
        case (nil, _): return true
        case (_, nil): return false
        case (let x?, let y?): return x < y
        }
    }

    // MARK: - icons

    /// The sender string to hand `Avatar` for a row or a header, and `nil` when
    /// no icon may be fetched for it.
    ///
    /// PRIVACY, and this is the one judgement in this file worth arguing with.
    /// `SenderIdentity` fetches a favicon only for robot and brand senders,
    /// because a human correspondent's domain must not leave the device; a rule
    /// pattern satisfies neither test, so on the old page every rule drew
    /// initials. Showing domain icons means widening that, and the widening is
    /// deliberately the narrowest one that answers the request: a WHOLE-DOMAIN
    /// rule (`*@garmin.com`) is a statement about a service, not about a
    /// person, and the owner wrote it as one. A rule naming one mailbox
    /// (`justice.dixon@useintuito.com`) stays on the existing rule and keeps
    /// its initials, because that is a private correspondent and the domain
    /// behind them is exactly what the file refuses to disclose.
    ///
    /// What this still discloses, stated plainly: the registrable domains the
    /// owner has written whole-domain rules for. That is a real disclosure and
    /// a smaller one than the page already makes for every brand in the inbox.
    static func iconPattern(_ pattern: String) -> String? {
        isWildcard(pattern) ? pattern : nil
    }
}
