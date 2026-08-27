// What a notification for one event SAYS, and the userInfo keys a tap on it
// routes by. Split out of Notifier.swift because there are now two posters of
// the same banner and they must agree exactly: the app, from the live event
// feed, and the notification service extension, rewriting a push that arrived
// while the app was not running. A banner whose copy or whose routing keys
// depended on which of the two posted it would be a visible seam.
//
// Pure by construction: same event in, same banner out. Nothing here posts,
// reads the keychain, or knows an account exists — `Notifier` and the extension
// each fold the account in on their own side.

import Foundation

enum EventBanner {
    // MARK: - userInfo keys

    /// A tap routes on the thread id, WITHIN the account named by the account
    /// id. Both are read on whatever queue the system delivers the tap to, and
    /// both must survive being written to disk by the system and read back into
    /// a later launch — hence strings, and hence the account id as its uuid
    /// string rather than as a UUID.
    static let threadKey = "passband.thread_id"
    static let eventKey = "passband.event_id"
    /// The posting account's uuid, as a string (userInfo has to survive being
    /// written to disk by the system and read back into a later launch).
    static let accountKey = "passband.account_id"
    /// Where the tap goes when there is no thread to open. Carried ONLY by the
    /// background auth banners; its absence is how every event banner says
    /// "open the thread named in `threadKey`".
    static let routeKey = "passband.route"
    static let authRoute = "auth"
    /// The Settings test banner. It routes nowhere, but it is marked because
    /// the system has to be told to DRAW it — see `presentation` — and because
    /// a tap on it is not a human opening their mail and must not be counted
    /// as one.
    static let testRoute = "test"

    // MARK: - content mapping

    /// The display copy for one event. Pure: same event in, same banner out.
    struct Copy: Equatable {
        var title: String
        var subtitle: String
        var body: String
        var threadIdentifier: String
        var sound: Bool
    }

    static func copy(for event: Event, now: Date = Date()) -> Copy {
        // "Sarah Chen <sarah@acme.com>" is not a notification title.
        let sender = flatten(SenderID.displayName(event.sender), max: 64)
        let summary = flatten(event.one_line, max: 240)
        let due = Fmt.deadlineChip(event.deadline, now: now)?.text ?? ""
        // Coalescing key: events on one thread stack into ONE group. The
        // fallback must be unique, or an empty thread id would glue unrelated
        // mail together.
        let group =
            event.thread_id.isEmpty ? "passband.event.\(event.id)" : event.thread_id

        // A read receipt on the user's OWN tracked mail, and NOT a triage
        // verdict: `sender` is the account's own address (using it as the title
        // would read as mail from yourself), `one_line` is the sent subject, and
        // `importance` is a placeholder. It renders on its own terms.
        if event.kind == .opened {
            return Copy(
                title: "Opened",
                subtitle: "",
                body: summary.isEmpty ? "Someone opened your email." : "Opened: \(summary)",
                threadIdentifier: group,
                // News, not an obligation — no chime.
                sound: false)
        }

        let subtitle: String
        switch event.kind {
        // Urgent is the dated-obligation tiers. When there is a real date, SAY it —
        // "2d PAST DUE" is why the banner is worth interrupting for.
        case .urgent: subtitle = due.isEmpty ? "needs attention" : due
        case .deadline: subtitle = due
        // No second line for the ordinary case: a subtitle on every
        // notification is a subtitle that means nothing. `.opened` returned
        // above and is listed only to keep this switch exhaustive.
        case .surfaced, .opened: subtitle = ""
        }

        return Copy(
            title: sender.isEmpty ? "Passband" : sender,
            subtitle: subtitle,
            // An empty one_line means triage stored no summary; say something
            // true rather than posting a blank banner.
            body: summary.isEmpty ? "New mail worth your attention." : summary,
            threadIdentifier: group,
            // Sound only for the time-bound kinds — a chime per surfaced email
            // is how a notification stream gets muted wholesale.
            sound: event.kind != .surfaced)
    }

    /// Collapse to one line and cap the length: notification text is a small
    /// fixed box.
    static func flatten(_ s: String, max: Int) -> String {
        let flat = s.split(whereSeparator: { $0.isNewline || $0 == "\t" })
            .joined(separator: " ")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return Fmt.truncate(flat, max)
    }
}
