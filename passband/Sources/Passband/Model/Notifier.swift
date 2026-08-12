// macOS notification delivery for the event feed: one banner per event, a tap
// opens that thread. Plus the ONE other thing worth interrupting a human for —
// auth mail landing in an account that is not on screen (`postAuth`, driven by
// BackgroundAuthWatch), whose tap opens the Auth view instead of a thread.
//
// The Event row is a denormalized snapshot, so a banner renders from the frame
// alone with no round trip. `copy(for:)` is that mapping, kept pure and separate
// from posting. On a dev machine the grant resets every recompile: an ad-hoc
// signature's identity is a hash of the build.
//
// EVERY BANNER NAMES ITS ACCOUNT. There is one event feed per account and the
// ids on those feeds are per-daemon SQLite ints, so two accounts hand this
// class the same event id and the same thread id for entirely unrelated mail.
// Both of the identifiers the system dedupes and groups on are therefore
// prefixed with the account uuid, and the uuid rides in `userInfo` so a tap can
// take the human to the mailbox the mail is actually in — switching accounts
// first if that is where it lives.

import UserNotifications

// Fronting the app and finding its window are AppKit-only ideas, and MainWindow
// itself is a macOS file — the notification half below is what ships everywhere.
#if os(macOS)
    import AppKit
#endif

@MainActor
final class Notifier {
    static let shared = Notifier()

    /// userInfo keys — a tap routes on the thread id, WITHIN the account named
    /// by the account id. `nonisolated` because the delegate reads the payload
    /// on whatever queue the system delivers it to.
    nonisolated static let threadKey = "passband.thread_id"
    nonisolated static let eventKey = "passband.event_id"
    /// The posting account's uuid, as a string (userInfo has to survive being
    /// written to disk by the system and read back into a later launch).
    nonisolated static let accountKey = "passband.account_id"
    /// Where the tap goes when there is no thread to open. Carried ONLY by the
    /// background auth banners; its absence is how every event banner says
    /// "open the thread named in `threadKey`".
    nonisolated static let routeKey = "passband.route"
    nonisolated static let authRoute = "auth"

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

