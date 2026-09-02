// ONE PENDING REFINEMENT, and the rule that a search which has been narrowed
// twelve times is a different search (docs/SEARCH.md §6.2).
//
// The reader keeps typing while the lane works. Every settled query after the
// first is the SAME need, narrower, so the lane refines rather than restarts —
// but it can only be told at a tool-result boundary, and three of those
// narrowings can pile up before one arrives. Only the newest is worth
// delivering: "abstract conf" then "abstract conference wifi" is one
// refinement, the second, and handing the model the abandoned one first would
// spend a turn answering a question nobody is still asking.
//
// A slot rather than a queue is the whole design, and it is a value type with
// no opinion about what it holds so the rule can be asserted with strings in
// test.sh instead of being reasoned about inside a live session.

import Foundation

/// The single-slot coalescing queue, plus the reset counter.
struct RefinementSlot<Value: Sendable>: Sendable {
    /// Refinements since the last reset. Public so the panel can show where a
    /// conversation stands without keeping a second counter that could drift
    /// from this one.
    private(set) var count = 0

    private var pending: Value?

    /// How many narrowings one conversation absorbs before it is started
    /// fresh. Twelve is a lot of typing about one need; past it, the reader has
    /// changed the subject and the history is now costing tokens to carry a
    /// conversation about something else.
    static var resetLimit: Int { 12 }

    var isPending: Bool { pending != nil }

    /// What the caller should do with a refinement it has just been handed.
    enum Outcome: Equatable, Sendable {
        /// Held for the next boundary, replacing whatever was waiting.
        case queued
        /// The limit: tear the conversation down and start it fresh on this
        /// text. The slot is empty afterwards — a restart carries its own
        /// words, so leaving a copy pending would deliver them twice.
        case restart
    }

    mutating func offer(_ value: Value) -> Outcome {
        count += 1
        if count >= Self.resetLimit {
            pending = nil
            count = 0
            return .restart
        }
        pending = value
        return .queued
    }

    /// Take the pending refinement, if there is one, and empty the slot.
    mutating func take() -> Value? {
        defer { pending = nil }
        return pending
    }

    /// Back to a conversation that has never been narrowed. Called wherever the
    /// lane itself is torn down (a new search, a seeded one, an account
    /// switch), so the counter never outlives the conversation it counts.
    mutating func reset() {
        pending = nil
        count = 0
    }
}
