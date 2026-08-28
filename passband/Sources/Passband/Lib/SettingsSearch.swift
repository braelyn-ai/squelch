// Finding a setting by describing it, rather than by knowing which of six
// panes somebody filed it under.
//
// THE INDEX IS A SEPARATE ARTEFACT FROM THE UI, and it has to be: the words a
// person reaches for are almost never the words on the control. Nobody hunting
// the remote-image switch types "images" — they type "pictures", "blocked",
// "tracking". So every setting carries a keyword list and a sentence written
// for the SEARCH, and neither is ever drawn on screen. They exist to be matched
// against; the result the user sees is the real control, live, in its own card.
//
// Ranking is deliberately dumb and deterministic — token overlap with
// per-field weights, no stemming, no fuzz. The corpus is thirty-odd entries
// written by hand, so a hand-tuned weight beats anything cleverer, and a pure
// function is one a test can pin (Tests/SettingsSearchTests.swift). Foundation
// only, for that reason: no SwiftUI, no Prefs, nothing to stand up.

import Foundation

/// The Settings sub-nav sections; the last-active one is restored on reopen.
///
/// Lives HERE rather than beside `Prefs` because the search index is the other
/// half of the same taxonomy: a section is a bucket of cards, and a card is a
/// bucket of entries. Keeping the three in one file is what lets the whole lot
/// compile into a headless test with no app around it.
enum SettingsSection: String, CaseIterable, Sendable {
    case general, mail, triage, assistant, privacy, account

    var label: String {
        switch self {
        case .general: "General"
        case .mail: "Mail"
        case .triage: "Triage"
        case .assistant: "Assistant"
        case .privacy: "Privacy"
        case .account: "Account"
        }
    }
}

/// One card on a settings pane — the unit a search result is DRAWN as.
///
/// Search ranks individual settings and then renders whole cards, because a
/// card is the smallest thing that still works: `MailSection` holds the image
/// switch and the thread switch, and splitting it so a result could show one
/// without the other would fork behaviour that two shells share. So a hit on
/// "pictures" surfaces the Mail card with both switches on it, and the entry
/// that matched is the reason it is at the top rather than something the user
/// has to read.
///
/// The raw values are stable ids; `label` mirrors what `SectionCard` prints.
enum SettingsCard: String, CaseIterable, Sendable {
    case connection, appearance, notifications, tour, whatsNew, developer, you
    case mail, signature, readTracking
    case triagePipeline, triageBudget, ranking
    case assistant
    case privacy
    case account

    var section: SettingsSection {
        switch self {
        case .connection, .appearance, .notifications, .tour, .whatsNew, .developer, .you:
            .general
        case .mail, .signature, .readTracking: .mail
        case .triagePipeline, .triageBudget, .ranking: .triage
        case .assistant: .assistant
        case .privacy: .privacy
        case .account: .account
        }
    }

    /// Whether this card renders at all on the platform in hand. The tour and
    /// the what's-new card are driven by the desktop's ActionLayer, which has
    /// no iOS host: the phone's settings never mount them, so a phone search
    /// must never offer them either.
    var isAvailable: Bool {
        #if os(macOS)
            true
        #else
            self != .tour && self != .whatsNew
        #endif
    }

    /// What the card calls itself on screen. Matched against too: somebody who
    /// remembers the heading and not the control still gets there.
    var label: String {
        switch self {
        case .connection: "Connection"
        case .appearance: "Appearance"
        case .notifications: "Notifications"
        case .tour: "Tour"
        case .whatsNew: "What's new"
        case .developer: "Developer"
        case .you: "You"
        case .mail: "Mail"
        case .signature: "Signature"
        case .readTracking: "Read tracking"
        case .triagePipeline: "How triage works"
        case .triageBudget: "Triage budget"
        case .ranking: "For your eyes"
        case .assistant: "Assistant"
        case .privacy: "Developer Telemetry"
        case .account: "Accounts"
        }
    }
}

