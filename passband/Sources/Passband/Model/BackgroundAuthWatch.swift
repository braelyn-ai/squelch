// THE BACKGROUND EAR FOR AUTH MAIL: one polling watcher per account that is
// NOT on screen, whose entire output is a banner saying which mailbox a login
// code just landed in.
//
// It exists because sealed mail deliberately never rides the event feed — the
// daemon keeps auth metadata off SSE — so an `EventStream` cannot say a word
// about it. The live account learns of it through SitrepPoller → AuthArrival;
// every other account would learn of it never, which for a client the human
// added a second mailbox to is the one gap that matters: a code arrives, the
// app is open, and nothing happens.
//
// DELIBERATELY THE THIN VERSION of that flow. No ring, no auto-reveal, no code
// in the notification. Revealing a body is audited server-side and belongs to
// the account the human is actually looking at, and a code parked in
// Notification Center is a code on the lock screen of an unattended Mac. This
// posts the kind and the mailbox; the tap switches accounts and lands on Auth,
// where asking for the code is the human's own act.
//
// Everything it needs is fixed at construction, exactly as `EventStream`'s is:
// the account it posts under and the credentials it dials with. Nothing here
// reads `AppStore.shared` or `APIClient.shared` — both are the ACTIVE account's
// world, and a watcher pointed at that would be polling the one mailbox it has
// nothing to say about.

import Foundation

@MainActor
final class BackgroundAuthWatch {
    /// Whose mailbox this watches. Rides along to every banner posted from it,
    /// so a tap can take the human to the account the code is actually in.
    let accountId: UUID

    /// The daemon this watcher talks to, FIXED FOR THE LIFE OF THE OBJECT —
    /// same rule as `EventStream`, and for the same reason: credentials that
    /// change reach the watcher by REPLACING it, never by mutating one, so it
    /// can never end up holding one account's URL with another's token.
    private let settings: ConnectionSettings

    /// Every 30s. Three times gentler than the sitrep poll on purpose: this is
    /// a mailbox nobody is looking at, and the only thing a wider interval
    /// costs is how long a code takes to reach a banner.
    private static let pollInterval: Duration = .seconds(30)

    /// Failure backoff, shaped like `SitrepPoller`'s rather than
    /// `EventStream`'s — the lighter of the two, which is what a background
    /// mailbox deserves. The base sits at TWICE the cadence so its jittered
    /// floor equals the healthy interval: a wedged daemon is never polled
    /// faster than a working one. The cap is the freshness window itself,
    /// because a poll that lands later than that can only ever come back with
    /// mail already too old to be worth a banner.
    private static let backoffBase: TimeInterval = 60
    private static let backoffCap: TimeInterval = AuthSeenSet.freshWindow

    /// Hard request deadline. Generous for a local daemon, and bounded so a
    /// daemon dying mid-request cannot park the loop's only request forever.
    private static let requestTimeout: TimeInterval = 15

    private let resident = ResidentTask()

    /// The persisted seen-set, shared with `AuthArrival` (see AuthSeenSet).
    /// Read from disk HERE, at construction, so a watcher restarted by a switch
    /// picks up everything the live flow recorded while it was the live one.
    private var seen: AuthSeenSet

    /// NOT waitsForConnectivity: a connect to a dead daemon should fail now and
    /// be retried by the backoff below, not park until the timeout. No URL
    /// cache either — one JSON list every 30s is not worth a second copy.
    private let session = Sessions.ephemeral(
        timeout: BackgroundAuthWatch.requestTimeout,
        resource: BackgroundAuthWatch.requestTimeout,
        cookies: .neverSent, urlCache: false,
        cachePolicy: .reloadIgnoringLocalCacheData,
        emptyHeaders: true, waitsForConnectivity: false)

    /// Refuses every redirect — an empty allow-list. The URL is
    /// operator-configured and the request carries the bearer header, so a 3xx
    /// from it is a misconfiguration, not a hop to follow. Refusal surfaces the
    /// 3xx, which fails the 200 check and backs off.
    private static let pinned = SchemePinned(allow: [])

    private static let decoder = JSONDecoder()

    init(accountId: UUID, settings: ConnectionSettings) {
        self.accountId = accountId
        self.settings = settings
        self.seen = AuthSeenSet(accountId: accountId)
    }

    /// Start watching. Idempotent.
    func start() {
        resident.start { [weak self] in await self?.run() }
    }

    func stop() {
        resident.stop()
    }

    // MARK: - the loop

