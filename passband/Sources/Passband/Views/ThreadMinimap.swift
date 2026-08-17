// THE MINIMAP — a slim rail down the LEFT edge of the reader that answers one
// question: where in this conversation am I? The thread drawn to scale, one nub
// per message, tinted by who wrote it, with the screenful you are looking at
// drawn over them. Click or drag it to jump.
//
// IT IS NOT A FULL-HEIGHT SCROLL BAR, and that is the point. A scroll bar
// stretches to fill whatever space it is given, so two short emails and two
// hundred long ones look identical. This is drawn at a FIXED scale — one
// screenful of mail is worth a fixed run of rail — so a short exchange really is
// just a couple of nubs and a long thread really does run down the window. Only
// a thread longer than the rail can hold gets squashed to fit.
//
// The heights it is drawn from are the cards' own measured heights (see
// MinimapGeometry for why heights and not the scroll's own numbers), which is
// also what keeps the highlight still: one screenful is one screenful wherever
// you are, so the window mark never changes size as you scroll.
//
// It is drawn in a Canvas because it redraws on every scroll tick: one draw pass
// costs nothing, a view tree per frame does. For the same reason its input lives
// in ThreadMap, an object the reader hands over — writing scroll geometry into
// the viewer's own @State would invalidate every message card and every
// sandboxed web frame sixty times a second.

import SwiftUI

/// What the reader has measured of its own mail. Written by ThreadViewer's
/// geometry readers, read only here.
@MainActor
@Observable
final class ThreadMap {
    /// Display index -> that card's height. STICKY: a height is a fact about the
    /// message, so once measured it is kept even when the lazy stack recycles the
    /// row. This is what the map is drawn from.
    private(set) var heights: [Int: CGFloat] = [:]
    /// Display index -> that card's frame relative to the visible area, so y = 0
    /// is the top of the window. LIVE, and only for mounted cards: it answers
    /// "where is the window", which is a question about right now.
    private(set) var frames: [Int: CGRect] = [:]

    /// Sub-pixel churn is not information: a layout pass that shifts a card by a
    /// third of a point must not cost a redraw.
    func note(_ index: Int, frame: CGRect) {
        guard frame.height > 0 else { return }
        if let old = frames[index], abs(old.minY - frame.minY) < 0.5,
            abs(old.height - frame.height) < 0.5
        {
            return
        }
        frames[index] = frame
        heights[index] = frame.height
    }

    /// A card the stack has recycled stops reporting, and a POSITION nobody is
    /// updating is a lie the window mark would be placed by. Its height stays:
    /// that one is still true.
    func drop(_ index: Int) {
        frames[index] = nil
    }

    /// A different thread is a different map.
    func forget() {
        heights.removeAll()
        frames.removeAll()
    }
}

// MARK: - the rail

struct ThreadMinimap: View {
    let map: ThreadMap
    /// One per message, in display order (oldest first).
    let marks: [Mark]
    let selected: Int
    /// The reader's visible height — one of these is what the window mark is
    /// worth, and it sets the drawing scale.
    let viewport: CGFloat
    let onSelect: (Int) -> Void

    /// What a nub is drawn as: its sender's colour, or its tier's when the
    /// message is an obligation — the one case worth seeing from anywhere in the
    /// thread, so it is drawn wider too.
    struct Mark: Equatable {
        var attention: Bool
        var tint: Color
    }

    @State private var hovering = false

    /// The rail's footprint. Reserved whether or not there is a map to draw, so
    /// a second message arriving does not shove the mail sideways.
    static let width: CGFloat = 26

    /// THE SCALE: how much rail ONE SCREENFUL of mail is worth. Small on purpose
    /// — it is what makes a two-email exchange read as two nubs instead of as a
    /// full-height bar that says nothing.
    private static let railPerScreen: CGFloat = 46
    /// Nub widths: a plain message, an obligation, the selected one.
    private static let barWidth: CGFloat = 3
    private static let attentionWidth: CGFloat = 4.5
    private static let selectedWidth: CGFloat = 5.5
    /// Air between two nubs, and the floor under one in a long thread.
    private static let barGap: CGFloat = 2
    private static let minBar: CGFloat = 4
    /// The window mark's width and its own floor.
    private static let markWidth: CGFloat = 12
    private static let minMark: CGFloat = 8

