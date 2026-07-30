// SIDEBAR — the slim icon rail that routes the primary views.
//
// Sitrep / Emails / Auth / Rules / Audit on top (mirroring the 1..5 keys via
// MainView.mainViews), Usage + Settings pinned below a divider and deliberately
// OUT of the digit sequence so adding them never renumbers 1..5.
//
// GLASS: ONE selector pane lives in a layer BEHIND the icons and travels to
// whichever is active, so the material slides between tabs while every glyph
// stays exactly where it is.
//
// It was built the other way first — the effect applied to each icon, with a
// shared `glassEffectID` inside a `GlassEffectContainer`, so the container
// would morph it from one to the next. Two things were wrong with that, both
// visible on screen:
//
//   * The glass's CONTENT is whatever it is applied to, so morphing carried the
//     ICON along with the material and the glyph flew between tabs.
//   * `.glassEffect` on every button put an interactive material over all seven
//     of them, which swallowed clicks — a tab often needed three or four
//     presses to route.
//
// A conditional `if active { glassEffect(…) }` avoids both, but a conditional
// modifier is two view identities, so every selection change tore the icon's
// subtree down and built a new one: that is the deselect FLICKER, and it is
// also why the morph never worked (a glassEffectID cannot be matched across a
// view that no longer exists). Hence one long-lived pane that moves, addressed
// by geometry rather than by identity.
//
// NO GlassEffectContainer HERE. A container hoists its descendants above its
// content view, which is what made a background pane render ON TOP of the glyph
// and forced the effect onto the icon in the first place. With a single pane
// there is nothing to merge or morph, so the container earns nothing and costs
// the one layering property this layout depends on.
//
// No count badges: the auth arrival flow already announces a fresh code loudly
// (a countdown ring, and a modal that presents the code), so a permanent number
// on the tab was redundant nagging about mail that is mostly history.

import SwiftUI

/// The rail's coordinate space. FILE-SCOPE rather than a static on the view:
/// the buttons read it from inside `onGeometryChange`'s Sendable closure, and a
/// static on a `@MainActor` View is main-actor isolated there.
private let railSpace = "sidebar-rail"

struct SidebarRail: View {
    @Environment(AppStore.self) private var store
    let namespace: Namespace.ID

    /// Where each icon sits, in the rail's own space, so the selector can be
    /// placed over the active one. Written by the buttons as they lay out.
    @State private var slots: [MainView: CGRect] = [:]
    /// True for the span of a slide, so the pane can firm up while it moves.
    @State private var traveling = false

    static let railWidth: CGFloat = 60
    static let iconWidth: CGFloat = 44
    static let iconHeight: CGFloat = 36
    static let selectorRadius: CGFloat = 11

    /// How long the pane takes to cross. Short enough to feel like a response
    /// to the click rather than a thing you wait out.
    private static let travel: Animation = .smooth(duration: 0.32)
    /// Tint the pane carries at rest, and the extra it picks up mid-travel.
    private static let restTint: Double = 0.26
    private static let travelTint: Double = 0.16

    /// Height of the strip the traffic lights live in, left unpainted by the
    /// rail's material below. Sits in the ~21pt of clear space between the
    /// bottom of the dots and the top of the first rail icon.
    static let titleBarHeight: CGFloat = 24

    var body: some View {
        railStack
            .coordinateSpace(.named(railSpace))
            // BEHIND the icons, and behind them for real: this is the layering
            // a GlassEffectContainer would have inverted. Ordered before the
            // rail material below so the material stays further back still.
            .background(alignment: .topLeading) { selector }
            // WHAT MAKES IT SLIDE — and only for a click. The selector is one
            // view whose offset is a function of the active view, so animating
            // that value moves it; handing `nil` instead snaps it.
            //
            // A pointer route is a continuous gesture and the travel reads as
            // the answer to it. A digit is instantaneous, so the same travel
            // only delays the answer by its own duration — those want opposite
            // treatment, and `routeWasPointer` is set in the very update that
            // changes `activeView`, so the right one is always in hand.
            //
            // Scoped HERE rather than a `withAnimation` at the call site: the
            // main content swap hangs off the same property, and a transaction
            // around `setView` would animate that too.
            .animation(store.routeWasPointer ? Self.travel : nil, value: store.activeView)
            .onChange(of: store.activeView) { _, _ in
                guard store.routeWasPointer else { return }
                // FIRMER IN FLIGHT. The pane gains a little opacity while it
                // crosses the gap and gives it back on arrival, so the motion
                // reads as one object moving rather than a tint fading along
                // the rail. Rising faster than it falls keeps the weight at the
                // start of the travel, where the eye picks it up.
                withAnimation(.easeOut(duration: 0.10)) { traveling = true }
                Task {
                    try? await Task.sleep(for: .seconds(0.12))
                    withAnimation(.easeIn(duration: 0.20)) { traveling = false }
                }
            }
            .background { railMaterial }
    }

