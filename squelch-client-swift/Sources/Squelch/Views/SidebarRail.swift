// SIDEBAR — the slim icon rail that routes the primary views.
//
// Sitrep / Emails / Auth / Rules / Audit on top (mirroring the 1..5 keys via
// MainView.mainViews), Usage + Settings pinned below a divider and deliberately
// OUT of the digit sequence so adding them never renumbers 1..5.
//
// GLASS: the rail is one GlassEffectContainer, and the ACTIVE item is its own
// tinted glass capsule carrying a shared glassEffectID. Moving between views
// makes that capsule FLOW from one icon to the next through the rail's material
// rather than cross-fading — the single most legible demonstration of the real
// material in the app, and something the CSS build could not do at all.

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
                        namespace: railGlass, showRings: view == .auth,
                        badge: badge(for: view))
                }

                Spacer(minLength: 8)

                Rectangle()
                    .fill(Palette.hairline)
                    .frame(width: 22, height: 0.5)
                    .padding(.vertical, 4)

                ForEach(MainView.bottomViews, id: \.self) { view in
                    RailButton(
                        view: view, keyNumber: nil, active: store.activeView == view,
                        namespace: railGlass, showRings: false, badge: nil)
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

    /// Unread-ish counts the rail is allowed to shout about. Auth is the only
    /// one that earns a badge: a login code arriving is time-critical.
    private func badge(for view: MainView) -> Int? {
        guard view == .auth else { return nil }
        let count = store.sitrep.sealed.count
        return count > 0 ? count : nil
    }
}

private struct RailButton: View {
    @Environment(AppStore.self) private var store
    let view: MainView
    let keyNumber: Int?
    let active: Bool
    let namespace: Namespace.ID
    let showRings: Bool
    let badge: Int?

    @State private var hovering = false

    var body: some View {
        Button {
            store.setView(view)
        } label: {
            ZStack {
                Image(systemName: view.symbol)
                    // iOS tab-bar sizing: a solid glyph at a single weight, with
                    // the ACTIVE state carried by color + the glass capsule
                    // behind it rather than by a heavier stroke.
                    .font(.system(size: 17))
                    .symbolVariant(.fill)
                    .foregroundStyle(active ? Palette.accent : Palette.inkDim)
                    .frame(width: 40, height: 34)

                if showRings { AuthRingsOverlay() }

                if let badge {
                    Text("\(badge)")
                        .font(Typo.num(9, weight: .bold))
                        .foregroundStyle(.white)
                        .padding(.horizontal, 4)
                        .padding(.vertical, 1)
                        .background(Capsule().fill(Palette.lock))
                        .offset(x: 13, y: -11)
                }
            }
        }
        .buttonStyle(.plain)
        .background {
            if active {
                // The morphing capsule: one glassEffectID shared across every
                // rail item means the material FLOWS to the newly-active one.
                Color.clear
                    .frame(width: 44, height: 36)
                    .glassEffect(
                        .regular.tint(Palette.accent.opacity(0.26)).interactive(),
                        in: .rect(cornerRadius: 11, style: .continuous)
                    )
                    .glassEffectID("rail-active", in: namespace)
            } else if hovering {
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
