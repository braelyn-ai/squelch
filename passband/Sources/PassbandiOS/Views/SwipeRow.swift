// THE DASHBOARD'S SWIPE RAIL. The mail tab gets SwiftUI's own `swipeActions`,
// because it is a real `List` and native full-swipe is better than anything
// hand-rolled. The sitrep cannot: its rows live inside `ZoneCard`s inside the
// page's ScrollView, and a `List` nested in a ScrollView is two scrollers
// fighting. So the same rail is drawn by hand here, to the same measurements and
// the same tints, and both surfaces read as one gesture.
//
// WHAT MAKES IT COEXIST WITH THE SCROLL. The gesture takes a 14pt minimum and
// then decides, once, on the first delta past it: a drag whose vertical
// component leads is REFUSED for its whole duration, so the page keeps
// scrolling under the finger and no rail ever peeks out mid-scroll. That
// decision is latched (`verdict`) rather than re-evaluated per frame, which is
// what stops a curved swipe from flickering between the two.
//
// The rails are painted UNDER the row and revealed by moving the row, not built
// as a stack that grows: one clip, one offset, and the row's own glass never
// changes identity.

import SwiftUI

struct SwipeRow<Content: View>: View {
    var leading: [SwipeVerb] = []
    var trailing: [SwipeVerb] = []
    var cornerRadius: CGFloat = 10
    @ViewBuilder var content: Content

    /// Width of one rail button. Wide enough for a 15pt glyph over a caption.
    private static var buttonWidth: CGFloat { 74 }
    /// How far past the rail a drag must go before letting go COMMITS the
    /// edge's primary verb instead of just resting the rail open.
    private static var commitOverrun: CGFloat { 56 }

    /// Where the row is resting between gestures (0, or a rail's width).
    @State private var resting: CGFloat = 0
    /// Live drag on top of `resting`.
    @State private var drag: CGFloat = 0
    /// Latched once per gesture: nil until the first delta past the minimum.
    @State private var verdict: Verdict?
    /// Edge-detects the commit threshold so the "you are past it" tap fires once.
    @State private var wasArmed = false

    private enum Verdict { case swipe, refused }

    private var offset: CGFloat { resting + drag }
    private var open: Bool { resting != 0 }

    private func railWidth(_ verbs: [SwipeVerb]) -> CGFloat {
        CGFloat(verbs.count) * Self.buttonWidth
    }
    private var leadingWidth: CGFloat { railWidth(leading) }
    private var trailingWidth: CGFloat { railWidth(trailing) }

    /// The verb a full swipe commits on whichever edge is showing: the
    /// destructive one if the edge has it, else the first.
    private func primary(_ verbs: [SwipeVerb]) -> SwipeVerb? {
        verbs.first { $0.destructive } ?? verbs.first
    }

    private var armed: Bool {
        if offset < 0, !trailing.isEmpty { return -offset > trailingWidth + Self.commitOverrun }
        if offset > 0, !leading.isEmpty { return offset > leadingWidth + Self.commitOverrun }
        return false
    }

    var body: some View {
        ZStack {
            // Only the rail being pulled paints, so a row at rest is exactly the
            // row and nothing is drawn behind every one of them.
            if offset > 0 {
                rail(leading, alignment: .leading, revealed: offset)
            } else if offset < 0 {
                rail(trailing, alignment: .trailing, revealed: -offset)
            }

            content
                // An OPAQUE ground under the row while it is off-centre, or the
                // rail shows straight through the row that is supposed to be
                // covering it. Canvas plus the zone wash rather than canvas
                // alone: these rows sit on a ZoneCard, so the page colour by
                // itself reads as a lighter panel sliding over a darker one.
                .background {
                    if open || drag != 0 {
                        ZStack {
                            Palette.canvas
                            Palette.glassTint
                        }
                    }
                }
                .offset(x: offset)
                // While a rail is out, the row itself is a CLOSE target, not a
                // door: a thumb reaching back for the mail it just swiped must
                // not open it.
                .overlay {
                    if open {
                        Rectangle()
                            .fill(.clear)
                            .contentShape(Rectangle())
                            .onTapGesture { close() }
                            .offset(x: offset)
                    }
                }
        }
        .clipShape(RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
        .gesture(swipe)
        .onChange(of: armed) { _, nowArmed in
            // Felt, not watched: the rail is past its commit point.
            if nowArmed, !wasArmed { Haptics.armed() }
            wasArmed = nowArmed
        }
    }

    // MARK: - rail

    private func rail(_ verbs: [SwipeVerb], alignment: Alignment, revealed: CGFloat) -> some View {
        HStack(spacing: 0) {
            ForEach(verbs) { verb in
                Button {
                    close()
                    Haptics.commit()
                    verb.run()
                } label: {
                    VStack(spacing: 3) {
                        Image(systemName: verb.symbol)
                            .font(.system(size: 15, weight: .semibold))
                        Text(verb.title)
                            .font(Typo.micro)
                    }
                    .foregroundStyle(.white)
                    .frame(width: Self.buttonWidth)
                    .frame(maxHeight: .infinity)
                    .background(verb.tint)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
        // Past the rail's own width the primary verb's colour stretches to fill
        // what the drag opened, which is what says "let go and this happens".
        .frame(maxWidth: .infinity, alignment: alignment)
        .background(
            (primary(verbs)?.tint ?? Palette.hairline)
                .opacity(revealed > railWidth(verbs) ? 1 : 0)
        )
        .frame(maxWidth: .infinity, alignment: alignment)
    }

    // MARK: - gesture

    private var swipe: some Gesture {
        DragGesture(minimumDistance: 14)
            .onChanged { value in
                if verdict == nil {
                    // ONE decision, on the first delta that clears the minimum.
                    // A vertical lead means the page is scrolling and this row is
                    // out of the conversation until the finger lifts.
                    verdict =
                        abs(value.translation.width) > abs(value.translation.height)
                        ? .swipe : .refused
                }
                guard verdict == .swipe else { return }
                let raw = value.translation.width
                // Rubber-band an edge with no verbs so the row never slides off
                // into empty space.
                if raw > 0, leading.isEmpty { drag = raw * 0.12 } else if raw < 0,
                    trailing.isEmpty
                {
                    drag = raw * 0.12
                } else {
                    drag = raw
                }
            }
            .onEnded { value in
                defer {
                    verdict = nil
                    wasArmed = false
                }
                guard verdict == .swipe else {
                    drag = 0
                    return
                }
                let landed = resting + value.translation.width
                let verbs = landed < 0 ? trailing : leading
                let width = railWidth(verbs)
                guard !verbs.isEmpty else {
                    settle(0)
                    return
                }
                let distance = abs(landed)
                if distance > width + Self.commitOverrun, let verb = primary(verbs) {
                    Haptics.commit()
                    settle(0)
                    verb.run()
                } else if distance > width / 2 {
                    settle(landed < 0 ? -width : width)
                } else {
                    settle(0)
                }
            }
    }

    private func settle(_ to: CGFloat) {
        drag = 0
        withAnimation(Motion.railSettle) { resting = to }
    }

    private func close() {
        settle(0)
    }
}
