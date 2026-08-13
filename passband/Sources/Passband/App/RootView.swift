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
        @Bindable var store = store

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
        // ADD ACCOUNT — the same form as the gate, over a working app. Hung on
        // the whole shell rather than on any one surface: it is raised from
        // Settings, from the rail's account menu, from the Accounts menu, and
        // by a pair link from a second daemon, and none of those can be sure
        // which page is on screen.
        .sheet(isPresented: $store.addAccountSheetOpen) {
            ConnectView(purpose: .addAccount)
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
                // EVERY account's ears, not just the live one's: the poller
                // above follows whichever mailbox is on screen, while the event
                // feeds (and, for the inactive accounts, the auth watches) are
                // how the human hears about mail in the ones that are not.
                AccountManager.shared.startAllFeeds()
                // Warm the emails page at CONNECT, not on its first visit: the
                // list lives in the store, so fetching it now means opening the
                // page lands on rows instead of on "loading mail…". This also
                // warms the head rows' threads, so the first click reads from
                // cache too. The inbox only — the noise page is a place you go.
                Task { await store.refreshMail(.inbox) }
                // Whether this daemon can track opens at all. Needed before the
                // first composer opens, and it decides whether the reader spends
                // any requests looking for receipts.
                Task { await store.refreshTrackingConfig() }
            } else {
                SitrepPoller.shared.stop()
                AccountManager.shared.stopAllFeeds()
                // The Connect gate is now the whole window, and it is the same
                // form the sheet holds. Leaving one stacked on the other would
                // offer two ways to connect at once, the front one adding an
                // account to an install that no longer has any.
                store.addAccountSheetOpen = false
            }
        }
    }
}

private struct LoadingGate: View {
    var body: some View {
        VStack(spacing: 14) {
            Text("passband")
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

        // The compose pane takes HALF the window — the routed page shrinks
        // beside it rather than disappearing under a modal. GeometryReader is
        // the window's own measure; the floor keeps the pane usable when the
        // window itself is narrow.
        GeometryReader { geo in
            let composeWidth = max(420, geo.size.width / 2)

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
                // The composer: a working surface in the layout, NOT an overlay
                // — no scrim, no blur, the page beside it stays live.
                if store.compose != nil {
                    ComposePane()
                        .frame(width: composeWidth)
                        .transition(.move(edge: .trailing).combined(with: .opacity))
                }
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
            // results visible beside the email. Esc sheds the panel first (the
            // email stays), a second Esc closes the reader.
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
                    // Same reservation for the compose pane: `c` from the reader
                    // opens it in the layout BELOW this layer, and the reader
                    // insetting past it is what keeps it visible.
                    if store.compose != nil {
                        Color.clear
                            .frame(width: composeWidth)
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
        // The tour measures dashboard regions and places its coach marks in
        // ONE space, and this ZStack is the closest common ancestor of the
        // sitrep that reports them and the action layer that draws on them.
        .coordinateSpace(.named(tourSpace))
        .animation(.easeOut(duration: 0.18), value: store.modalOverlayOpen)
        .animation(.smooth(duration: 0.22), value: store.sideView)
        // The BOOL, never the ComposeState: the state changes on every
        // keystroke, and animating that would smear typing.
        .animation(.smooth(duration: 0.22), value: store.compose != nil)
        // threadId is deliberately NOT animated — opening a thread is a jump.
        .keyBindings(.global, globalBindings)
        .onChange(of: store.sitrep.sealed) { _, sealed in
            AuthArrival.shared.observe(sealed: sealed)
        }
        // THE TOUR'S TRIGGER: the first sync of the session landing. Not
        // `onAppear` — the board is empty until a pull returns, and a tour that
        // opens over "You're all clear." has nothing to point at. Not connect
        // either, which can succeed against a daemon that then goes dark.
        // A reconnect clears `lastRefresh`, so this fires again; the tour's own
        // once-a-session flag is what stops it starting twice.
        .onChange(of: store.lastRefresh) { old, new in
            if old == nil, new != nil { store.tour.maybeStart() }
        }
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
        // ⌘⇧K starts over. Bound as the CAPITAL letter, which is how the
        // registry spells shift for a letter key, and matched by the exact pass
        // ahead of the lowercase ⌘K above.
        bindings.append(
            KeyBinding("K", "new ask chat", meta: true, allowInInput: true) {
                store.assistant.clear()
                store.askBarOpen = true
            })
        // Global, not list-only: `/`, `\` and `?` must work on every surface,
        // including the sitrep the app lands on. All three stay input-guarded
        // (no allowInInput), so typing them into a field still types the
        // character.
        bindings.append(
            KeyBinding("/", "search") { store.openSearch() })
        // Undo is global because the toasts are: an action fired from any
        // surface parks its undo in the ActionLayer, which advertises `u`
        // everywhere. Declining, so with nothing pending the key stays free
        // for surface bindings (the thread viewer's `u` = unsubscribe).
        bindings.append(
            KeyBinding(declining: "u", "undo last action") {
                guard !store.undos.isEmpty else { return false }
                Task { await store.fireUndo() }
                return true
            })
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