/// One searchable setting. `title` is the control's own name; `blurb` and
/// `keywords` are written for the matcher and are NEVER rendered.
struct SettingsEntry: Sendable {
    var card: SettingsCard
    var title: String
    /// A sentence about what the setting does, in the words somebody would use
    /// to describe wanting it.
    var blurb: String
    /// The other names for it: synonyms, symptoms, and the phrase somebody
    /// types when they know the effect but not the vocabulary.
    var keywords: [String]
}

/// A card that matched, with the score that ordered it.
struct SettingsHit: Sendable, Equatable {
    var card: SettingsCard
    var score: Double
}

enum SettingsSearch {
    // MARK: - the index

    /// EVERY setting in the app, once each. A control with no entry here is a
    /// control nobody can find by describing it, which is why the coverage
    /// assertion in the test suite fails a card that gains one and no words.
    static let entries: [SettingsEntry] = generalEntries + mailEntries + triageEntries
        + assistantEntries + privacyEntries + accountEntries

    private static let generalEntries: [SettingsEntry] = [
        SettingsEntry(
            card: .connection,
            title: "Server URL",
            blurb: "The address of the squelchd daemon this app talks to.",
            keywords: [
                "server", "url", "address", "host", "hostname", "daemon", "backend",
                "connect", "connection", "endpoint", "port", "localhost", "8848", "ip",
                "reconnect", "offline", "cannot connect",
            ]),
        SettingsEntry(
            card: .connection,
            title: "API token",
            blurb: "The bearer token that proves this app may talk to your daemon.",
            keywords: [
                "api token", "token", "auth", "authentication", "bearer", "credential",
                "secret", "password", "login", "sign in", "keychain", "squelch_api_token",
                "unauthorized", "401",
            ]),
        SettingsEntry(
            card: .appearance,
            title: "Theme",
            blurb: "Whether the app is drawn light, dark, or however the system is set.",
            keywords: [
                "theme", "appearance", "dark", "dark mode", "light", "light mode", "night",
                "night mode", "color", "colour", "colors", "scheme", "auto", "system",
                "contrast", "brightness", "look",
            ]),
        SettingsEntry(
            card: .notifications,
            title: "Notification sound",
            blurb: "Which chime plays when an urgent or deadline banner arrives.",
            keywords: [
                "sound", "chime", "tone", "audio", "noise", "ring", "ringtone", "alert",
                "alert sound", "notification", "notifications", "banner", "beep", "mute",
                "silent", "quiet", "morse", "static", "carrier",
            ]),
        SettingsEntry(
            card: .notifications,
            title: "Test banner",
            blurb: "Posts a notification right now to prove banners reach you.",
            keywords: [
                "test", "test notification", "post a banner", "banner", "notifications",
                "not working", "no notifications", "permission", "allow notifications",
                "do not disturb", "focus", "alerts",
            ]),
        SettingsEntry(
            card: .tour,
            title: "Replay the tour",
            blurb: "Runs the seven step first run walkthrough over your own board again.",
            keywords: [
                "tour", "walkthrough", "onboarding", "tutorial", "guide", "intro",
                "introduction", "first run", "getting started", "replay", "help", "again",
                "how it works",
            ]),
        SettingsEntry(
            card: .whatsNew,
            title: "Release notes",
            blurb: "What the version you are running brought, in the app and in the daemon.",
            keywords: [
                "whats new", "what is new", "release notes", "changelog", "changes",
                "version", "news", "updates", "latest", "new features",
            ]),
        SettingsEntry(
            card: .developer,
            title: "Dev mode",
            blurb: "Adds the re-triage buttons that re-run the triage pipeline on a thread.",
            keywords: [
                "developer", "dev mode", "debug", "advanced", "retriage", "re-triage",
                "re triage", "rerun triage", "internal", "power user",
            ]),
        SettingsEntry(
            card: .you,
            title: "Your name",
            blurb: "The name the sitrep greets you by.",
            keywords: [
                "name", "your name", "my name", "display name", "nickname", "greeting",
                "hello", "profile", "who am i", "you",
            ]),
    ]

