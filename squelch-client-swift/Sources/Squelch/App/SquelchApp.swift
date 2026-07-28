// App entry point. Opens ONE window whose background is the native AppKit
// backdrop — that is the layer every `.glassEffect` in the app refracts.
//
// The menu bar carries the ⌘-chords that macOS expects to find there (⌘K, ⌘[,
// ⌘]), so they show up in Help > Search and read as native. They ALSO exist in
// the key registry, because the registry is where the app's layering semantics
// live; the menu items simply call the same handlers.

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
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

/// Native menu equivalents for the app's global chords. These duplicate
/// registry bindings on purpose: the registry owns dispatch semantics, the menu
/// owns discoverability.
struct SquelchCommands: Commands {
    let store: AppStore
    let prefs: Prefs

    var body: some Commands {
        CommandGroup(replacing: .newItem) {}

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
            Button("Search…") { store.openSide(.search(query: "")) }
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
            // Deliberately NO keyboardShortcut: `\` is bound in the key registry
            // instead. A menu shortcut with no modifier fires even while a text
            // field has focus, which would make it impossible to type a
            // backslash anywhere in the app. The registry's input guard is the
            // whole reason that dispatch layer exists.
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
