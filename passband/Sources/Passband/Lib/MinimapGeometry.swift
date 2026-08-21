// The thread minimap's arithmetic, with no SwiftUI in it: the thread's own text
// in, a scale drawing of it out. Separated from the rail that draws it because
// this is the part that can be wrong — and because a pure file is a file that
// can be asserted (Tests/MinimapGeometryTests.swift).
//
// THE MAP IS DRAWN FROM THE MAIL, NOT FROM THE LAYOUT, and that is the whole
// design. A card's real height is only knowable once the lazy stack has laid it
// out, which happens as you scroll — so a map drawn from real heights is a map
// that redraws itself under you while you read it, and a position indicator that
// moves for reasons other than your position is worse than a rough one. Every
// height here is a guess made from the message's own text, so the map is a pure
// function of the thread: identical on every frame, every open, no matter where
// you have scrolled or what has been measured.
//
// The one live number is WHERE THE WINDOW IS, and it is deliberately taken as a
// FRACTION — how far into a card the window's top edge sits — and then mapped
// onto that card's drawn length. A card twice as tall as its guess still hands
// the mark over to its neighbour at exactly the seam between their two nubs.

import CoreGraphics

enum MinimapGeometry {
    /// The thread drawn to scale, in MAP POINTS (this file's own units: the sum
    /// of what the messages are guessed to be worth, not what anything measured).
    struct Layout: Equatable {
        /// Each message's top edge, measured from the top of the first.
        var tops: [CGFloat]
        var heights: [CGFloat]
        var total: CGFloat
        /// Where the top edge of the window falls in the same space, or nil when
        /// no card has reported a position yet.
        var offset: CGFloat?
    }

    /// The shortest a message is ever drawn as. A one-line reply still has a
    /// header, a sender, and air around it.
    static let minimumCard: CGFloat = 96
    /// And the tallest. A newsletter can genuinely run ten screenfuls, but this
    /// is a guess from a character count and a guess does not get to own the
    /// whole rail.
    static let longestGuess: CGFloat = 2400

    /// Rendered text metrics for the reader's column, near enough for a drawing.
    private static let charsPerLine: CGFloat = 105
    private static let lineHeight: CGFloat = 20
    /// Header, sender row, padding, rule: what a message costs before its text.
    private static let cardChrome: CGFloat = 132
    private static let attachmentStrip: CGFloat = 54
    /// Bytes of markup per byte of visible text, for the one case that has to be
    /// counted through the tags. A blunt instrument for a blunt situation.
    private static let markupRatio: CGFloat = 6

    /// WHAT A MESSAGE IS WORTH ON THE RAIL, from the only thing knowable without
    /// laying it out: how much text is in it.
    ///
    /// IT COUNTS LINES AND NOTHING ELSE — images, tables, and signature cards are
    /// all worth zero. Deliberately: guessing a picture's size from its markup is
    /// guessing, and the rail only draws for a thread of two or more messages,
    /// which is a conversation, which is text. A newsletter is one message in a
    /// thread of its own and has no rail to be wrong about.
    static func estimate(text: String, html: String? = nil, attachments: Int = 0) -> CGFloat {
        min(
            card(bodyHeight: textHeight(text: text, html: html), attachments: attachments),
            longestGuess)
    }

    /// A CARD AROUND A BODY OF A KNOWN HEIGHT — the chrome and the strip that
    /// `estimate` adds to its guess, so a card whose body WebKit actually
    /// measured is drawn on the same scale as one nobody has rendered yet.
    ///
    /// Uncapped, and that is the difference between the two: `longestGuess`
    /// exists because a number derived from a character count has not earned
    /// the whole rail. A measurement has. A newsletter that really is ten
    /// screenfuls has to be drawn as ten screenfuls or the rail is lying about
    /// the shape of the thread — which is the whole thing it is for.
    static func card(bodyHeight: CGFloat, attachments: Int = 0) -> CGFloat {
        max(cardChrome + bodyHeight + (attachments > 0 ? attachmentStrip : 0), minimumCard)
    }

