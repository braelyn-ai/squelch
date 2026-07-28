// SIDEBAR — the slim icon rail that routes the primary views.
//
// Sitrep / Emails / Auth / Rules / Audit on top (mirroring the 1..5 keys via
// MainView.mainViews), Usage + Settings pinned below a divider and deliberately
// OUT of the digit sequence so adding them never renumbers 1..5.
//
// GLASS: the rail is one GlassEffectContainer, and the ACTIVE item carries a
// shared glassEffectID. Moving between views makes that material FLOW from one
// icon to the next rather than cross-fading — the clearest demonstration of the
// real material in the app, and something the CSS build could not do at all.
//
// No count badges: the auth arrival flow already announces a fresh code loudly
// (a countdown ring, and a modal that presents the code), so a permanent number
// on the tab was redundant nagging about mail that is mostly history.

import SwiftUI

struct SidebarRail: View {
    @Environment(AppStore.self) private var store
    let namespace: Namespace.ID
    @Namespace private var railGlass

    private static let railWidth: CGFloat = 60

    var body: some View {
        GlassEffectContainer(spacing: 14) {
            VStack(spacing: 6) {
                // Leave room for the traffic lights — the title bar is hidden
                // but the buttons still live in that corner.
                Color.clear.frame(height: 30)

                ForEach(Array(MainView.mainViews.enumerated()), id: \.element) { index, view in
                    RailButton(
                        view: view, keyNumber: index + 1, active: store.activeView == view,
                        namespace: railGlass, showRings: view == .auth)
                }

                Spacer(minLength: 8)

                Rectangle()
                    .fill(Palette.hairline)
                    .frame(width: 22, height: 0.5)
                    .padding(.vertical, 4)

                ForEach(MainView.bottomViews, id: \.self) { view in
                    RailButton(
                        view: view, keyNumber: nil, active: store.activeView == view,
                        namespace: railGlass, showRings: false)
                }
            }
            .padding(.vertical, 10)
            .frame(width: Self.railWidth)
            .frame(maxHeight: .infinity)
        }
        .background {
            // The rail is the most translucent surface in the window, but not
            // arbitrarily thin: it has to hold its own value regardless of what
            // the wallpaper is doing, or its icons stop being legible over a
            // dark desktop.
            Rectangle()
                .fill(.thinMaterial)
                .overlay(Palette.glassTint.opacity(0.5))
                .overlay(alignment: .trailing) {
                    Rectangle().fill(Palette.hairline).frame(width: 0.5)
                }
                .ignoresSafeArea()
        }
    }
}

private struct RailButton: View {
    @Environment(AppStore.self) private var store
    let view: MainView
    let keyNumber: Int?
    let active: Bool
    let namespace: Namespace.ID
    let showRings: Bool

    @State private var hovering = false

    var body: some View {
        Button {
            store.setView(view)
        } label: {
            ZStack {
                Image(systemName: view.symbol)
                    .font(.system(size: 17, weight: active ? .semibold : .regular))
                    .foregroundStyle(active ? Palette.accent : Palette.inkDim)
                    .frame(width: 44, height: 36)
                    // The material goes on the ICON ITSELF, not in a background
                    // layer. A GlassEffectContainer deliberately raises its
                    // descendants above its content view, so a glass pane added
                    // as a sibling background rendered ON TOP of the glyph and
                    // washed it out. Applying the effect here makes the icon the
                    // glass's content, which is where it belongs anyway.
                    .glassSelector(active, id: "rail-active", in: namespace)

                if showRings { AuthRingsOverlay() }
            }
        }
        .buttonStyle(.plain)
        .background {
            if hovering && !active {
                RoundedRectangle(cornerRadius: 11, style: .continuous)
                    .fill(Palette.hairline.opacity(0.7))
                    .frame(width: 44, height: 36)
            }
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
