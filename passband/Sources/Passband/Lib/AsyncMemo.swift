// The two caching primitives every table in this app was hand-rolling.
//
// LRUMap is a bounded dictionary that evicts the least-recently-used entry
// instead of flushing (dropping EVERYTHING at the cap throws away the hot
// entries along with the cold ones) and instead of paying an
// O(n) array scan per touch — recency is an intrusive linked list threaded
// through the keys, so get/set/evict are all O(1) on the render path.
//
// AsyncMemo is "resolve this key once": a synchronous peek for a view rebuilt
// mid-flight, a parked task so two callers asking at once share one fetch, and
// the same LRU bound underneath. `keep` decides whether a FAILURE is a cached
// verdict or a retry — the sites differ, deliberately, and that difference is
// the whole reason the parameter exists.

import Foundation

/// A dictionary with a ceiling, oldest-touched out first. Not thread-safe and
/// deliberately a value type: callers already own their isolation (a @MainActor
/// class, a lock, `nonisolated(unsafe)` under one), so this adds none of its own.
struct LRUMap<Key: Hashable, Value> {
    /// The recency list lives IN the entries: `newer`/`older` are keys, not
    /// indices, so nothing shifts when an entry in the middle is touched.
    private struct Node {
        var value: Value
        var newer: Key?
        var older: Key?
    }

    private var nodes: [Key: Node] = [:]
    private var newest: Key?
    private var oldest: Key?

    /// Entry ceiling. At least 1 — a zero-capacity cache is a bug, not a config.
    let limit: Int

    init(limit: Int) { self.limit = max(1, limit) }

    var count: Int { nodes.count }

    /// Read AND mark most-recently-used. `mutating` for that reason alone.
    mutating func get(_ key: Key) -> Value? {
        guard let value = nodes[key]?.value else { return nil }
        promote(key)
        return value
    }

    /// Read without touching recency — for bookkeeping that asks "is this still
    /// here?" and must not keep a dead entry alive by asking.
    func peek(_ key: Key) -> Value? { nodes[key]?.value }

    /// Insert or update, evicting the least-recently-used entry if that pushed
    /// the table over its limit. Returns the evicted key so a caller holding a
    /// SIDE table keyed the same way can drop its row in the same breath.
    @discardableResult
    mutating func set(_ key: Key, _ value: Value) -> Key? {
        if nodes[key] != nil {
            nodes[key]?.value = value
            promote(key)
            return nil
        }
        nodes[key] = Node(value: value, newer: nil, older: newest)
        if let head = newest { nodes[head]?.newer = key }
        newest = key
        if oldest == nil { oldest = key }
        guard nodes.count > limit, let evicted = oldest else { return nil }
        removeValue(forKey: evicted)
        return evicted
    }

    @discardableResult
    mutating func removeValue(forKey key: Key) -> Value? {
        guard let node = nodes[key] else { return nil }
        unlink(key, node)
        nodes.removeValue(forKey: key)
        return node.value
    }

    mutating func removeAll() {
        nodes.removeAll(keepingCapacity: true)
        newest = nil
        oldest = nil
    }

    /// Move an entry already in the table to the head of the recency list.
    private mutating func promote(_ key: Key) {
        guard newest != key, let node = nodes[key] else { return }
        unlink(key, node)
        nodes[key]?.newer = nil
        nodes[key]?.older = newest
        if let head = newest { nodes[head]?.newer = key }
        newest = key
        if oldest == nil { oldest = key }
    }

    /// Splice a node's neighbours together. Leaves the node itself in the
    /// dictionary — `promote` relinks it, `removeValue` drops it.
    private mutating func unlink(_ key: Key, _ node: Node) {
        if let newer = node.newer {
            nodes[newer]?.older = node.older
        } else if newest == key {
            newest = node.older
        }
        if let older = node.older {
            nodes[older]?.newer = node.newer
        } else if oldest == key {
            oldest = node.newer
        }
    }
}

/// Carries a main-actor value through `Task`'s Sendable requirement. The task
/// body and every joiner are @MainActor, so the box never actually crosses an
/// isolation boundary — it exists so the memo can hold NSImages and other
/// main-actor-only values without those types claiming to be Sendable.
private final class Boxed<Value>: @unchecked Sendable {
    let value: Value
    init(_ value: Value) { self.value = value }
}

/// Resolve-once-per-key, bounded. A caller either gets the memoized value, joins
/// the resolve already running for that key, or starts the only one.
@MainActor
final class AsyncMemo<Key: Hashable, Value> {
    private var entries: LRUMap<Key, Value>
    /// Parked resolves, so a preload and a card scrolling into view share a fetch.
    private var inFlight: [Key: Task<Boxed<Value>, Never>] = [:]
    /// Which results are worth remembering. The default keeps every verdict —
    /// THAT IS THE NEGATIVE CACHE, and dropping it turns a failed lookup into a
    /// re-fetch on every render. Sites whose failures must stay retryable (a
    /// favicon that was merely offline) pass a predicate that rejects them.
    private let keep: (Value) -> Bool

    init(limit: Int, keep: @escaping (Value) -> Bool = { _ in true }) {
        self.entries = LRUMap(limit: limit)
        self.keep = keep
    }

    /// The memoized value, without starting any work — a view rebuilt mid-flight
    /// reads this in `body` instead of flashing empty while its `.task`
    /// re-confirms what we already hold. nil means "not resolved", which for an
    /// Optional `Value` is distinct from a resolved nil.
    func cached(_ key: Key) -> Value? { entries.get(key) }

    /// Join a resolve already in flight, or nil when none is. For a caller that
    /// owns a GATE the memo doesn't know about: it must still hand back what a
    /// running fetch produces even when it would refuse to start a new one.
    func joinPending(_ key: Key) async -> Value? {
        guard let running = inFlight[key] else { return nil }
        return await running.value.value
    }

    /// Memoized value, joined resolve, or a new one — in that order.
    @discardableResult
    func resolve(_ key: Key, _ make: @escaping () async -> Value) async -> Value {
        if let hit = entries.get(key) { return hit }
        if let running = inFlight[key] { return await running.value.value }

        let task = Task<Boxed<Value>, Never> { [weak self] in
            let value = await make()
            self?.store(key, value)
            return Boxed(value)
        }
        inFlight[key] = task
        return await task.value.value
    }

    /// Record the verdict and retire the parked task, in that order: both run
    /// before any joiner resumes, so nobody sees a settled key still in flight.
    private func store(_ key: Key, _ value: Value) {
        if keep(value) { entries.set(key, value) }
        inFlight[key] = nil
    }
}
