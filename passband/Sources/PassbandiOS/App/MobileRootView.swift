// The phone shell. Same three-state gate as the Mac's RootView — loading until
// the keychain answers, the Connect gate until there is an identity, the shell
// after — and the same connect-time lifecycle: poller up, every account's event
// feed up, the inbox and the tracking config warmed.
//
// What differs is only the shape. A Mac gets a rail and a routed page; a phone
// gets a tab bar, which is a different navigation model rather than a smaller
// one: a rail is a directory you read, and a tab bar is three or four places you
// LIVE in. So the rail's seven destinations land here as three tabs — sitrep,
// quick look, mail — plus the search role parked at the trailing end.
//
// WHAT CAME OFF THE BAR, and where it went. Settings and the login codes are not
// places anyone lives; they are reference surfaces you visit and leave, so they
// hang from a navigation bar instead of each spending permanent bar width —
// settings behind the person glyph leading the sitrep's, the codes behind a key
// trailing Quick Look's, which is the tab that already holds everything else the
// app pulled out of your mail for you to look up. The agent's own tab is gone
// too: one text field per screen is what a phone actually has, so the assistant
// lives behind the search field and the chat is a push off that stack. Two doors
// for "ask me something" was one too many.
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

/// The phone's four destinations: three tabs, plus search parked at the end of
/// the bar. Sitrep is the landing surface, matching the Mac.
///
/// THREE, BECAUSE A TAB IS A CLAIM. Every tab says "you will be here often", and
/// a bar that makes that claim six times has stopped ranking anything. What is
/// left is the three surfaces a day actually alternates between: what needs you,
/// what was pulled out of your mail, and the mail itself.
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
    case sitrep, records, mail, search

    /// Left-to-right position in the tab bar. Only the slide direction reads it:
    /// a tab further right has to arrive from the right. Search is highest
    /// because its `.search` role parks it at the trailing end of the bar,
    /// after mail.
    var index: Int {
        switch self {
        case .sitrep: 0
        case .records: 1
        case .mail: 2
        case .search: 3
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
                // The Connect gate is now the whole screen, and it is the same
                // form this sheet holds. Leaving one stacked on the other would
                // offer two ways to connect at once, the front one adding an
                // account to an install that no longer has any.
                store.addAccountSheetOpen = false
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
                        // `c` ON THE MAC IS GLOBAL; here the new-message door
                        // is on the two surfaces you would reach for it from —
                        // this one, and the dashboard the app opens on — and not
                        // on all four, which would be four buttons for one
                        // composer. Both raise THE SAME `store.compose`, so the
                        // draft, the autosave and the send ceremony are one code
                        // path. Replies do not come through here at all: they
                        // open in the reader, where the thread they answer is.
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
            // The search role parks this tab apart from the others in iOS 26's
            // tab bar, next to the minimize affordance, which is where a phone
            // user reaches for it.
            //
            // AND IT IS THE AGENT'S TAB TOO. The field asks or searches by how
            // many words are in it, and the chat is a push on this same stack —
            // so the only thing this shell has to know about the agent is that
            // there is nothing here to know.
            //
            // IT KEEPS A THREAD DESTINATION because a hit is mail and a hit you
            // cannot open is a citation — and because the agent's citations and
            // email cards open onto this stack as well.
            Tab(value: MobileTab.search, role: .search) {
                NavigationStack {
                    MobileSearchView()
                        .threadDestination(active: tab == .search)
                }
                .tabSlide(.search, selection: tab)
            }
        }
        // Scrolling down surrenders the bar to the content and brings it back on
        // the way up: a mail list is a reading surface first. Every tab here is a
        // real ScrollView or List, so all of them drive it.
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
        // SHARE PASSBAND, off the same `store.shareSheetOpen` the Mac's shell
        // presents, so the button in Settings works on both and the flag is
        // never set into the void. A full-height sheet for the same reason the
        // composer is one: it autofocuses a field, and the keyboard takes the
        // bottom half of a phone.
        // Gated on `ShareGate` exactly like the Mac's, so the two platforms
        // never disagree about whether an invite code can be handed out.
        // The pointer sheet is short, so it gets `.medium` rather than the
        // composer's full height.
        .sheet(isPresented: shareOpen) {
            if ShareGate.invitesEnabled {
                SharePanel()
                    .presentationDetents([.large])
                    .presentationDragIndicator(.visible)
                    .presentationBackground(Palette.canvas)
            } else {
                ShareWaitlistPanel()
                    .presentationDetents([.medium])
                    .presentationDragIndicator(.visible)
                    .presentationBackground(Palette.canvas)
            }
        }
        // ADD ACCOUNT — the same `ConnectView` the gate is, off the same
        // `store.addAccountSheetOpen` the Mac's shell presents, so the button in
        // Settings works on both and the flag is never set into the void.
        //
        // Hung on the SHELL and not on the Account page, for the reason the
        // Mac's is: the flag is raised from the Account pane, from the selector
        // above it, and by a pair link arriving from a second daemon, and none
        // of those can be sure which page is on screen.
        //
        // Full height because the form autofocuses a field and the keyboard
        // takes the bottom half of a phone — the same trade the composer makes.
        .sheet(isPresented: addAccountOpen) {
            ConnectView(purpose: .addAccount)
                .presentationDetents([.large])
                .presentationDragIndicator(.visible)
                .presentationBackground(Palette.canvas)
        }
        // THE TWO-WEEK ASK, over the shell on both platforms.
        .overlay { ShareNudgeModal() }
        .onChange(of: store.shareAvailable) { _, canShare in
            ShareNudge.shared.askIfEarned(canShare: canShare)
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

    /// The share sheet's flag, as a Binding, so this file keeps its rule: no
    /// `@Bindable` shadow, every modal presented off a derived binding. There is
    /// no close verb to funnel through here because the sheet owns nothing but
    /// itself; a drag-down is a dismissal and nothing else.
    private var shareOpen: Binding<Bool> {
        Binding(
            get: { store.shareSheetOpen },
            set: { store.shareSheetOpen = $0 })
    }

    /// The add-account sheet is a plain flag on the store rather than a derived
    /// presence: `ConnectView` owns the whole flow behind it and has nothing to
    /// tear down, so a dragged-down sheet is a dismissal and nothing else.
    private var addAccountOpen: Binding<Bool> {
        Binding(
            get: { store.addAccountSheetOpen },
            set: { store.addAccountSheetOpen = $0 })
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
        // Captured as a plain value: `visualEffect`'s closure is Sendable and
        // runs off the main actor, so it may not touch main-actor state — it
        // gets the number, not the property.
        let travel = parked
        return content
            // `visualEffect` and not `.offset(x:)` over a GeometryReader: it hands
            // over the proxy for the view's OWN size at draw time, so the shift
            // costs no layout pass and no extra wrapper around each tab's stack.
            // Asking `UIScreen` for the width instead would be a guess that the
            // content is screen-wide, and iOS 26 deprecated the main-screen
            // accessor for exactly that reason.
            .visualEffect { effect, proxy in
                effect.offset(x: travel * proxy.size.width)
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

/// The thread viewer as a NavigationStack destination, driven by `store.threadId`
/// — the same state the Mac's overlay is gated on, so nothing about opening mail
/// is per-platform. Popping (the back chevron, the edge swipe) runs
/// `closeThread()`: the exact call the Mac's close button makes, so the
/// queue and the pending reply are cleared the same way rather than left behind.
///
/// THE READER BELONGS TO THE TAB THAT OPENED IT. A TabView keeps every visited
/// tab's stack mounted, and all four destinations watch the same store field; if
/// each simply presented `store.threadId` whenever it was frontmost, switching
/// tabs mid-read would re-push the same reader onto the new tab's stack (the
/// tab bar stays tappable under a pushed reader). So each destination keeps its
/// own `mine` and captures the store's thread only while its tab is active:
/// switch away and the reader stays parked on the tab you left it on; open a
/// different thread elsewhere and the stale stack lets go silently, without
/// closing the thread the visible tab is showing.
private struct ThreadDestination: ViewModifier {
    @Environment(AppStore.self) private var store
    let active: Bool

    /// The thread THIS tab's stack owns. Captured from `store.threadId` only
    /// while the tab is frontmost, so a store-level thread never leaks onto a
    /// stack that didn't open it.
    @State private var mine: String?

    func body(content: Content) -> some View {
        content
            .navigationDestination(item: openThread) { threadId in
                ThreadViewer(threadId: threadId)
                    .id(threadId)
                    // The subject is the first thing under the bar already; a
                    // second copy of it up here, truncated to a phrase, is
                    // noise. The bar stays for its back chevron.
                    .navigationTitle("")
                    .navigationBarTitleDisplayMode(.inline)
            }
            .onChange(of: store.threadId) { _, newValue in
                if active {
                    // Frontmost: this tab drives. Opening claims the thread,
                    // closing (from anywhere: pop, account teardown) releases it.
                    mine = newValue
                } else if newValue != mine {
                    // Another tab opened a different thread, or the thread was
                    // closed while this tab held it off-screen: this stack's
                    // reader describes state that is gone, so it pops silently.
                    mine = nil
                }
            }
    }

    private var openThread: Binding<String?> {
        Binding(
            get: { mine },
            set: { newValue in
                guard newValue == nil, let owned = mine else { return }
                mine = nil
                // A user-driven pop closes the store's thread only when it is
                // still the one this stack owned; a stack releasing a stale
                // reader must not close what another tab is showing.
                if active, store.threadId == owned { store.closeThread() }
            })
    }
}

extension View {
    fileprivate func threadDestination(active: Bool) -> some View {
        modifier(ThreadDestination(active: active))
    }
}
