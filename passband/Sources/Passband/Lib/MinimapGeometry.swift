// The thread minimap's arithmetic, with no SwiftUI in it: measured card
// rectangles in, a scale drawing of the thread out. Separated from the rail that
// draws it because this is the part that can be wrong — and because a pure file
// is a file that can be asserted (Tests/MinimapGeometryTests.swift).
//
// IT IS BUILT FROM HEIGHTS, NEVER FROM THE SCROLL'S OWN NUMBERS. The reader's
// stack is lazy, so `contentSize` is a running guess that grows as you travel
// and a card's reported position can be a whole thread out of date — measured,
// one card claimed a top of 1744 inside a document the scroll called 1493 tall.
// A card's HEIGHT survives all of that: it is a fact about the message, not
// about where the scroll currently believes it is. So the map is a prefix sum of
// heights, and the one live reading it needs — where the window sits — is read
// back off whichever mounted card is nearest the top edge, in that same space.
//
// Messages nobody has scrolled to yet have no measured height, and they take the
// average of the ones that do: a map that is right about the whole and vague
// about the parts it has not seen, which sharpens as the reader travels.

import CoreGraphics

enum MinimapGeometry {
    /// The thread drawn to scale, in MAIL POINTS (the reader's own units, not
    /// the rail's — the rail applies one scale factor to all of it).
    struct Layout: Equatable {
        /// Each message's top edge, measured from the top of the first.
        var tops: [CGFloat]
        var heights: [CGFloat]
        var total: CGFloat
        /// Where the top edge of the window falls in the same space, or nil when
        /// no card has reported a position yet.
        var offset: CGFloat?
    }

    /// What an unmeasured message is worth when NOTHING has been measured yet —
    /// one screenful is the least surprising guess for mail nobody has seen.
    static let unmeasuredFallback: CGFloat = 600

    static func layout(heights known: [Int: CGFloat], frames: [Int: CGRect], count: Int) -> Layout? {
        guard count > 0 else { return nil }
        let measured = (0..<count).compactMap { known[$0] }.filter { $0 > 0 }
        let average =
            measured.isEmpty
            ? unmeasuredFallback : measured.reduce(0, +) / CGFloat(measured.count)

        var tops: [CGFloat] = []
        var heights: [CGFloat] = []
        var running: CGFloat = 0
        for i in 0..<count {
            let h = (known[i] ?? 0) > 0 ? (known[i] ?? 0) : average
            tops.append(running)
            heights.append(h)
            running += h
        }
        return Layout(
            tops: tops, heights: heights, total: running,
            offset: offset(frames: frames, tops: tops, count: count))
    }

    /// WHERE THE WINDOW IS, read off the mounted card whose own top edge is
    /// nearest to the window's — the one straddling it, or its neighbour. A card
    /// reports its frame relative to the visible area, so `top - minY` is the
    /// mail point currently sitting at the top of the window. Nearest, rather
    /// than any card, because a frame from a row the stack recycled can be stale,
    /// and the card at the top edge is by definition the one still being laid out.
    static func offset(frames: [Int: CGRect], tops: [CGFloat], count: Int) -> CGFloat? {
        var best: (distance: CGFloat, offset: CGFloat)?
        for (i, frame) in frames where i >= 0 && i < count && frame.height > 0 {
            let distance = abs(frame.minY)
            guard best == nil || distance < best!.distance else { continue }
            best = (distance, tops[i] - frame.minY)
        }
        return best?.offset
    }

    /// Which message a mail point lands in. Off either end clamps to that end, so
    /// a drag that runs off the rail still means the message it ran off.
    static func index(atMailY y: CGFloat, tops: [CGFloat], heights: [CGFloat]) -> Int {
        guard !tops.isEmpty else { return 0 }
        for i in tops.indices where y < tops[i] + heights[i] {
            return max(0, i)
        }
        return tops.count - 1
    }
}
