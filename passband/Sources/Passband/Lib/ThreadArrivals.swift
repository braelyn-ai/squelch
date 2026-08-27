// WHAT TO DO ABOUT MAIL THAT LANDS IN THE THREAD SOMEBODY IS READING.
//
// A reply to the email on screen is heard about TWICE: the live event feed
// carries it the moment triage emits it, and the 10s sitrep poll finds the same
// row a beat later (and is the only one that finds it at all if the feed was
// down, or if the daemon judged the message unworthy of an event but the bands
// still moved). Two reporters, one arrival — so this ledger sits between them
// and the reader.
//
// The two things that happen next have DIFFERENT rules, which is the whole
// reason this is a type rather than a boolean:
//
//   REFETCH is idempotent and self-healing. It repeats on every report of a
//   message the reader's copy predates, so a fetch that failed is simply asked
//   again by the next poll — and the asking stops on its own the moment the
//   viewer adopts a copy that holds it, with nothing to reset.
//
//   ANNOUNCE is once, ever. The feed and the poll both see the same email, and
//   a human told twice about one message would sooner be told nothing.
//
// Pure and Foundation-only: no store, no clock, no network. The policy is
// asserted in ThreadArrivalsTests rather than reasoned about.

import Foundation

/// The outcome of offering one arrival to the thread on screen.
struct Admission: Equatable, Sendable {
    /// The reader holds a copy of the thread that predates this message, so it
    /// is worth going and getting a newer one.
    var refetch: Bool
    /// FIRST report of this message: the one moment there is news to tell.
    /// Never true without `refetch` — there is nothing to say about mail the
    /// reader is already looking at.
    var announce: Bool

    static let ignore = Admission(refetch: false, announce: false)
}

/// The per-thread ledger of what the human has already been told about.
struct ThreadArrivals: Equatable, Sendable {
    /// The thread the ledger is FOR. Mail for any other thread is not this
    /// type's business: it is not on screen, the bands already list it, and the
    /// notification feed has already posted its banner.
    private(set) var threadId: String?
    private var announced: Set<Int> = []

    /// Point the ledger at the thread now on screen.
    ///
    /// A REOPEN of the same thread keeps what it holds, and deliberately: the
    /// viewer stays mounted across one and may never re-adopt, so forgetting
    /// here would re-announce mail that is already sitting in the stack.
    mutating func reset(to threadId: String?) {
        guard threadId != self.threadId else { return }
        self.threadId = threadId
        announced.removeAll()
    }

    /// Offer one arrival.
    ///
    /// `held` is the newest message id THE READER HAS — nil while nothing has
    /// landed in the viewer yet, and that is not a gap to fill: the load
    /// already in flight is fetching a copy that holds this message, and a
    /// refetch racing it would be a second answer to a question nobody asked
    /// twice.
    ///
    /// Ids are the daemon's own row ids, so newer really is greater; a report
    /// at or below what the reader holds is the poll rediscovering mail that
    /// is already on screen, which is most of what it reports.
    mutating func admit(thread: String, message: Int, held: Int?) -> Admission {
        guard thread == threadId, let held, message > held else { return .ignore }
        // The set is written AFTER the id check and never before, which is what
        // keeps it small: an id the reader has since adopted fails above and
        // never reaches this line, so what accumulates is only ever the handful
        // of messages genuinely in flight.
        return Admission(refetch: true, announce: announced.insert(message).inserted)
    }
}
