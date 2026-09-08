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

    /// The words last handed to the model, first question or narrowing. A
    /// refinement that repeats them is not a narrowing at all: §6.2 defines one
    /// as "each settled query after the first" being the reader NARROWING, and
    /// the same words are the same need at the same width.
    ///
    /// This exists because the panel refetches for things that are not the
    /// reader typing. Flipping the sort re-runs the search with the identical
    /// words, and so does retrying a failed one; without this, each of those
    /// spent a turn out of the eight, a request on the reader's key, and one of
    /// the narrowings before the conversation is torn down, to tell the model
    /// "the reader refined the search to" exactly what it was already reading.
    private(set) var lastDelivered: String?

    /// How many narrowings one conversation absorbs before it is started
    /// fresh. Twelve is a lot of typing about one need; the thirteenth is a
    /// reader who has changed the subject, and the history is by then costing
    /// tokens to carry a conversation about something else.
    static var resetLimit: Int { 12 }

    var isPending: Bool { pending != nil }

    /// What the caller should do with a refinement it has just been handed.
    enum Outcome: Equatable, Sendable {
        /// Held for the next boundary, replacing whatever was waiting.
        case queued
        /// The same words the model already has. Nothing is queued, nothing is
        /// counted, and the caller does nothing at all.
        case duplicate
        /// Past the limit: tear the conversation down and start it fresh on
        /// this text. The slot is empty afterwards — a restart carries its own
        /// words, so leaving a copy pending would deliver them twice.
        case restart
    }

    /// Offer one narrowing, keyed by the text the model would be shown. The key
    /// is separate from the value because the value carries the local hits too,
    /// and those move under a query that has not changed a character.
    mutating func offer(_ value: Value, key: String) -> Outcome {
        guard key != lastDelivered else { return .duplicate }
        count += 1
        lastDelivered = key
        if count > Self.resetLimit {
            pending = nil
            count = 0
            return .restart
        }
        pending = value
        return .queued
    }

    /// Record words the caller has just handed to the model by some other door:
    /// the FIRST question of a conversation, and the restart that follows the
    /// twelfth narrowing. Both are text the model now has, so both have to move
    /// `lastDelivered` or the very next repeat of them reads as a narrowing.
    mutating func markDelivered(_ key: String) {
        lastDelivered = key
    }

    /// Take the pending refinement, if there is one, and empty the slot.
    mutating func take() -> Value? {
        defer { pending = nil }
        return pending
    }

    /// PUT ONE BACK, because taking it is not the same as the model getting it.
    /// The lane takes a refinement at a tool-result boundary and writes it into
    /// the wire history; if the turn after that fails, the history rolls back
    /// past the very message carrying it, and those words reached nobody. The
    /// slot is where they wait to be asked again.
    ///
    /// A NEWER PENDING ONE WINS, which is the coalescing rule doing its usual
    /// job rather than an edge case: if the reader narrowed again while the
    /// doomed turn was in flight, what is in the slot is what they are still
    /// asking, and the words coming back are the question they moved on from.
    ///
    /// Neither the count nor `lastDelivered` moves. This is not a new
    /// narrowing, and the words have to stay deliverable even though they were
    /// offered once already: `lastDelivered` records what the MODEL has, and
    /// after a rollback the model has nothing.
    mutating func putBack(_ value: Value) {
        guard pending == nil else { return }
        pending = value
    }

    /// Back to a conversation that has never been narrowed. Called wherever the
    /// lane itself is torn down (a new search, a seeded one, an account
    /// switch), so the counter never outlives the conversation it counts.
    mutating func reset() {
        pending = nil
        count = 0
        // AND THE MEMORY OF WHAT WAS SAID. A fresh conversation has heard
        // nothing, so the first question it is given must reach it even when it
        // happens to be the words the last one ended on.
        lastDelivered = nil
    }
}
