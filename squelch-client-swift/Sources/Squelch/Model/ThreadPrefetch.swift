// Thread prefetch — the "instant open" machinery.
//
// The thread viewer used to cold-start on every open: fetch the thread, build
// the document, then let images trickle in. This lets the inbox warm all of it
// BEFORE the click:
//   - prefetch(id): fetch + LRU-cache the ClientThreadView.
//   - cached(id):   a fresh cached view (or nil) — the viewer renders it
//                   synchronously on mount, no loading flash, no refetch.
//
// The WKWebView handles image loading itself (and its shared URL cache means a
// warmed thread's images are usually already resident), so unlike the Tauri
// build there is no separate byte-pinning cache to maintain — the flicker that
// forced one came from srcdoc rebuilds, which don't happen here.
//
// Also holds the measured-height memory, so reopening a message renders at its
// final size on the first paint.

import Foundation

@MainActor
final class ThreadPrefetch {
    static let shared = ThreadPrefetch()

    /// Sized to hold the whole For-your-eyes list (the sitrep preloads every
    /// standing item) plus inbox hover-warms without LRU churn.
    private static let cacheMax = 60
    /// A cached view older than this refetches on real open (mail can change).
    private static let freshDefault: TimeInterval = 60

    private struct Entry {
        var view: ClientThreadView
        var ts: Date
        var fresh: TimeInterval
    }

    private var cache: [String: Entry] = [:]
    /// Insertion order for LRU eviction (most-recent last).
    private var order: [String] = []
    private var inflight: Set<String> = []

    private init() {}

    private func put(_ threadId: String, _ view: ClientThreadView, fresh: TimeInterval) {
        cache[threadId] = Entry(view: view, ts: Date(), fresh: fresh)
        order.removeAll { $0 == threadId }
        order.append(threadId)
        while order.count > Self.cacheMax {
            let oldest = order.removeFirst()
            cache.removeValue(forKey: oldest)
        }
    }

    /// Fire-and-forget: fetch + cache. Deduped while in flight; a fresh cache
    /// hit is a no-op.
    ///
    /// `fresh` is a per-entry TTL: right-rail records (banking/receipts) stay
    /// valid as long as their column shows them, so their cached threads
    /// outlive the 60s default. A repeat prefetch may EXTEND a TTL, never
    /// shorten it.
    func prefetch(_ threadId: String, fresh: TimeInterval? = nil) {
        guard !threadId.isEmpty else { return }
        let ttl = fresh ?? Self.freshDefault
        if var hit = cache[threadId], Date().timeIntervalSince(hit.ts) < max(hit.fresh, ttl) {
            if ttl > hit.fresh {
                hit.fresh = ttl
                cache[threadId] = hit
            }
            return
        }
        guard !inflight.contains(threadId) else { return }
        inflight.insert(threadId)
        Task { [weak self] in
            defer { self?.inflight.remove(threadId) }
            // Prefetch is best-effort; the real open surfaces any error.
            guard let view = try? await APIClient.shared.getThread(threadId) else { return }
            self?.put(threadId, view, fresh: ttl)
        }
    }

    /// A fresh cached view for instant render, or nil.
    func cached(_ threadId: String) -> ClientThreadView? {
        guard let hit = cache[threadId], Date().timeIntervalSince(hit.ts) < hit.fresh else {
            return nil
        }
        return hit.view
    }

    /// Fetch THROUGH the cache, returning the view: a fresh hit resolves
    /// immediately. Used by newsletter hero thumbnails, which need the html.
    func fetch(_ threadId: String, fresh: TimeInterval? = nil) async throws -> ClientThreadView {
        let ttl = fresh ?? Self.freshDefault
        if let hit = cache[threadId], Date().timeIntervalSince(hit.ts) < max(hit.fresh, ttl) {
            return hit.view
        }
        let view = try await APIClient.shared.getThread(threadId)
        put(threadId, view, fresh: ttl)
        return view
    }

    /// Let the viewer's own (authoritative) fetch feed the cache — the next
    /// reopen of the same thread is then instant too.
    func note(_ threadId: String, _ view: ClientThreadView) {
        put(threadId, view, fresh: Self.freshDefault)
    }

    /// Warm a staggered batch so a fresh list never stampedes the daemon.
    /// `immediate` rows go now; the rest trickle at `spacing`.
    func warm(_ threadIds: [String], immediate: Int = 5, spacing: Duration = .milliseconds(120)) {
        for id in threadIds.prefix(immediate) { prefetch(id) }
        let rest = Array(threadIds.dropFirst(immediate))
        guard !rest.isEmpty else { return }
        Task { [weak self] in
            for id in rest {
                try? await Task.sleep(for: spacing)
                if Task.isCancelled { return }
                self?.prefetch(id)
            }
        }
    }
}

/// Remembered rendered heights for email bodies, keyed by message id. A frame
/// with a remembered height renders at its final size instantly — zero resize
/// on reopen, which is what the reader perceives as flicker.
@MainActor
final class FrameHeights {
    static let shared = FrameHeights()
    private var heights: [String: CGFloat] = [:]
    private init() {}

    func get(_ key: String) -> CGFloat? { heights[key] }
    func set(_ key: String, _ height: CGFloat) { heights[key] = height }
    func clear(_ key: String) { heights.removeValue(forKey: key) }
}
