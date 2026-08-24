// THE CHANGELOG, and the only hand-authored copy of it.
//
// Everything else is generated FROM this table: docs/CHANGELOG.md, the body of
// a GitHub release, and whatever the site grows later (Tools/ChangelogTool.swift
// behind make-changelog.sh). The direction is the point. The What's New card
// reads this table directly, so the notes a human is shown in the app cannot be
// stale; generating the other way round would put a codegen step between
// writing a note and shipping it, and a stale generated file ships silently.
// XcodeGen already teaches that lesson once per new file.
//
// PURE ON PURPOSE: no SwiftUI, no Bundle, no store. Which version this build
// actually is arrives as a parameter, which is what lets the test suite and the
// markdown generator compile this file on its own.
//
// HOUSE RULES for the prose, because it is user-facing copy:
//   * no em dashes
//   * say what the human can now do, not what the commit did
//   * every item names its surface, because the two halves ship separately:
//     the app updates itself, the daemon is rolled (hosted) or pulled as an
//     image (self-host), and a note nobody can locate is a note nobody trusts

/// A dotted release version, ordered by component. Failable rather than
/// lenient: a stamp that does not parse is not a version this code should
/// silently rank, and every caller already has an honest answer for nil.
struct ReleaseVersion: Comparable, CustomStringConvertible {
    /// The dotted components, as written. Missing trailing components compare
    /// as zero, so 0.0.4 and 0.0.4.0 are the same release.
    let parts: [Int]
    let text: String

    init?(_ text: String) {
        let fields = text.split(separator: ".", omittingEmptySubsequences: false)
        let parsed = fields.map { Int($0) }
        guard !parsed.isEmpty, !parsed.contains(where: { $0 == nil }) else { return nil }
        self.parts = parsed.map { $0! }
        self.text = text
    }

    static func < (lhs: Self, rhs: Self) -> Bool {
        for index in 0..<max(lhs.parts.count, rhs.parts.count) {
            let left = index < lhs.parts.count ? lhs.parts[index] : 0
            let right = index < rhs.parts.count ? rhs.parts[index] : 0
            if left != right { return left < right }
        }
        return false
    }

    /// Equality is by ORDER, not by spelling: a stamp written as "0.0.4" by one
    /// build has to satisfy a build calling itself "0.0.4.0".
    static func == (lhs: Self, rhs: Self) -> Bool { !(lhs < rhs) && !(rhs < lhs) }

    var description: String { text }
}

/// Which half of Passband a note landed in. The app updates itself through
/// Sparkle; the daemon is rolled onto hosted tenants or pulled as an image on a
/// self-host box. One list, two surfaces, so a reader knows whether a change is
/// already theirs or arrives with their next daemon.
enum ReleaseSurface: String, CaseIterable, Sendable {
    case app, daemon

    var label: String {
        switch self {
        case .app: "App"
        case .daemon: "Daemon"
        }
    }
}

/// One bullet in a release.
struct ReleaseItem: Sendable {
    let surface: ReleaseSurface
    let text: String

    init(_ surface: ReleaseSurface, _ text: String) {
        self.surface = surface
        self.text = text
    }
}

/// One shipped version, keyed by the APP's version. Daemon work rides in the
/// release it shipped alongside rather than carrying a version of its own: the
/// daemon has no screen to read notes on, and two independent stamps buy a
/// second thing to get wrong for a difference nobody can see.
struct ReleaseNote: Identifiable, Sendable {
    /// Marketing version, matching passband/VERSION for that tag.
    let version: String
    /// ISO date, printed as written.
    let date: String
    /// The release in one line, in the app's own voice.
    let headline: String
    let items: [ReleaseItem]

    var id: String { version }
    var semver: ReleaseVersion? { ReleaseVersion(version) }

    /// The bullets for one surface, in authored order.
    func items(on surface: ReleaseSurface) -> [ReleaseItem] {
        items.filter { $0.surface == surface }
    }
}

