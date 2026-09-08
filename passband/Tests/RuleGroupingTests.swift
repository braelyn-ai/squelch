// The shape of the rules page, asserted rather than eyeballed.
//
// This suite exists because the interesting half of the design is INVISIBLE on
// the mailbox it was built against: eleven rules naming eleven different
// domains produce eleven plain rows and never once exercise the nesting. The
// header/child structure, the labels that drop the `*@`, the apex-first
// ordering and the selection order that j/k walks are all only reachable from a
// mailbox that writes several rules for one domain — so they are pinned here,
// where the fixture can be whatever the code needs to survive.
//
// The privacy line gets its own section. `iconPattern` widens what
// SenderIdentity is willing to send to an icon service, and a widening that
// reaches one address too far does not crash and does not fail a build.

import Foundation

@main
@MainActor
struct RuleGroupingTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        patternsComeApart()
        labelsDropWhatTheReaderKnows()
        oneDomainOneRuleStaysFlat()
        severalRulesNestUnderTheirDomain()
        sectionsAreOrderedAndCounted()
        selectionWalksWhatIsDrawn()
        onlyDomainRulesMayFetchAnIcon()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - fixtures

    static func rule(_ id: Int, _ pattern: String, _ d: Disposition, want: String = "")
        -> SenderRule
    {
        SenderRule(
            id: id, account_id: 1, match_pattern: pattern, want_text: want, disposition: d,
            updated_at: "2026-09-01T00:00:00Z")
    }

    // MARK: - anatomy

    static func patternsComeApart() {
        expect(RuleGrouping.host("*@billing.garmin.com") == "billing.garmin.com", "host of a wildcard")
        expect(RuleGrouping.local("*@billing.garmin.com") == nil, "a wildcard has no local part")
        expect(RuleGrouping.local("noreply@garmin.com") == "noreply", "a mailbox does")
        expect(RuleGrouping.isWildcard("*@garmin.com"), "whole-domain rule")
        expect(!RuleGrouping.isWildcard("noreply@garmin.com"), "mailbox rule")

        // The grouping key is the REGISTRABLE domain, so a subdomain rule and
        // an apex rule land together.
        expect(RuleGrouping.domainKey("*@billing.garmin.com") == "garmin.com", "subdomain folds up")
        expect(RuleGrouping.domainKey("*@garmin.com") == "garmin.com", "apex is its own key")
        expect(RuleGrouping.subdomain("*@billing.garmin.com") == "billing", "subdomain label")
        expect(RuleGrouping.subdomain("*@garmin.com") == "", "apex has no subdomain")

        // A compound suffix must not be read as the site, or `co.uk` becomes a
        // group of its own and every British sender piles into it. This is
        // inherited from SenderID rather than re-derived, and that is the point.
        expect(
            RuleGrouping.domainKey("*@news.marks-and-spencer.co.uk") == "marks-and-spencer.co.uk",
            "compound suffix survives grouping")
        expect(
            RuleGrouping.subdomain("*@news.marks-and-spencer.co.uk") == "news",
            "and its subdomain still comes off")

        // Degenerate patterns must not crash or vanish.
        expect(RuleGrouping.host("garmin.com") == nil, "no @ means no host")
        expect(RuleGrouping.domainKey("garmin.com") == "garmin.com", "and it groups under itself")
    }

    // MARK: - labels

    static func labelsDropWhatTheReaderKnows() {
        // THE ASK: a bare `*@` is noise. "the owner wrote a rule for
        // futureme.org" is the whole of what `*@futureme.org` says.
        expect(
            RuleGrouping.standaloneLabel("*@futureme.org") == "futureme.org",
            "the wildcard is dropped")
        expect(
            RuleGrouping.standaloneLabel("*@notify.pic-time.com") == "notify.pic-time.com",
            "a lone subdomain rule keeps its full host")
        // A mailbox rule keeps its local part: that IS the distinguishing half.
        expect(
            RuleGrouping.standaloneLabel("justice.dixon@useintuito.com")
                == "justice.dixon@useintuito.com",
            "a person's address is shown whole")

        // Nested under a header that already says the domain.
        expect(RuleGrouping.nestedLabel("*@garmin.com") == "all senders", "the apex needs words")
        expect(RuleGrouping.nestedLabel("*@billing.garmin.com") == "billing", "just the subdomain")
        expect(RuleGrouping.nestedLabel("noreply@garmin.com") == "noreply", "just the mailbox")
        expect(
            RuleGrouping.nestedLabel("noreply@billing.garmin.com") == "noreply · billing",
            "both halves when both distinguish")
    }

    // MARK: - the tree

    static func oneDomainOneRuleStaysFlat() {
        // The user's actual mailbox: every rule a different domain. No headers,
        // no indentation, and the label is the full host.
        let sections = RuleGrouping.sections([
            rule(1, "*@futureme.org", .surface),
            rule(2, "*@notify.pic-time.com", .surface),
        ])
        expect(sections.count == 1, "one disposition present")
        let groups = sections[0].groups
        expect(groups.count == 2, "two domains")
        expect(groups.allSatisfy { !$0.showsHeader }, "a lone rule needs no header")
        expect(groups.allSatisfy { $0.rows.count == 1 }, "one row each")
        expect(groups.allSatisfy { !$0.rows[0].nested }, "and nothing is indented")
        expect(groups[0].domain == "futureme.org", "domains sort alphabetically")
        expect(groups[1].rows[0].label == "notify.pic-time.com", "full host on a flat row")
    }

    static func severalRulesNestUnderTheirDomain() {
        let sections = RuleGrouping.sections([
            rule(1, "*@billing.garmin.com", .filtered),
            rule(2, "*@garmin.com", .filtered),
            rule(3, "noreply@garmin.com", .filtered),
            rule(4, "*@loopnet.com", .filtered),
        ])
        let groups = sections[0].groups
        expect(groups.count == 2, "garmin.com and loopnet.com")

        let garmin = groups[0]
        expect(garmin.domain == "garmin.com", "grouped by registrable domain")
        expect(garmin.showsHeader, "three rules earn a header")
        expect(garmin.rows.count == 3, "all three gather")
        expect(garmin.rows.allSatisfy(\.nested), "and all three indent")

        // APEX FIRST, then the mailbox at the apex, then the subdomain: the
        // broadest claim on the domain reads first.
        expect(
            garmin.rows.map(\.label) == ["all senders", "noreply", "billing"],
            "apex before mailbox before subdomain, got \(garmin.rows.map(\.label))")

        // The lone domain beside it is untouched by its neighbour's nesting.
        expect(!groups[1].showsHeader, "a single rule stays flat in mixed company")
        expect(groups[1].rows[0].label == "loopnet.com", "and keeps its plain label")
    }

    static func sectionsAreOrderedAndCounted() {
        let sections = RuleGrouping.sections([
            rule(1, "*@a.com", .squelch),
            rule(2, "*@b.com", .filtered),
            rule(3, "*@c.com", .surface),
            rule(4, "*@d.com", .surface),
        ])
        expect(
            sections.map(\.disposition) == [.surface, .filtered, .squelch],
            "allow, then filter, then mute")
        expect(sections.map(\.disposition.label) == ["allow", "filter", "mute"], "as the owner reads them")
        expect(sections[0].ruleCount == 2, "a section counts its rules, not its domains")

        // An absent disposition draws no empty section.
        let onlyMutes = RuleGrouping.sections([rule(1, "*@a.com", .squelch)])
        expect(onlyMutes.count == 1, "empty sections are omitted")
        expect(onlyMutes[0].disposition == .squelch, "and the present one survives")
        expect(RuleGrouping.sections([]).isEmpty, "no rules, no sections")
    }

    static func selectionWalksWhatIsDrawn() {
        // j/k indexes THIS, not the fetch order. If the two disagree the
        // highlight jumps around the page as the reader holds j.
        let fetched = [
            rule(1, "*@zoox.com", .squelch),
            rule(2, "*@billing.garmin.com", .surface),
            rule(3, "*@garmin.com", .surface),
            rule(4, "*@apple.com", .filtered),
        ]
        let ordered = RuleGrouping.ordered(RuleGrouping.sections(fetched))
        expect(ordered.count == fetched.count, "every rule is reachable")
        expect(
            ordered.map(\.id) == [3, 2, 4, 1],
            "drawn order: allow(garmin apex, garmin sub), filter, mute — got \(ordered.map(\.id))")
        expect(Set(ordered.map(\.id)) == Set(fetched.map(\.id)), "nothing is dropped or duplicated")
    }

    // MARK: - privacy

    static func onlyDomainRulesMayFetchAnIcon() {
        // A whole-domain rule is a statement about a service. The owner wrote
        // it as one, so its domain may be resolved.
        expect(RuleGrouping.iconPattern("*@garmin.com") != nil, "a domain rule may fetch")
        expect(RuleGrouping.iconPattern("*@billing.garmin.com") != nil, "a subdomain rule too")

        // THE LINE. A rule naming one mailbox names a person, and that
        // person's domain is precisely what SenderIdentity exists to keep on
        // the device. It draws initials instead.
        expect(
            RuleGrouping.iconPattern("justice.dixon@useintuito.com") == nil,
            "a private correspondent's domain never leaves")
        expect(
            RuleGrouping.iconPattern("noah.c@easypromptfundssolutionshere.info") == nil,
            "not even for an obvious spammer — the shape is what decides")

        // A group is eligible only when something in it is a domain rule, so a
        // domain holding nothing but human addresses stays dark.
        let humans = RuleGrouping.sections([
            rule(1, "alice@acme.com", .squelch),
            rule(2, "bob@acme.com", .squelch),
        ])
        expect(!humans[0].groups[0].iconEligible, "two people at one domain is still two people")

        let mixed = RuleGrouping.sections([
            rule(1, "alice@acme.com", .squelch),
            rule(2, "*@acme.com", .squelch),
        ])
        expect(mixed[0].groups[0].iconEligible, "one domain rule makes the domain fair game")
    }

    // MARK: - harness

    static func expect(_ condition: Bool, _ what: String) {
        checks += 1
        if !condition {
            failures += 1
            print("  FAIL: \(what)")
        }
    }
}
