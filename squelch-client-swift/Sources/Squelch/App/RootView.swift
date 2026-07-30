// App shell. Boots settings from the keychain, shows Connect until connected,
// then stacks, bottom to top: rail + routed main view, side panels, the
// fullscreen thread viewer, the action layer (toasts, compose, palettes).
//
// The global 1..5 / ⌘[ / ⌘] / ⌘K bindings register HERE in the "global"
// context, so they compose with whatever context is active rather than being
// gated by it — nav must work even from inside a modal panel.

import SwiftUI

struct RootView: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        Group {
            switch store.connStatus {
            case .loading:
                LoadingGate()
            case .connected:
                MainShell()
            default:
                ConnectView()
            }
        }
        .task {
            // Pay WebKit's process-launch cost at boot rather than on the first
            // email the reader opens.
            EmailWebView.warmProcess()
            await store.loadSettings()
        }
        .onChange(of: store.connStatus) { _, status in
            if status == .connected {
                SitrepPoller.shared.start()
                EventStream.shared.start()
            } else {
                SitrepPoller.shared.stop()
                EventStream.shared.stop()
            }
        }
    }
}

private struct LoadingGate: View {
    var body: some View {
        VStack(spacing: 14) {
            Text("squelch")
                .font(Typo.serif(34, weight: .medium))
                .foregroundStyle(Palette.ink)
            ProgressView()
                .controlSize(.small)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct MainShell: View {
    @Environment(AppStore.self) private var store
    @Namespace private var shellGlass

    var body: some View {
        @Bindable var store = store

        ZStack(alignment: .topLeading) {
            // EVERYTHING A MODAL SITS ON TOP OF, blurred as one layer while a
            // modal is up. Blurring is what keeps the app visible behind ⌘K:
            // layout and colour survive a defocus, a material scrim paints
            // over them.
            ZStack(alignment: .topLeading) {
            HStack(spacing: 0) {
                SidebarRail(namespace: shellGlass)
                VStack(spacing: 0) {
                    ConnectionBanner()
                    // Daemon down with nothing ever loaded: empty bands would
                    // read as an empty inbox. Settings stays reachable so the
                    // token/URL can be fixed.
                    if store.daemonDown && store.activeView != .settings {
                        DaemonDownPane()
                    } else {
                        routedBody
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }

            // Side panel surfaces (browse / search).
            if store.sideView.isOpen {
                SidePanel()
                    .transition(.move(edge: .trailing).combined(with: .opacity))
                    .zIndex(10)
            }

            // Email viewer — above the side panels, below the action layer so
            // compose/undo/palette stay on top. It fills the window EXCEPT an
            // open side panel's strip: opening a hit from search must leave the
            // results visible beside the email. Esc closes the reader, a second
            // Esc the panel.
            if let threadId = store.threadId {
                HStack(spacing: 0) {
                    // THE RAIL STAYS: the reader insets past it instead of
                    // covering it, so 1..5 stay clickable while you read.
                    Color.clear
                        .frame(width: SidebarRail.railWidth)
                        .allowsHitTesting(false)
                    ThreadViewer(threadId: threadId)
                        .id(threadId)
                    if store.sideView.isOpen {
                        // Reserves the strip without taking clicks off the panel
                        // sitting under it.
                        Color.clear
                            .frame(width: sidePanelWidth)
                            .allowsHitTesting(false)
                    }
                }
                // NO TRANSITION. Opening an email is a jump, not a dissolve: a
                // crossfade shows two surfaces at once on a surface the reader
                // enters and leaves constantly, which reads as lag.
                .zIndex(20)
            }
            }
            .blur(radius: store.modalOverlayOpen ? 9 : 0)

            // Global overlays: undo toasts, compose ceremony, palettes. OUTSIDE
            // the blurred stack — the modal itself must stay sharp.
            ActionLayer()
                .zIndex(30)
        }
        .animation(.easeOut(duration: 0.18), value: store.modalOverlayOpen)
        .animation(.smooth(duration: 0.22), value: store.sideView)
        // threadId is deliberately NOT animated — opening a thread is a jump.
        .keyBindings(.global, globalBindings)
        .onChange(of: store.sitrep.sealed) { _, sealed in
            AuthArrival.shared.observe(sealed: sealed)
        }
    }

    @ViewBuilder
    private var routedBody: some View {
        switch store.activeView {
        case .sitrep: SitrepView()
        case .emails: EmailsView()
        case .auth: RoutedHost(view: .auth) { AuthView() }
        case .rules: RoutedHost(view: .rules) { RulesView() }
        case .audit: RoutedHost(view: .audit) { AuditView() }
        case .usage: UsageView()
        case .settings: SettingsView()
        }
    }

    /// 1..5 view nav + ⌘[ back / ⌘] forward + ⌘K ask bar. The ⌘ chords use the
    /// `meta` flag so a bare "[" / "]" never triggers them; `allowInInput` keeps
    /// them working with a search/compose field focused, since a chord is not a
    /// typed character.
    private var globalBindings: [KeyBinding] {
        var bindings: [KeyBinding] = MainView.mainViews.enumerated().map { index, view in
            KeyBinding("\(index + 1)", "go to \(view.rawValue)") { store.setView(view) }
        }
        bindings.append(
            KeyBinding("[", "back", meta: true, allowInInput: true) { store.goBack() })
        bindings.append(
            KeyBinding("]", "forward", meta: true, allowInInput: true) { store.goForward() })
        bindings.append(
            KeyBinding("k", "ask your inbox", meta: true, allowInInput: true) {
                store.askBarOpen = true
            })
        // Global, not list-only: `/`, `\` and `?` must work on every surface,
        // including the sitrep the app lands on. All three stay input-guarded
        // (no allowInInput), so typing them into a field still types the
        // character.
        bindings.append(
            KeyBinding("/", "search") { store.openSearch() })
        bindings.append(
            KeyBinding("\\", "toggle light/dark theme") { Prefs.shared.flipTheme() })
        bindings.append(
            KeyBinding("?", "keyboard shortcuts") { store.shortcutsOpen.toggle() })
        return bindings
    }
}

/// Host for the full main views behind the rail: Auth / Rules / Audit.
///
/// These inner views register their list-style keys into the "modal" KeyContext
/// and never push a context themselves, so this host pushes it while mounted.
/// Only one routed view is mounted at a time, so there is never a competing
/// "list" set active; the global 1..5 keys still fire because "global"
/// composes with "modal".
struct RoutedHost<Content: View>: View {
    @Environment(AppStore.self) private var store
    let view: MainView
    @ViewBuilder var content: Content

    private var title: String {
        switch view {
        case .rules: "Rules — sender rules"
        case .audit: "Audit — agent & app actions"
        default: view.label
        }
    }

    var body: some View {
        Group {
            if view == .auth {
                // Auth owns its entire surface (own header band, two-column
                // body), so it opts out of the shared chrome.
                content
            } else {
                VStack(alignment: .leading, spacing: 0) {
                    RoutedHeader(title: title)
                    content
                }
            }
        }
        .keyContext(.modal)
        // Esc = back to the sitrep. Overlays these views open push their OWN
        // contexts above this one, so their Esc wins while they're up and this
        // fires only from the bare list.
        .keyBindings(.modal, [
            KeyBinding("Escape", "back to sitrep") { store.setView(.sitrep) }
        ])
    }
}

/// The shared page header for routed views.
struct RoutedHeader<Trailing: View>: View {
    let title: String
    @ViewBuilder var trailing: Trailing

    init(title: String, @ViewBuilder trailing: () -> Trailing) {
        self.title = title
        self.trailing = trailing()
    }

    var body: some View {
        HStack(spacing: 10) {
            Text(title)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Palette.ink)
            Spacer(minLength: 8)
            trailing
        }
        .padding(.horizontal, 22)
        .padding(.vertical, 14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .bottom) { Hairline() }
    }
}

extension RoutedHeader where Trailing == EmptyView {
    init(title: String) {
        self.init(title: title) { EmptyView() }
    }
}