    /// HOW TALL THIS BODY'S TEXT DRAWS, with no card around it. Split out of
    /// `estimate` because the READER needs the same number for a different
    /// reason: a web frame that has not measured itself yet has to be given a
    /// height, and until this existed that height was a flat 120 points. A
    /// message that scrolled into view at 120 and then measured at 900 shoved
    /// everything below it down by the difference, which is the mail lurching
    /// under somebody who is reading it. The guess does not have to be right —
    /// it has to be close enough that the correction is not a jump.
    ///
    /// Counted in UTF-8 bytes rather than characters because a long thread runs
    /// this per message and byte count is free where grapheme count is a walk.
    static func textHeight(text: String, html: String? = nil) -> CGFloat {
        var lines: CGFloat = 0
        var run: CGFloat = 0
        var ink = false
        for byte in text.utf8 {
            if byte == 0x0A {  // \n — a hard break ends the line early
                lines += max(1, (run / charsPerLine).rounded(.up))
                run = 0
            } else if byte != 0x0D {
                run += 1
                if byte != 0x20 && byte != 0x09 { ink = true }
            }
        }
        lines += max(1, (run / charsPerLine).rounded(.up))

        // NO PLAIN-TEXT TWIN. The daemon flattens HTML at ingest so this is rare,
        // but a message that arrives as markup alone would otherwise count as one
        // line and draw as the smallest nub on the rail — the one shape a long
        // message must never take. Markup is mostly tags, hence the discount.
        if !ink, let html, !html.isEmpty {
            let visible = CGFloat(html.utf8.count) / markupRatio
            lines = max(1, (visible / charsPerLine).rounded(.up))
        }

        return lines * lineHeight
    }

    /// `estimates` carries one entry per message and IS the map. Nothing measured
    /// enters this drawing; `frames` is read for one thing only, which is where
    /// the window currently sits.
    static func layout(estimates: [CGFloat], frames: [Int: CGRect]) -> Layout? {
        guard !estimates.isEmpty else { return nil }

        var tops: [CGFloat] = []
        var heights: [CGFloat] = []
        var running: CGFloat = 0
        for e in estimates {
            let h = max(e, minimumCard)
            tops.append(running)
            heights.append(h)
            running += h
        }
        return Layout(
            tops: tops, heights: heights, total: running,
            offset: offset(frames: frames, tops: tops, heights: heights))
    }

    /// WHERE THE WINDOW IS, read off THE CARD THE TOP EDGE IS INSIDE — the one
    /// whose top is at or above the edge and whose bottom is below it. Exactly
    /// one card satisfies that at a time, because the cards tile the thread, and
    /// that is what makes the answer stable.
    ///
    /// It is emphatically NOT "the card whose top edge is nearest", which is
    /// where the mark's jitter came from: that rule hands over at the HALFWAY
    /// point of each message, to a card the window has not reached yet, and reads
    /// it at negative progress against a different nub's length. The two answers
    /// disagree by however much the two guesses disagree, so the mark jumped at
    /// every half-message and flickered between them at the crossover. Handing
    /// over at the SEAM instead is continuous: the last point of one card is
    /// `tops[i] + heights[i]`, which is exactly `tops[i + 1]`.
    ///
    /// The reading is a FRACTION of the card and not a distance in points,
    /// because points here are guessed points: a card measuring 1400 where the
    /// map guessed 700 is half-read at -700, and half of its nub is where the
    /// mark belongs.
    static func offset(frames: [Int: CGRect], tops: [CGFloat], heights: [CGFloat]) -> CGFloat? {
        var inside: (top: CGFloat, index: Int)?
        var nearest: (distance: CGFloat, index: Int)?
        for (i, frame) in frames where i >= 0 && i < tops.count && frame.height > 0 {
            if frame.minY <= 0 && frame.minY + frame.height > 0 {
                // The DEEPEST such card wins. A row the stack recycled leaves its
                // last rectangle behind, and a stale one from far above can still
                // look like it contains the edge; the live card is the lower one.
                if inside == nil || frame.minY > inside!.top { inside = (frame.minY, i) }
            }
            let distance = abs(frame.minY)
            if nearest == nil || distance < nearest!.distance { nearest = (distance, i) }
        }
        // No card contains the edge only when the reader is in the room BELOW the
        // last message, or before anything has been laid out.
        guard let pick = inside?.index ?? nearest?.index, let frame = frames[pick] else {
            return nil
        }
        return tops[pick] + (-frame.minY / frame.height) * heights[pick]
    }
}
