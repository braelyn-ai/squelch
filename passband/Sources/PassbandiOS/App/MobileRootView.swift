// The phone shell. Same three-state gate as the Mac's RootView — loading until
// the keychain answers, the Connect gate until there is an identity, the shell
// after — and the same connect-time lifecycle: poller up, every account's event
// feed up, the inbox and the tracking config warmed.
//
// What differs is only the shape. A Mac gets a rail and a routed page; a phone
// gets a tab bar, which is a different navigation model rather than a smaller
// one, so the rail's seven destinations collapse to four and the rest arrive as
// the tabs that own them grow up.
//
// AND THE READER IS A PUSH. On the Mac the thread viewer is a zIndex-20 layer
// that covers the window; here it is a NavigationStack destination — but driven
// by the SAME `store.threadId`, so every surface that opens mail (a list row, a
// record zone, a newsletter card) opens it through the identical store call and
// leaving nils the identical state.
//
// THE ACTION LAYER, MINUS THE PARTS THAT ARE FURNITURE. `Views/ActionLayer.swift`
// is excluded from this target because it hosts the ⌘K ask bar, not because its
// contents are desktop-shaped: the toasts, the rule editor, the 2FA modal and
// the triage palette are all state on AppStore, and this shell mounts the same
// views off the same flags. Nothing below owns a flag of its own — every one of
// them is written by the verbs in Model/Actions.swift.
//
// THE PROCESS DECK IS DESKTOP-ONLY. `p` opens a card deck on the Mac, where it
// floats over a board you can still see; a phone had to give it the whole screen,
// which made it a mode rather than a pass — and a mode you enter from a tab bar,
// leave through a full-screen cover, and cannot see your mail behind. The phone
// works the same queue through the surfaces it already has, so `processModeOpen`
// is now a flag no phone code writes or reads.

import SwiftUI

/// The five phone destinations. Sitrep is the landing surface, matching the Mac.
///
/// RECORDS IS A TAB HERE AND A RAIL THERE. On the Mac the record zones are
/// pinned beside the work surface, read WHILE working it; a phone has no room to
/// pin anything, and stacking them under the obligations would put reference
/// material behind a scroll past everything you owe. So they get a destination.
///
/// `.records` READS "QUICK LOOK" ON THE BAR. The user-facing name changed; the
/// case did not, because the store, the Mac and every zone type still call this
/// material records and renaming the symbol would churn all of it to relabel one
/// tab.
private enum MobileTab: Hashable {
    case sitrep, records, mail, search, settings

    /// Left-to-right position in the tab bar. Only the slide direction reads it:
    /// a tab further right has to arrive from the right. Search is highest
    /// because its `.search` role parks it at the trailing end of the bar,
    /// after settings.
    var index: Int {
        switch self {
        case .sitrep: 0
        case .records: 1
        case .mail: 2
        case .settings: 3
        case .search: 4
        }
    }
}

struct MobileRootView: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        Group {
            switch store.connStatus {
            case .loading:
                MobileLoadingGate()
            case .connected:
                MobileShell()
            default:
                ConnectView()
            }
        }
        // The canvas the glass refracts. On the Mac this is the window backdrop
        // behind the whole scene; a phone has no window to make translucent, so
        // the palette paints it directly.
        .background(Palette.canvas.ignoresSafeArea())
        .task {
            // Pay WebKit's process-launch cost at boot rather than on the first
            // email the reader opens — the reader is here now, so this is worth
            // the same as it is on the Mac.
            EmailWebView.warmProcess()
            await store.loadSettings()
        }
        .onChange(of: store.connStatus) { _, status in
            if status == .connected {
                SitrepPoller.shared.start()
                // One feed per account, live or not, plus an auth watch on each
                // account that is not live — same as the Mac. The phone has no
                // account switcher yet, so today that is a list of one and no
                // watches at all; it is the same call because the lifecycle is
                // the shell's job and the shell is the only part that differs.
                AccountManager.shared.startAllFeeds()
                Task { await store.refreshMail(.inbox) }
                Task { await store.refreshTrackingConfig() }
            } else {
                SitrepPoller.shared.stop()
                AccountManager.shared.stopAllFeeds()
            }
        }
    }
}

private struct MobileLoadingGate: View {
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

// MARK: - the tab shell

private struct MobileShell: View {
    @Environment(AppStore.self) private var store
    @State private var tab: MobileTab = .sitrep