enum ReleaseNotes {
    /// THE TABLE. Newest first, and the only place a release note is written.
    static let all: [ReleaseNote] = [
        ReleaseNote(
            version: "0.0.4",
            date: "2026-08-24",
            headline: "Forwarding, reminders, and mail that renders like its sender meant it to.",
            items: [
                ReleaseItem(
                    .app,
                    "Forward what you are reading. The f key in the reader opens a composer with the "
                        + "original already in it, quote and attachments and all, and shows you "
                        + "exactly what is going along before you send it."),
                ReleaseItem(
                    .app,
                    "Park an email until later. The h key asks when, in the words you would use (\"next "
                        + "tuesday\", \"the 24th\"), and the thread comes back at the top of your "
                        + "board on the day you named."),
                ReleaseItem(
                    .app,
                    "Embedded images render where their sender put them, instead of collecting at "
                        + "the bottom as attachments."),
                ReleaseItem(
                    .app,
                    "The window reads as one bar. The email's subject sits up beside the traffic "
                        + "lights, and the mail bar stops shuffling when you switch pages."),
                ReleaseItem(
                    .app,
                    "Unsubscribing or blocking a sender closes the email and moves you on, rather "
                        + "than leaving you holding the thing you just got rid of."),
                ReleaseItem(
                    .app,
                    "The banking card clears once you have actually seen it, and stays cleared "
                        + "across a restart."),
                ReleaseItem(
                    .app,
                    "The s key in the reader searches everything from that sender."),
                ReleaseItem(
                    .app,
                    "This card. New versions say what they brought, once, and never again."),
                ReleaseItem(
                    .daemon,
                    "Senders writing in Chinese, Japanese or Korean arrive as their names instead "
                        + "of a row of question marks."),
                ReleaseItem(
                    .daemon,
                    "Login codes and 2FA mail stay sealed even when they are worded oddly enough "
                        + "to dodge the usual patterns."),
                ReleaseItem(
                    .daemon,
                    "A triage outage no longer costs you mail. Rows stay queued when the model is "
                        + "unreachable and get judged when it comes back, instead of being filed "
                        + "on a guess."),
                ReleaseItem(
                    .daemon,
                    "Reminders and forwarding are served by the daemon, so both work from any "
                        + "client you have paired."),
            ]),
        ReleaseNote(
            version: "0.0.3",
            date: "2026-08-18",
            headline: "Updates that take one click, and a reader that holds still.",
            items: [
                ReleaseItem(
                    .app,
                    "A new version arrives as a card in the window with a single Update button "
                        + "that installs it and relaunches, instead of two dialogs asking the same "
                        + "question twice."),
                ReleaseItem(
                    .app,
                    "Attachments open in Quick Look, the same panel the Finder uses. Photos render "
                        + "in the column and open on a click."),
                ReleaseItem(
                    .app,
                    "A thread reads oldest to newest with the newest parked at the top, and the "
                        + "rail beside it holds still while you scroll."),
                ReleaseItem(
                    .app,
                    "Settings stamps the version in the corner, selectable, because that is the "
                        + "first thing any bug report asks for."),
                ReleaseItem(
                    .daemon,
                    "Triage verdicts expire, so a sender you have since taught it about is judged "
                        + "again rather than on a months-old opinion."),
                ReleaseItem(
                    .daemon,
                    "A pattern match no longer decides a tier by itself, and asking for a "
                        + "second opinion buys the model more context rather than just more time."),
                ReleaseItem(
                    .daemon,
                    "Asking for a re-triage outranks the age cutoff, so an old thread can still be "
                        + "reconsidered."),
            ]),
        ReleaseNote(
            version: "0.0.2",
            date: "2026-08-14",
            headline: "Packages that track themselves, and a gate that asks the right question first.",
            items: [
                ReleaseItem(
                    .app,
                    "The connect screen asks where your mail should run before it asks for a "
                        + "credential, so the answer you give matches the install you have."),
                ReleaseItem(
                    .app,
                    "Newsletter images load over plain HTTP, which is where a surprising amount of "
                        + "newsletter art still lives."),
                ReleaseItem(
                    .app,
                    "The shipment card says what the carrier said, not just what the email "
                        + "claimed."),
                ReleaseItem(
                    .daemon,
                    "Package tracking talks to four carriers directly, and a shipment goes quiet "
                        + "seven days after it lands."),
                ReleaseItem(
                    .daemon,
                    "A failed sign-in no longer forfeits the message it was working on."),
            ]),
        ReleaseNote(
            version: "0.0.1",
            date: "2026-08-12",
            headline: "The first build.",
            items: [
                ReleaseItem(.app, "Passband, on your desk."),
                ReleaseItem(.daemon, "squelchd, triaging your mail on a machine you control."),
            ]),
    ]

    /// The releases a human last stamped at `lastSeen` has not read, newest
    /// first.
    ///
    /// Bounded ABOVE by the running build on purpose: this table can describe a
    /// version newer than the copy compiled around it (a note written before the
    /// tag lands), and announcing features that are not installed is worse than
    /// announcing them late.
    ///
    /// NO STAMP is one of two people. A fresh install, who should be reading the
    /// tour rather than a back catalogue, and an install that predates this
    /// feature and is owed the release it just took. The caller separates them
    /// (see WhatsNew.maybeShow); the seed here is the second reading, floored at
    /// the release BEFORE the running one so this build announces itself and
    /// nothing older.
    static func unseen(lastSeen: String?, running: String) -> [ReleaseNote] {
        guard let ceiling = ReleaseVersion(running) else { return [] }
        let floor = lastSeen.flatMap(ReleaseVersion.init) ?? previous(before: ceiling)
        return ordered
            .filter { $0.version <= ceiling }
            .filter { pair in
                guard let floor else { return true }
                return pair.version > floor
            }
            .map(\.note)
    }

    /// What to stamp once the card is closed: the newest note actually SHOWN,
    /// never the running build. A copy of the app carrying no note for some
    /// release must leave that release unstamped so a later build can still
    /// announce it, rather than swallowing it on the reader's behalf.
    static func newestShown(_ notes: [ReleaseNote]) -> String? {
        notes.compactMap { note in note.semver.map { ($0, note.version) } }
            .max { $0.0 < $1.0 }?.1
    }

    /// The newest release strictly older than `version`, or nil when this is
    /// the oldest one on record.
    static func previous(before version: ReleaseVersion) -> ReleaseVersion? {
        ordered.first { $0.version < version }?.version
    }

    /// The newest note this build is at or past, for "what's new in this
    /// version" asked directly. At or BELOW rather than exactly equal: a build
    /// cut without a note of its own (a hotfix, a version bumped ahead of the
    /// writing) should answer with the last thing that actually shipped rather
    /// than with nothing.
    static func newest(atOrBelow running: String) -> ReleaseNote? {
        guard let ceiling = ReleaseVersion(running) else { return nil }
        return ordered.first { $0.version <= ceiling }?.note
    }

    /// The table with unparseable rows dropped and the order enforced rather
    /// than trusted. `all` is authored newest first; this is what guarantees it.
    private static var ordered: [(note: ReleaseNote, version: ReleaseVersion)] {
        all.compactMap { note in note.semver.map { (note: note, version: $0) } }
            .sorted { $0.version > $1.version }
    }
}
