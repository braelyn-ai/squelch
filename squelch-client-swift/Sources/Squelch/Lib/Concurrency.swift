// Structured-concurrency plumbing shared by the caches and the resident loops.

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

// MARK: - resident loops

/// The lifecycle every resident loop in this client shares: ONE long-lived
/// task, started idempotently, cancelled on stop.
///
/// Holds the task rather than a running flag, because a flag cannot cancel: the
/// handle is both the "already running" answer and the only way to stop what a
/// previous `start()` began.
@MainActor
final class ResidentTask {
    private var task: Task<Void, Never>?

    /// True between `start()` and `stop()`. For an owner whose start does more
    /// than the loop — registering an observer, say — and must not do it twice.
    var isRunning: Bool { task != nil }

    /// Run `body` until it returns or is cancelled. A second call while one is
    /// running is a no-op, never a second loop racing the first.
    func start(_ body: @escaping @MainActor () async -> Void) {
        guard task == nil else { return }
        task = Task { await body() }
    }

    func stop() {
        task?.cancel()
        task = nil
    }
}

/// Jittered exponential backoff for a resident loop retrying a server it cannot
/// fix. A VALUE type: each loop owns its own schedule, so one loop's recovery
/// can never shorten another's window.
struct Backoff {
    private let base: TimeInterval
    private let cap: TimeInterval
    private var window: TimeInterval

    init(base: TimeInterval, cap: TimeInterval) {
        self.base = base
        self.cap = cap
        self.window = base
    }

    /// Back to the base window. Call on success — sustained failure is what
    /// widens the gap, not the mere number of attempts.
    mutating func reset() { window = base }

    /// Sleep the current window, then widen it toward the cap. Full jitter over
    /// the lower half of the window: jitter is not decoration — a daemon restart
    /// drops every client at once, and identical backoffs mean they all come
    /// back in the same millisecond, forever.
    ///
    /// Cancellation is swallowed rather than thrown: the caller's own
    /// `Task.isCancelled` check ends the loop one line later.
    mutating func sleep() async {
        let delay = TimeInterval.random(in: (window / 2)...window)
        try? await Task.sleep(for: .seconds(delay))
        window = min(window * 2, cap)
    }
}
