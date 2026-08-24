// The two-week ask, and every way it must NOT fire. These exercise the pure
// rule — the lifecycle around it (notifications, UserDefaults, the day counter)
// is out of reach of a headless suite, which is why the rule takes its inputs
// as arguments.
//
// A nudge is only ever wrong later, and it is wrong in a way nobody reports:
// somebody who gets asked on their second afternoon does not file a bug, they
// just think less of the app. So the boundaries are pinned here.

import Foundation

@main
@MainActor
struct ShareNudgeTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        theHappyCase()
        bothHalvesOfTwoWeeks()
        askedOnceEver()
        theDaemonHasAVeto()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    static let now = Date(timeIntervalSince1970: 1_755_600_000)
    static let day: TimeInterval = 24 * 3600

    static func ago(_ days: Double) -> Date { now.addingTimeInterval(-days * day) }

    /// Two weeks in, used on plenty of days, never asked: this is the person
    /// the whole feature exists for.
    static func theHappyCase() {
        expect(
            ShareNudge.earned(
                canShare: true, asked: false, firstUse: ago(14), activeDays: 7, now: now),
            "a fortnight of real use earns the ask")
        expect(
            ShareNudge.earned(
                canShare: true, asked: false, firstUse: ago(400), activeDays: 300, now: now),
            "and it stays earned for as long as it goes unasked")
    }

    /// "Two weeks of usage" is TWO facts. Either one alone is not it.
    static func bothHalvesOfTwoWeeks() {
        // Installed a fortnight ago and opened twice: a fortnight, not two
        // weeks of usage.
        expect(
            !ShareNudge.earned(
                canShare: true, asked: false, firstUse: ago(30), activeDays: 2, now: now),
            "elapsed time alone is not usage")

        // Used hard, but only since Tuesday. Nobody has an opinion yet.
        expect(
            !ShareNudge.earned(
                canShare: true, asked: false, firstUse: ago(5), activeDays: 5, now: now),
            "usage alone is not two weeks")

        // The boundaries themselves, both inclusive.
        expect(
            !ShareNudge.earned(
                canShare: true, asked: false, firstUse: ago(13), activeDays: 99, now: now),
            "one day short is short")
        expect(
            ShareNudge.earned(
                canShare: true, asked: false, firstUse: ago(14), activeDays: 7, now: now),
            "the fourteenth day counts")
        expect(
            !ShareNudge.earned(
                canShare: true, asked: false, firstUse: ago(99), activeDays: 6, now: now),
            "one active day short is short")

        // No first-launch stamp at all reads as "not yet", never as "forever
        // ago": the conservative direction.
        expect(
            !ShareNudge.earned(
                canShare: true, asked: false, firstUse: nil, activeDays: 99, now: now),
            "no first-use stamp never earns an ask")
    }

    /// The rule this file exists for. Once asked, never again, however long
    /// they go on using it.
    static func askedOnceEver() {
        expect(
            !ShareNudge.earned(
                canShare: true, asked: true, firstUse: ago(365), activeDays: 300, now: now),
            "asked once is asked forever")
    }

    /// A daemon that cannot mint invites is never nudged toward a button whose
    /// only outcome is a refusal.
    static func theDaemonHasAVeto() {
        expect(
            !ShareNudge.earned(
                canShare: false, asked: false, firstUse: ago(365), activeDays: 300, now: now),
            "a daemon that cannot share is never nudged")
    }

    static func expect(_ cond: Bool, _ what: String) {
        checks += 1
        if !cond {
            failures += 1
            print("  FAIL: \(what)")
        }
    }
}
