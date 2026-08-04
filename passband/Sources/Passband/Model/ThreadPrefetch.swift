// Thread prefetch — the "instant open" machinery. `prefetch(id)` fetches and
// LRU-caches a ClientThreadView; `cached(id)` hands the viewer a fresh one to
// render synchronously on mount, with no loading flash or refetch. A cached
// thread also warms its PREPARED bodies (strip / dedupe / link scan) off the
// main actor, so the open is scan-free as well as network-free. Also holds the
// measured-height memory, so a reopened message paints at its final size.

import Foundation

@MainActor
final class ThreadPrefetch {
    static let shared = ThreadPrefetch()

    /// Sized to hold the whole For-your-eyes list (the sitrep preloads every
    /// standing item) plus inbox hover-warms, without LRU churn.
    private static let cacheMax = 60
    /// A cached view older than this refetches on real open — mail can change.
    private static let freshDefault: TimeInterval = 60

    private struct Entry {
        var view: ClientThreadView
        var ts: Date
        var fresh: TimeInterval
    }

    private var cache = LRUMap<String, Entry>(limit: cacheMax)
    /// Ids being fetched. A Set rather than parked tasks because prefetch is
    /// fire-and-forget: nobody joins a running fetch, they take the cache hit
    /// it leaves behind (which is why this is not an `AsyncMemo` — a hit here
    /// is a hit only while it is FRESH, and freshness is not memoizable).
    private var inflight: Set<String> = []
    /// threadId -> the warmer's repeated-image map. Rides the same LRU as the
    /// view it was derived from, being meaningful only beside one.
    private var repeated: [String: [Int: Set<String>]] = [:]

    private init() {}

    private func put(_ threadId: String, _ view: ClientThreadView, fresh: TimeInterval) {
        // A view leaving the cache takes its repeated-image map with it — the
        // map is meaningful only beside the view it was derived from.
        if let evicted = cache.set(threadId, Entry(view: view, ts: Date(), fresh: fresh)) {
            repeated.removeValue(forKey: evicted)
        }
        warmBodies(threadId, view)
    }

