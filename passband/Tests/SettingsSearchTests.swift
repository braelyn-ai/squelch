// The settings search, which is a ranking and therefore cannot be verified by
// looking at it: green here proves the plumbing, and only a fixture proves the
// ANSWER. So the suite is written as the queries a person actually types —
// "dark mode", "pictures", "how much does this cost" — each pinned to the card
// it must put first.
//
// Two failures matter more than a merely mediocre order:
//
//   * A word that answers nothing (the AND rule). If "dark signature" returned
//     both cards, searching would get vaguer the more you said, and a search
//     that punishes precision is one people stop using after two tries.
//   * A card with no words. The index is hand-written and the app is not, so
//     the coverage check below is what fails when somebody adds a control and
//     no way to find it.

import Foundation

@main
@MainActor
struct SettingsSearchTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        theWordsOnTheControl()
        theWordsPeopleActuallyUse()
        phrasesBeatTheirParts()
        everyTermMustLand()
        typingForwards()
        wholeQuestions()
        oneCardPerHit()
        theIndexIsWellFormed()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - the queries

    /// The easy half: somebody who can see the control and is only avoiding a
    /// hunt through six panes.
    static func theWordsOnTheControl() {
        first("signature", is: .signature)
        first("theme", is: .appearance)
        first("telemetry", is: .privacy)
        first("server url", is: .connection)
        first("read tracking", is: .readTracking)
        first("assistant model", is: .assistant)
        first("dev mode", is: .developer)
        first("notification sound", is: .notifications)
    }

    /// The half the keyword lists exist for: the word for the EFFECT, which is
    /// almost never the word on the switch.
    static func theWordsPeopleActuallyUse() {
        first("dark mode", is: .appearance)
        first("night", is: .appearance)
        first("pictures", is: .mail)
        first("blocked images", is: .mail)
        first("bubbles", is: .mail)
        first("chime", is: .notifications)
        first("posthog", is: .privacy)
        first("opt out", is: .privacy)
        first("sign off", is: .signature)
        first("walkthrough", is: .tour)
        first("changelog", is: .whatsNew)
        first("refer a friend", is: .account)
        first("mailbox", is: .account)
        first("how expensive", is: .triageBudget)
        first("dollars", is: .triageBudget)
        first("bring your own key", is: .assistant)
        first("greeting", is: .you)
        first("8848", is: .connection)
    }

    /// Two ordinary words that name one specific thing. Both halves match half
    /// the app; said together they must beat everything they are made of.
    static func phrasesBeatTheirParts() {
        first("read tracking", is: .readTracking)
        first("for your eyes", is: .ranking)
        first("stage 2", is: .triageBudget)
        first("do not disturb", is: .notifications)
        // "api key" is the assistant's; "api token" is the daemon's. The two
        // live one word apart and mean entirely different secrets.
        first("api key", is: .assistant)
        first("api token", is: .connection)
    }

    /// AND, not OR. A query whose terms belong to different cards is a query
    /// nothing satisfies, and saying so is the honest answer.
    static func everyTermMustLand() {
        empty("dark signature")
        empty("zzzz")
        empty("theme zzzz")
        empty("")
        empty("   ")
        // Punctuation is not a term. A query that is only punctuation has said
        // nothing, and must not be read as a match on everything.
        empty("!!!")
    }

    /// The list has to be useful mid-word, because it is read while typing.
    static func typingForwards() {
        first("notif", is: .notifications)
        first("signa", is: .signature)
        first("telem", is: .privacy)
        // Past the word, too: keywords are singular or plural, never both.
        first("notifications", is: .notifications)
    }

    /// A search field invites a sentence, and half the people who use one type
    /// the whole question. Under AND, one throwaway word would zero the lot, so
    /// the filler list is what keeps a question worth as much as a noun.
    static func wholeQuestions() {
        first("how do i turn off images", is: .mail)
        first("how much does this cost", is: .triageBudget)
        first("what is my name", is: .you)
        first("where do i put my api key", is: .assistant)
        // All filler and nothing else: the words are given back rather than
        // thrown away, so this is still the triage explainer's question.
        first("how", is: .triagePipeline)
    }

    /// Three of the triage budget's settings share one card. A search that
    /// listed it three times would be reporting on the index rather than
    /// answering the question.
    static func oneCardPerHit() {
        let cards = SettingsSearch.matches("cap")
        expect(cards.count == Set(cards).count, "no card appears twice in one result list")
        expect(cards.contains(.triageBudget), "\"cap\" finds the budget card")
    }

    /// The index against the app it indexes.
    static func theIndexIsWellFormed() {
        // Every card that renders is findable. This is the check that fails
        // when a new setting ships with no words attached.
        for card in SettingsCard.allCases where card.isAvailable {
            expect(
                SettingsSearch.entries.contains { $0.card == card },
                "card \(card.rawValue) has at least one indexed setting")
        }
        for entry in SettingsSearch.entries {
            expect(!entry.title.isEmpty, "\(entry.card.rawValue) entry has a title")
            expect(!entry.blurb.isEmpty, "\(entry.title) has a blurb")
            expect(entry.keywords.count >= 5, "\(entry.title) carries enough keywords")
            for keyword in entry.keywords {
                expect(
                    keyword == SettingsSearch.normalize(keyword),
                    "keyword \"\(keyword)\" is already normalized")
                expect(!SettingsSearch.tokens(keyword).isEmpty, "keyword \"\(keyword)\" has words")
            }
            // A setting has to answer to its own name, or the index is
            // describing something the screen does not.
            expect(
                SettingsSearch.matches(entry.title).contains(entry.card),
                "\"\(entry.title)\" finds its own card")
        }
        // Section labels are matched against, so they have to be real words.
        for section in SettingsSection.allCases {
            expect(!SettingsSearch.tokens(section.label).isEmpty, "\(section.rawValue) has a label")
        }
    }

    // MARK: - helpers

    static func first(_ query: String, is card: SettingsCard) {
        let cards = SettingsSearch.matches(query)
        expect(
            cards.first == card,
            "\"\(query)\" ranks \(card.rawValue) first (got \(cards.first?.rawValue ?? "nothing"))")
    }

    static func empty(_ query: String) {
        let cards = SettingsSearch.matches(query)
        expect(
            cards.isEmpty,
            "\"\(query)\" answers nothing (got \(cards.map(\.rawValue).joined(separator: ", ")))")
    }

    static func expect(_ cond: Bool, _ what: String) {
        checks += 1
        if !cond {
            failures += 1
            print("  FAIL: \(what)")
        }
    }
}
