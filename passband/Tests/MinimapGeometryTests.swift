// The thread minimap's scale drawing. It is asserted rather than eyeballed
// because the rail is a position indicator: one off-by-a-card and it points at
// the wrong part of the conversation, confidently, forever.
//
// Two cases are the reason this suite exists. A lazy stack has not laid out the
// messages nobody has scrolled to, so the map has to be drawable from a PARTIAL
// set of heights. And it recycles rows it has scrolled past, leaving their last
// reported position behind — a stale rectangle that must not be what the window
// mark is placed by.

import CoreGraphics
import Foundation

@main
@MainActor
struct MinimapGeometryTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        emptyThread()
        everyHeightKnown()
        unmeasuredTakeTheAverage()
        nothingMeasuredAtAll()
        windowFromACard()
        windowPrefersTheCardAtTheEdge()
        staleFramesLoseToLiveOnes()
        hitTest()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - cases

    static func emptyThread() {
        equal(MinimapGeometry.layout(heights: [:], frames: [:], count: 0) == nil, true, "no messages")
    }

    /// The plain case: three measured cards stack into a prefix sum.
    static func everyHeightKnown() {
        let l = MinimapGeometry.layout(
            heights: [0: 200, 1: 500, 2: 300], frames: [:], count: 3)
        equal(l?.tops, [0, 200, 700], "tops are a prefix sum")
        equal(l?.heights, [200, 500, 300], "heights are kept as measured")
        equal(l?.total, 1000, "total is their sum")
        equal(l?.offset == nil, true, "no frames means no window")
    }

    /// A thread scrolled to the end: the early messages were never laid out, so
    /// they stand in at the average of the ones that were. The map is right about
    /// the whole and vague about what it has not seen.
    static func unmeasuredTakeTheAverage() {
        let l = MinimapGeometry.layout(heights: [2: 300, 3: 500], frames: [:], count: 4)
        equal(l?.heights, [400, 400, 300, 500], "unmeasured take the average of the measured")
        equal(l?.tops, [0, 400, 800, 1100], "and stack in order")
        equal(l?.total, 1600, "total counts the stand-ins")
    }

    /// Nothing measured yet — the first frame of a fresh thread. Every message is
    /// worth one screenful rather than zero, so the rail opens as the shape of a
    /// conversation instead of as a dot.
    static func nothingMeasuredAtAll() {
        let l = MinimapGeometry.layout(heights: [:], frames: [:], count: 2)
        equal(l?.total, MinimapGeometry.unmeasuredFallback * 2, "two fallback messages")
        equal(l?.tops, [0, MinimapGeometry.unmeasuredFallback], "stacked")
    }

    /// The window's own position, read off a card: message 1 starts at 200 and
    /// its top is 50 points BELOW the window's top edge, so the window's top edge
    /// is at mail point 150.
    static func windowFromACard() {
        let l = MinimapGeometry.layout(
            heights: [0: 200, 1: 500], frames: [1: rect(50, 500)], count: 2)
        equal(l?.offset, 150, "window top is the card's top minus its screen offset")
    }

    /// Two cards mounted, one straddling the top edge: the straddling one is
    /// nearest to it and is the one the reading is taken from. Both agree here,
    /// which is the point — the answer does not depend on which card is asked.
    static func windowPrefersTheCardAtTheEdge() {
        let l = MinimapGeometry.layout(
            heights: [0: 400, 1: 600],
            frames: [0: rect(-250, 400), 1: rect(150, 600)], count: 2)
        equal(l?.offset, 250, "the card at the edge places the window")
    }

    /// THE RECYCLED ROW. Card 0 was scrolled far past and its frame stopped being
    /// updated; card 1 is live and near the edge. The stale rectangle is further
    /// from the top edge, so it loses — the window lands where the live card says.
    static func staleFramesLoseToLiveOnes() {
        let l = MinimapGeometry.layout(
            heights: [0: 400, 1: 600],
            frames: [0: rect(-5000, 400), 1: rect(-20, 600)], count: 2)
        equal(l?.offset, 420, "the live card wins")
        // A frame for a message this thread no longer has is ignored outright.
        let gone = MinimapGeometry.layout(
            heights: [0: 400], frames: [7: rect(0, 400)], count: 1)
        equal(gone?.offset == nil, true, "index past the end is ignored")
    }

    /// The rail's hit test, in mail points. Clamped at both ends so a drag that
    /// runs off the map still means the message it ran off.
    static func hitTest() {
        let tops: [CGFloat] = [0, 200, 700]
        let heights: [CGFloat] = [200, 500, 300]
        equal(MinimapGeometry.index(atMailY: 0, tops: tops, heights: heights), 0, "the first")
        equal(MinimapGeometry.index(atMailY: 199, tops: tops, heights: heights), 0, "its last point")
        equal(MinimapGeometry.index(atMailY: 200, tops: tops, heights: heights), 1, "the seam")
        equal(MinimapGeometry.index(atMailY: 950, tops: tops, heights: heights), 2, "the last")
        equal(MinimapGeometry.index(atMailY: 4000, tops: tops, heights: heights), 2, "past the end")
        equal(MinimapGeometry.index(atMailY: -80, tops: tops, heights: heights), 0, "above the top")
        equal(MinimapGeometry.index(atMailY: 10, tops: [], heights: []), 0, "no messages")
    }

    // MARK: - helpers

    /// A card `height` tall whose top edge is `top` below the window's top edge.
    static func rect(_ top: CGFloat, _ height: CGFloat) -> CGRect {
        CGRect(x: 0, y: top, width: 900, height: height)
    }

    // MARK: - assertions

    static func equal<T: Equatable>(_ got: T, _ want: T, _ label: String, line: Int = #line) {
        checks += 1
        if got != want {
            failures += 1
            print("FAIL (line \(line)): \(label)\n  want: \(want)\n   got: \(got)")
        }
    }
}
