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

    /// In-flight guard shared by the interval poller AND the manual refresh
    /// button, so a user poke never races an overlapping scheduled pull.
    private var inFlight = false
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

    /// Fetch the sitrep read model once and write it into the store. No-ops if
    /// a pull is already in flight or the door isn't connected.
    func pull() async {
        guard !inFlight, store.connStatus == .connected else { return }
        inFlight = true
        defer { inFlight = false }

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
            store.sitrep = SitrepData(
                standing: s.items, new: f.items, open: o.items, stats: st, sealed: sl)
            store.refreshError = nil
            store.lastRefresh = Date()

            // Keep a valid selection: if nothing selected, land on the first row.
            if store.selectedId == nil, let first = store.orderedIds.first {
                store.selectedId = first
            }
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
