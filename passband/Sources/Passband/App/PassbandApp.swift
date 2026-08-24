// App entry point. Opens ONE window whose background is the native AppKit
// backdrop — that is the layer every `.glassEffect` in the app refracts.

import AppKit
import SwiftUI

@main
struct PassbandApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var store = AppStore.shared
    @State private var prefs = Prefs.shared
    @State private var accounts = AccountManager.shared

    init() {
        Typo.registerBundledFonts()
    }

    var body: some Scene {
        Window("Passband", id: "main") {
            RootView()
                .environment(store)
                .environment(prefs)
                .preferredColorScheme(prefs.theme.colorScheme)
                .frame(minWidth: 980, minHeight: 640)
                .background(WindowBackdrop().ignoresSafeArea())
                .background(WindowConfigurator())
                .onAppear { KeyMonitor.shared.install() }
                // passband://pair links. Parked on the store rather than acted
                // on here: only the Connect gate can pair, and an install that
                // already has an identity must not re-pair over it.
                .onOpenURL { store.receivePairLink($0) }
        }
        .defaultSize(width: 1320, height: 880)
        .windowStyle(.hiddenTitleBar)
        .windowToolbarStyle(.unifiedCompact)
        .commands { PassbandCommands(store: store, prefs: prefs, accounts: accounts) }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        // Install before launch finishes: a notification tapped while Passband
        // was not running is delivered the moment a delegate exists, and a
        // center without one drops it.
        Notifier.shared.install()
        // Attachment bytes staged for Quick Look are torn down when the panel
        // closes; this is the sweep for the ones a crash or a hard quit stranded.
        StagedAttachment.purgeRoot()
        // Same rule, same reason: a banner's sender tile is written to disk for
        // the system to carry off, and this clears the ones it never took.
        NotificationIcon.purgeRoot()
        Analytics.start()
        // The boot view never passes through route(to:), so its screen event
        // is recorded here or not at all.
        Analytics.screen(AppStore.shared.activeView.rawValue)
    }

    // FALSE, because the app is a notifier: terminating on last-window-close
    // would drop the event stream, so the notification the window was closed to
    // wait for would never arrive. ⌘Q and the app menu still quit.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    /// Dock click with nothing on screen: put the window back.
    func applicationShouldHandleReopen(
        _ sender: NSApplication, hasVisibleWindows flag: Bool
    ) -> Bool {
        if !flag { MainWindow.show() }
        return true
    }
}

/// Native menu equivalents for the app's global chords. These duplicate
/// registry bindings on purpose: the registry owns dispatch semantics (its
/// layering and input guard), the menu owns discoverability.
struct PassbandCommands: Commands {
    let store: AppStore
    let prefs: Prefs
    let accounts: AccountManager
    @ObservedObject private var updater = Updater.shared

    /// ⌘number SWITCHES ACCOUNTS. One constant, because the decision behind it
    /// is one decision: the digits used to route the Go menu's five views, and
    /// they were taken away from it (the bare `1`–`5` in RootView's global
    /// bindings still route, input-guarded, which is the whole reason the menu
    /// could give the chord up). A menu shortcut fires app-wide — inside text
    /// fields, inside modals — and for "show me my other mailbox" that is
    /// correct: it is not a character anyone means to type.
    private static let accountChordModifiers: EventModifiers = [.command]

    /// How many accounts get a chord. Nine digits, and ⌘0 is not a tenth.
    private static let chordedAccounts = 9

    /// The chord for the account at one position, or none past the ninth —
    /// those switch from the menu (or by cycling), which is the honest whole of
    /// what a tenth account can have.
    private static func accountChord(_ index: Int) -> KeyboardShortcut? {
        guard index < chordedAccounts else { return nil }
        return KeyboardShortcut(
            KeyEquivalent(Character("\(index + 1)")), modifiers: accountChordModifiers)
    }