    private static let mailEntries: [SettingsEntry] = [
        SettingsEntry(
            card: .mail,
            title: "Remote images",
            blurb: "Whether pictures in an email load automatically or wait for you to ask.",
            keywords: [
                "images", "image", "pictures", "photos", "graphics", "remote images",
                "load images", "blocked images", "show images", "broken images",
                "tracking pixels", "privacy", "attachments not showing",
            ]),
        SettingsEntry(
            card: .mail,
            title: "Thread style",
            blurb: "The shape a conversation is drawn in: stacked email cards or chat bubbles.",
            keywords: [
                "thread", "threads", "thread style", "conversation", "conversations",
                "chat", "bubbles", "messages", "email style", "layout", "view", "display",
                "automatic", "reading",
            ]),
        SettingsEntry(
            card: .signature,
            title: "Signature",
            blurb: "The markdown block added under new messages and replies as you draft them.",
            keywords: [
                "signature", "sig", "sign off", "signoff", "closing", "footer", "regards",
                "sent from", "markdown", "outgoing", "compose", "replies",
            ]),
        SettingsEntry(
            card: .readTracking,
            title: "Read tracking",
            blurb: "Whether mail you send carries a pixel that reports when it was opened.",
            keywords: [
                "read tracking", "read receipts", "receipts", "tracking", "track", "pixel",
                "opens", "opened", "seen", "was it read", "delivery", "confirmation",
            ]),
    ]

    private static let triageEntries: [SettingsEntry] = [
        SettingsEntry(
            card: .triagePipeline,
            title: "How triage works",
            blurb: "The path a new email takes: sealing, your rules, then the two model stages.",
            keywords: [
                "triage", "how triage works", "pipeline", "stages", "stage 1", "stage 2",
                "escalation", "escalate", "seal", "sealed", "sender rules", "rules",
                "model", "models", "heuristic", "explainer", "diagram", "sorting",
                "filtering", "why was this filed",
            ]),
        SettingsEntry(
            card: .triageBudget,
            title: "Stage 1 daily cap",
            blurb: "How many emails a day the first model may look at before triage falls back.",
            keywords: [
                "stage 1", "stage one", "daily cap", "cap", "caps", "limit", "limits",
                "budget", "quota", "throttle", "per day", "global cap", "calls",
            ]),
        SettingsEntry(
            card: .triageBudget,
            title: "Stage 2 daily caps",
            blurb: "How many escalations a day are allowed per thread, per sender, and overall.",
            keywords: [
                "stage 2", "stage two", "escalation cap", "per thread", "per sender",
                "thread cap", "sender cap", "global cap", "daily cap", "caps", "limit",
                "budget", "quota",
            ]),
        SettingsEntry(
            card: .triageBudget,
            title: "Spend estimate",
            blurb: "What triage costs you a day at your current caps and usage.",
            keywords: [
                "cost", "costs", "spend", "spending", "price", "pricing", "estimate",
                "money", "dollars", "bill", "billing", "budget", "expensive", "monthly",
                "per month", "tokens", "usage",
            ]),
        SettingsEntry(
            card: .ranking,
            title: "For your eyes ranking",
            blurb: "Whether the top zone leans on how soon something is due or how bad it is.",
            keywords: [
                "ranking", "rank", "order", "ordering", "sort", "sorting", "for your eyes",
                "sitrep", "urgency", "severity", "importance", "priority", "deadline",
                "time", "due", "blend", "slider", "weight",
            ]),
    ]

