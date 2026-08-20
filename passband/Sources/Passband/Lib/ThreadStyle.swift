// HOW A THREAD IS DRAWN. `classic` is the stack of email cards the reader has
// always been; `bubbles` reads the same mail as a conversation, with the user's
// own side right-aligned.
//
// THREE ANSWERS, TWO STYLES. `ThreadStyleDefault` is what Settings holds and it
// has an Automatic that reads the thread; `ThreadStyle` is what the reader ends
// up drawing, and it stays binary because everything downstream of the decision
// (the minimap's measure, the frame cache key, the per-thread ledger) is one of
// two things and not one of three.
//
// THE READER OUTRANKS THE GUESS, ALWAYS. A thread switched by hand is written
// into ThreadStyleLedger and pinned there for good: `automatic` re-reads the
// thread every time it is opened, so an unpinned answer is a moving target, and
// "not this one" should only ever have to be said once.
//
// PURE ON PURPOSE — no SwiftUI, no account, nothing to construct. The minimap's
// arithmetic takes one of these (a bubble is a narrower measure, so it is a
// different number of lines), and both that file and the guess below are
// asserted by test suites built from source files alone.

import Foundation

enum ThreadStyle: String, CaseIterable, Sendable {
    case classic, bubbles

    var label: String {
        switch self {
        case .classic: "Email"
        case .bubbles: "Chat"
        }
    }

    /// The other one, which is what the reader's toggle switches to.
    var flipped: ThreadStyle {
        self == .classic ? .bubbles : .classic
    }

    /// Named for where the button GOES, not for where it is: the control shows
    /// the style you are one press away from.
    var symbol: String {
        switch self {
        case .classic: "list.bullet.rectangle"
        case .bubbles: "bubble.left.and.bubble.right"
        }
    }

    var actionLabel: String {
        switch self {
        case .classic: "email style"
        case .bubbles: "chat style"
        }
    }

    var actionHelp: String {
        switch self {
        case .classic: "read this thread as email cards"
        case .bubbles: "read this thread as chat bubbles"
        }
    }

    /// The per-message key the frame pool and the height memory file a rendered
    /// document under. THE STYLE IS PART OF IT because both are width-dependent:
    /// a bubble measures its document at the bubble's measure, and handing that
    /// frame (or that remembered height) to a full-width card paints the message
    /// at the wrong size until it re-measures. Classic keeps the bare message id
    /// so nothing that already clears by id has to learn a second spelling.
    func frameKey(_ messageId: Int) -> String {
        self == .classic ? String(messageId) : "\(messageId).chat"
    }
}

/// WHAT SETTINGS HOLDS: the two styles as standing orders, plus Automatic,
/// which is not a style but an instruction to look at the thread first. Kept
/// apart from `ThreadStyle` so nothing downstream ever has to handle a third
/// case that is not a way of drawing anything.
enum ThreadStyleDefault: String, CaseIterable, Sendable {
    case auto, classic, bubbles

    var label: String {
        switch self {
        case .auto: "Automatic"
        case .classic: "Email"
        case .bubbles: "Chat"
        }
    }

    /// The style this answer names outright, or nil when it names none — which
    /// is Automatic, and means the thread itself has to be read.
    var fixed: ThreadStyle? {
        switch self {
        case .auto: nil
        case .classic: .classic
        case .bubbles: .bubbles
        }
    }
}

// MARK: - the automatic choice

extension ThreadStyle {
    /// ONE MESSAGE, REDUCED TO THE FOUR THINGS THE GUESS ASKS ABOUT. Deliberately
    /// not the message itself: the wire type belongs to the daemon and drags the
    /// whole decoder in with it, and this has to build from source files alone.
    struct Sample: Sendable {
        /// True for the user's own copy, false for received, nil for a daemon too
        /// old to say — and unknown is not participation, see `automatic`.
        var fromMe: Bool?
        /// Length of the message's OWN words, with the quoted history it replies
        /// under already stripped (Quotes.splitText). A one-line "yes" under
        /// forty lines of chain is a one-line message.
        var freshChars: Int
        /// Whether this message is a document rather than a note — see
        /// `htmlHeavy`. One of these vetoes the whole thread.
        var htmlHeavy: Bool
        /// The from address as it arrived; `automatic` canonicalizes it.
        var sender: String
    }

