// Keeps the sitrep read model fresh: the three bands + stats + sealed metadata,
// every 10s and on window focus, written into the store for views to read. A
// failing daemon widens that interval — see `run()`.
//
// Each band carries its own server-side `band` filter so the buckets match the
// server's definitions exactly. Sealed is metadata-only — never bodies.

import AppKit
import Foundation

@MainActor
final class SitrepPoller {
    static let shared = SitrepPoller()

    private static let pollInterval: Duration = .seconds(10)
    private static let pageLimit = 200

    /// Failure backoff. The base sits at TWICE the healthy cadence so its
    /// jittered floor equals `pollInterval`: a wedged daemon is never polled
    /// faster than a working one. The cap bounds how stale the sitrep can be
    /// once the daemon comes back.
    private static let backoffBase: TimeInterval = 20
    private static let backoffCap: TimeInterval = 60

    /// The in-flight pull, shared by the interval poller and every manual
    /// caller. Held as the TASK rather than a flag so an overlapping caller can
    /// JOIN it — a caller that merely skips cannot know whether what is on
    /// screen predates what it asked about.
    private var inFlight: Task<Bool, Never>?
    private let resident = ResidentTask()
    private var focusObserver: NSObjectProtocol?

    private var store: AppStore { AppStore.shared }

    private init() {}

    /// Start interval + focus polling. Idempotent.
    func start() {
        // The observer rides on the loop's own guard: `ResidentTask` refuses a
        // second loop by itself, but nothing would stop a second `start()` from
        // stacking a second focus observer.
        guard !resident.isRunning else { return }
        resident.start { [weak self] in await self?.run() }
        focusObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
        ) { _ in
            Task { @MainActor in await SitrepPoller.shared.pull() }
        }
    }

    func stop() {
        resident.stop()
        if let focusObserver { NotificationCenter.default.removeObserver(focusObserver) }
        focusObserver = nil
    }

    /// The healthy cadence while the daemon answers, jittered backoff while it
    /// does not. Without the backoff a wedged daemon takes five requests every
    /// 10s for as long as the app is open — and every client does it in step.
    private func run() async {
        var backoff = Backoff(base: Self.backoffBase, cap: Self.backoffCap)
        while !Task.isCancelled {
            let ok = await pull()
            if Task.isCancelled { return }
            if ok {
                backoff.reset()
                try? await Task.sleep(for: Self.pollInterval)
            } else {
                await backoff.sleep()
            }
        }
    }

    /// Fetch the read model once and write it into the store. Joins a pull
    /// already in flight rather than stacking a second; no-ops when the door is
    /// not connected.
    ///
    /// Returns whether the daemon ANSWERED, which only the backoff loop reads. A
    /// door that is not connected reports true: nothing was asked of the daemon,
    /// so there is nothing to back off from, and the loop keeps the cadence that
    /// notices the reconnect.
    @discardableResult
    func pull() async -> Bool {
        if let inFlight { return await inFlight.value }
        guard store.connStatus == .connected else { return true }
        let task = Task { await self.performPull() }
        inFlight = task
        let ok = await task.value
        if inFlight == task { inFlight = nil }
        return ok
    }

    /// Re-read every surface a triage correction can move mail between, NOW —
    /// bands are otherwise a poll behind and zones a TTL behind, long enough
    /// that the mail looks like it never moved.
    ///
    /// JOIN THEN PULL: a poll already running may have read the daemon before
    /// the correction committed, so its rows cannot be the ones we settle on.
    func refreshAfterCorrection() async {
        if let inFlight { _ = await inFlight.value }
        async let bands: Bool = pull()
        async let zones: Void = store.refreshZones(force: true)
        _ = await (bands, zones)
    }

    /// Returns whether the daemon answered; the caller decides what a failure costs.
    private func performPull() async -> Bool {
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
            // ASSIGN ONLY ON CHANGE: @Observable notifies on assignment, not on
            // value difference, so writing an identical read model every 10s
            // re-lays out the whole dashboard for nothing.
            if next != store.sitrep { store.sitrep = next }
            if store.refreshError != nil { store.refreshError = nil }
            store.lastRefresh = Date()
            // The bands half of the launch image warm's input; refreshZones
            // below supplies the rest.
            ImageWarmer.shared.noteSitrepLanded()

            // Keep a valid selection: if nothing selected, land on the first row.
            if store.selectedId == nil, let first = store.orderedIds.first {
                store.selectedId = first
            }

            // Zones warm from here, not just from SitrepView's mount, or the
            // first visit of a session still pays five round-trips. Its own TTL
            // makes this a date compare on most polls.
            await store.refreshZones()
            return true
        } catch {
            // Keep the kind so the UI can say "daemon unreachable" vs "token
            // rejected" rather than one undifferentiated failure.
            if let api = error as? APIError {
                store.refreshError = RefreshError(message: api.message, kind: api.kind)
            } else {
                store.refreshError = RefreshError(message: "refresh failed", kind: .unknown)
            }
            return false
        }
    }

    /// Manual refresh: poke the daemon to poll Gmail now, then re-pull so fresh
    /// mail shows without waiting out the ~45s server or 10s client poll. The
    /// poke is fire-and-forget, hence two pulls — one when rows have likely
    /// landed, one a beat later for a slower Gmail round trip.
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
