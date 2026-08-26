// A dev re-triage as the CLIENT sees it: a total, a count, and the three ways
// the wait can end. The daemon's own view of the same run is
// `GET /client/retriage` (see `RetriageProgress`); this is what the blocking
// modal renders, so every state it can be in needs copy, including the two the
// daemon cannot describe — a route it is too old to serve, and a route that
// stopped answering.

import Foundation

struct RetriageRun: Sendable, Equatable {
    /// Rows in the run. Seeded from the kick's own `reset` so the modal opens
    /// with a real number, then replaced by the server's live total.
    var total = 0
    var done = 0
    /// True once a progress poll has answered. Until it does, `done` is a
    /// guess of zero rather than a measurement — the difference matters,
    /// because 0 of 40 and "not counting yet" look identical otherwise.
    var counted = false
    /// The daemon has no `GET /client/retriage`. The re-triage itself is still
    /// running — the reset landed — so this is "we cannot watch it", not
    /// "it failed", and the modal says exactly that.
    var unsupported = false
    /// Polling gave up. Same distinction: the daemon is doing the work or it
    /// is not, and either way we have stopped being able to tell.
    var failure: String?
    /// When `done` last moved, seeded at the kick.
    var lastAdvance = Date()
    /// The counter has not moved in [`stallSeconds`]. NOT an end state — the
    /// poll keeps running and a run that wakes up carries on — it only opens
    /// the way out, because this modal has no other one and a wedged daemon
    /// would otherwise cost the user a force-quit.
    var stalled = false

    /// Sized on what a human will stare at, NOT on the daemon's cadence — which
    /// this client cannot see and which has already moved once (`sync.poll_secs`
    /// went 45 -> 5), so a threshold derived from it would be wrong on exactly
    /// the installs it was tuned against.
    ///
    /// The asymmetry is what makes that safe: firing early only OPENS A DOOR —
    /// nothing closes, nothing stops, and a run that wakes up clears the flag on
    /// its next poll — while firing late is a person stuck behind a dead counter
    /// with force-quit as the only way out. So err short.
    private static let stallSeconds: TimeInterval = 90

    /// Adopt a poll. The SERVER's total wins over the kick's seed: a second
    /// kick, or a per-message re-triage from the fix palette, is the same run
    /// to the queues and should be the same run here.
    ///
    /// `done` NEVER GOES BACKWARDS on screen. It genuinely can on the wire —
    /// new mail landing mid-run joins the queues, and a row can be re-stamped —
    /// and a counter that walks back reads as a bug in a modal the user is
    /// already trapped behind.
    mutating func adopt(_ p: RetriageProgress, at now: Date = Date()) {
        counted = true
        total = max(p.total, 0)
        let next = min(max(done, p.done), total)
        if next != done { lastAdvance = now }
        done = next
        stalled = now.timeIntervalSince(lastAdvance) > Self.stallSeconds
    }

    /// Nothing left in either queue. Only ever true once a poll has ANSWERED:
    /// the seeded state is `0 done of N`, and an unanswered `0 of 0` must not
    /// read as a finished run.
    var finished: Bool { counted && done >= total }

    /// Whether the wait can still end on its own. False parks the modal on its
    /// close button.
    var watching: Bool { !unsupported && failure == nil }

    /// Whether to offer the way out. A live, moving run does NOT — that is the
    /// whole point of the modal — but a run nobody can watch or that has stopped
    /// moving must never be a window the user cannot get back.
    var canClose: Bool { !watching || stalled }

    /// 0...1 for the bar, and a full bar only for a run that really finished.
    var fraction: Double {
        guard counted, total > 0 else { return 0 }
        return Double(done) / Double(total)
    }
}
