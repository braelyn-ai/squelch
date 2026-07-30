// SIDEBAR — the slim icon rail: Sitrep / Emails / Auth / Rules / Audit on the
// 1..5 keys; Usage + Settings sit below a divider, out of that sequence so adding
// one never renumbers it. ONE long-lived pane behind the icons is the selector,
// moved by GEOMETRY not identity: per-icon glass drags the glyph along and
// swallows clicks, and a GlassEffectContainer would hoist the pane over it.

import SwiftUI

/// The rail's coordinate space. FILE-SCOPE, not a static on the view: the
/// buttons read it inside `onGeometryChange`'s Sendable closure, where a static
/// on a `@MainActor` View is main-actor isolated.
private let railSpace = "sidebar-rail"

struct SidebarRail: View {
    @Environment(AppStore.self) private var store
    let namespace: Namespace.ID

    /// Where each icon sits in the rail's own space, written by the buttons as
    /// they lay out, so the selector can be placed over the active one.
    @State private var slots: [MainView: CGRect] = [:]
    /// True for the span of a slide, so the pane can firm up while it moves.
    @State private var traveling = false

    static let railWidth: CGFloat = 60
    static let iconWidth: CGFloat = 44
    static let iconHeight: CGFloat = 36
    static let selectorRadius: CGFloat = 11

    /// How long the pane takes to cross — short enough to read as a response to
    /// the click rather than a thing you wait out.
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
            // BEHIND the icons, and ordered before the rail material below so
            // that material stays further back still.
            .background(alignment: .topLeading) { selector }
            // WHAT MAKES IT SLIDE, and only for a pointer route: the travel
            // reads as the answer to a continuous gesture, while a digit is
            // instantaneous and the same travel would only delay it. Scoped HERE
            // rather than `withAnimation` at the call site — the main content
            // swap hangs off the same property and would animate too.
            .animation(store.routeWasPointer ? Self.travel : nil, value: store.activeView)
            .onChange(of: store.activeView) { _, _ in
                guard store.routeWasPointer else { return }
                // Firmer in flight: the pane gains opacity while it crosses and
                // gives it back on arrival, rising faster than it falls so the
                // weight sits at the start of the travel where the eye is.
                withAnimation(.easeOut(duration: 0.10)) { traveling = true }
                Task {
                    try? await Task.sleep(for: .seconds(0.12))
                    withAnimation(.easeIn(duration: 0.20)) { traveling = false }
                }
            }
            .background { railMaterial }
    }

    /// The travelling pane: absent only until the first layout pass reports a
    /// slot, then ONE view for the life of the rail — which is what lets its
    /// offset animate instead of it being rebuilt somewhere else.
    @ViewBuilder
    private var selector: some View {
        if let slot = slots[store.activeView] {
            Color.clear
                .frame(width: slot.width, height: slot.height)
                .glassEffect(
                    .regular.tint(Palette.accent.opacity(Self.restTint)),
                    in: .rect(cornerRadius: Self.selectorRadius, style: .continuous)
                )
                // The travel tint is an OVERLAY, not a change to the Glass value:
                // `Glass` is not animatable, so tinting the material itself would
                // jump. A colour's opacity interpolates.
                .overlay {
                    RoundedRectangle(cornerRadius: Self.selectorRadius, style: .continuous)
                        .fill(Palette.accent.opacity(traveling ? Self.travelTint : 0))
                }
                // NOT `.interactive()`: that material takes the hits it tracks,
                // and this pane sits over whichever button is active, so an
                // interactive selector eats that tab's clicks.
                .allowsHitTesting(false)
                .offset(x: slot.minX, y: slot.minY)
        }
    }

    private var railStack: some View {
            VStack(spacing: 6) {
                // NO TOP SPACER: the first icon sits on the SAME LINE as the
                // "squelch" wordmark beside it, which is what makes the rail read
                // as part of the page header. The traffic lights end ~19pt above
                // where it starts, so no extra clearance is needed.
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
            // The most translucent surface in the window, but not arbitrarily
            // thin: it must hold its value over any wallpaper or its icons stop
            // being legible. It STARTS BELOW THE TITLE BAR — the window is
            // .hiddenTitleBar with a full-size content view, so material running
            // up behind the traffic lights reads as the dots clipping the rail.
            // The strip must stay UNPAINTED rather than take a second copy of the
            // backdrop: that gradient is relative to its own frame, so a 24pt box
            // compresses it and the seam shows.
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
                    // The rail's selector is placed from this. Nothing about the
                    // glyph changes with selection except weight and colour, so
                    // it never moves and its subtree is never rebuilt.
                    .onGeometryChange(for: CGRect.self) {
                        $0.frame(in: .named(railSpace))
                    } action: { onSlot($0) }

                if showRings { AuthRingsOverlay() }
            }
            // THE WHOLE TILE IS THE TARGET. Without this the hit region is the
            // GLYPH, and every rail symbol is an OUTLINE — a click through the
            // middle of the envelope lands in a hole and the tab silently ignores
            // it. The hover wash hides the bug: `.onHover` sits outside the
            // button and lights up the full tile either way.
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background {
            // Opacity rather than an `if`: a conditional is two view identities,
            // and swapping them on hover throws away the button's subtree.
            RoundedRectangle(cornerRadius: SidebarRail.selectorRadius, style: .continuous)
                .fill(Palette.hairline.opacity(hovering && !active ? 0.7 : 0))
                .frame(width: SidebarRail.iconWidth, height: SidebarRail.iconHeight)
        }
        .onHover { hovering = $0 }
        .help(keyNumber.map { "\(view.label) · \($0)" } ?? view.label)
        .accessibilityLabel(view.label)
    }
}

/// AUTH COUNTDOWN RINGS — a 60s sweep over the auth rail icon per active
/// AuthRing in the store; each removes itself when its sweep completes.
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
                // A late mount starts partway, so the ring still finishes ~60s
                // after it was armed and never over-runs.
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
