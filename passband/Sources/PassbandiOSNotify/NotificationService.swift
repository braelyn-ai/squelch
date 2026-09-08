// The notification service extension: what turns a blind push into a banner
// that says something.
//
// The relay is blind BY DESIGN (see squelch-relay/src/handlers.rs) — it carries
// an event id, a collapse id, and a generic "New mail surfaced" alert, because
// neither it nor Apple is entitled to know what the mail says. `mutable-content`
// on that push hands the payload here first, inside the user's own device,
// where the daemon can be asked directly. This process is the only place the
// two halves meet.
//
// EVERYTHING HERE FAILS OPEN, to the alert the relay already wrote. No daemon,
// no credentials, no network, no time left: the human still gets told there is
// mail, just not what it is. A silent notification would be worse than a vague
// one.

import Foundation
import UserNotifications

/// Which mailbox, and which of its events. The push's `event_id` is
/// `"<account uuid>:<event id>"` — the tag the client registered, joined to a
/// per-daemon int that means nothing without it. An untagged id (a bare number,
/// from a daemon or a client that predates the tag) is unroutable ON PURPOSE:
/// two accounts' event 41 are different mail, and guessing between them would
/// show the wrong mailbox's business on the lock screen.
struct PushRoute {
    let accountId: UUID
    let eventId: Int

    /// The relay's payload key, verbatim.
    static let eventIdKey = "event_id"

    init?(userInfo: [AnyHashable: Any]) {
        guard let raw = userInfo[Self.eventIdKey] as? String,
            let colon = raw.lastIndex(of: ":"),
            let accountId = UUID(uuidString: String(raw[raw.startIndex..<colon])),
            let eventId = Int(raw[raw.index(after: colon)...])
        else { return nil }
        self.accountId = accountId
        self.eventId = eventId
    }
}

/// Main-actor isolated in full, because the fetch and the expiry timer race for
/// the same one-shot handler and something has to serialize them.
///
/// The isolation ASSUMES what Apple's own extension template assumes: both
/// callbacks arrive on this process's main queue. The SDK does not annotate
/// `UNNotificationServiceExtension` to say so, but the template mutates two
/// stored properties from both callbacks with no locking of its own, which is
/// only sound on that reading. Stated here because it is an assumption, not a
/// checked fact.
@MainActor
final class NotificationService: UNNotificationServiceExtension {
    /// STATIC, so the two fetches below can be static too. See the launch
    /// site: they run as concurrent children and this object is a
    /// main-actor class, so anything they touch has to not be `self`.
    private nonisolated(unsafe) static let session = Sessions.ephemeral(
        timeout: 10, resource: 20,
        cachePolicy: .reloadIgnoringLocalCacheData, emptyHeaders: true)

    /// A SECOND SESSION, ON A SHORTER LEASH, for the badge count alone.
    ///
    /// The two fetches are awaited together, so the banner cannot be delivered
    /// until the slower one finishes — a stalled count would delay the alert
    /// itself. That is the wrong way round: the badge is a nicety and the banner
    /// is the reason this process exists, so the count gets a fraction of the
    /// time and drops out quietly when it cannot be had cheaply.
    ///
    /// ITS OWN SESSION rather than a shorter `timeoutInterval` on the request,
    /// because the configuration's `timeoutIntervalForRequest` and the request's
    /// own value both apply and which one wins is not a thing to guess at in a
    /// process that gets killed for being slow. Two configurations, two
    /// unambiguous ceilings.
    ///
    /// Not a theoretical hazard: reads of this band go through the daemon's
    /// store mutex, and a slow one there is a thing that has actually happened.
    private nonisolated(unsafe) static let countSession = Sessions.ephemeral(
        timeout: 4, resource: 8,
        cachePolicy: .reloadIgnoringLocalCacheData, emptyHeaders: true)

    /// Held so `serviceExtensionTimeWillExpire` can still answer: the system
    /// kills this process if nothing calls the handler, and a killed extension
    /// delivers the original push anyway — but only after making the human wait
    /// for the timeout.
    private var handler: ((UNNotificationContent) -> Void)?
    private var original: UNNotificationContent?
    private var work: Task<Void, Never>?

