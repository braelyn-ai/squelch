// The state machine behind the ONE modal the app cannot be used around. Every
// case here is a way that modal misreports, and the two that matter most are
// opposites: a run that reads FINISHED early uncovers the app mid-rewrite, and
// a run that never reads finished is a window nobody gets back.

import Foundation

@main
@MainActor
struct RetriageRunTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        seeded()
        adopting()
        neverBackwards()
        stalling()
        deadEnds()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    /// The kick has not answered yet. `0 done of 0` is the seeded state and it
    /// must NOT read as a finished run — that is the modal opening and closing
    /// in the same frame, having blocked nothing.
    static func seeded() {
        let run = RetriageRun()
        equal(run.finished, false, "an unanswered run is never finished")
        equal(run.counted, false, "and it knows it has not counted")
        equal(run.canClose, false, "a live run offers no way out")
        equal(run.watching, true, "because it can still end on its own")
        equal(run.fraction, 0, "and its bar is empty")
    }

    static func adopting() {
        var run = RetriageRun(total: 40)
        equal(run.finished, false, "a seeded total is still not a count")

        run.adopt(.init(total: 37, done: 0, started_at: "x"))
        equal(run.total, 37, "the SERVER's total wins over the kick's seed")
        equal(run.counted, true, "a poll that answered is a count")
        equal(run.finished, false, "0 of 37 is not finished")

        run.adopt(.init(total: 37, done: 37, started_at: "x"))
        equal(run.finished, true, "37 of 37 is")
        equal(run.fraction, 1, "and the bar is full")

        // A run the daemon reports as empty — every row sealed or aged out
        // mid-flight — is over, not stuck at zero forever.
        var empty = RetriageRun(total: 5)
        empty.adopt(.init(total: 0, done: 0, started_at: nil))
        equal(empty.finished, true, "an emptied run finishes rather than hanging")
    }

    /// `done` genuinely can fall on the wire: new mail joins the queues mid-run,
    /// and a row can be re-stamped. A counter that walks backwards inside a modal
    /// nobody can dismiss reads as a hang.
    static func neverBackwards() {
        var run = RetriageRun(total: 40)
        run.adopt(.init(total: 40, done: 30, started_at: "x"))
        run.adopt(.init(total: 40, done: 12, started_at: "x"))
        equal(run.done, 30, "done never retreats")

        // ...but it is still bounded by the total, or the bar overruns.
        run.adopt(.init(total: 20, done: 12, started_at: "x"))
        equal(run.done, 20, "and never exceeds the total it is counting toward")
        equal(run.finished, true, "which is what lets a shrunken run finish")
    }

    /// The escape hatch. It must not open on a healthy run, must open on a dead
    /// one, and must CLOSE AGAIN if the run wakes up — a door that sticks open
    /// invites closing a run that was only slow.
    static func stalling() {
        let t0 = Date(timeIntervalSince1970: 1_000_000)
        var run = RetriageRun(total: 100)

        run.adopt(.init(total: 100, done: 10, started_at: "x"), at: t0)
        equal(run.stalled, false, "progress is not a stall")

        run.adopt(.init(total: 100, done: 10, started_at: "x"), at: t0.addingTimeInterval(60))
        equal(run.stalled, false, "a minute of quiet is a slow cycle, not a stall")
        equal(run.canClose, false, "so no door yet")

        run.adopt(.init(total: 100, done: 10, started_at: "x"), at: t0.addingTimeInterval(95))
        equal(run.stalled, true, "past the window it is a stall")
        equal(run.canClose, true, "and the door opens")
        equal(run.watching, true, "while the poll keeps running")

        run.adopt(.init(total: 100, done: 20, started_at: "x"), at: t0.addingTimeInterval(100))
        equal(run.stalled, false, "a run that wakes up is not stalled")
        equal(run.canClose, false, "and the door closes behind it")
    }

    /// The two ends the daemon cannot describe. Both stop the wait, and both
    /// MUST offer the way out: neither can ever reach `finished` on its own.
    static func deadEnds() {
        var old = RetriageRun(total: 12)
        old.unsupported = true
        equal(old.watching, false, "a daemon with no route cannot be watched")
        equal(old.canClose, true, "so the modal must be closable")
        equal(old.finished, false, "and it never claims to have finished")

        var lost = RetriageRun(total: 12)
        lost.failure = "lost the daemon"
        equal(lost.watching, false, "nor can a dead connection")
        equal(lost.canClose, true, "same door")
        equal(lost.finished, false, "same refusal to claim success")
    }

    // MARK: - assertions

    static func equal<T: Equatable>(
        _ got: T, _ want: T, _ label: String, line: Int = #line
    ) {
        checks += 1
        if got != want {
            failures += 1
            print("FAIL (line \(line)): \(label)\n  want: \(want)\n   got: \(got)")
        }
    }
}