    /// Half the messages must be shorter than this for the thread to read as
    /// talk. A paragraph is about 400 characters, so the line is drawn at "most
    /// of these are shorter than a paragraph".
    static let chatMedianChars = 400
    /// Two people and one interloper. A fourth voice is a group, and a group
    /// reads as a list of contributions rather than as two sides.
    static let chatMaxSenders = 3
    /// Bytes of markup per byte of visible text past which it is a document.
    static let chatMarkupRatio = 4
    /// Pictures past which the message is a layout rather than a line with an
    /// image in it. A signature logo and an inline screenshot are both fine.
    static let chatMaxImages = 2

    /// WHAT THIS THREAD LOOKS LIKE, when Settings has been left on Automatic.
    ///
    /// Bubbles only when EVERY test passes, because the cost of the two mistakes
    /// is not the same: a conversation drawn as cards is the reader's mail as it
    /// has always looked, while a receipt trail drawn as chat is a surface
    /// pretending a robot is talking to you. So the guess is a veto chain and
    /// classic is what survives any doubt.
    ///
    /// A daemon too old to send `is_sent` leaves every side unknown, which fails
    /// participation on the first test and lands the whole app on classic. That
    /// is the intended behaviour and not a fallback: there is no chat to draw if
    /// nothing can be put on the reader's own side.
    static func automatic(_ samples: [Sample]) -> ThreadStyle {
        guard !samples.isEmpty else { return .classic }
        // BOTH SIDES SPOKE. A thread the reader has never answered is mail about
        // them, not with them, however short and however chatty it reads.
        guard participated(samples.map(\.fromMe)) else { return .classic }
        // A SMALL CAST, by canonical address — the same trim-and-lower the rest
        // of the app calls a sender's identity.
        let senders = Set(
            samples.map { $0.sender.trimmingCharacters(in: .whitespaces).lowercased() })
        guard senders.count <= chatMaxSenders else { return .classic }
        // NOTHING HEAVY. One newsletter in the middle of a thread is enough to
        // make a bubble column absurd, so one is enough to end this.
        guard !samples.contains(where: \.htmlHeavy) else { return .classic }
        // SHORT, ON BALANCE. The median and not the mean: one long message in a
        // chatty thread is a chatty thread, and the mean says otherwise.
        guard median(samples.map(\.freshChars)) < chatMedianChars else { return .classic }
        return .bubbles
    }

    /// The first veto, standing on its own because it is the one test answerable
    /// from `is_sent` alone — every other test needs the body work a Sample pays
    /// for. A caller that asks this first spares a rejected thread (which is
    /// most mail) the quote-splitting and markup-scanning of every message it
    /// was never going to draw as chat.
    static func participated(_ sides: [Bool?]) -> Bool {
        sides.contains(where: { $0 == true }) && sides.contains(where: { $0 != true })
    }

    /// Whether a message is a DOCUMENT rather than a note: markup that dwarfs
    /// the words it carries, a table (which is how mail lays out a page), or
    /// enough pictures to be a layout. A cheap scan on purpose — this runs over
    /// every message of every thread as it opens, and the question is coarse
    /// enough that parsing the markup would buy nothing.
    static func htmlHeavy(html: String?, plain: String) -> Bool {
        guard let html, !html.isEmpty else { return false }
        // Bytes and not characters: a ratio does not care which unit it is in,
        // and a native string already knows its utf8 count while counting its
        // characters walks it — over every message of every thread, on open.
        if html.utf8.count > chatMarkupRatio * max(plain.utf8.count, 1) { return true }
        if html.range(of: "<table", options: .caseInsensitive) != nil { return true }
        return imageTags(html) > chatMaxImages
    }

    /// `<img` occurrences, counted no further than it takes to answer the
    /// question: a newsletter has hundreds and the third one has already decided.
    private static func imageTags(_ html: String) -> Int {
        var found = 0
        var from = html.startIndex
        while let hit = html.range(
            of: "<img", options: .caseInsensitive, range: from..<html.endIndex)
        {
            found += 1
            if found > chatMaxImages { return found }
            from = hit.upperBound
        }
        return found
    }

    /// The middle value, averaging the two middles for an even count. Empty is
    /// unreachable here (`automatic` guards it) and answers 0 rather than trap.
    private static func median(_ values: [Int]) -> Int {
        let sorted = values.sorted()
        guard !sorted.isEmpty else { return 0 }
        let mid = sorted.count / 2
        return sorted.count.isMultiple(of: 2) ? (sorted[mid - 1] + sorted[mid]) / 2 : sorted[mid]
    }
}