    override func didReceive(
        _ request: UNNotificationRequest,
        withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void
    ) {
        handler = contentHandler
        original = request.content

        guard let route = PushRoute(userInfo: request.content.userInfo) else {
            return deliver(request.content)
        }
        work = Task { @MainActor [weak self] in
            guard let self else { return }
            // BOTH CALLS AT ONCE, because there is one timeout for the pair.
            // `async let` spawns a CONCURRENT child, which is why both
            // fetches are STATIC: this is a main-actor class and not Sendable,
            // so a child task that captured `self` — even to call a nonisolated
            // method on it — would be sending it across. Neither call needs the
            // instance anyway; each is a keychain read, a request and a decode.
            // The handler and the expiry timer, which is what the isolation is
            // actually for, stay up here where the results land.
            // The banner's own fetch and the badge's are independent questions
            // to the same daemon, and running them back to back would spend two
            // round trips out of the seconds this process gets — enough, on a
            // slow link, to lose the enrichment that is the whole point of the
            // extension in order to add a number to an icon.
            async let banner = Self.fetch(route)
            async let due = Self.dueToday(route)
            let (event, count) = await (banner, due)
            guard let event else {
                return self.deliver(Self.badged(request.content, count: count))
            }
            self.deliver(
                Self.badged(Self.content(for: event, accountId: route.accountId), count: count))
        }
    }

    /// The app icon's number, stamped onto whatever banner is about to go out.
    ///
    /// A NIL COUNT LEAVES THE BADGE ALONE, which is why this takes an optional
    /// rather than defaulting to zero: `badge = 0` CLEARS the icon, so a failed
    /// or timed-out fetch that fell back to zero would tell someone with four
    /// overdue bills that they were clear. Not knowing and knowing it is nothing
    /// are different answers and the payload can say both.
    private static func badged(_ content: UNNotificationContent, count: Int?)
        -> UNNotificationContent
    {
        guard let count,
            let mutable = content.mutableCopy() as? UNMutableNotificationContent
        else { return content }
        mutable.badge = NSNumber(value: count)
        return mutable
    }

    /// How many obligations are overdue or due by end of today, for the mailbox
    /// this push came from — the same `NeedToday.count` the app's own sitrep
    /// headline is built from, over the same standing band.
    ///
    /// `peek=true` IS LOAD-BEARING AND NOT AN OPTIMIZATION. A plain read of this
    /// band STAMPS every row it returns as surfaced (see the daemon's updates
    /// handler: peek's only effect is skipping that ledger write), which would
    /// promote `new` to `open` and empty the new band. Refreshing a number must
    /// not be able to mark mail as seen — least of all from a background process
    /// the user never opened.
    ///
    /// Counted HERE rather than asked for: the daemon serves no due-today
    /// figure, and inventing one server-side would be a second definition of
    /// "today" evaluated in a second timezone. The rows come back and the shared
    /// pure function decides, exactly as it does in the app.
    private static func dueToday(_ route: PushRoute) async -> Int? {
        guard let settings = try? SettingsStore.load(accountId: route.accountId) else { return nil }

        var base = settings.serverURL.trimmingCharacters(in: .whitespacesAndNewlines)
        while base.hasSuffix("/") { base.removeLast() }
        guard var comps = URLComponents(string: base + "/client/updates") else { return nil }
        comps.queryItems = [
            URLQueryItem(name: "band", value: "standing"),
            URLQueryItem(name: "peek", value: "true"),
            // THE SAME PAGE THE APP READS. This route defaults to fifty rows
            // when nobody says otherwise, and the poller asks for
            // NeedToday.bandLimit — so omitting it here counted a smaller set
            // than the headline the badge is meant to agree with, and under-
            // reported, because this band is ordered by importance and the rows
            // past a cut are not the least due ones.
            URLQueryItem(name: "limit", value: String(NeedToday.bandLimit)),
        ]
        guard let url = comps.url else { return nil }

        var request = URLRequest(url: url, timeoutInterval: 4)
        request.setValue("Bearer \(settings.apiToken)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Accept")

        // Silent on every failure, like the fetch above and for the same reason:
        // the URL names the user's daemon and the header held a capability. A
        // miss here costs a stale badge, which the app corrects the moment it is
        // opened.
        guard let (data, response) = try? await Self.countSession.data(for: request),
            (response as? HTTPURLResponse)?.statusCode == 200,
            let page = try? JSONDecoder().decode(Page<AttentionUpdate>.self, from: data)
        else { return nil }
        return NeedToday.count(page.items)
    }

    /// Out of time. Whatever the fetch was doing, the original alert goes out
    /// now — this is the fallback the relay's generic copy exists for.
    override func serviceExtensionTimeWillExpire() {
        work?.cancel()
        if let original { deliver(original) }
    }

    /// Call the system's handler at most once. Both the fetch and the expiry
    /// path can reach here, and the second call is a crash in some OS versions.
    private func deliver(_ content: UNNotificationContent) {
        guard let handler else { return }
        self.handler = nil
        handler(content)
    }

