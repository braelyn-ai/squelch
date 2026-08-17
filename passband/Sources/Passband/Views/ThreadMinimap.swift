// THE MINIMAP — a slim rail down the LEFT edge of the reader that answers one
// question: where in this conversation am I? The thread drawn to scale, one nub
// per message, coloured by who wrote it, with the screenful you are looking at
// drawn over them.
//
// IT IS A READOUT, NOT A CONTROL. Clicking it to jump was tried and pulled: a
// rail is a poor 26-point target for picking one message out of forty, and j/k
// already move the selection precisely. Nothing here takes a gesture, so nothing
// here can compete with the scroll for one.
//
// IT IS NOT A FULL-HEIGHT SCROLL BAR, and that is the point. A scroll bar
// stretches to fill whatever space it is given, so two short emails and two
// hundred long ones look identical. This is drawn at a FIXED scale — one
// screenful of mail is worth a fixed run of rail — so a short exchange really is
// just a couple of nubs and a long thread really does run down the window. Only
// a thread longer than the rail can hold gets squashed to fit.
//
// NOTHING HERE MOVES BECAUSE YOU SCROLLED. The nubs are drawn from the messages'
// own text (see MinimapGeometry) and not from anything the layout measures, so
// the drawing is settled before the first frame and stays put for as long as the
// thread is open. The map hangs from the TOP of the rail rather than being
// centred in it, so its start is nailed down too. The only thing a scroll tick
// may change is where the window mark sits — and not its size: one screenful is
// one screenful wherever you are.
//
// It is drawn in a Canvas because it redraws on every scroll tick: one draw pass
// costs nothing, a view tree per frame does. For the same reason its input lives
// in ThreadMap, an object the reader hands over — writing scroll geometry into
// the viewer's own @State would invalidate every message card and every
// sandboxed web frame sixty times a second.

import SwiftUI

/// WHERE THE READER'S CARDS ARE, right now. Written by ThreadViewer's geometry
/// readers, read only here, and deliberately nothing else: heights are not kept,
/// because a map that learned from them would redraw itself while you read.
@MainActor
@Observable
final class ThreadMap {
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
    }

    /// A card the stack has recycled stops reporting, and a POSITION nobody is
    /// updating is a lie the window mark would be placed by.
    func drop(_ index: Int) {
        frames[index] = nil
    }

    /// A different thread is a different map.
    func forget() {
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

    /// What a nub is drawn as. COLOUR IS THE SENDER, always and only: a rail
    /// where the tint means two different things depending on the message is a
    /// rail you have to decode. An obligation is said with WIDTH instead, which
    /// is a channel nothing else is using.
    struct Mark: Equatable {
        var attention: Bool
        var tint: Color
        /// What this message is worth on the map, from its own text.
        var estimate: CGFloat
    }

    /// The rail's footprint. Reserved whether or not there is a map to draw, so
    /// a second message arriving does not shove the mail sideways.
    static let width: CGFloat = 26

    /// THE SCALE: how much rail ONE SCREENFUL of mail is worth. Small on purpose
    /// — it is what makes a two-email exchange read as two nubs instead of as a
    /// full-height bar that says nothing.
    private static let railPerScreen: CGFloat = 46
    /// Air above and below the map, so the first and last nubs are not welded to
    /// the window edges.
    private static let inset: CGFloat = 12
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
            ? MinimapGeometry.layout(estimates: marks.map(\.estimate), frames: map.frames) : nil

        GeometryReader { geo in
            if let layout, viewport > 0 {
                canvas(
                    layout: layout,
                    scale: scale(total: layout.total, rail: geo.size.height - Self.inset * 2)
                )
                .transition(.opacity)
            }
        }
        .frame(width: Self.width)
        // A whisper beside the mail. Static: there is nothing to aim at here, so
        // there is nothing for the pointer arriving to promise.
        .opacity(0.85)
        // The rail takes no input at all, and a decorative layer that eats a
        // click meant for the mail beside it is a bug waiting to be filed.
        .allowsHitTesting(false)
        .animation(.easeOut(duration: 0.22), value: layout == nil)
        .help("where you are in the thread")
    }

    /// Rail points per map point: the fixed scale, until the thread outgrows the
    /// rail and has to be squashed to fit.
    private func scale(total: CGFloat, rail: CGFloat) -> CGFloat {
        min(max(rail, 1) / max(total, 1), Self.railPerScreen / max(viewport, 1))
    }

    private func canvas(layout: MinimapGeometry.Layout, scale: CGFloat) -> some View {
        Canvas(rendersAsynchronously: false) { ctx, size in
            let length = layout.total * scale
            let top = Self.inset

            // 1. THE SCREENFUL YOU ARE LOOKING AT, behind the nubs so they stay
            // legible inside it. Its height is the viewport's own, to scale —
            // which is why it never changes size while you scroll.
            if let offset = layout.offset {
                let height = max(Self.minMark, min(length, viewport * scale))
                let y = min(max(top + offset * scale, top), top + max(0, length - height))
                let mark = Path(
                    roundedRect: CGRect(
                        x: (size.width - Self.markWidth) / 2, y: y, width: Self.markWidth,
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
                        x: (size.width - 1.5) / 2, y: top, width: 1.5, height: length),
                    cornerRadius: 0.75),
                with: .color(Palette.hairline))

            // 3. The messages. The selected nub draws LAST so a long thread's
            // crowding can never bury the one you are on.
            for i in marks.indices where i != selected {
                nub(ctx, size: size, index: i, layout: layout, scale: scale)
            }
            if marks.indices.contains(selected) {
                nub(ctx, size: size, index: selected, layout: layout, scale: scale)
            }
        }
    }

    private func nub(
        _ ctx: GraphicsContext, size: CGSize, index: Int, layout: MinimapGeometry.Layout,
        scale: CGFloat
    ) {
        let mark = marks[index]
        let isSelected = index == selected
        let width =
            isSelected
            ? Self.selectedWidth : (mark.attention ? Self.attentionWidth : Self.barWidth)
        let height = max(Self.minBar, layout.heights[index] * scale - Self.barGap)
        let top = Self.inset + layout.tops[index] * scale + Self.barGap / 2
        // ALWAYS the sender's colour. Being the selected message is said by width
        // and by full strength, never by repainting it in the accent — the whole
        // point of the rail is that a colour means a person.
        let color = mark.tint.opacity(isSelected ? 1 : (mark.attention ? 0.9 : 0.5))
        ctx.fill(
            Path(
                roundedRect: CGRect(
                    x: (size.width - width) / 2, y: top, width: width, height: height),
                cornerRadius: width / 2, style: .continuous),
            with: .color(color))
    }
}
