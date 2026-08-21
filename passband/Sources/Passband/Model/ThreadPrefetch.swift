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
    /// Bumped by `wipe()`. Every fetch this class starts captures it and files
    /// nothing once it no longer matches: a thread fetched for the old account
    /// must not land in the new account's cache, and the trickle of a batch
    /// warm must not keep asking the NEW daemon for the OLD one's thread ids.
    private var generation = 0

    private init() {}

    private func put(
        _ threadId: String, _ view: ClientThreadView, fresh: TimeInterval, gen: Int
    ) {
        guard gen == generation else { return }
        _ = cache.set(threadId, Entry(view: view, ts: Date(), fresh: fresh))
        warmBodies(view)
    }

    /// Drop everything, cache and in-flight bookkeeping alike. An account
    /// switch: thread ids, the message ids inside the views, and the
    /// repeated-image maps keyed off both all belong to one daemon.
    func wipe() {
        generation &+= 1
        cache.removeAll()
        inflight.removeAll()
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
        let gen = generation
        Task { [weak self] in
            defer { self?.settled(threadId, gen: gen) }
            // Prefetch is best-effort; the real open surfaces any error.
            guard let view = try? await APIClient.shared.getThread(threadId) else { return }
            self?.put(threadId, view, fresh: ttl, gen: gen)
        }
    }

    /// Retire an in-flight marker — unless a `wipe()` has emptied the table
    /// since, in which case the marker under this id is a NEWER fetch's and
    /// clearing it would let a third copy start.
    private func settled(_ threadId: String, gen: Int) {
        guard gen == generation else { return }
        inflight.remove(threadId)
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
        let gen = generation
        let view = try await APIClient.shared.getThread(threadId)
        put(threadId, view, fresh: ttl, gen: gen)
        return view
    }

    /// Let the viewer's own (authoritative) fetch feed the cache, so the next
    /// reopen is instant too. Synchronous, so the live generation is by
    /// definition the one this view was fetched under.
    func note(_ threadId: String, _ view: ClientThreadView) {
        put(threadId, view, fresh: Self.freshDefault, gen: generation)
    }

    // MARK: - prepared bodies

    /// Preprocess every body in a freshly cached thread OFF the main actor —
    /// otherwise opening a thread pays for a regex walk of every body plus a
    /// runloop beat before the first frame has html to load.
    ///
    /// Fire-and-forget and idempotent: a key already in PreparedBodies is
    /// skipped. Everything crossing out is a value type and Trackers /
    /// ImageRepeats are pure, so only the hand-back needs the main actor.
    private func warmBodies(_ view: ClientThreadView) {
        Task.detached(priority: .utility) {
            for message in view.messages {
                guard let html = message.html, !html.isEmpty else { continue }
                // The tracker policy is part of the prepared identity, so the
                // warmer must key on the SAME one the card will render under or
                // every known-sender body misses and re-scans on the main path.
                let allow = message.allowsTrackers
                // The PARTS are part of that identity too — the cid rewrite
                // resolves against them — so they are passed for the same
                // reason: warm under a different key and every body with an
                // attachment misses and re-scans on the main path.
                let parts = message.attachmentList
                let key = EmailWebView.Prepared.cacheKey(html, allow, parts)
                guard PreparedBodies.shared.get(key) == nil else { continue }
                PreparedBodies.shared.set(
                    key,
                    EmailWebView.Prepared.make(
                        from: html, allowTrackers: allow, attachments: parts))
            }
        }
    }

    // MARK: - batch warming

    /// Warm a staggered batch so a fresh list never stampedes the daemon.
    /// `immediate` rows go now; the rest trickle at `spacing`.
    func warm(_ threadIds: [String], immediate: Int = 5, spacing: Duration = .milliseconds(120)) {
        for id in threadIds.prefix(immediate) { prefetch(id) }
        let rest = Array(threadIds.dropFirst(immediate))
        guard !rest.isEmpty else { return }
        let gen = generation
        Task { [weak self] in
            for id in rest {
                try? await Task.sleep(for: spacing)
                if Task.isCancelled { return }
                // The list belongs to ONE account. A switch mid-trickle must
                // abandon the rest of it rather than ask the new daemon for
                // thread ids it has never heard of.
                guard let self, gen == self.generation else { return }
                self.prefetch(id)
            }
        }
    }
}

/// Preprocessed email bodies, keyed by `EmailWebView.Prepared.cacheKey` — the
/// html and tracker policy.
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
///
/// It also keeps the GUESSED height of a body nothing has measured yet — the
/// size a message is given while it is scrolling into view for the first time.
/// The two are deliberately separate maps: a measurement is what the document
/// turned out to be, a guess is what its text says it will be, and a guess must
/// never be remembered as though a frame had reported it.
@MainActor
final class FrameHeights {
    static let shared = FrameHeights()
    private var heights: [String: CGFloat] = [:]
    private var guesses: [String: CGFloat] = [:]
    private init() {}

    func get(_ key: String) -> CGFloat? { heights[key] }
    func set(_ key: String, _ height: CGFloat) { heights[key] = height }
    func clear(_ key: String) {
        heights.removeValue(forKey: key)
        guesses.removeValue(forKey: key)
    }

    /// The guessed height for this message, computed once and kept. Memoized
    /// because the caller is a view body — it is asked on every render of a
    /// message card, and the answer is a walk of the body's text (the quoted
    /// chain has to be split off it first, or a one-line reply quoting a long
    /// thread is guessed at the length of the whole thread).
    func guess(_ key: String, _ make: () -> CGFloat) -> CGFloat {
        if let known = guesses[key] { return known }
        let made = make()
        guesses[key] = made
        return made
    }

    /// Forget every height. An account switch: the keys are message ids, one
    /// daemon's, so a surviving entry paints the new account's mail at the old
    /// account's size and then snaps — which reads as a rendering glitch.
    func wipeAll() {
        heights.removeAll()
        guesses.removeAll()
    }
}
