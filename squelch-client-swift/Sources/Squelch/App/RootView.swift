// App shell. Boots settings from the keychain, shows Connect until connected,
// then mounts the rail + the routed main view + the side-panel/overlay surfaces.
//
// LAYERING (bottom to top), matching the desktop client's z-model exactly:
//   0. rail + routed main view
//   1. side panels (browse / search)
//   2. the fullscreen thread viewer
//   3. the action layer — toasts, compose, palettes, the code modal
//
// The global 1..5 / ⌘[ / ⌘] / ⌘K bindings are registered HERE in the "global"
// context, so they compose with whatever context is active (list / sitrep /
// modal / thread) rather than being gated by it — nav must always work, even
// from inside a modal panel.
//
// Ported from squelch-desktop/src/App.tsx.

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
            // email the reader opens. See EmailWebView.warmProcess.
            EmailWebView.warmProcess()
            await store.loadSettings()
        }
        .onChange(of: store.connStatus) { _, status in
            if status == .connected { SitrepPoller.shared.start() } else { SitrepPoller.shared.stop() }
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
            // modal is up. Blurring the content is what keeps the app VISIBLE
            // behind ⌘K — layout and colour survive a defocus, where a material
            // scrim just paints over them.
            ZStack(alignment: .topLeading) {
            HStack(spacing: 0) {
                SidebarRail(namespace: shellGlass)
                VStack(spacing: 0) {
                    ConnectionBanner()
                    // Daemon down with nothing ever loaded: there is no data to
                    // show, so showing empty bands would LIE. Settings stays
                    // reachable so the token/URL can be fixed.
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
            // compose/undo/palette stay on top. It fills the window EXCEPT for
            // an open side panel's strip: opening a hit from search must leave
            // the results you're working through visible beside the email, not
            // swallow them. Esc closes the reader, a second Esc the panel.
            if let threadId = store.threadId {
                HStack(spacing: 0) {
                    // THE RAIL STAYS. Reading an email is not leaving the app,
                    // so the nav you navigate BY should not disappear underneath
                    // it — the reader insets past the rail instead of covering
                    // it, and 1..5 stay clickable while you read.
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
                // NO TRANSITION. Opening an email is a jump, not a dissolve —
                // a crossfade means every open and close spends a fifth of a
                // second showing you two surfaces at once, which reads as lag
                // rather than polish on a surface you enter and leave constantly.
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
        // threadId is deliberately NOT animated — see the reader's zIndex block.
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

    /// 1..5 view nav + ⌘[ back / ⌘] forward + ⌘K ask bar. Digits are otherwise
    /// unbound; the ⌘ chords use the `meta` flag so a bare "[" / "]" never
    /// triggers them. allowInInput keeps history nav and ⌘K working even with a
    /// search/compose field focused (they are chords, not typed characters).
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
        // The help overlay files these under "App", so they belong in the
        // GLOBAL context rather than only in the inbox list — pressing `?` on
        // the sitrep should open help, not nothing. Neither collides with any
        // other binding, and both stay input-guarded (no allowInInput), so
        // typing "?" into search still types a question mark.
        // `/` was registered ONLY in the Emails list, so it did nothing on the
        // surface the app actually LANDS on. Search is a global affordance —
        // ⌘F already opens it from anywhere — and it stays input-guarded, so
        // typing "/" into a field still types a slash.
        bindings.append(
            KeyBinding("/", "search") { store.openSearch() })
        bindings.append(
            KeyBinding("\\", "toggle light/dark theme") { Prefs.shared.flipTheme() })
        bindings.append(
            KeyBinding("?", "keyboard shortcuts") { store.shortcutsOpen.toggle() })
        return bindings
    }
}

/// ROUTED VIEW HOST — Auth / Rules / Audit, promoted from side panels to full
/// main views behind the rail.
///
/// These inner views register their list-style keys into the "modal" KeyContext
/// and never push a context themselves, so this host pushes it while mounted —
/// exactly like SidePanel does. Only one routed view is mounted at a time, so
/// there is never a competing "list" set active. The global 1..5 keys keep
/// firing here because "global" composes with "modal".
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
                // Auth owns its entire surface — its own header band and a
                // two-column body — so it opts out of the shared chrome.
                content
            } else {
                VStack(alignment: .leading, spacing: 0) {
                    RoutedHeader(title: title)
                    content
                }
            }
        }
        .keyContext(.modal)
        // Esc = back to the sitrep (the home surface). Overlays these views open
        // push their OWN contexts above this one, so their Esc wins while
        // they're up and this fires only from the bare list.
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
        .overlay(alignment: .bottom) {
            Rectangle().fill(Palette.hairline).frame(height: 0.5)
        }
    }
}

extension RoutedHeader where Trailing == EmptyView {
    init(title: String) {
        self.init(title: title) { EmptyView() }
    }
}
