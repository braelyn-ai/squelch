// SIDEBAR — the slim icon rail: Sitrep / Emails / Auth / Rules / Audit on the
// 1..5 keys; Share + Usage + Settings sit below a divider, out of that sequence
// so adding one never renumbers it. Share is the odd one there: it opens a sheet
// rather than routing, so it carries no selector (see `shareButton`). ONE long-lived pane behind the icons is the selector,
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
    /// The share tile's own hover, which `RailButton` keeps for itself.
    @State private var shareHovering = false

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
                // NO TOP SPACER: the clearance is the top bar's, applied to the
                // whole stack below. The first icon starts UNDER the page header
                // beside it rather than on its line — the rail is what the bar
                // runs above, so an icon level with the title would be an icon
                // sitting in the bar.
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

                shareButton

                ForEach(MainView.bottomViews, id: \.self) { view in
                    RailButton(
                        view: view, keyNumber: nil, active: store.activeView == view,
                        showRings: false, onSlot: { slots[view] = $0 })
                }

                accountBadge
            }
            // The app draws from the window's true top edge now, so this is the
            // rail's whole clearance for the title strip — not a nudge on top of
            // a safe area that no longer applies.
            .padding(.top, TopBar.height + 10)
            .padding(.bottom, 10)
            .frame(width: Self.railWidth)
            .frame(maxHeight: .infinity)
            .ignoresSafeArea(edges: .top)
    }

    /// HAND A FRIEND AN INVITE. Above Usage in the bottom group, because it
    /// belongs with the things that are about your account rather than with the
    /// five destinations on the number keys.
    ///
    /// Deliberately NOT a `RailButton`, for the same reason `accountBadge` is
    /// not: it opens a sheet rather than routing, so it has no `MainView` to be
    /// active for, takes no selector pane, and reports no slot. What it does
    /// borrow is the tile — the same 44x36 target and the same hover wash — so
    /// it reads as one of the rail's own rather than as a thing pinned to it.
    ///
    /// ABSENT unless the daemon says it can share, like the button in Settings.
    /// A rail is 60 points wide and its whole argument is that everything in it
    /// earns the space; an icon that can only refuse does not.
    @ViewBuilder
    private var shareButton: some View {
        if store.shareAvailable {
            Button {
                store.shareSheetOpen = true
            } label: {
                Image(systemName: "gift")
                    .font(.system(size: 16, weight: .regular))
                    .foregroundStyle(Palette.inkDim)
                    .frame(width: Self.iconWidth, height: Self.iconHeight)
                    // THE WHOLE TILE IS THE TARGET, for the reason RailButton
                    // spells out: every rail symbol is an outline, and a click
                    // through the middle of one otherwise lands in a hole.
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .background {
                RoundedRectangle(cornerRadius: Self.selectorRadius, style: .continuous)
                    .fill(Palette.hairline.opacity(shareHovering ? 0.7 : 0))
                    .frame(width: Self.iconWidth, height: Self.iconHeight)
            }
            .onHover { shareHovering = $0 }
            .help("share Passband with a friend")
            .accessibilityLabel("share Passband")
        }
    }

    /// WHICH MAILBOX YOU ARE IN, at the foot of the rail — and the fastest way
    /// to another one. Absent with a single account: a switcher between one
    /// thing is chrome, and the rail's whole argument is that it holds only
    /// what earns its 60 points.
    ///
    /// Deliberately NOT a `RailButton`: it routes nowhere, so it takes no
    /// selector pane and reports no slot.
    @ViewBuilder
    private var accountBadge: some View {
        let manager = AccountManager.shared
        if manager.accounts.count > 1 {
            Menu {
                // No `.keyboardShortcut` on these, deliberately: the Accounts
                // menu in the menu bar owns the ⌘numbers, and a second
                // registration of the same chord is a conflict, not a shortcut.
                ForEach(manager.accounts) { account in
                    Button {
                        Task { await manager.switchTo(account.id) }
                    } label: {
                        if account.id == manager.activeId {
                            Label(account.displayName, systemImage: "checkmark")
                        } else {
                            Text(account.displayName)
                        }
                    }
                }
                Divider()
                Button("Add Account…") { store.addAccountSheetOpen = true }
            } label: {
                Text(manager.active?.initial ?? "?")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Palette.accent)
                    .frame(width: 26, height: 26)
                    .background(Circle().fill(Palette.accentSoft))
                    .overlay(Circle().strokeBorder(Palette.accent.opacity(0.35), lineWidth: 0.75))
            }
            // The default menu chrome is a bordered well with a chevron, which
            // in a 60pt icon rail reads as a broken button. The badge IS the
            // affordance.
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .frame(width: Self.iconWidth, height: Self.iconHeight)
            .help(manager.active.map { "account: \($0.displayName)" } ?? "accounts")
            .accessibilityLabel("switch account")
        }
    }

    /// The one rounded corner the rail owns: the top-trailing shoulder, where
    /// the material tucks under the top bar. The other three corners meet
    /// window edges and stay square.
    private static let shoulderRadius: CGFloat = 12

    private var railMaterial: some View {
            // The most translucent surface in the window, but not arbitrarily
            // thin: it must hold its value over any wallpaper or its icons stop
            // being legible. It runs up into the top safe area and stops under
            // the TOP BAR — the same line the page's own header ends on, so the
            // rail's top edge and that header's rule read as one rule across the
            // window, dipping only for the shoulder. Above it is the strip the
            // traffic lights live in, left unpainted: material running up BEHIND
            // the dots reads as the dots clipping the rail.
            //
            // FULLSCREEN HAS NO TRAFFIC LIGHTS — the strip they live in is not
            // on screen, so the rail runs to the true top instead of reserving
            // it, and the shoulder goes square: a corner on a window edge, like
            // the other three.
            let fullscreen = WindowState.shared.isFullscreen
            let radius = fullscreen ? 0 : Self.shoulderRadius
            let shape = UnevenRoundedRectangle(
                topLeadingRadius: 0, bottomLeadingRadius: 0,
                bottomTrailingRadius: 0, topTrailingRadius: radius,
                style: .continuous)
            return shape
                .fill(.thinMaterial)
                .overlay(shape.fill(Palette.glassTint.opacity(0.5)))
                // The hairline follows the silhouette — trailing edge, shoulder,
                // top — where a straight trailing rule would cut the curve.
                .overlay(
                    RailEdge(radius: radius)
                        .stroke(Palette.hairline, lineWidth: 0.5)
                )
                .padding(.top, fullscreen ? 0 : TopBar.height)
                .ignoresSafeArea(edges: [.top, .bottom])
    }
}

/// The rail material's visible edge: up the trailing side, around the
/// top-trailing shoulder, and along the top to the window's edge. The leading
/// and bottom edges sit on window edges and draw nothing.
private struct RailEdge: Shape {
    var radius: CGFloat

    func path(in rect: CGRect) -> Path {
        var p = Path()
        p.move(to: CGPoint(x: rect.maxX, y: rect.maxY))
        p.addLine(to: CGPoint(x: rect.maxX, y: rect.minY + radius))
        // Square (fullscreen): the top edge IS the window's edge, and window
        // edges draw nothing.
        guard radius > 0 else { return p }
        p.addQuadCurve(
            to: CGPoint(x: rect.maxX - radius, y: rect.minY),
            control: CGPoint(x: rect.maxX, y: rect.minY))
        p.addLine(to: CGPoint(x: rect.minX, y: rect.minY))
        return p
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