    var body: some View {
        // ONE MESSAGE IS NOT A CONVERSATION: there is no "where am I in it" to
        // answer, so the rail stays out of the way entirely.
        let layout =
            marks.count >= 2
            ? MinimapGeometry.layout(
                heights: map.heights, frames: map.frames, count: marks.count) : nil

        Color.clear
            .frame(width: Self.width)
            .overlay {
                if let layout, viewport > 0 {
                    GeometryReader { geo in
                        let scale = scale(total: layout.total, rail: geo.size.height)
                        let origin = (geo.size.height - layout.total * scale) / 2
                        canvas(layout: layout, scale: scale, origin: origin)
                            .contentShape(Rectangle())
                            .gesture(
                                DragGesture(minimumDistance: 0)
                                    .onChanged { value in
                                        jump(
                                            to: value.location.y, layout: layout, scale: scale,
                                            origin: origin)
                                    }
                            )
                    }
                    .padding(.vertical, 12)
                    .transition(.opacity)
                }
            }
            // At rest the rail is a whisper beside the mail; under the pointer it
            // firms up into something you can aim at.
            .opacity(hovering ? 1 : 0.78)
            .animation(.easeOut(duration: 0.16), value: hovering)
            .animation(.easeOut(duration: 0.22), value: layout == nil)
            .onHover { hovering = $0 }
            .help("where you are in the thread — click or drag to jump")
    }

    /// Rail points per mail point: the fixed scale, until the thread outgrows the
    /// rail and has to be squashed to fit.
    private func scale(total: CGFloat, rail: CGFloat) -> CGFloat {
        min(rail / max(total, 1), Self.railPerScreen / max(viewport, 1))
    }

    private func canvas(layout: MinimapGeometry.Layout, scale: CGFloat, origin: CGFloat)
        -> some View
    {
        Canvas(rendersAsynchronously: false) { ctx, size in
            let length = layout.total * scale

            // 1. THE SCREENFUL YOU ARE LOOKING AT, behind the nubs so they stay
            // legible inside it. Its height is the viewport's own, to scale —
            // which is why it never changes size while you scroll.
            if let offset = layout.offset {
                let height = max(Self.minMark, min(length, viewport * scale))
                let top = min(max(origin + offset * scale, origin), origin + length - height)
                let mark = Path(
                    roundedRect: CGRect(
                        x: (size.width - Self.markWidth) / 2, y: top, width: Self.markWidth,
                        height: height),
                    cornerRadius: 4, style: .continuous)
                ctx.fill(mark, with: .color(Palette.ink.opacity(0.07)))
                ctx.stroke(mark, with: .color(Palette.hairlineStrong), lineWidth: 0.75)
            }

            // 2. The spine, which is what the gaps between messages read as. It
            // runs the length of the THREAD, not of the rail: the map is an
            // object of a size, and a line past its end would deny that.
            ctx.fill(
                Path(
                    roundedRect: CGRect(
                        x: (size.width - 1.5) / 2, y: origin, width: 1.5, height: length),
                    cornerRadius: 0.75),
                with: .color(Palette.hairline))

            // 3. The messages. The selected nub draws LAST so a long thread's
            // crowding can never bury the one you are on.
            for i in marks.indices where i != selected {
                nub(ctx, size: size, index: i, layout: layout, scale: scale, origin: origin)
            }
            if marks.indices.contains(selected) {
                nub(ctx, size: size, index: selected, layout: layout, scale: scale, origin: origin)
            }
        }
    }

    private func nub(
        _ ctx: GraphicsContext, size: CGSize, index: Int, layout: MinimapGeometry.Layout,
        scale: CGFloat, origin: CGFloat
    ) {
        let mark = marks[index]
        let isSelected = index == selected
        let width =
            isSelected
            ? Self.selectedWidth : (mark.attention ? Self.attentionWidth : Self.barWidth)
        let height = max(Self.minBar, layout.heights[index] * scale - Self.barGap)
        let top = origin + layout.tops[index] * scale + Self.barGap / 2
        let color = isSelected ? Palette.accent : mark.tint.opacity(mark.attention ? 0.95 : 0.6)
        ctx.fill(
            Path(
                roundedRect: CGRect(
                    x: (size.width - width) / 2, y: top, width: width, height: height),
                cornerRadius: width / 2, style: .continuous),
            with: .color(color))
    }

    /// Rail point -> message. Selecting is all it does: the reader's own index
    /// watcher owns the scroll, so a jump from here lands exactly where a `j`
    /// would have.
    private func jump(
        to y: CGFloat, layout: MinimapGeometry.Layout, scale: CGFloat, origin: CGFloat
    ) {
        guard scale > 0 else { return }
        let hit = MinimapGeometry.index(
            atMailY: (y - origin) / scale, tops: layout.tops, heights: layout.heights)
        if hit != selected { onSelect(hit) }
    }
}
