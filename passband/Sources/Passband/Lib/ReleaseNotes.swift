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
//   * every item names its surface, because the three parts ship separately:
//     the Mac app updates itself, the phone comes through TestFlight, the
//     daemon is rolled (hosted) or pulled as an image (self-host), and a note
//     nobody can locate is a note nobody trusts

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

/// Which part of Passband a note landed in. The Mac app updates itself through
/// Sparkle; the phone comes through TestFlight or the App Store; the daemon is
/// rolled onto hosted tenants or pulled as an image on a self-host box. One
/// list, three surfaces, so a reader knows whether a change is already theirs
/// or arrives with their next daemon.
enum ReleaseSurface: String, CaseIterable, Sendable {
    case app, ios, daemon

    var label: String {
        switch self {
        case .app: "Mac"
        case .ios: "iPhone"
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

/// One shipped version, keyed by the MAC APP's version. Daemon and phone work
/// ride in the release they shipped alongside rather than being keyed on their
/// own: the daemon has no screen to read notes on, the phone reads its notes
/// from the store, and a second key here would be a second thing to get wrong
/// for a difference nobody can see. Both still carry their own version and tag
/// (daemon-X, passband-ios-X); those are numbered on their own cadence, and the
/// phone's is expected to drift as its UX does.
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
            version: "0.0.6",
            date: "2026-08-27",
            headline: "Move through emails faster, better search, and notifications to your phone.",
            items: [
                ReleaseItem(
                    .app,
                    "Recency is a factor in search. Old mail is demoted. A clearly better old "
                        + "match still wins. Configurable."),
                ReleaseItem(.app, "Settings has a search field."),
                ReleaseItem(
                    .app,
                    "Shift-E (and Shift-D) finish the email and open the next one. Plain e and "
                        + "d finish it and close."),
                ReleaseItem(.app, "Immediately see new emails for a thread you have open"),
                ReleaseItem(.app, "CC and BCC everywhere an email is written"),
                ReleaseItem(
                    .app,
                    "When your mailbox is catching up after a week away, the re-triage screen "
                        + "says so and counts (1,240 of 4,500 messages) rather than telling you "
                        + "the counter has not moved in a while."),
                ReleaseItem(.app, "Cmd-Enter saves a rule"),
                ReleaseItem(
                    .app,
                    "The check now chip is gone from Shipments. Carriers are polled on a "
                        + "schedule."),
                ReleaseItem(.app, "No more goddam emdashes"),
                ReleaseItem(.ios, "Mobile notifications finally!!"),
                ReleaseItem(.ios, "Switch accounts, and add one"),
                ReleaseItem(.ios, "Mark emails as done in the thread"),
                ReleaseItem(.ios, "Conversational threads look like message bubbles"),
                ReleaseItem(.ios, "CC and BCC in the composer and the inline reply"),
                ReleaseItem(.ios, "Send to groups that you made in the desktop app"),
                ReleaseItem(.ios, "Attachments open in Quick Look and can be saved to Files."),
                ReleaseItem(.ios, "All desktop updates"),
                ReleaseItem(.daemon, "More easily send mail to yourself"),
                ReleaseItem(
                    .daemon,
                    "A CC you emptied stays empty rather than being refilled from the parent."),
                ReleaseItem(.daemon, "Better metrics reporting"),
                ReleaseItem(
                    .daemon,
                    "Push notifications have account names, so a phone with two mailboxes "
                        + "never shows one mailbox's business under the other's name"),
                ReleaseItem(.daemon, "Better semantic search embedding"),
                ReleaseItem(
                    .daemon,
                    "Clearer shipment item names determined by the triage model"),
            ]),
        ReleaseNote(
            version: "0.0.5",
            date: "2026-08-25",
            headline:
                "Groups you can address as one, and a mailbox that says so when it stops working.",
            items: [
                ReleaseItem(
                    .app,
                    "Address a group by name. Groups sits at the bottom of the rail on Cmd-6, "
                        + "holds a named audience, and a Groups button beside the To line puts "
                        + "one into an email."),
                ReleaseItem(
                    .app,
                    "How a group is addressed is settled once, when you make it: everybody on the "
                        + "To line, everybody blind, or one separate email per person. The review "
                        + "before you send says which of the three you are about to do."),
                ReleaseItem(
                    .app,
                    "A group shows what has already gone to it, including the mail you sent those "
                        + "people before the group existed."),
                ReleaseItem(
                    .app,
                    "The composer has a Bcc row, on the messages you want one on."),
                ReleaseItem(
                    .app,
                    "When your mailbox stops working, Passband says so. An expired Google sign-in "
                        + "used to look exactly like nobody writing to you; now a banner says what "
                        + "broke and since when, and on a hosted account it carries the link that "
                        + "repairs it."),
                ReleaseItem(
                    .app,
                    "Re-triage runs in front of you with the queue counting down, instead of a "
                        + "toast over a board still showing the old verdicts."),
                ReleaseItem(
                    .app,
                    "One click on a row in Auth opens the mail the code arrived in."),
                ReleaseItem(
                    .app,
                    "Shipment cards read as one line, and asking one to check now no longer takes "
                        + "the app down with it."),
                ReleaseItem(
                    .app,
                    "The assistant's Opus setting is Opus 5, and a preference saved on the older "
                        + "model moves forward by itself."),
                ReleaseItem(
                    .app,
                    "Inviting a friend hands them the waitlist for now. An invite cannot be "
                        + "redeemed until Google's review clears, so the sheet says so rather than "
                        + "minting a code that would bounce."),
                ReleaseItem(
                    .daemon,
                    "Sending to a group one person at a time happens in the background, so a "
                        + "twelve person list goes out without the send timing out, and the Groups "
                        + "page watches it happen."),
                ReleaseItem(
                    .daemon,
                    "A send that reached some of a group and not the rest says so, and says how "
                        + "many of each."),
                ReleaseItem(
                    .daemon,
                    "Blind copies go out blind and are still recorded, so a bcc-only send lists in "
                        + "your sent mail with everyone it actually reached."),
                ReleaseItem(
                    .daemon,
                    "A model outage no longer costs you the rest of the day. A call the gateway "
                        + "turns away for free is refunded to your triage budget, instead of "
                        + "burning the day's allowance in minutes and leaving mail unjudged until "
                        + "midnight."),
                ReleaseItem(
                    .daemon,
                    "The assistant answers again on hosted accounts, whichever model you picked "
                        + "for it."),
                ReleaseItem(
                    .daemon,
                    "A hosted mailbox that lost its Google consent is reconnected by the person "
                        + "who owns it, from the banner, rather than by somebody reaching into the "
                        + "cluster on their behalf."),
                ReleaseItem(
                    .daemon,
                    "Your daemon notices the moment a mailbox loses its sign-in and remembers "
                        + "since when, which is what the banner in the app is reading."),
            ]),
        ReleaseNote(
            version: "0.0.4",
            date: "2026-08-24",
            headline: "Forwarding, reminders, invites, and mail that renders like its sender meant it to.",
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
                    "Invite a friend. The share sheet writes the mail, shows you exactly what is "
                        + "going out, and sends it from your own mailbox under your own name."),
                ReleaseItem(
                    .app,
                    "Threads read the way you want them to. Email cards, chat bubbles, or "
                        + "Automatic, which picks per thread from how the conversation actually "
                        + "reads."),
                ReleaseItem(
                    .app,
                    "Embedded images render where their sender put them, instead of collecting at "
                        + "the bottom as attachments."),
                ReleaseItem(
                    .app,
                    "Notification banners carry the sender's own mark, a brand's logo or a "
                        + "correspondent's initials, so you know who wrote before you open it."),
                ReleaseItem(
                    .app,
                    "The window reads as one bar. The email's subject sits up beside the traffic "
                        + "lights, and the mail bar stops shuffling when you switch pages."),
                ReleaseItem(
                    .app,
                    "A long thread scrolls without stutter, however far back it goes."),
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
                ReleaseItem(
                    .daemon,
                    "New mail reaches the screen in seconds. Gmail is polled every five seconds "
                        + "rather than every forty-five, so a login code is in front of you about "
                        + "as fast as it arrives."),
                ReleaseItem(
                    .daemon,
                    "An invite you send goes out through your own Gmail, and the address you sent "
                        + "it to never leaves your machine."),
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