    /// The travelling pane. Absent only until the first layout pass has
    /// reported where anything is; after that it is ONE view for the life of
    /// the rail, which is what lets its offset animate instead of it being
    /// rebuilt somewhere else.
    @ViewBuilder
    private var selector: some View {
        if let slot = slots[store.activeView] {
            Color.clear
                .frame(width: slot.width, height: slot.height)
                .glassEffect(
                    .regular.tint(Palette.accent.opacity(Self.restTint)),
                    in: .rect(cornerRadius: Self.selectorRadius, style: .continuous)
                )
                // The travel tint is an OVERLAY, not a change to the Glass
                // value: `Glass` is not animatable, so tinting the material
                // itself would jump to the new value and back. A colour's
                // opacity interpolates, so the firming reads as continuous.
                .overlay {
                    RoundedRectangle(cornerRadius: Self.selectorRadius, style: .continuous)
                        .fill(Palette.accent.opacity(traveling ? Self.travelTint : 0))
                }
                // NOT `.interactive()`. That material tracks the pointer and
                // takes the hits it tracks, and this pane sits over whichever
                // button is active — an interactive selector is what made a
                // tab need several clicks to route.
                .allowsHitTesting(false)
                .offset(x: slot.minX, y: slot.minY)
        }
    }

    private var railStack: some View {
            VStack(spacing: 6) {
                // NO TOP SPACER. The first icon sits on the SAME LINE as the
                // "squelch" wordmark beside it, which is what makes the rail
                // read as part of the page header rather than as a strip that
                // starts late. A 30pt spacer used to hold it 36pt below that
                // line (30 plus the stack's own 6) to clear the traffic lights
                // — but they end ~19pt above where the icon now starts, so the
                // clearance was paying for itself twice.
                ForEach(Array(MainView.mainViews.enumerated()), id: \.element) { index, view in
                    RailButton(
                        view: view, keyNumber: index + 1, active: store.activeView == view,
                        showRings: view == .auth, onSlot: { slots[view] = $0 })
                }

                Spacer(minLength: 8)

                Rectangle()
                    .fill(Palette.hairline)
                    .frame(width: 22, height: 0.5)
                    .padding(.vertical, 4)

                ForEach(MainView.bottomViews, id: \.self) { view in
                    RailButton(
                        view: view, keyNumber: nil, active: store.activeView == view,
                        showRings: false, onSlot: { slots[view] = $0 })
                }
            }
            .padding(.vertical, 10)
            .frame(width: Self.railWidth)
            .frame(maxHeight: .infinity)
    }

    private var railMaterial: some View {
            // The rail is the most translucent surface in the window, but not
            // arbitrarily thin: it has to hold its own value regardless of what
            // the wallpaper is doing, or its icons stop being legible over a
            // dark desktop.
            //
            // IT STARTS BELOW THE TITLE BAR. The window is .hiddenTitleBar with
            // a full-size content view, so this material used to run up behind
            // the traffic lights: the three dots sat on the rail's own colour
            // while everything to their right sat on the window backdrop, and
            // that mismatch read as the dots CLIPPING the sidebar.
            //
            // Leaving the strip UNPAINTED is what makes the top bar exactly the
            // background — it IS the background, the same WindowBackdrop the
            // rest of the window sits on, so it cannot drift out of match with
            // the theme or the palette. Painting a second copy into a 24pt box
            // does not work: the backdrop's gradient is relative to its own
            // frame, so a short box compresses it and the seam shows.
            Rectangle()
                .fill(.thinMaterial)
                .overlay(Palette.glassTint.opacity(0.5))
                .overlay(alignment: .trailing) {
                    Rectangle().fill(Palette.hairline).frame(width: 0.5)
                }
                .padding(.top, Self.titleBarHeight)
                .ignoresSafeArea(edges: .bottom)
    }
}

