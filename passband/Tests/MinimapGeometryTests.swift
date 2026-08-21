// The thread minimap's scale drawing. It is asserted rather than eyeballed
// because the rail is a position indicator: one off-by-a-card and it points at
// the wrong part of the conversation, confidently, forever.
//
// Two cases are the reason this suite exists. A lazy stack has not laid out the
// messages nobody has scrolled to, so the map has to be drawable from a PARTIAL
// set of heights — and drawable the SAME WAY before and after a card is
// measured, or the rail crawls while you read. And it recycles rows it has
// scrolled past, leaving their last reported position behind — a stale rectangle
// that must not be what the window mark is placed by.

import CoreGraphics
import Foundation

@main
@MainActor
struct MinimapGeometryTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        emptyThread()
        theMapIsTheEstimates()
        measurementNeverMovesTheMap()
        estimateGrowsWithTheText()
        theStyleIsTheMeasure()
        windowFromACard()
        windowIsAFractionNotADistance()
        windowUsesTheCardItIsInsideNotTheNearest()
        windowPrefersTheCardAtTheEdge()
        staleFramesLoseToLiveOnes()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - cases

    static func emptyThread() {
        equal(MinimapGeometry.layout(estimates: [], frames: [:]) == nil, true, "no messages")
    }

    /// The drawing is the estimates, stacked. Nothing else is an input.
    static func theMapIsTheEstimates() {
        let l = MinimapGeometry.layout(estimates: [200, 500, 300], frames: [:])
        equal(l?.tops, [0, 200, 700], "tops are a prefix sum")
        equal(l?.heights, [200, 500, 300], "heights are the estimates")
        equal(l?.total, 1000, "total is their sum")
        equal(l?.offset == nil, true, "no frames means no window")
        // Nothing is ever worth nothing: a message with no text at all still has
        // a header and a sender to draw.
        let empty = MinimapGeometry.layout(estimates: [0, 0], frames: [:])
        equal(empty?.total, MinimapGeometry.minimumCard * 2, "a floor under every message")
    }

    /// THE WHOLE POINT. The same thread laid out, measured, scrolled through and
    /// recycled draws exactly the same map — only the window mark moves. A rail
    /// that redraws itself while you read it is worse than a rough one.
    static func measurementNeverMovesTheMap() {
        let estimates: [CGFloat] = [200, 300, 400, 500]
        let fresh = MinimapGeometry.layout(estimates: estimates, frames: [:])
        // Every card measured, and measured nothing like its guess.
        let read = MinimapGeometry.layout(
            estimates: estimates,
            frames: [0: rect(-3000, 1400), 1: rect(-1600, 900), 2: rect(-700, 2200)])
        equal(read?.tops, fresh?.tops, "the nubs do not move")
        equal(read?.heights, fresh?.heights, "and they do not resize")
        equal(read?.total, fresh?.total, "the map is the same length")
        equal(read?.offset != nil, true, "only the window mark has anything to say")
    }

    /// The guess itself: more text is more rail, and both ends are bounded.
    static func estimateGrowsWithTheText() {
        let short = MinimapGeometry.estimate(text: "thanks!")
        let long = MinimapGeometry.estimate(text: String(repeating: "word ", count: 400))
        equal(short >= MinimapGeometry.minimumCard, true, "a one-liner still has a card")
        equal(long > short, true, "a long message is worth more rail")
        equal(
            MinimapGeometry.estimate(text: String(repeating: "x", count: 400_000))
                <= MinimapGeometry.longestGuess, true, "and a guess is capped")
        // Hard breaks end a line early: five newlines are five lines, not one.
        equal(
            MinimapGeometry.estimate(text: "a\nb\nc\nd\ne") > MinimapGeometry.estimate(text: "abcde"),
            true, "newlines count as lines")
        // An attachment strip is a real thing on the card, so it is real here.
        equal(
            MinimapGeometry.estimate(text: "hi", attachments: 2)
                > MinimapGeometry.estimate(text: "hi"), true, "attachments add a strip")

        // IMAGES ARE WORTH NOTHING, on purpose: a picture and a paragraph of the
        // same markup length are the same guess, because the alternative is
        // guessing what a picture is going to measure.
        let picture = "<p>see below</p><img src='x.png'><img src='y.png'>"
        equal(
            MinimapGeometry.estimate(text: "see below", html: picture),
            MinimapGeometry.estimate(text: "see below"), "markup does not count when text does")

        // MARKUP WITH NO TEXT AT ALL still has to be worth more than the floor:
        // one line is the one shape a long message must never take.
        let markup = String(repeating: "<td style='padding:0'>hello there</td>", count: 200)
        equal(
            MinimapGeometry.estimate(text: "", html: markup) > MinimapGeometry.estimate(text: ""),
            true, "html stands in when the plain text is missing")
        equal(
            MinimapGeometry.estimate(text: " \n \n ", html: markup)
                == MinimapGeometry.estimate(text: "", html: markup), true,
            "whitespace is not text")

    }

    /// THE SAME MAIL THROUGH TWO MEASURES. A chat bubble is narrower than the
    /// column, so a paragraph is more lines there and the rail has to say so —
    /// while the caption over a bubble costs less than a card's header row, so a
    /// short message is worth LESS. Both directions matter: a map drawn for the
    /// wrong style points at the wrong part of the conversation.
    static func theStyleIsTheMeasure() {
        let paragraph = String(repeating: "word ", count: 400)
        equal(
            MinimapGeometry.estimate(text: paragraph, style: .bubbles)
                > MinimapGeometry.estimate(text: paragraph, style: .classic), true,
            "a narrower measure is more lines")
        equal(
            MinimapGeometry.estimate(text: "thanks!", style: .bubbles)
                <= MinimapGeometry.estimate(text: "thanks!", style: .classic), true,
            "and a caption costs less than a header row")
        // The default is the style every existing caller means, so nothing
        // outside the reader changes shape by not asking.
        equal(
            MinimapGeometry.estimate(text: paragraph),
            MinimapGeometry.estimate(text: paragraph, style: .classic),
            "not asking means the email card")
        // The bounds are the rail's, not a style's: neither end moves.
        equal(
            MinimapGeometry.estimate(text: "", style: .bubbles) >= MinimapGeometry.minimumCard,
            true, "a bubble still has a floor")
        equal(
            MinimapGeometry.estimate(text: String(repeating: "x", count: 400_000), style: .bubbles)
                <= MinimapGeometry.longestGuess, true, "and the same ceiling")
    }

    /// The window's own position, read off a card: message 1 is drawn from 200 to
    /// 700, and its own top edge is 50 points BELOW the window's — a tenth of the
    /// card's real height above the edge, so a tenth of its nub is too.
    static func windowFromACard() {
        let l = MinimapGeometry.layout(estimates: [200, 500], frames: [1: rect(50, 500)])
        equal(l?.offset, 150, "window top is a fraction of the card, mapped onto its nub")
    }

    /// THE CARD THAT MEASURED NOTHING LIKE ITS GUESS. Message 1 is drawn as 500
    /// and laid out as 2000, and the window is halfway down it: the mark belongs
    /// halfway down its nub, at 200 + 250, and NOT 1000 points past the end of a
    /// map that is only 700 long.
    static func windowIsAFractionNotADistance() {
        let l = MinimapGeometry.layout(estimates: [200, 500], frames: [1: rect(-1000, 2000)])
        equal(l?.offset, 450, "progress through the card is what places the mark")
        // And at the seam it hands over exactly: the bottom of card 1 is the end
        // of the map, wherever the real card ended.
        let seam = MinimapGeometry.layout(estimates: [200, 500], frames: [1: rect(-2000, 2000)])
        equal(seam?.offset, 700, "the last point of the card is the last point of the map")
    }

    /// THE JITTER. Two cards, each measuring 1000, drawn as nubs of 500 and 1000
    /// — and the window sits 60% of the way down the FIRST one. The card the edge
    /// is inside is card 0, so the mark is 60% down its nub, at 300. The card
    /// whose top edge is merely NEAREST is card 1 (400 away, against 600), which
    /// would put the mark at 100 and snap it back the moment the scroll crossed
    /// halfway. The rule has to be containment, not proximity.
    static func windowUsesTheCardItIsInsideNotTheNearest() {
        let l = MinimapGeometry.layout(
            estimates: [500, 1000], frames: [0: rect(-600, 1000), 1: rect(400, 1000)])
        equal(l?.offset, 300, "the card containing the edge places the mark")

        // AND IT HANDS OVER CONTINUOUSLY. A point before the seam and a point
        // after it are a point apart on the rail, not a jump between two nubs of
        // different lengths.
        let before = MinimapGeometry.layout(
            estimates: [500, 1000], frames: [0: rect(-990, 1000), 1: rect(10, 1000)])
        let after = MinimapGeometry.layout(
            estimates: [500, 1000], frames: [0: rect(-1000, 1000), 1: rect(0, 1000)])
        equal(before?.offset, 495, "just short of the seam")
        equal(after?.offset, 500, "and at it, which is the next nub's own top")
    }

    /// Two cards mounted, one straddling the top edge: the straddling one is
    /// nearest to it and is the one the reading is taken from. Both agree here,
    /// which is the point — the answer does not depend on which card is asked.
    static func windowPrefersTheCardAtTheEdge() {
        let l = MinimapGeometry.layout(
            estimates: [400, 600], frames: [0: rect(-250, 400), 1: rect(150, 600)])
        equal(l?.offset, 250, "the card at the edge places the window")
    }

    /// THE RECYCLED ROW. Card 0 was scrolled far past and its frame stopped being
    /// updated; card 1 is live and near the edge. The stale rectangle is further
    /// from the top edge, so it loses — the window lands where the live card says.
    static func staleFramesLoseToLiveOnes() {
        let l = MinimapGeometry.layout(
            estimates: [400, 600], frames: [0: rect(-5000, 400), 1: rect(-20, 600)])
        equal(l?.offset, 420, "the live card wins")
        // A frame for a message this thread no longer has is ignored outright.
        let gone = MinimapGeometry.layout(estimates: [400], frames: [7: rect(0, 400)])
        equal(gone?.offset == nil, true, "index past the end is ignored")
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
