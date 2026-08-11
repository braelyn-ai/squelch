// macOS notification delivery for the event feed: one banner per event, a tap
// opens that thread.
//
// The Event row is a denormalized snapshot, so a banner renders from the frame
// alone with no round trip. `copy(for:)` is that mapping, kept pure and separate
// from posting. On a dev machine the grant resets every recompile: an ad-hoc
// signature's identity is a hash of the build.

import AppKit
import UserNotifications

@MainActor
final class Notifier {
    static let shared = Notifier()

    /// userInfo keys — a tap routes on the thread id. `nonisolated` because the
    /// delegate reads the payload on whatever queue the system delivers it to.
    nonisolated static let threadKey = "passband.thread_id"
    nonisolated static let eventKey = "passband.event_id"

    /// UNUserNotificationCenter holds its delegate WEAKLY. This property is the
    /// only strong reference in the process — assigning a freshly-made delegate
    /// inline would deallocate it immediately and every tap would go nowhere.
    private let delegate = NotificationDelegate()
    private var asked = false

    private init() {}

    /// Install the delegate. Call from applicationDidFinishLaunching: a
    /// notification tapped while Passband was not running is delivered as soon as
    /// the delegate exists, and a center without one drops it.
    func install() {
        UNUserNotificationCenter.current().delegate = delegate
        Self.installSounds()
    }

    /// Copy the bundled chimes into ~/Library/Sounds. UNNotificationSound
    /// resolves names against that folder reliably; resolution against the app
    /// bundle is famously flaky on macOS, so the bundle is treated purely as the
    /// shipping vehicle. Idempotent: a copy that already matches by size is
    /// left alone, and any failure just means the system default plays.
    private static func installSounds() {
        let fm = FileManager.default
        guard let library = fm.urls(for: .libraryDirectory, in: .userDomainMask).first
        else { return }
        let sounds = library.appendingPathComponent("Sounds", isDirectory: true)
        try? fm.createDirectory(at: sounds, withIntermediateDirectories: true)
        for choice in NotificationSound.allCases {
            guard let resource = choice.resourceName, let installed = choice.installedFileName,
                let src = Bundle.main.url(
                    forResource: resource, withExtension: "caf", subdirectory: "Sounds")
            else { continue }
            let dst = sounds.appendingPathComponent(installed)
            let size = { (url: URL) in
                (try? fm.attributesOfItem(atPath: url.path)[.size] as? Int) ?? nil
            }
            if fm.fileExists(atPath: dst.path) {
                if size(src) == size(dst) { continue }
                try? fm.removeItem(at: dst)
            }
            try? fm.copyItem(at: src, to: dst)
        }
    }

    /// The UN sound for a preference choice. Falls back to the system default
    /// for `.system` (and, at delivery time, for an installed file gone missing).
    static func sound(for choice: NotificationSound) -> UNNotificationSound {
        guard let installed = choice.installedFileName else { return .default }
        return UNNotificationSound(named: UNNotificationSoundName(installed))
    }

    /// Ask for the alert/sound grant, once per launch. A denial is not worth
    /// surfacing — `post` simply becomes a no-op.
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
    /// fixed box, and the system's own truncation lands mid-word with no
    /// ellipsis to say so.
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
        if copy.sound { content.sound = Self.sound(for: Prefs.shared.notificationSound) }

        // The event id as the REQUEST id makes a re-delivered frame (a replay
        // overlapping the live seam after a reconnect) replace its own banner
        // rather than stack a second copy.
        let request = UNNotificationRequest(
            identifier: "passband.event.\(event.id)", content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    // MARK: - delegate behaviour

    /// What to do with a notification that arrives while the app is running.
    /// Frontmost WITH a visible window => no banner, only the list: the human is
    /// already looking at the sitrep the row is on. The window check matters —
    /// "active with zero visible windows" is a common state under residency, and
    /// there the banner is the app's only voice.
    nonisolated static func presentation(
        appActive: Bool, windowVisible: Bool
    ) -> UNNotificationPresentationOptions {
        (appActive && windowVisible) ? [.list] : [.banner, .sound, .list]
    }

    /// A tap: front the app, restore the window if it was closed, open the
    /// thread.
    func handleTap(threadId: String?) {
        Analytics.capture("notification_opened", ["has_thread": !(threadId ?? "").isEmpty])
        NSApp.activate(ignoringOtherApps: true)
        MainWindow.show()
        guard let threadId, !threadId.isEmpty else { return }
        AppStore.shared.openThread(threadId)
    }
}

/// The center's delegate, retained by `Notifier.shared`.
///
/// Deliberately NOT main-actor isolated: these callbacks are protocol
/// requirements with no isolation of their own, so each method hops to the main
/// actor carrying a plain `String?` — `[AnyHashable: Any]` is not Sendable and
/// must be read on this side.
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