    // No `@Bindable` shadow here: every modal below is presented off a derived
    // Binding that funnels dismissal through the store's own close verb, so
    // nothing needs to write a store flag directly.
    var body: some View {
        TabView(selection: $tab) {
            Tab("Sitrep", systemImage: MainView.sitrep.symbol, value: MobileTab.sitrep) {
                NavigationStack {
                    MobileSitrepView()
                        .threadDestination(active: tab == .sitrep)
                }
                .tabSlide(.sitrep, selection: tab)
            }
            // `square.grid.2x2` and not a box: the tab is a grid of glanceable
            // cards, one per zone, and the icon should say the shape of what is
            // behind it. `archivebox` promised storage — a place things go to be
            // put away — which is the opposite of a surface you check in passing.
            Tab("Quick Look", systemImage: "square.grid.2x2", value: MobileTab.records) {
                NavigationStack {
                    MobileRecordsView()
                        .threadDestination(active: tab == .records)
                }
                .tabSlide(.records, selection: tab)
            }
            Tab("Mail", systemImage: MainView.emails.symbol, value: MobileTab.mail) {
                NavigationStack {
                    EmailsView()
                        .navigationTitle("Mail")
                        .navigationBarTitleDisplayMode(.inline)
                        // `c` ON THE MAC IS GLOBAL; here the new-message door is
                        // ONE door, and it is on the mail. A phone's global verb
                        // is a tab, and a compose button repeated across five of
                        // them is five buttons for one composer. Replies do not
                        // come through here at all — they open in the reader,
                        // where the thread they answer is.
                        .toolbar {
                            ToolbarItem(placement: .topBarTrailing) {
                                Button { store.openComposeNew() } label: {
                                    Image(systemName: "square.and.pencil")
                                }
                                .accessibilityLabel("New message")
                            }
                        }
                        .threadDestination(active: tab == .mail)
                }
                .tabSlide(.mail, selection: tab)
            }
            Tab("Settings", systemImage: MainView.settings.symbol, value: MobileTab.settings) {
                NavigationStack {
                    PlaceholderTab(
                        title: "Settings",
                        symbol: MainView.settings.symbol,
                        line:
                            "Connection, theme, signature and telemetry move over with the settings pane."
                    )
                }
                .tabSlide(.settings, selection: tab)
            }
            // The search role parks this tab apart from the others in iOS 26's
            // tab bar, next to the minimize affordance, which is where a phone
            // user reaches for it.
            Tab(value: MobileTab.search, role: .search) {
                NavigationStack {
                    PlaceholderTab(
                        title: "Search",
                        symbol: "magnifyingglass",
                        line: "Full-text search over the mail the daemon has ingested.")
                }
                .tabSlide(.search, selection: tab)
            }
        }
        // Scrolling down surrenders the bar to the content and brings it back on
        // the way up: a mail list is a reading surface first. All three tabs that own
        // one are a real ScrollView/List, so both drive it.
        .tabBarMinimizeBehavior(.onScrollDown)
        // Toasts ride ABOVE the tab bar, where the Mac's stack rides above the
        // rail. The offset clears the bar at rest; when the bar minimizes on
        // scroll the stack simply sits a little higher than it needs to, which is
        // the harmless direction to be wrong in.
        .overlay(alignment: .bottom) {
            MobileToastHost()
                .padding(.bottom, 68)
        }
        // The two modals the Mac stacks in its action layer, mounted here on the
        // same store flags. Overlays rather than sheets: both are their own scrim
        // already (OverlayScrim), both dismiss by tapping off, and a sheet would
        // put a second card shape around a card.
        .overlay {
            if let request = store.ruleEditor {
                RuleEditor(request: request) { store.closeRuleEditor() }
            }
        }
        .overlay {
            if !store.authQueue.isEmpty { AuthCodeModal() }
        }
        // `v`. A SHEET here and a top-anchored command bar there, for one reason:
        // the palette autofocuses a text field, and a phone answers that by
        // raising the keyboard over the bottom half of the screen. A sheet is the
        // only presentation that gets out of its way.
        .sheet(isPresented: triageFixOpen) {
            if let target = store.triageFix {
                TriageFixPalette(target: target) { store.closeTriageFix() }
            }
        }
        // `c` / ⌘N. THE SAME `ComposePane` the Mac opens as a half-window pane,
        // off THE SAME `store.compose` — which is what makes the draft restore,
        // the autosave and the send ceremony one code path rather than two. A
        // pane needs the page beside it to stay live; a phone has no beside, so
        // it is a full-height sheet.
        .sheet(isPresented: composeOpen) {
            ComposePane()
                .presentationDetents([.large])
                .presentationDragIndicator(.visible)
                .presentationBackground(Palette.canvas)
                // A drag-down mid-send would leave the request in flight with
                // nothing on screen to report the verdict to. Every other moment
                // is dismissable: closing FLUSHES the draft, so nothing is lost.
                .interactiveDismissDisabled(store.compose?.sending == true)
                .scrollDismissesKeyboard(.interactively)
        }
    }

    /// Presented-ness derived from the store's own `triageFix`, so dismissing the
    /// sheet by dragging it down runs the same `closeTriageFix()` the palette's
    /// cancel does and the state can never disagree with what is on screen.
    private var triageFixOpen: Binding<Bool> {
        Binding(
            get: { store.triageFix != nil },
            set: { if !$0 { store.closeTriageFix() } })
    }