    private static let assistantEntries: [SettingsEntry] = [
        SettingsEntry(
            card: .assistant,
            title: "Chats via",
            blurb: "Whether assistant chats go through your daemon's plan or your own key.",
            keywords: [
                "relay", "byok", "bring your own key", "my own key", "transport",
                "chats via", "hosted", "plan", "assistant budget", "who pays",
            ]),
        SettingsEntry(
            card: .assistant,
            title: "Assistant API key",
            blurb: "Your own Anthropic key, kept in this device's keychain, spent only by the assistant.",
            keywords: [
                "api key", "key", "anthropic key", "claude key", "sk-ant", "byok",
                "assistant key", "secret", "keychain", "credential", "provider key",
            ]),
        SettingsEntry(
            card: .assistant,
            title: "Assistant model",
            blurb: "Which Claude model answers when you ask your inbox something.",
            keywords: [
                "model", "assistant model", "claude", "opus", "haiku", "sonnet", "llm",
                "ai", "smarter", "faster", "cheaper", "which model",
            ]),
    ]

    private static let privacyEntries: [SettingsEntry] = [
        SettingsEntry(
            card: .privacy,
            title: "Developer telemetry",
            blurb: "How much anonymous usage data leaves the app, and how to send none.",
            keywords: [
                "telemetry", "analytics", "privacy", "tracking", "posthog", "data",
                "usage data", "anonymous", "opt out", "opt-out", "stop sending",
                "diagnostics", "metrics", "share data",
            ]),
    ]

    private static let accountEntries: [SettingsEntry] = [
        SettingsEntry(
            card: .account,
            title: "Accounts",
            blurb: "The mailboxes this install knows about, and the chords that switch them.",
            keywords: [
                "account", "accounts", "mailbox", "mailboxes", "add account",
                "remove account", "delete account", "switch account", "multiple accounts",
                "second account", "rename", "sign out", "log out", "disconnect", "gmail",
            ]),
        SettingsEntry(
            card: .account,
            title: "Live account",
            blurb: "Which daemon, provider, and triage model the account on screen is running.",
            keywords: [
                "live account", "current account", "provider", "triage model", "server",
                "which model", "status", "info",
            ]),
        SettingsEntry(
            card: .account,
            title: "Invites",
            blurb: "Send a friend an invite to Passband from your own address.",
            keywords: [
                "invite", "invites", "invitation", "share", "sharing", "refer", "referral",
                "friend", "friends", "give access",
            ]),
    ]

    // MARK: - matching

    /// The ranked cards for a query, best first. Empty for a blank query and
    /// for one that matches nothing — the caller distinguishes those by asking
    /// the query itself, not the result.
    static func matches(_ query: String) -> [SettingsCard] {
        hits(query).map(\.card)
    }

    /// Ranked cards WITH their scores. Cards, not entries: three keyword sets
    /// can point at the same card (a triage cap, an escalation cap, the spend
    /// estimate all live on one), and a result list that showed it three times
    /// would be a worse answer, not a fuller one.
    static func hits(_ query: String) -> [SettingsHit] {
        let terms = terms(of: query)
        guard !terms.isEmpty else { return [] }
        let phrase = normalize(query)

        var best: [SettingsCard: Double] = [:]
        var extras: [SettingsCard: Int] = [:]
        for entry in entries where entry.card.isAvailable {
            let score = score(entry, terms: terms, phrase: phrase)
            guard score > 0 else { continue }
            if let standing = best[entry.card] {
                best[entry.card] = Swift.max(standing, score)
                extras[entry.card, default: 0] += 1
            } else {
                best[entry.card] = score
            }
        }

        // A card several of whose settings match is a better answer than one
        // where a single entry scraped through, but only slightly: the margin
        // must never outrank a genuinely stronger single hit.
        let ordering = Dictionary(
            uniqueKeysWithValues: SettingsCard.allCases.enumerated().map { ($1, $0) })
        return
            best
            .map { SettingsHit(card: $0.key, score: $0.value + 0.25 * Double(extras[$0.key] ?? 0)) }
            .sorted {
                // Declaration order breaks ties, so a query with two equal
                // answers lists them the way the panes do rather than however
                // the dictionary happened to hash.
                $0.score == $1.score
                    ? ordering[$0.card, default: 0] < ordering[$1.card, default: 0]
                    : $0.score > $1.score
            }
    }

