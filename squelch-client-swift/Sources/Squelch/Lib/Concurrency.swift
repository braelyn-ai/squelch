// Structured-concurrency plumbing shared by the caches.

import Foundation

/// Run `body` over every item, never more than `width` at a time: top the group
/// up to the width, wait for one to finish, top it up again. A plain
/// `for … { group.addTask }` would enqueue all of them at once, and these items
/// are network-bound work that ends in a decode — a wide fan-out queues CPU
/// behind the scroll it is meant to smooth.
func withBoundedTaskGroup<T: Sendable>(
    width: Int, over items: [T], _ body: @escaping @Sendable (T) async -> Void
) async {
    await withTaskGroup(of: Void.self) { group in
        var next = 0
        var running = 0
        while next < items.count || running > 0 {
            while running < width, next < items.count {
                let item = items[next]
                next += 1
                running += 1
                group.addTask { await body(item) }
            }
            await group.next()
            running -= 1
        }
    }
}
