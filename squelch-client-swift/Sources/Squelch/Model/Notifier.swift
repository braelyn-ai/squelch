// macOS notification delivery for the event feed: one banner per event, tapping
// one opens that thread.
//
// The Event row is a DENORMALIZED SNAPSHOT (sender, one-line, tier, deadline),
// so the whole banner renders from the frame alone — no round trip on the path
// between "mail arrived" and "the human sees it". `copy(for:)` is that mapping
// and is a pure function of the event, kept separate from the posting so its
// edge cases (empty summary, unknown kind, a one-liner with newlines in it) can
// be read straight through.
//
// PERMISSION, on a dev machine: an ad-hoc signature's identity is a hash of the
// build, so every recompile looks like a different app to the notification
// database and the grant resets. That is a signing artifact, not a bug here —
// see build.sh's note on why local builds are ad-hoc.

import AppKit
import UserNotifications

@MainActor
final class Notifier {
    static let shared = Notifier()

    /// userInfo keys. The thread id is the whole point of the payload: it is
    /// what a tap routes on. `nonisolated` because the delegate reads the
    /// payload on whatever queue the system delivered it to.
    nonisolated static let threadKey = "squelch.thread_id"
    nonisolated static let eventKey = "squelch.event_id"

    /// UNUserNotificationCenter holds its delegate WEAKLY. This property is the
    /// only strong reference in the process — assigning a freshly-made delegate
    /// inline would deallocate it immediately and every tap would go nowhere.
    private let delegate = NotificationDelegate()
    private var asked = false

    private init() {}

    /// Install the delegate. Call from applicationDidFinishLaunching: a
    /// notification tapped while Squelch was not running is delivered as soon
    /// as the delegate exists, and a center without one drops it.
    func install() {
        UNUserNotificationCenter.current().delegate = delegate
    }

    /// Ask for the alert/sound grant, once per launch. A denial is not an error
    /// worth surfacing — `post` below simply becomes a no-op, which is exactly
    /// what "no thanks" should mean.
    func requestAuthorizationIfNeeded() async {
        guard !asked else { return }
        asked = true
        _ = try? await UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound])
    }

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
        // "Sarah Chen <sarah@acme.com>" is not a notification title. The app's
        // own sender parser is the one place that knows how to shorten one.
        let sender = flatten(SenderID.displayName(event.sender), max: 64)
        let summary = flatten(event.one_line, max: 240)
        let due = Fmt.deadlineChip(event.deadline, now: now)?.text ?? ""

        let subtitle: String
        switch event.kind {
        // Urgent is the standing band: past-due or deadline tier. When there is
        // a real date, SAY the date — "2d PAST DUE" is the entire reason the
        // banner is worth interrupting for. Otherwise a plain nudge.
        case .urgent: subtitle = due.isEmpty ? "needs attention" : due
        case .deadline: subtitle = due
        // The ordinary above-the-line case gets no second line. A subtitle on
        // every notification is a subtitle that means nothing.
        case .surfaced: subtitle = ""
        }

        return Copy(
            title: sender.isEmpty ? "Squelch" : sender,
            subtitle: subtitle,
            // An empty one_line means triage stored no summary. Say something
            // true rather than posting a blank banner.
            body: summary.isEmpty ? "New mail worth your attention." : summary,
            // Coalescing key: three events on one thread stack into ONE group
            // in Notification Center instead of three separate piles. Falls
            // back to something unique so an empty thread id cannot glue
            // unrelated mail together.
            threadIdentifier: event.thread_id.isEmpty
                ? "squelch.event.\(event.id)" : event.thread_id,
            // Sound is reserved for the two kinds that are actually time-bound.
            // A chime for every surfaced email is how a notification stream
            // gets muted wholesale.
            sound: event.kind != .surfaced)
    }

    /// Collapse to one line and cap the length. Notification text is drawn in a
    /// small fixed box: an embedded newline eats one of the few lines available
    /// and a 4KB model-written summary is truncated by the system anyway, in
    /// the middle of a word and without an ellipsis to say so.
    private static func flatten(_ s: String, max: Int) -> String {
        let flat = s.split(whereSeparator: { $0.isNewline || $0 == "\t" })
            .joined(separator: " ")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        guard flat.count > max else { return flat }
        return String(flat.prefix(max - 1)).trimmingCharacters(in: .whitespaces) + "…"
    }

    // MARK: - posting

    func post(_ event: Event) {
        let copy = Self.copy(for: event)
        let content = UNMutableNotificationContent()
        content.title = copy.title
        if !copy.subtitle.isEmpty { content.subtitle = copy.subtitle }
        content.body = copy.body
        content.threadIdentifier = copy.threadIdentifier
        content.userInfo = [Self.threadKey: event.thread_id, Self.eventKey: event.id]
        if copy.sound { content.sound = .default }

        // The event id as the REQUEST id makes a re-delivered frame (a replay
        // overlapping the live seam after a reconnect) replace its own banner
        // instead of stacking a second identical one.
        let request = UNNotificationRequest(
            identifier: "squelch.event.\(event.id)", content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    // MARK: - delegate behaviour

    /// What to do with a notification that arrives while the app is running.
    ///
    /// Frontmost app WITH its window up => NO banner. The human is looking at
    /// Squelch; sliding a card over the sitrep to tell them about a row already
    /// on that sitrep is the app talking over itself. It still lands in
    /// Notification Center, so nothing is lost if they were looking at another
    /// view. The window check matters under residency: "active with zero
    /// visible windows" (closed the window, app lingers) is a common state, and
    /// there the banner is the only voice the app has.
    nonisolated static func presentation(
        appActive: Bool, windowVisible: Bool
    ) -> UNNotificationPresentationOptions {
        (appActive && windowVisible) ? [.list] : [.banner, .sound, .list]
    }

    /// A tap: front the app, put the window back if it was closed, open the
    /// thread. Setting `threadId` is all it takes — RootView gates the reader
    /// on it (see AppStore.openThread).
    func handleTap(threadId: String?) {
        NSApp.activate(ignoringOtherApps: true)
        MainWindow.show()
        guard let threadId, !threadId.isEmpty else { return }
        AppStore.shared.openThread(threadId)
    }
}

/// The center's delegate. Retained by `Notifier.shared`; see the note there on
/// why that matters.
///
/// Deliberately NOT main-actor isolated: these callbacks are protocol
/// requirements with no isolation of their own, so the class stays nonisolated
/// and each method hops to the main actor carrying a plain `String?` —
/// `[AnyHashable: Any]` is not Sendable and must be read on this side.
final class NotificationDelegate: NSObject, UNUserNotificationCenterDelegate {
    func userNotificationCenter(
        _ center: UNUserNotificationCenter, willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        let (active, visible) = await MainActor.run {
            (NSApp.isActive, MainWindow.find()?.isVisible == true)
        }
        return Notifier.presentation(appActive: active, windowVisible: visible)
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse
    ) async {
        let threadId =
            response.notification.request.content.userInfo[Notifier.threadKey] as? String
        await MainActor.run { Notifier.shared.handleTap(threadId: threadId) }
    }
}
