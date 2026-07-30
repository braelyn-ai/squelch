// Finding the app's ONE window from AppKit.
//
// The scene is `Window("Squelch", id: "main")` — a singleton, so there is
// exactly one window to find. It has to be findable from places SwiftUI's
// `openWindow` cannot reach: a notification tap and a Dock-icon reopen both
// arrive at the AppDelegate, outside any view's environment.

import AppKit

@MainActor
enum MainWindow {
    /// Bring the window back to the front. Creates nothing: SwiftUI keeps a
    /// `Window` scene's NSWindow alive across a close and reuses it on reopen,
    /// so "reopen" here means ordering the existing one back in.
    static func show() {
        find()?.makeKeyAndOrderFront(nil)
    }

    /// Matching is defensive on purpose. `.hiddenTitleBar` can leave `title`
    /// empty, and SwiftUI's window identifier is an internal string that merely
    /// CONTAINS the scene id — so try the id, then the title, then settle for
    /// the first one left. Panels are filtered out: popovers, tooltips and
    /// field editors are windows too, and ordering one of those front instead
    /// would look like nothing happened.
    ///
    /// NOT filtered on `canBecomeMain`, which is false for an invisible window
    /// — i.e. false for exactly the closed window this exists to bring back.
    static func find() -> NSWindow? {
        let candidates = NSApp.windows.filter { !($0 is NSPanel) }
        return candidates.first { $0.identifier?.rawValue.contains("main") == true }
            ?? candidates.first { $0.title == "Squelch" }
            ?? candidates.first
    }
}