    /// Post one event's banner on behalf of one account. `copy(for:)` stays
    /// account-blind — the display copy is the same wherever the mail landed —
    /// and the account is folded into the two IDENTIFIERS below plus userInfo.
    func post(_ event: Event, accountId: UUID) {
        let account = accountId.uuidString
        let copy = Self.copy(for: event)
        let content = UNMutableNotificationContent()
        content.title = copy.title
        if !copy.subtitle.isEmpty { content.subtitle = copy.subtitle }
        content.body = copy.body
        // The coalescing group, NAMESPACED BY ACCOUNT. Thread ids come from the
        // daemon (and the fallback is built from an event id), so two accounts
        // can hand us the same one for unrelated mail — unprefixed, the system
        // would stack two mailboxes' banners into a single group as though they
        // were one conversation.
        content.threadIdentifier = "\(account).\(copy.threadIdentifier)"
        content.userInfo = [
            Self.threadKey: event.thread_id,
            Self.eventKey: event.id,
            Self.accountKey: account,
        ]
        if copy.sound { content.sound = Self.sound(for: Prefs.shared.notificationSound) }

        // The event id as the REQUEST id makes a re-delivered frame (a replay
        // overlapping the live seam after a reconnect) replace its own banner
        // rather than stack a second copy. Also account-prefixed, and for a
        // sharper reason than the group is: event ids are per-daemon SQLite
        // ints, so account B's event 41 would REPLACE account A's event 41 —
        // one banner silently eating the other.
        let request = UNNotificationRequest(
            identifier: "passband.event.\(account).\(event.id)", content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    /// Post one BACKGROUND auth banner: a mailbox that is not on screen has
    /// just been sent a login code (or a reset, or a sign-in alert), and the
    /// only thing this notification exists to say is which mailbox to go to.
    /// The live account never comes through here — its auth mail gets the ring,
    /// the audited auto-reveal and the code modal instead.
    ///
    /// WHAT IS DELIBERATELY NOT IN IT: the code, and the subject line that so
    /// often IS the code ("725104 is your Acme code" is a real subject). A
    /// reveal is audited server-side and belongs to the account the human is
    /// looking at; a notification is not a place to leave a credential.
    func postAuth(_ meta: SealedMeta, accountId: UUID, accountName: String) {
        let account = accountId.uuidString
        let content = UNMutableNotificationContent()
        // The kind leads, the mailbox follows: WHICH account this is happening
        // in is the entire reason the banner is worth reading, and `AuthCopy`
        // is the app's one vocabulary for the other half ("sealed" is internal
        // jargon and never reaches a human).
        content.title = "\(AuthCopy.label(meta.kind)) · \(accountName)"
        let sender = Self.flatten(SenderID.displayName(meta.sender), max: 64)
        content.body = sender.isEmpty ? "New auth mail." : "from \(sender)"
        // One group per account's auth mail: a login code and the sign-in alert
        // behind it are the same conversation, and account-prefixed because two
        // daemons' auth mail is not.
        content.threadIdentifier = "\(account).passband.auth"
        content.userInfo = [
            Self.accountKey: account,
            Self.routeKey: Self.authRoute,
        ]
        // Always a chime. A login code is the definition of time-bound — it
        // expires while you are not looking at it.
        content.sound = Self.sound(for: Prefs.shared.notificationSound)

        // Message ids are per-daemon SQLite ints, so the account prefix is
        // load-bearing here for the same reason it is on an event's identifier:
        // unprefixed, account B's message 41 would silently REPLACE account A's
        // banner for message 41. Within one account, re-posting the same id
        // replaces its own banner rather than stacking a second copy.
        let request = UNNotificationRequest(
            identifier: "passband.auth.\(account).\(meta.id)", content: content, trigger: nil)
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

    /// Where a tap lands once the right mailbox is on screen.
    private enum TapTarget {
        case thread(String?)
        case auth
    }

    /// A tap: front the app, restore the window if it was closed, open the
    /// thread — in the account the banner was posted from, switching to it
    /// first when that is not the account currently on screen.
    func handleTap(threadId: String?, accountId: UUID?) {
        Analytics.capture("notification_opened", ["has_thread": !(threadId ?? "").isEmpty])
        deliver(.thread(threadId), accountId: accountId)
    }

    /// A background auth banner's tap: the Auth view of the mailbox the code
    /// arrived in. NOT a reveal — the human lands on the list and asks for the
    /// code themselves, which is the audited act this whole flow keeps in their
    /// hands.
    ///
    /// The same analytics event as a thread tap, because it is the same act (a
    /// banner opened) and the bool already says which shape it had.
    func handleAuthTap(accountId: UUID?) {
        Analytics.capture("notification_opened", ["has_thread": false])
        deliver(.auth, accountId: accountId)
    }

    /// The shared body of both taps. ONE definition, because the account rules
    /// — a payload with no account, an account since removed, the Connect gate,
    /// a switch that declines — are the same rules whatever the banner was
    /// about, and two copies of them would eventually stop agreeing.
    ///
    /// Synchronous because the delegate's entry point is, so the cross-account
    /// path hands itself to a Task: opening MUST come after the switch, which
    /// tears the whole world down (including any open thread) on its way
    /// through.
    private func deliver(_ target: TapTarget, accountId: UUID?) {
        // No account on the payload: a banner posted by a build from before
        // notifications carried one, still sitting in Notification Center. The
        // live account is the only guess available, and it is the same one that
        // build would have opened it against.
        guard let accountId, accountId != AccountManager.shared.activeId else {
            front()
            open(target)
            return
        }
        // An account that has since been REMOVED. Its ids address a daemon this
        // install no longer has credentials for, and opening one against
        // whoever is live would show a stranger's mail — so the tap fronts the
        // app and stops there, which is the honest whole of what can still be
        // done about it.
        guard AccountManager.shared.accounts.contains(where: { $0.id == accountId }) else {
            front()
            return
        }
        // NOT FROM BEHIND THE CONNECT GATE. A switch assumes a live world to
        // replace — it never touches `connStatus`, so from the gate it would
        // point the client at a daemon while the screen is still asking the
        // human to name one, and nothing would show for it. Fronting the app
        // puts the banner's own tap where it can still be acted on.
        guard AppStore.shared.connStatus == .connected else {
            front()
            return
        }
        Task {
            await AccountManager.shared.switchTo(accountId)
            front()
            // The switch is allowed to decline (one already running) and
            // allowed to fail (the credentials behind the record are gone, and
            // it lands on the Connect gate). Either way a DIFFERENT mailbox is
            // on screen, and thread ids are per-daemon: opening one here would
            // show whatever that id happens to name in the wrong account. The
            // Auth view is no safer — it renders the live account's codes.
            guard AccountManager.shared.activeId == accountId else { return }
            open(target)
        }
    }

    /// Bring the app forward. AppKit-only, and a no-op elsewhere: on the phone
    /// the tap has already foregrounded the app by the time this runs.
    private func front() {
        #if os(macOS)
            NSApp.activate(ignoringOtherApps: true)
            MainWindow.show()
        #endif
    }

    private func open(_ target: TapTarget) {
        switch target {
        case .thread(let threadId):
            guard let threadId, !threadId.isEmpty else { return }
            AppStore.shared.openThread(threadId)
        case .auth:
            // The routed page on the Mac. The phone's tab bar owns its own
            // navigation and has no Auth tab yet, so there this sets a view
            // nothing renders — exactly what `openThread` already does on that
            // shell, and it starts working the day the tab arrives.
            AppStore.shared.setView(.auth)
        }
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
            #if os(macOS)
                (NSApp.isActive, MainWindow.find()?.isVisible == true)
            #else
                // Off macOS the banner always wins until there is a real
                // foreground check: a suppressed notification is a lost one.
                (false, false)
            #endif
        }
        return Notifier.presentation(appActive: active, windowVisible: visible)
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse
    ) async {
        let userInfo = response.notification.request.content.userInfo
        let threadId = userInfo[Notifier.threadKey] as? String
        // Stored as a string and parsed here rather than crossing as one: a
        // payload that survived a relaunch (or came from an older build) can
        // hold anything, and an unparseable id must read as "no account", not
        // as some other account.
        let accountId = (userInfo[Notifier.accountKey] as? String).flatMap(UUID.init(uuidString:))
        // An auth banner carries no thread to open — routing it as an ordinary
        // one would front the app and then do nothing.
        let isAuth = (userInfo[Notifier.routeKey] as? String) == Notifier.authRoute
        await MainActor.run {
            if isAuth {
                Notifier.shared.handleAuthTap(accountId: accountId)
            } else {
                Notifier.shared.handleTap(threadId: threadId, accountId: accountId)
            }
        }
    }
}