    /// Poll, sleep, repeat — and back off instead of sleeping when the daemon
    /// did not answer. ONE REQUEST IN FLIGHT by construction: this loop is the
    /// only caller of `poll()` and it awaits its own, so a slow daemon delays
    /// the next request rather than stacking one on top of it.
    private func run() async {
        // Ask for the notification grant here as well as on the feed's first
        // connect: an install whose only auth mail arrives in a background
        // account still has to be allowed to say so.
        await Notifier.shared.requestAuthorizationIfNeeded()

        var backoff = Backoff(base: Self.backoffBase, cap: Self.backoffCap)
        while !Task.isCancelled {
            let answered = await poll()
            if Task.isCancelled { return }
            if answered {
                backoff.reset()
                try? await Task.sleep(for: Self.pollInterval)
            } else {
                await backoff.sleep()
            }
        }
    }

    /// One pull of the sealed metadata. Returns whether the daemon ANSWERED,
    /// which only the backoff above reads. Never throws: every failure here is
    /// "ask again in a moment".
    private func poll() async -> Bool {
        // This account went live while the loop was sleeping. `AuthArrival`
        // owns its auth mail now — including the seen-set the two of us persist
        // under one key — so this watcher takes its hands off and waits to be
        // stopped, which the switch is already doing. Reported as answered:
        // nothing was asked of the daemon, so there is nothing to back off from.
        guard AccountManager.shared.activeId != accountId else { return true }
        guard let request = buildRequest() else { return false }
        do {
            let (data, response) = try await session.data(for: request, delegate: Self.pinned)
            // 401 (token rotated), 404 (older daemon) and 5xx are identical from
            // here: back off and try again. Telling the human their token is
            // wrong is the live account's job, and this is not it.
            guard let http = response as? HTTPURLResponse, http.statusCode == 200,
                let sealed = try? Self.decoder.decode([SealedMeta].self, from: data)
            else { return false }
            observe(sealed)
            return true
        } catch {
            // Refused / DNS / timeout / cancellation. Silent and stored
            // nowhere: the error text can embed the server URL, and this is a
            // background poll nobody is waiting on.
            return false
        }
    }

    /// Fold one answer into the seen-set and post a banner per fresh arrival.
    private func observe(_ sealed: [SealedMeta]) {
        // Re-checked AFTER the round trip, not just before it: a switch can
        // land inside the request, and the instant this account is the live one
        // its seen-set has another writer. Dropping the answer on the floor is
        // safe because the set is PERSISTED: every id this watcher fired for
        // was saved before the switch, so `AuthArrival` reads it from disk and
        // stays quiet, and an id seen here but never saved is equally unseen
        // there — the live side fires the one notification instead. Exactly
        // once, either way; that argument leans on the save-on-fire rule in
        // `AuthSeenSet.arrivals`, not on any first-observation seeding.
        guard AccountManager.shared.activeId != accountId else { return }
        let arrivals = seen.arrivals(in: sealed)
        guard !arrivals.isEmpty else { return }
        // Oldest first, so the newest code is the last banner posted and sits
        // on top of the stack.
        let ordered = arrivals.sorted {
            AuthSeenSet.age($0.received_at) > AuthSeenSet.age($1.received_at)
        }
        let name = displayName
        for m in ordered {
            Notifier.shared.postAuth(m, accountId: accountId, accountName: name)
        }
    }

    /// What the banner calls this mailbox. Read LIVE rather than captured at
    /// construction, so a label typed in Settings reaches the next banner. The
    /// fallback costs no keychain read: the credentials this watcher already
    /// holds name the daemon.
    private var displayName: String {
        AccountManager.shared.accounts.first { $0.id == accountId }?.displayName
            ?? AccountRecord.host(from: settings.serverURL)
    }

    // MARK: - request

    /// The one request this class makes, assembled the way `EventStream`
    /// assembles its own: from the injected settings, with the bearer token in
    /// the Authorization header and NOWHERE else — never in the URL, never in
    /// an error message.
    private func buildRequest() -> URLRequest? {
        var base = settings.serverURL
        while base.hasSuffix("/") { base.removeLast() }
        guard let url = URL(string: base + "/client/sealed") else { return nil }

        var req = URLRequest(url: url, timeoutInterval: Self.requestTimeout)
        req.httpMethod = "GET"
        req.setValue("Bearer \(settings.apiToken)", forHTTPHeaderField: "Authorization")
        req.setValue("application/json", forHTTPHeaderField: "Accept")
        req.setValue("no-store", forHTTPHeaderField: "Cache-Control")
        req.cachePolicy = .reloadIgnoringLocalCacheData
        return req
    }
}
