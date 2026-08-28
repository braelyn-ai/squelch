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
    private let session = Sessions.ephemeral(
        timeout: 10, resource: 20,
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
            guard let event = await self.fetch(route) else {
                return self.deliver(request.content)
            }
            self.deliver(Self.content(for: event, accountId: route.accountId))
        }
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
    private func fetch(_ route: PushRoute) async -> Event? {
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
        guard let (data, response) = try? await session.data(for: request),
            (response as? HTTPURLResponse)?.statusCode == 200
        else { return nil }
        return try? JSONDecoder().decode(Event.self, from: data)
    }

    /// The banner the app itself would have posted for this event, built from
    /// the same mapping so a push and a live frame are indistinguishable.
    private static func content(for event: Event, accountId: UUID) -> UNNotificationContent {
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