    /// Ask THIS account's daemon what the event was. The bearer comes from the
    /// same per-account keychain slots the app writes, which is the whole
    /// reason the extension shares its keychain access group.
    private static func fetch(_ route: PushRoute) async -> Event? {
        guard let settings = try? SettingsStore.load(accountId: route.accountId) else { return nil }

        var base = settings.serverURL.trimmingCharacters(in: .whitespacesAndNewlines)
        while base.hasSuffix("/") { base.removeLast() }
        guard let url = URL(string: "\(base)/client/events/\(route.eventId)") else { return nil }

        var request = URLRequest(url: url, timeoutInterval: 10)
        request.setValue("Bearer \(settings.apiToken)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Accept")

        // A 404 is the daemon's answer for "not yours or not there" and is a
        // fallback, not an error worth distinguishing. Nothing about the
        // failure is logged: the URL names the user's daemon and the header
        // held a capability.
        guard let (data, response) = try? await Self.session.data(for: request),
            (response as? HTTPURLResponse)?.statusCode == 200
        else { return nil }
        return try? JSONDecoder().decode(Event.self, from: data)
    }

    /// The banner the app itself would have posted for this event, built from
    /// the same mapping so a push and a live frame are indistinguishable.
    private static func content(for event: Event, accountId: UUID) -> UNNotificationContent {
        switch EventBanner.routing(for: event) {
        case .threadBanner: return threadContent(for: event, accountId: accountId)
        case .authSignal(let kind): return authContent(kind: kind, event: event, accountId: accountId)
        }
    }

    /// AUTH MAIL. The phone cannot poll in the background, so this push is the
    /// only way a login code reaches a phone at all — and it is the one banner
    /// here that is NOT built from what the mail says. Two fields of the event
    /// are read, the kind and the from address, and both are metadata of the
    /// class `/client/sealed` already serves; `one_line` is not read, because a
    /// subject is so often the code itself and a lock screen is not a place to
    /// leave a credential. (The daemon writes a fixed per-kind phrase into that
    /// field for exactly this row, but the rule here does not lean on it.)
    ///
    /// The route marker is what keeps the tap off a thread fetch: a sealed
    /// thread is one the daemon refuses to serve, so an ordinary event banner
    /// would open the app onto nothing. `handleAuthTap` lands on the Auth list,
    /// where asking for the code is the human's own audited act.
    ///
    /// No account name: this process reads credentials out of the shared
    /// keychain and has no access to the app's account labels, so the kind
    /// stands alone. `EventBanner.authCopy` takes nil for that and says so.
    private static func authContent(
        kind: SealedKind, event: Event, accountId: UUID
    ) -> UNNotificationContent {
        let account = accountId.uuidString
        let copy = EventBanner.authCopy(kind: kind, sender: event.sender, accountName: nil)
        let content = UNMutableNotificationContent()
        content.title = copy.title
        content.body = copy.body
        content.threadIdentifier = "\(account).\(copy.threadIdentifier)"
        content.userInfo = [
            EventBanner.accountKey: account,
            EventBanner.routeKey: EventBanner.authRoute,
        ]
        // Always a chime, and the system's rather than the user's chosen one:
        // the app's sound preference lives in ITS UserDefaults, which this
        // process does not share.
        content.sound = copy.sound ? .default : nil
        return content
    }

    /// The ordinary event's banner: who the mail is from, what triage made of
    /// it, and a tap that opens the thread.
    private static func threadContent(for event: Event, accountId: UUID)
        -> UNNotificationContent
    {
        let copy = EventBanner.copy(for: event)
        let account = accountId.uuidString
        let content = UNMutableNotificationContent()
        content.title = copy.title
        if !copy.subtitle.isEmpty { content.subtitle = copy.subtitle }
        content.body = copy.body
        // Account-prefixed for the reason `Notifier` prefixes its own: thread
        // ids are per-daemon, so two mailboxes would otherwise stack unrelated
        // mail into one conversation.
        content.threadIdentifier = "\(account).\(copy.threadIdentifier)"
        // The keys a tap routes on, written exactly as the live path writes
        // them — `NotificationDelegate` cannot tell the two apart, and must not.
        content.userInfo = [
            EventBanner.threadKey: event.thread_id,
            EventBanner.eventKey: event.id,
            EventBanner.accountKey: account,
        ]
        // The system default rather than the user's chosen chime: the app's
        // sound preference lives in ITS UserDefaults, which this process does
        // not share. Silencing the kinds the app silences matters more than
        // which chime the rest use.
        content.sound = copy.sound ? .default : nil
        return content
    }
}
