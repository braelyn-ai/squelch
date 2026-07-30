// ONE-SHOT LAUNCH WARM: pull the sitrep's images onto disk before the reader
// opens anything, and pin them to the mail they belong to.
//
// ThreadPrefetch already makes an open network-free for the JSON, and
// PreparedBodies makes it scan-free — but the images were still a cold fetch per
// message, per launch, because WebKit's store was memory-only. This closes the
// last gap: by the time the first email is opened, its art is already local, and
// it STAYS local because the entry is pinned until that message is marked done.
//
// ORDER IS THE PRODUCT DECISION. For-your-eyes mail warms first, all of it,
// before a single marketing image is fetched. The standing band is what the
// reader is going to open; the newsletter zone is what they are going to skim.
//
// THE PREF IS AN ABSOLUTE GATE. With "load remote images" off, warming does
// NOTHING — prefetching is precisely the network request that pref exists to
// prevent, and doing it "invisibly, into a cache" would be worse than doing it
// on screen, not better. Reconciliation still runs, because dropping stale pins
// touches no network.
//
// Fires once per launch, once BOTH data sources have landed: the sitrep bands
// (SitrepPoller) and the zones (AppStore.refreshZones). Either alone would warm
// half the list and never come back for the rest.

import Foundation

@MainActor
final class ImageWarmer {
    static let shared = ImageWarmer()

    private var sitrepLanded = false
    private var zonesLanded = false
    private var started = false

    private init() {}

    func noteSitrepLanded() {
        sitrepLanded = true
        startIfReady()
    }

    func noteZonesLanded() {
        zonesLanded = true
        startIfReady()
    }

    private func startIfReady() {
        guard sitrepLanded, zonesLanded, !started else { return }
        started = true
        Task { await warmSitrepOnce(store: AppStore.shared) }
    }

    func warmSitrepOnce(store: AppStore) async {
        // Pins first: message ids the read model no longer carries were resolved
        // while the app was closed, and their images should fall back into the
        // LRU rather than holding a permanent exemption.
        await ImageStore.shared.reconcile(activePins: Self.activePins(store))

        guard Prefs.shared.loadRemoteImages else { return }

        for update in store.sitrep.standing {
            await warmThread(update.thread_id)
        }
        for newsletter in store.zones.newsletters {
            await warmThread(newsletter.latestThreadId)
        }
    }

    /// Every message id the sitrep still surfaces. Newsletters contribute their
    /// aggregated updates, which are the same message ids the bands use.
    private static func activePins(_ store: AppStore) -> Set<Int> {
        var ids = Set<Int>()
        for update in store.sitrep.standing + store.sitrep.new + store.sitrep.open {
            ids.insert(update.id)
        }
        for newsletter in store.zones.newsletters {
            for item in newsletter.items { ids.insert(item.id) }
        }
        return ids
    }

    /// Warm one thread's bodies. The thread is normally already cached (the
    /// sitrep prefetches standing rows, HeroCache resolves newsletter threads),
    /// so this is usually a dictionary hit followed by the image fetches.
    private func warmThread(_ threadId: String) async {
        guard !threadId.isEmpty,
            let view = try? await ThreadPrefetch.shared.fetch(threadId)
        else { return }

        let repeats =
            ThreadPrefetch.shared.cachedRepeatedImages(threadId)
            ?? ThreadPrefetch.repeatedImages(in: view)

        for message in view.messages {
            guard let html = message.html, !html.isEmpty else { continue }
            let seenEarlier = repeats[message.id] ?? []
            let prepared = await Self.prepared(html, seenEarlier)
            guard !prepared.imageURLs.isEmpty else { continue }
            await ImageStore.shared.warm(urls: prepared.imageURLs, pin: message.id)
        }
    }

    /// The warm cache's answer, or a fresh pass OFF the main actor — the same
    /// scans ThreadPrefetch runs, so a body it has already prepared costs one
    /// dictionary probe and a body it has not is not paid for here on the main
    /// thread.
    private static func prepared(_ html: String, _ seenEarlier: Set<String>)
        async -> EmailWebView.Prepared
    {
        let key = EmailWebView.Prepared.cacheKey(html, seenEarlier)
        if let warm = PreparedBodies.shared.get(key) { return warm }
        let made = await Task.detached(priority: .utility) {
            EmailWebView.Prepared.make(from: html, seenEarlier: seenEarlier)
        }.value
        PreparedBodies.shared.set(key, made)
        return made
    }
}
