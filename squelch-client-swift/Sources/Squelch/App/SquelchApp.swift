// App entry point. Opens ONE window whose background is the native AppKit
// backdrop — that is the layer every `.glassEffect` in the app refracts.

import AppKit
import SwiftUI

@main
struct SquelchApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var store = AppStore.shared
    @State private var prefs = Prefs.shared

    init() {
        Typo.registerBundledFonts()
    }

    var body: some Scene {
        Window("Squelch", id: "main") {
            RootView()
                .environment(store)
                .environment(prefs)
                .preferredColorScheme(prefs.theme.colorScheme)
                .frame(minWidth: 980, minHeight: 640)
                .background(WindowBackdrop().ignoresSafeArea())
                .background(WindowConfigurator())
                .onAppear { KeyMonitor.shared.install() }
        }
        .defaultSize(width: 1320, height: 880)
        .windowStyle(.hiddenTitleBar)
        .windowToolbarStyle(.unifiedCompact)
        .commands { SquelchCommands(store: store, prefs: prefs) }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        // Install before launch finishes: a notification tapped while Squelch
        // was not running is delivered the moment a delegate exists, and a
        // center without one drops it.
        Notifier.shared.install()
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
struct SquelchCommands: Commands {
    let store: AppStore
    let prefs: Prefs

    var body: some Commands {
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
            ForEach(Array(MainView.mainViews.enumerated()), id: \.element) { index, view in
                Button(view.label) { store.setView(view) }
                    .keyboardShortcut(
                        KeyEquivalent(Character("\(index + 1)")), modifiers: [.command])
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

        CommandMenu("Inbox") {
            Button("Ask Your Inbox…") { store.askBarOpen = true }
                .keyboardShortcut("k", modifiers: [.command])
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