    /// Fire-and-forget fetch + cache. Deduped while in flight; a fresh hit is a
    /// no-op. `fresh` is a per-entry TTL (right-rail records outlive the 60s
    /// default), and a repeat prefetch may EXTEND a TTL, never shorten it.
    func prefetch(_ threadId: String, fresh: TimeInterval? = nil) {
        guard !threadId.isEmpty else { return }
        let ttl = fresh ?? Self.freshDefault
        if var hit = cache.get(threadId), Date().timeIntervalSince(hit.ts) < max(hit.fresh, ttl) {
            if ttl > hit.fresh {
                hit.fresh = ttl
                cache.set(threadId, hit)
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
        guard let hit = cache.get(threadId), Date().timeIntervalSince(hit.ts) < hit.fresh else {
            return nil
        }
        return hit.view
    }

    /// Fetch THROUGH the cache: a fresh hit resolves immediately.
    func fetch(_ threadId: String, fresh: TimeInterval? = nil) async throws -> ClientThreadView {
        let ttl = fresh ?? Self.freshDefault
        if let hit = cache.get(threadId), Date().timeIntervalSince(hit.ts) < max(hit.fresh, ttl) {
            return hit.view
        }
        let view = try await APIClient.shared.getThread(threadId)
        put(threadId, view, fresh: ttl)
        return view
    }

    /// Let the viewer's own (authoritative) fetch feed the cache, so the next
    /// reopen is instant too.
    func note(_ threadId: String, _ view: ClientThreadView) {
        put(threadId, view, fresh: Self.freshDefault)
    }

    // MARK: - prepared bodies

    /// The warmer's repeated-image map for a thread. nil is a normal answer
    /// (unfinished or evicted) — the viewer computes its own rather than wait.
    func cachedRepeatedImages(_ threadId: String) -> [Int: Set<String>]? { repeated[threadId] }

    /// Preprocess every body in a freshly cached thread OFF the main actor —
    /// otherwise opening a thread pays for a regex walk of every body plus a
    /// runloop beat before the first frame has html to load.
    ///
    /// Fire-and-forget and idempotent: a key already in PreparedBodies is
    /// skipped. Everything crossing out is a value type and Trackers /
    /// ImageRepeats are pure, so only the hand-back needs the main actor.
    private func warmBodies(_ threadId: String, _ view: ClientThreadView) {
        Task.detached(priority: .utility) {
            let map = Self.repeatedImages(in: view)
            for message in view.messages {
                guard let html = message.html, !html.isEmpty else { continue }
                let seenEarlier = map[message.id] ?? []
                // The tracker policy is part of the prepared identity, so the
                // warmer must key on the SAME one the card will render under or
                // every known-sender body misses and re-scans on the main path.
                let allow = message.allowsTrackers
                let key = EmailWebView.Prepared.cacheKey(html, seenEarlier, allow)
                guard PreparedBodies.shared.get(key) == nil else { continue }
                PreparedBodies.shared.set(
                    key,
                    EmailWebView.Prepared.make(
                        from: html, seenEarlier: seenEarlier, allowTrackers: allow))
            }
            await MainActor.run { ThreadPrefetch.shared.noteRepeated(threadId, map) }
        }
    }

    /// Only remember the map while the view it describes is still cached — a
    /// thread evicted mid-scan must not leave its map behind.
    private func noteRepeated(_ threadId: String, _ map: [Int: Set<String>]) {
        // `peek`: a bookkeeping check must not keep a cold entry alive by asking.
        guard cache.peek(threadId) != nil else { return }
        repeated[threadId] = map
    }

    /// messageId -> image srcs already shown by a CHRONOLOGICALLY earlier
    /// message, so this walks `view.messages` (server order, oldest first) and
    /// not the newest-first order the reader sees — display order would suppress
    /// the copy in the message that introduced the image.
    ///
    /// Scans TRACKER-STRIPPED html UNCONDITIONALLY, known sender or not: letting
    /// a pixel register as a "first occurrence" could suppress a real image
    /// sharing its src, and a per-message open is exactly what an allowed
    /// tracker is for — deduping one away would under-report it. `nonisolated`
    /// because the warmer runs it off the main actor and the cold path on it.
    nonisolated static func repeatedImages(in view: ClientThreadView) -> [Int: Set<String>] {
        var seen = Set<String>()
        var out: [Int: Set<String>] = [:]
        for message in view.messages {
            out[message.id] = seen
            guard let html = message.html, !html.isEmpty else { continue }
            seen.formUnion(ImageRepeats.sources(Trackers.strip(html).html))
        }
        return out
    }

    // MARK: - batch warming

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

/// Preprocessed email bodies, keyed by `EmailWebView.Prepared.cacheKey` — the
/// html AND the suppression set, since the same body prepared against a
/// different thread is a different document.
///
/// LOCK-BASED RATHER THAN @MainActor, deliberately: the warmer fills it from a
/// detached task while `EmailWebView.init` — which SwiftUI runs nonisolated,
/// before any `.task` or `onAppear` — reads it to seed state, and neither can
/// await.
final class PreparedBodies: @unchecked Sendable {
    static let shared = PreparedBodies()

    /// Several threads' worth of messages — enough that walking a queue back
    /// and forth never re-scans, bounded because each entry holds a body copy.
    private static let cap = 300

    private let lock = NSLock()
    /// The LRU is not thread-safe on its own; the lock that publishes these
    /// entries across isolation domains is what makes touching it safe.
    private var entries = LRUMap<Int, EmailWebView.Prepared>(limit: cap)

    private init() {}

    func get(_ key: Int) -> EmailWebView.Prepared? {
        lock.lock()
        defer { lock.unlock() }
        return entries.get(key)
    }

    func set(_ key: Int, _ prepared: EmailWebView.Prepared) {
        lock.lock()
        defer { lock.unlock() }
        entries.set(key, prepared)
    }
}

/// Remembered rendered heights, keyed by message id: a frame with one renders
/// at its final size instantly, with no resize on reopen (the reader perceives
/// that resize as flicker).
@MainActor
final class FrameHeights {
    static let shared = FrameHeights()
    private var heights: [String: CGFloat] = [:]
    private init() {}

    func get(_ key: String) -> CGFloat? { heights[key] }
    func set(_ key: String, _ height: CGFloat) { heights[key] = height }
    func clear(_ key: String) { heights.removeValue(forKey: key) }
}
