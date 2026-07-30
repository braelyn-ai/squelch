// Polling that keeps the sitrep read model fresh: fetches the three bands +
// stats + sealed metadata every 10s and on window focus. Writes results into
// the store; views just read store.sitrep.
//
// Each band is fetched with its own server-side `band` filter so the buckets
// match the server's definitions exactly. Sealed is metadata-only (never
// bodies here).
//
// Ported from squelch-desktop/src/state/useSitrep.ts.

import AppKit
import Foundation

@MainActor
final class SitrepPoller {
    static let shared = SitrepPoller()

    private static let pollInterval: Duration = .seconds(10)
    private static let pageLimit = 200

    /// The in-flight pull, shared by the interval poller AND every manual
    /// caller, so a poke never races an overlapping scheduled pull. Held as the
    /// TASK rather than a flag so an overlapping caller can JOIN it: a caller
    /// that skips has no idea whether the answer on screen is older than what it
    /// asked about.
    private var inFlight: Task<Void, Never>?
    private var task: Task<Void, Never>?
    private var focusObserver: NSObjectProtocol?

    private var store: AppStore { AppStore.shared }

    private init() {}

    /// Start interval + focus polling. Idempotent.
    func start() {
        guard task == nil else { return }
        task = Task { [weak self] in
            while !Task.isCancelled {
                await self?.pull()
                try? await Task.sleep(for: Self.pollInterval)
            }
        }
        focusObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
        ) { _ in
            Task { @MainActor in await SitrepPoller.shared.pull() }
        }
    }

    func stop() {
        task?.cancel()
        task = nil
        if let focusObserver { NotificationCenter.default.removeObserver(focusObserver) }
        focusObserver = nil
    }

    /// Fetch the sitrep read model once and write it into the store. Joins a
    /// pull already in flight rather than stacking a second one; no-ops if the
    /// door isn't connected.
    func pull() async {
        if let inFlight {
            await inFlight.value
            return
        }
        guard store.connStatus == .connected else { return }
        let task = Task { await self.performPull() }
        inFlight = task
        await task.value
        if inFlight == task { inFlight = nil }
    }

    /// Re-read every surface a triage correction can move mail BETWEEN, NOW. The
    /// bands are otherwise up to a poll behind and the zones up to a TTL behind,
    /// which is minutes — long enough that the mail looks like it never moved.
    ///
    /// JOIN THEN PULL, rather than pull: a poll already running may have read
    /// the daemon before the correction committed, so its rows are allowed to be
    /// stale and cannot be the ones we settle on. The two halves then run
    /// together, and `pull`'s own trailing zone refresh joins the forced pass
    /// below instead of costing a second round of five requests.
    func refreshAfterCorrection() async {
        if let inFlight { await inFlight.value }
        async let bands: Void = pull()
        async let zones: Void = store.refreshZones(force: true)
        _ = await (bands, zones)
    }

    private func performPull() async {
        do {
            async let standing = APIClient.shared.getUpdates(
                UpdatesParams(band: .standing, limit: Self.pageLimit))
            async let fresh = APIClient.shared.getUpdates(
                UpdatesParams(band: .new, limit: Self.pageLimit))
            async let open = APIClient.shared.getUpdates(
                UpdatesParams(band: .open, limit: Self.pageLimit))
            async let stats = APIClient.shared.getStats()
            async let sealed = APIClient.shared.listSealed()

            let (s, f, o, st, sl) = try await (standing, fresh, open, stats, sealed)
            let next = SitrepData(
                standing: s.items, new: f.items, open: o.items, stats: st, sealed: sl)
            // ASSIGN ONLY ON CHANGE. @Observable notifies on assignment, not on
            // value difference, so writing an identical read model every 10s
            // invalidated every view reading it and re-laid out the whole
            // dashboard — for nothing. Most polls return identical data.
            if next != store.sitrep { store.sitrep = next }
            if store.refreshError != nil { store.refreshError = nil }
            store.lastRefresh = Date()
            // The bands half of the launch image warm's input; refreshZones
            // below supplies the other half.
            ImageWarmer.shared.noteSitrepLanded()

            // Keep a valid selection: if nothing selected, land on the first row.
            if store.selectedId == nil, let first = store.orderedIds.first {
                store.selectedId = first
            }

            // Keep the dashboard's ZONES warm from here too, not just from the
            // view. The bands above are only half the sitrep; if the zones only
            // loaded when SitrepView mounted, the FIRST visit of a session still
            // paid for five round-trips. Its own TTL makes this a no-op most
            // polls (see AppStore.refreshZones), so the cost is one date compare.
            await store.refreshZones()
        } catch {
            // Keep the kind so the UI can say "daemon unreachable" vs "token
            // rejected" instead of one undifferentiated failure.
            if let api = error as? APIError {
                store.refreshError = RefreshError(message: api.message, kind: api.kind)
            } else {
                store.refreshError = RefreshError(message: "refresh failed", kind: .unknown)
            }
        }
    }

    /// MANUAL refresh: poke the daemon to poll Gmail NOW, then re-pull the read
    /// model so freshly-ingested mail shows without waiting out the ~45s server
    /// poll or the 10s client poll. The server poke is fire-and-forget, so we
    /// pull once right after the rows are likely landed and once more a beat
    /// later to catch a slower Gmail round trip.
    func triggerMailRefresh() async {
        guard store.connStatus == .connected else { return }
        // Poke failure is non-fatal — pullSitrep surfaces its own error.
        _ = try? await APIClient.shared.refreshMail()
        try? await Task.sleep(for: .milliseconds(400))
        await pull()
        try? await Task.sleep(for: .milliseconds(1600))
        await pull()
    }
}