private struct RailButton: View {
    @Environment(AppStore.self) private var store
    let view: MainView
    let keyNumber: Int?
    let active: Bool
    let showRings: Bool
    /// Where this icon landed, in the rail's space, so the rail can park the
    /// selector on it.
    let onSlot: (CGRect) -> Void

    @State private var hovering = false

    var body: some View {
        Button {
            store.setView(view, viaPointer: true)
        } label: {
            ZStack {
                Image(systemName: view.symbol)
                    .font(.system(size: 17, weight: active ? .semibold : .regular))
                    .foregroundStyle(active ? Palette.accent : Palette.inkDim)
                    .frame(width: SidebarRail.iconWidth, height: SidebarRail.iconHeight)
                    // The rail's selector is drawn from this, one layer down —
                    // nothing about the glyph itself changes with selection
                    // except its weight and colour, so it never moves and its
                    // subtree is never rebuilt.
                    .onGeometryChange(for: CGRect.self) {
                        $0.frame(in: .named(railSpace))
                    } action: { onSlot($0) }

                if showRings { AuthRingsOverlay() }
            }
            // THE WHOLE TILE IS THE TARGET. Without this the hit region is the
            // GLYPH, and every rail symbol is an OUTLINE — a click through the
            // middle of the envelope, or between the slider bars, lands in a
            // hole and the tab silently ignores it. That is the "takes one to
            // four clicks" bug: the misses are wherever the strokes are not.
            //
            // The hover wash never showed it, because `.onHover` and the
            // background below sit OUTSIDE the button and always covered the
            // full tile — so the rail lit up everywhere it would not click.
            // Same trap TextActionStyle documents for text labels.
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background {
            // Opacity rather than an `if`: a conditional here is two view
            // identities, and swapping between them on hover throws away the
            // button's subtree for the sake of a wash.
            RoundedRectangle(cornerRadius: SidebarRail.selectorRadius, style: .continuous)
                .fill(Palette.hairline.opacity(hovering && !active ? 0.7 : 0))
                .frame(width: SidebarRail.iconWidth, height: SidebarRail.iconHeight)
        }
        .onHover { hovering = $0 }
        .help(keyNumber.map { "\(view.label) · \($0)" } ?? view.label)
        .accessibilityLabel(view.label)
    }
}

/// AUTH COUNTDOWN RING — the 60s sweep drawn over the auth rail icon when a
/// fresh auth message arrives. One ring per active AuthRing in the store; when
/// its sweep completes it removes itself. Ambient by design.
private struct AuthRingsOverlay: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        ForEach(store.authRings) { ring in
            AuthRingView(ring: ring)
        }
    }
}

private struct AuthRingView: View {
    @Environment(AppStore.self) private var store
    let ring: AuthRing
    @State private var progress: CGFloat = 1

    private static let size: CGFloat = 34

    var body: some View {
        Circle()
            .trim(from: 0, to: progress)
            .stroke(Palette.lock, style: StrokeStyle(lineWidth: 2, lineCap: .round))
            .rotationEffect(.degrees(-90))
            .frame(width: Self.size, height: Self.size)
            .allowsHitTesting(false)
            .task {
                // If a late mount lands mid-sweep, start partway so the ring
                // still finishes ~60s after it was armed (never over-runs).
                let elapsed = Date().timeIntervalSince(ring.startedAt)
                let remaining = max(0, ringSeconds - elapsed)
                guard remaining > 0 else {
                    store.expireAuthRing(ring.id)
                    return
                }
                progress = CGFloat(remaining / ringSeconds)
                withAnimation(.linear(duration: remaining)) { progress = 0 }
                try? await Task.sleep(for: .seconds(remaining))
                store.expireAuthRing(ring.id)
            }
    }
}