    /// Same contract for the composer: the store owns whether it is open, and a
    /// dragged-down sheet runs the identical `closeCompose()` the footer's cancel
    /// does — so the draft is flushed exactly once either way.
    private var composeOpen: Binding<Bool> {
        Binding(
            get: { store.compose != nil },
            set: { if !$0 { store.closeCompose() } })
    }
}

// MARK: - directional tab slide

/// The arriving tab's content enters from the side its tab sits on: a tab further
/// right in the bar slides in from the right, further left from the left. The bar
/// is a row and the tabs are positions along it, so a switch should read as travel
/// between two places rather than as one screen being swapped for another.
///
/// THE NATIVE `TabView` STAYS. Hand-rolling a pager would buy this transition and
/// cost everything only the real control has: iOS 26's `.search`-role placement
/// (the parked trailing tab beside the minimize affordance), the
/// `tabBarMinimizeBehavior` surrender on scroll, and a live NavigationStack per
/// tab. So selection stays the bar's business and only the CONTENT moves — a
/// modifier on each tab's stack, not a new container around all of them.
///
/// FIRST VISIT DOES NOT SLIDE, and that is the honest behavior rather than a
/// gap: a TabView builds a tab's content the moment you first select it, and
/// `onChange` does not fire for the change that mounted the view. Every later
/// switch has the content already alive to move.
private struct TabSlide: ViewModifier {
    let tab: MobileTab
    let selection: MobileTab
    /// Travel as a FRACTION of the content's own width, not points: -1 is parked
    /// off the leading edge, +1 off the trailing one, 0 is home. Storing the trip
    /// this way is what lets the offset be applied without anyone measuring a
    /// screen — see `visualEffect` below.
    @State private var parked: CGFloat = 0

    func body(content: Content) -> some View {
        content
            // `visualEffect` and not `.offset(x:)` over a GeometryReader: it hands
            // over the proxy for the view's OWN size at draw time, so the shift
            // costs no layout pass and no extra wrapper around each tab's stack.
            // Asking `UIScreen` for the width instead would be a guess that the
            // content is screen-wide, and iOS 26 deprecated the main-screen
            // accessor for exactly that reason.
            .visualEffect { effect, proxy in
                effect.offset(x: parked * proxy.size.width)
            }
            .onChange(of: selection) { old, new in
                // Only the tab being switched TO moves, and only when it really
                // was a switch — reselecting the current tab is a no-op, not a
                // slide of zero distance.
                guard new == tab, old != tab else { return }
                // Park the content off the correct edge with animation OFF, then
                // fly it home on the next turn of the loop. Both writes in one
                // pass would collapse into a single render at zero and nothing
                // would move at all.
                var jump = Transaction()
                jump.disablesAnimations = true
                withTransaction(jump) {
                    parked = new.index > old.index ? 1 : -1
                }
                Task { @MainActor in
                    withAnimation(Motion.tabSlide) { parked = 0 }
                }
            }
    }
}

extension View {
    fileprivate func tabSlide(_ tab: MobileTab, selection: MobileTab) -> some View {
        modifier(TabSlide(tab: tab, selection: selection))
    }
}

// MARK: - the reader, as a push

/// The thread viewer as a NavigationStack destination, bound to `store.threadId`
/// — the same state the Mac's overlay is gated on, so nothing about opening mail
/// is per-platform. Popping (the back chevron, the edge swipe) writes nil, which
/// runs `closeThread()`: the exact call the Mac's close button makes, so the
/// queue and the pending reply are cleared the same way rather than left behind.
///
/// `active` IS LOAD-BEARING. A TabView keeps every visited tab's stack mounted,
/// so both destinations see the same store field; without this, opening a thread
/// from the sitrep would silently push the same reader onto the mail tab's stack
/// too. Off-tab the binding reads nil (that stack pops) and refuses to write
/// (the pop must not close a thread the visible tab is still showing).
private struct ThreadDestination: ViewModifier {
    @Environment(AppStore.self) private var store
    let active: Bool

    func body(content: Content) -> some View {
        content.navigationDestination(item: openThread) { threadId in
            ThreadViewer(threadId: threadId)
                .id(threadId)
                // The subject is the first thing under the bar already; a second
                // copy of it up here, truncated to a phrase, is noise. The bar
                // stays for its back chevron.
                .navigationTitle("")
                .navigationBarTitleDisplayMode(.inline)
        }
    }

    private var openThread: Binding<String?> {
        Binding(
            get: { active ? store.threadId : nil },
            set: { newValue in
                guard active, newValue == nil else { return }
                store.closeThread()
            })
    }
}

extension View {
    fileprivate func threadDestination(active: Bool) -> some View {
        modifier(ThreadDestination(active: active))
    }
}

// MARK: - placeholders

/// An honest stub: says what belongs here and does not pretend to hold it.
private struct PlaceholderTab: View {
    let title: String
    let symbol: String
    let line: String

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: symbol)
                .font(.system(size: 30, weight: .light))
                .foregroundStyle(Palette.inkFaintest)
            Text(title.lowercased())
                .font(Typo.serif(24, weight: .medium))
                .foregroundStyle(Palette.ink)
            Text(line)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkFaint)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 300)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Palette.canvas)
        .navigationTitle(title)
        .navigationBarTitleDisplayMode(.inline)
    }
}