    /// AND across the query's terms: every term must find something, so
    /// "dark signature" answers nothing rather than answering both. A search
    /// that gets broader as you type more is a search you stop trusting.
    private static func score(_ entry: SettingsEntry, terms: [String], phrase: String) -> Double {
        var total = 0.0
        for term in terms {
            let best = termScore(entry, term)
            guard best > 0 else { return 0 }
            total += best
        }
        // The whole query said as one phrase inside a name or a keyword: "read
        // tracking" is two ordinary words that mean one specific switch.
        if phrase.count >= 3 {
            if normalize(entry.title).contains(phrase) { total += 8 }
            else if entry.keywords.contains(where: { normalize($0).contains(phrase) }) {
                total += 6
            }
        }
        return total
    }

    /// One term against one entry, across every field it could hit. Fields are
    /// weighted by how deliberate a match in them is: a control's own name is
    /// the strongest signal, a word from its sentence the weakest.
    private static func termScore(_ entry: SettingsEntry, _ term: String) -> Double {
        var best = 0.0
        best = Swift.max(best, 6 * fieldScore(tokens(entry.title), term))
        for keyword in entry.keywords {
            best = Swift.max(best, 5 * fieldScore(tokens(keyword), term))
        }
        best = Swift.max(best, 3 * fieldScore(tokens(entry.card.label), term))
        best = Swift.max(best, 2 * fieldScore(tokens(entry.card.section.label), term))
        best = Swift.max(best, 1.5 * fieldScore(tokens(entry.blurb), term))
        return best
    }

    /// The best any word in one field does against the term.
    private static func fieldScore(_ words: [String], _ term: String) -> Double {
        var best = 0.0
        for word in words {
            if word == term { return 1 }
            if word.hasPrefix(term) {
                // Typing forwards: "notif" is on its way to "notifications".
                best = Swift.max(best, 0.7)
            } else if term.hasPrefix(word), word.count >= 4 {
                // Typing past it: "notifications" should still find "notify".
                best = Swift.max(best, 0.5)
            }
        }
        return best
    }

    // MARK: - text

    /// Lowercase, apostrophes dropped so "what's" and "whats" are one word.
    static func normalize(_ text: String) -> String {
        text.lowercased()
            .replacingOccurrences(of: "'", with: "")
            .replacingOccurrences(of: "\u{2019}", with: "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Words, by anything that is not a letter or a digit. Digits are kept
    /// because "stage 2" and "8848" are things people search for.
    static func tokens(_ text: String) -> [String] {
        normalize(text)
            .split(whereSeparator: { !$0.isLetter && !$0.isNumber })
            .map(String.init)
    }

    /// Words somebody types on the way to the word they mean. They are dropped
    /// from the QUERY only, never from the index, and only when something else
    /// survives — "how much does this cost" has to answer the same as "cost",
    /// while "how" on its own is still a fair search for "how triage works".
    ///
    /// This matters because matching is AND: without it, one throwaway word in
    /// a sentence-shaped query zeroes every card, and the person who typed a
    /// whole question gets less than the person who typed one noun.
    private static let filler: Set<String> = [
        "a", "am", "an", "and", "any", "are", "be", "can", "did", "do", "does", "find", "for",
        "from", "get", "go", "has", "have", "how", "i", "if", "in", "is", "it", "just", "let",
        "make", "me", "much", "my", "need", "of", "off", "on", "or", "please", "put", "see",
        "set", "should", "so", "some", "that", "the", "there", "this", "to", "turn", "use",
        "using", "want", "was", "way", "we", "were", "what", "when", "where", "which", "why",
        "will", "with", "would", "you", "your",
    ]

    private static func terms(of query: String) -> [String] {
        let all = tokens(query)
        let meaty = all.filter { !filler.contains($0) }
        return meaty.isEmpty ? all : meaty
    }
}
