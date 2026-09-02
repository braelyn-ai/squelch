// macOS notification delivery for the event feed: one banner per event, a tap
// opens that thread. Plus the ONE other thing worth interrupting a human for —
// auth mail landing in an account that is not on screen (`postAuth`, driven by
// BackgroundAuthWatch), whose tap opens the Auth view instead of a thread.
//
// The Event row is a denormalized snapshot, so a banner renders from the frame
// alone with no round trip. `EventBanner.copy(for:)` is that mapping, kept pure
// and in a file of its own because the notification extension posts the same
// banner from the same event. On a dev machine the grant resets every recompile: an ad-hoc
// signature's identity is a hash of the build.
//
// Every banner also carries the SENDER's mark as an attachment — see
// NotificationIcon. The icon at the left of a notification is the app's and
// always will be; the thumbnail beside the copy is the only place a banner gets
// to say who the mail is from, so that is what goes there. Drawing it is
// synchronous and stays that way: posting is the last thing that happens after
// a caller has already recorded the event as seen, so a post that waits on
// anything is a post that can be lost.
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

    // MARK: - posting

    /// Post one event's banner on behalf of one account. `EventBanner.copy(for:)`
    /// stays
    /// account-blind — the display copy is the same wherever the mail landed —
    /// and the account is folded into the two IDENTIFIERS below plus userInfo.
    func post(_ event: Event, accountId: UUID) {
        // A SEALED EVENT IS NOT MAIL WITH A THREAD TO OPEN. `EventStream` already
        // routes it to the auth path and never reaches here, and this guard is
        // the belt on top of that: the banner below would name the sender of a
        // login code, and its tap would ask for a thread the daemon refuses to
        // serve. Defensive on purpose — routing lives in ONE pure function
        // (`EventBanner.routing`) and a future caller that forgets to consult it
        // must fail closed rather than leak an auth arrival into the thread lane.
        guard case .threadBanner = EventBanner.routing(for: event) else { return }
        let account = accountId.uuidString
        let copy = EventBanner.copy(for: event)
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
            EventBanner.threadKey: event.thread_id,
            EventBanner.eventKey: event.id,
            EventBanner.accountKey: account,
        ]
        if copy.sound { content.sound = Self.sound(for: Prefs.shared.notificationSound) }

        // The event id as the REQUEST id makes a re-delivered frame (a replay
        // overlapping the live seam after a reconnect) replace its own banner
        // rather than stack a second copy. Also account-prefixed, and for a
        // sharper reason than the group is: event ids are per-daemon SQLite
        // ints, so account B's event 41 would REPLACE account A's event 41 —
        // one banner silently eating the other.
        // A read receipt draws NO tile. `copy(for:)` goes out of its way not to
        // read as mail from yourself, and `sender` on an `.opened` row is the
        // account's own address — so an avatar of the reader is exactly the
        // impression the copy is avoiding, and if their own local-part happens
        // to be a robot shape (support@, billing@, hello@) it would send their
        // own domain out to be looked up to decorate a receipt about their own
        // sent mail.
        send(
            content, identifier: "passband.event.\(account).\(event.id)",
            sender: event.kind == .opened ? "" : event.sender)
    }

    /// Attach the sender's mark and hand the banner to the system.
    ///
    /// SYNCHRONOUS, and the comment is here because the other shape is so
    /// tempting: waiting on a first-sight domain logo would make every banner
    /// prettier and some of them late or missing. Both callers record the event
    /// as seen BEFORE they post — `EventStream` writes its cursor, the auth
    /// watcher saves its id set — so a post deferred behind anything at all is
    /// a post a quit can swallow with nothing left to retry it. `postAuth`'s
    /// callers also depend on the ORDER holding: they sort oldest-first so the
    /// newest code lands on top of the stack, which only survives if posting is
    /// straight-line. The tile draws with the logo already in hand, and `warm`
    /// goes and gets the one that was not.
    private func send(_ content: UNMutableNotificationContent, identifier: String, sender: String) {
        content.attachments = NotificationIcon.attachments(for: sender, id: identifier)
        let request = UNNotificationRequest(identifier: identifier, content: content, trigger: nil)
        // The system takes ownership of a tile only once it accepts the request
        // carrying it. A rejected request leaves the bytes with us, and a PNG
        // per refused banner is exactly the accumulation the tile root exists
        // to prevent.
        let tiles = content.attachments.map(\.url)
        UNUserNotificationCenter.current().add(request) { error in
            guard error != nil else { return }
            Task { @MainActor in NotificationIcon.discard(tiles) }
        }
        NotificationIcon.warm(sender)
    }

    /// Post one BACKGROUND auth banner: a mailbox that is not on screen has
    /// just been sent a login code (or a reset, or a sign-in alert), and the
    /// only thing this notification exists to say is which mailbox to go to.
    /// The live account never comes through here — its auth mail gets the ring,
    /// the audited auto-reveal and the code modal instead.
    ///
    /// The copy is `EventBanner.authCopy`, shared with the iOS extension, which
    /// posts the same banner for the same mail off a push. What it deliberately
    /// leaves out is argued there.
    func postAuth(_ meta: SealedMeta, accountId: UUID, accountName: String) {
        let account = accountId.uuidString
        let copy = EventBanner.authCopy(
            kind: meta.kind, sender: meta.sender, accountName: accountName)
        let content = UNMutableNotificationContent()
        content.title = copy.title
        content.body = copy.body
        // Account-prefixed because two daemons' auth mail is not one
        // conversation, exactly as an event's group is.
        content.threadIdentifier = "\(account).\(copy.threadIdentifier)"
        content.userInfo = [
            EventBanner.accountKey: account,
            EventBanner.routeKey: EventBanner.authRoute,
        ]
        if copy.sound { content.sound = Self.sound(for: Prefs.shared.notificationSound) }

        // Message ids are per-daemon SQLite ints, so the account prefix is
        // load-bearing here for the same reason it is on an event's identifier:
        // unprefixed, account B's message 41 would silently REPLACE account A's
        // banner for message 41. Within one account, re-posting the same id
        // replaces its own banner rather than stacking a second copy.
        //
        // The tile is the SERVICE's logo, which is the one thing this banner is
        // allowed to be specific about: it says who wants the code, while the
        // code itself and the subject that so often contains it stay behind the
        // audited reveal.
        send(content, identifier: "passband.auth.\(account).\(meta.id)", sender: meta.sender)
    }

    /// Post a banner shaped exactly like the real thing, on demand. Settings
    /// offers it for the same reason the sound picker plays its chime: whether
    /// notifications actually arrive — past the system's own permission, focus
    /// modes and Do Not Disturb — is not a question anyone should have to
    /// answer by waiting for mail.
    ///
    /// It is addressed FROM PASSBAND, so the tile it draws is the initials
    /// fallback: `hello@` is a robot local-part, but passband.app is not in the
    /// icon service, and a domain that answers nothing is remembered as such
    /// for a week.
    ///
    /// Returns whether the system will SHOW it, which is not the same as
    /// whether it was posted — the grant is the one refusal worth reporting,
    /// because a test that does nothing in exactly that case is not a test.
    /// Focus modes and Do Not Disturb are not visible from here and are what
    /// the Settings hint points at when this returns true and nothing appears.
    @discardableResult
    func postTest() async -> Bool {
        await requestAuthorizationIfNeeded()
        let status = await UNUserNotificationCenter.current()
            .notificationSettings().authorizationStatus
        guard status == .authorized || status == .provisional else { return false }

        let content = UNMutableNotificationContent()
        content.title = "Passband"
        content.subtitle = "test banner"
        content.body = "Notifications are working. This is what mail worth reading looks like."
        content.threadIdentifier = "passband.test"
        content.sound = Self.sound(for: Prefs.shared.notificationSound)
        // The one banner that names no account, because it belongs to none.
        // Both identifiers are still unique against every real one — an event's
        // are "passband.event.<uuid>.<id>" and a group is "<uuid>.<thread>", so
        // nothing this posts can group with, or replace, a piece of mail. It
        // does replace ITSELF, which is what a second press should do.
        //
        // The route marker is load-bearing twice over: `presentation` draws
        // this one even with the app frontmost — Settings is a page in the main
        // window, so the ordinary rule would file the test banner silently into
        // Notification Center and the button would look broken — and the
        // delegate reads it to keep a self-test out of the open-rate metric.
        content.userInfo = [EventBanner.routeKey: EventBanner.testRoute]
        send(content, identifier: "passband.test", sender: "Passband <hello@passband.app>")
        return true
    }

    // MARK: - delegate behaviour

    /// What to do with a notification that arrives while the app is running.
    /// Frontmost WITH a visible window => no banner, only the list: the human is
    /// already looking at the sitrep the row is on. The window check matters —
    /// "active with zero visible windows" is a common state under residency, and
    /// there the banner is the app's only voice.
    ///
    /// The test banner is the exception, and it has to be: it is posted from a
    /// button on a page INSIDE the main window, so the rule above would suppress
    /// it every single time it is used. A banner nobody asked for is noise while
    /// you are looking at the list it came from; a banner you just pressed a
    /// button for is the whole point.
    nonisolated static func presentation(
        appActive: Bool, windowVisible: Bool, isTest: Bool = false
    ) -> UNNotificationPresentationOptions {
        if isTest { return [.banner, .sound, .list] }
        return (appActive && windowVisible) ? [.list] : [.banner, .sound, .list]
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
        // live account is the only guess available — and the guess then walks
        // the SAME guards as a named account below. "No account" must not be a
        // wider door than naming one: the old shape opened such a tap straight
        // through, firing an authenticated request from behind the Connect
        // gate with whatever credentials the client still held.
        let resolved = accountId ?? AccountManager.shared.activeId
        // An account that has since been REMOVED — or nothing named and no
        // live account to guess. Its ids address a daemon this install no
        // longer has credentials for, and opening one against whoever is live
        // would show a stranger's mail — so the tap fronts the app and stops
        // there, which is the honest whole of what can still be done about it.
        guard let resolved,
            AccountManager.shared.accounts.contains(where: { $0.id == resolved })
        else {
            front()
            return
        }
        // The live account's own banner opens in place; no switch to run.
        // Mid-boot counts: a tap that LAUNCHED the app arrives while status is
        // still `.loading`, and `open` only parks view state the shell shows
        // once the world is up. Only the gate states refuse — there is no
        // world to park into, and the client's config (if any survives) is not
        // this tap's to spend.
        if resolved == AccountManager.shared.activeId {
            front()
            switch AppStore.shared.connStatus {
            case .disconnected, .error: break
            case .loading, .connecting, .connected: open(target)
            }
            return
        }
        // A DIFFERENT account's banner needs a switch, and a switch assumes a
        // fully-live world to replace — it never touches `connStatus`, so from
        // the gate (or mid-boot) it would point the client at a daemon while
        // the screen is still sorting out which one is live, and nothing would
        // show for it. Fronting the app puts the banner's own tap where it can
        // still be acted on.
        guard AppStore.shared.connStatus == .connected else {
            front()
            return
        }
        Task {
            await AccountManager.shared.switchTo(resolved)
            front()
            // The switch is allowed to decline (one already running) and
            // allowed to fail (the credentials behind the record are gone, and
            // it lands on the Connect gate). Either way a DIFFERENT mailbox is
            // on screen, and thread ids are per-daemon: opening one here would
            // show whatever that id happens to name in the wrong account. The
            // Auth view is no safer — it renders the live account's codes.
            guard AccountManager.shared.activeId == resolved else { return }
            open(target)
        }
    }

    /// Bring the app forward. AppKit-only, and a no-op elsewhere: on the phone
    /// the tap has already foregrounded the app by the time this runs.
    ///
    /// Not private: a tapped test banner does this and nothing else.
    func front() {
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
        let route = notification.request.content.userInfo[EventBanner.routeKey] as? String
        let (active, visible) = await MainActor.run {
            #if os(macOS)
                (NSApp.isActive, MainWindow.find()?.isVisible == true)
            #else
                // Off macOS the banner always wins until there is a real
                // foreground check: a suppressed notification is a lost one.
                (false, false)
            #endif
        }
        return Notifier.presentation(
            appActive: active, windowVisible: visible, isTest: route == EventBanner.testRoute)
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse
    ) async {
        let userInfo = response.notification.request.content.userInfo
        let threadId = userInfo[EventBanner.threadKey] as? String
        // Stored as a string and parsed here rather than crossing as one: a
        // payload that survived a relaunch (or came from an older build) can
        // hold anything, and an unparseable id must read as "no account", not
        // as some other account.
        let accountId = (userInfo[EventBanner.accountKey] as? String).flatMap(UUID.init(uuidString:))
        // An auth banner carries no thread to open — routing it as an ordinary
        // one would front the app and then do nothing.
        let route = userInfo[EventBanner.routeKey] as? String
        await MainActor.run {
            switch route {
            case EventBanner.authRoute:
                Notifier.shared.handleAuthTap(accountId: accountId)
            // A self-test opens nothing and is COUNTED as nothing: routing it
            // through handleTap would file a `notification_opened` and quietly
            // inflate the one metric that says whether banners are worth
            // posting at all.
            case EventBanner.testRoute:
                Notifier.shared.front()
            default:
                Notifier.shared.handleTap(threadId: threadId, accountId: accountId)
            }
        }
    }
}