    var body: some Commands {
        // Directly under "About Passband", where every Mac app keeps it.
        CommandGroup(after: .appInfo) {
            Button("Check for Updates…") { updater.check() }
                .disabled(!updater.canCheck)
        }

        // ⌘N composes from ANYWHERE, including the surfaces where the bare `c` is
        // deliberately out of reach (inside a modal, or with a text field focused).
        CommandGroup(replacing: .newItem) {
            Button("New Message") { store.openComposeNew() }
                .keyboardShortcut("n")
        }

        // Settings is a routed view rather than a separate window, so the
        // standard ⌘, routes instead of opening one.
        CommandGroup(replacing: .appSettings) {
            Button("Settings…") { store.setView(.settings) }
                .keyboardShortcut(",", modifiers: .command)
        }

        CommandMenu("Go") {
            // NO KEYBOARD SHORTCUT. These five used to hold ⌘1–⌘5; the digits
            // went to account switching, and the BARE `1`–`5` in
            // RootView.globalBindings do the routing — which they always did,
            // behind the dispatcher's input guard, so nothing was lost but the
            // menu's ability to advertise them.
            ForEach(Array(MainView.mainViews.enumerated()), id: \.element) { _, view in
                Button(view.label) { store.setView(view) }
            }
            Divider()
            Button("Usage") { store.setView(.usage) }
            Button("Settings") { store.setView(.settings) }
            Divider()
            Button("Back") { store.goBack() }
                .keyboardShortcut("[", modifiers: [.command])
                .disabled(!store.canGoBack)
            Button("Forward") { store.goForward() }
                .keyboardShortcut("]", modifiers: [.command])
                .disabled(!store.canGoForward)
        }

        // ACCOUNTS — one entry per mailbox this install knows about, on the
        // digits the Go menu gave up. Present even with a single account: it is
        // where "Add Account…" lives, and a menu that appears only once you
        // already have two is a feature nobody finds.
        CommandMenu("Accounts") {
            ForEach(Array(accounts.accounts.enumerated()), id: \.element.id) { index, account in
                Button {
                    Task { await accounts.switchTo(account.id) }
                } label: {
                    // The live one wears a check, the way every account picker
                    // on this platform says which one you are looking at.
                    if account.id == accounts.activeId {
                        Label(account.displayName, systemImage: "checkmark")
                    } else {
                        Text(account.displayName)
                    }
                }
                .keyboardShortcut(Self.accountChord(index))
                // Nothing to switch INTO from the Connect gate: the app has no
                // identity on screen, and pointing the client at a daemon
                // behind a screen that is still asking for one is a switch the
                // human cannot see happen.
                .disabled(store.connStatus != .connected)
            }
            Divider()
            // DELIBERATELY CHORDLESS. ⌘` is the system's window cycler, and
            // every account inside the first nine already has a digit of its
            // own — this is the tenth-account escape hatch and a place for the
            // menu to say the list wraps, not a primary way to move.
            Button("Next Account") { Task { await accounts.switchToNext() } }
                .disabled(store.connStatus != .connected || accounts.accounts.count < 2)
            Divider()
            Button("Add Account…") { store.addAccountSheetOpen = true }
                // The gate is already a connect form. Offering a second one on
                // top of it would add an account to an install that has none.
                .disabled(store.connStatus != .connected)
        }

        CommandMenu("Inbox") {
            Button("Ask Your Inbox…") { store.askBarOpen = true }
                .keyboardShortcut("k", modifiers: [.command])
            Button("New Ask Chat") {
                store.assistant.clear()
                store.askBarOpen = true
            }
            .keyboardShortcut("k", modifiers: [.command, .shift])
            Button("Search…") { store.openSearch() }
                .keyboardShortcut("f", modifiers: [.command])
            Divider()
            Button("Check for New Mail") {
                Task { await SitrepPoller.shared.triggerMailRefresh() }
            }
            .keyboardShortcut("r", modifiers: [.command])
            Divider()
            Button("Undo Last Action") { Task { await store.fireUndo() } }
                .keyboardShortcut("z", modifiers: [.command])
                .disabled(store.undos.isEmpty)
        }

        CommandGroup(after: .toolbar) {
            // Deliberately NO keyboardShortcut: a menu shortcut with no
            // modifier fires even while a text field has focus, so `\` is bound
            // in the key registry instead, behind its input guard.
            Button(prefs.theme == .dark ? "Light Appearance" : "Dark Appearance") {
                prefs.flipTheme()
            }
        }

        CommandGroup(replacing: .help) {
            Button("Keyboard Shortcuts") { store.shortcutsOpen = true }
                .keyboardShortcut("/", modifiers: [.command])
        }
    }
}
