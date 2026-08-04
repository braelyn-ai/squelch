// ONE-SHOT LAUNCH WARM: pull the sitrep's images onto disk before the reader opens
// anything, and pin them to the mail they belong to so they stay until it is done.
//
// ORDER MATTERS: for-your-eyes mail warms first, all of it, before a single
// marketing image. The remote-images pref is an ABSOLUTE gate — a prefetch is
// exactly the request that pref exists to prevent, and doing it invisibly into a
// cache would be worse, not better; reconciliation still runs, being network-free.
// Fires once per launch, once BOTH the bands and the zones have landed (either
// alone warms half the list and never returns for the rest).

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
        // Pins first: ids the read model no longer carries were resolved while the
        // app was closed, so their images fall back into the LRU rather than
        // holding a permanent exemption.
        await ImageStore.shared.reconcile(activePins: Self.activePins(store))

        guard Prefs.shared.loadRemoteImages else { return }

        for update in store.sitrep.standing {
            await warmThread(update.thread_id)
        }
        for newsletter in store.zones.newsletters {
            await warmThread(newsletter.latestThreadId)
        }
    }

    /// Every message id the sitrep still surfaces, newsletters included (their
    /// aggregated updates are the same message ids the bands use).
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

    /// Warm one thread's bodies. The thread is normally already cached, so this is
    /// usually a dictionary hit followed by the image fetches.
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

    /// The warm cache's answer, or a fresh pass OFF the main actor: an
    /// already-prepared body costs one dictionary probe, and one that is not
    /// prepared is never scanned on the main thread.
    ///
    /// ALWAYS the STRIPPED preparation, even for a known sender whose pixels the
    /// reader will keep: this runs before the mail is opened, and pre-fetching a
    /// tracking pixel would report an open that never happened. The known-sender
    /// body is prepared for real when the card renders it.
    private static func prepared(_ html: String, _ seenEarlier: Set<String>)
        async -> EmailWebView.Prepared
    {
        let key = EmailWebView.Prepared.cacheKey(html, seenEarlier, false)
        if let warm = PreparedBodies.shared.get(key) { return warm }
        let made = await Task.detached(priority: .utility) {
            EmailWebView.Prepared.make(from: html, seenEarlier: seenEarlier, allowTrackers: false)
        }.value
        PreparedBodies.shared.set(key, made)
        return made
    }
}
