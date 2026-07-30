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
// Caching the JSON is only half of an instant open, though: every body still
// had to be tracker-stripped, de-duped and link-scanned before it could be
// handed to a frame, and that ran on the main actor at open time. So a cached
// thread ALSO warms its prepared bodies (PreparedBodies) off the main actor —
// see warmBodies.
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
    /// threadId -> the warmer's repeated-image map. Rides the same LRU as the
    /// views it was derived from, because it is only meaningful next to one.
    private var repeated: [String: [Int: Set<String>]] = [:]

    private init() {}

    private func put(_ threadId: String, _ view: ClientThreadView, fresh: TimeInterval) {
        cache[threadId] = Entry(view: view, ts: Date(), fresh: fresh)
        order.removeAll { $0 == threadId }
        order.append(threadId)
        while order.count > Self.cacheMax {
            let oldest = order.removeFirst()
            cache.removeValue(forKey: oldest)
            repeated.removeValue(forKey: oldest)
        }
        warmBodies(threadId, view)
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

    // MARK: - prepared bodies

    /// The warmer's repeated-image map for a thread, or nil if it has not
    /// finished (or the thread was evicted). nil is a normal answer — the
    /// viewer computes its own rather than waiting on us.
    func cachedRepeatedImages(_ threadId: String) -> [Int: Set<String>]? { repeated[threadId] }

    /// Preprocess every body in a freshly cached thread OFF the main actor.
    ///
    /// These are the exact scans EmailWebView used to run in a `.task` at open
    /// time, per message, on the main actor — so opening a thread paid for a
    /// full regex walk of every body in it PLUS a runloop beat before the first
    /// frame had any html to load. Running them here means the cache that
    /// already makes the open network-free makes it scan-free too.
    ///
    /// Fire-and-forget and idempotent: a key already in PreparedBodies is
    /// skipped, so re-prefetching a thread costs one dictionary probe per
    /// message. Everything crossing out is a value type (ClientThreadView,
    /// Prepared) and Trackers/ImageRepeats are pure, so nothing here needs the
    /// main actor except the hand-back at the end.
    private func warmBodies(_ threadId: String, _ view: ClientThreadView) {
        Task.detached(priority: .utility) {
            let map = Self.repeatedImages(in: view)
            for message in view.messages {
                guard let html = message.html, !html.isEmpty else { continue }
                let seenEarlier = map[message.id] ?? []
                let key = EmailWebView.Prepared.cacheKey(html, seenEarlier)
                guard PreparedBodies.shared.get(key) == nil else { continue }
                PreparedBodies.shared.set(
                    key, EmailWebView.Prepared.make(from: html, seenEarlier: seenEarlier))
            }
            await MainActor.run { ThreadPrefetch.shared.noteRepeated(threadId, map) }
        }
    }

    /// Only remember the map while the view it describes is still cached — a
    /// thread evicted mid-scan must not leave its map behind.
    private func noteRepeated(_ threadId: String, _ map: [Int: Set<String>]) {
        guard cache[threadId] != nil else { return }
        repeated[threadId] = map
    }

    /// messageId -> image srcs already shown by an earlier message.
    ///
    /// "EARLIER" MEANS CHRONOLOGICALLY EARLIER, so this walks `view.messages`
    /// (server order, oldest first) and NOT the newest-first order the reader
    /// sees. Walking the display order instead would keep the newest copy of a
    /// signature and suppress the original — hiding the image in the message
    /// that actually introduced it.
    ///
    /// Scans TRACKER-STRIPPED html for the same reason the strip runs first in
    /// EmailWebView: a tracking pixel is removed from every message anyway, so
    /// letting one register as a "first occurrence" here could suppress a real
    /// image that shares its src and leave the thread showing none at all.
    ///
    /// `nonisolated` because the warmer runs it off the main actor, and the
    /// viewer's cold path runs it on.
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
/// html AND the suppression set that produced them, because the same body
/// prepared against a different thread is a different document.
///
/// LOCK-BASED RATHER THAN @MainActor, and that is the entire point of it: the
/// warmer fills it from a detached task while `EmailWebView.init` — which
/// SwiftUI runs nonisolated, before any `.task` or `onAppear` can — reads it to
/// seed its state. An actor-isolated cache could serve neither of those without
/// an await, and an await in a View initializer is not a thing.
final class PreparedBodies: @unchecked Sendable {
    static let shared = PreparedBodies()

    /// Several threads' worth of messages: enough that walking a queue back and
    /// forth never re-scans, bounded because each entry holds a copy of a
    /// message body.
    private static let cap = 300

    private let lock = NSLock()
    private var entries: [Int: EmailWebView.Prepared] = [:]
    /// Insertion order for LRU eviction (most-recent last).
    private var order: [Int] = []

    private init() {}

    func get(_ key: Int) -> EmailWebView.Prepared? {
        lock.lock()
        defer { lock.unlock() }
        return entries[key]
    }

    func set(_ key: Int, _ prepared: EmailWebView.Prepared) {
        lock.lock()
        defer { lock.unlock() }
        if entries.updateValue(prepared, forKey: key) == nil { order.append(key) }
        while order.count > Self.cap {
            entries.removeValue(forKey: order.removeFirst())
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
