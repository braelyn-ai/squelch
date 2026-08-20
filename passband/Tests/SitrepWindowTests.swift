// The banking card's window rule (issue #82): the last 24 hours, or everything
// since the zone was last seen, whichever reaches further back. These exercise
// the pure statics — the lifecycle plumbing (notifications, UserDefaults) is
// deliberately out of reach of a headless suite, which is why the rule is
// factored to take its inputs as arguments.

import Foundation

@main
@MainActor
struct SitrepWindowTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        cutoffRule()
        admitsRule()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    static let now = Date(timeIntervalSince1970: 1_755_600_000)
    static let day: TimeInterval = 24 * 3600

    static func cutoffRule() {
        // First run: no stamp has ever been written, so the floor alone decides
        // — which is also what clears a pre-window backlog on update day.
        expect(
            SitrepWindow.cutoff(now: now, lastSeen: nil) == now.addingTimeInterval(-day),
            "no stamp falls back to the 24h floor")

        // Away for three days: the whole gap shows, once.
        let threeDays = now.addingTimeInterval(-3 * day)
        expect(
            SitrepWindow.cutoff(now: now, lastSeen: threeDays) == threeDays,
            "a stamp older than the floor wins (show the gap)")

        // A quick app switch minutes ago must not collapse the window below
        // the floor.
        let justLeft = now.addingTimeInterval(-300)
        expect(
            SitrepWindow.cutoff(now: now, lastSeen: justLeft)
                == now.addingTimeInterval(-day),
            "a fresh stamp never narrows the window under 24h")

        // A stamp from the future (clock skew) degrades to the floor, not to an
        // empty card.
        let future = now.addingTimeInterval(3600)
        expect(
            SitrepWindow.cutoff(now: now, lastSeen: future)
                == now.addingTimeInterval(-day),
            "a future stamp cannot push the cutoff past the floor")
    }

    static func admitsRule() {
        let cutoff = now.addingTimeInterval(-day)

        expect(
            SitrepWindow.admits(now, cutoff: cutoff),
            "a fresh row is in")
        expect(
            SitrepWindow.admits(cutoff, cutoff: cutoff),
            "the boundary instant is in, not out")
        expect(
            !SitrepWindow.admits(cutoff.addingTimeInterval(-1), cutoff: cutoff),
            "an older row is out")
        // Unparseable timestamps are IN: hiding a row over a formatting quirk
        // is the wrong failure.
        expect(
            SitrepWindow.admits(nil, cutoff: cutoff),
            "nil (unparseable) is in")
    }

    // MARK: - harness

    static func expect(_ ok: Bool, _ label: String) {
        checks += 1
        if !ok {
            failures += 1
            print("FAIL: \(label)")
        }
    }
}
