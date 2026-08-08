// Sparkle auto-update, held app-wide. One controller for the process: Sparkle
// asserts if a second updater starts for the same bundle, so everything that
// needs it (the menu item, a future Settings row) reads this shared instance.
//
// The vendored framework is prebuilt (vendor/fetch-sparkle.sh); the feed URL
// and EdDSA public key live in Info.plist. Update UX is Sparkle's stock
// standard driver: it asks for permission before the first scheduled check,
// so a fresh install never phones home undisclosed.

import Combine
import Sparkle

@MainActor
final class Updater: ObservableObject {
    static let shared = Updater()

    private let controller: SPUStandardUpdaterController

    /// Mirrors Sparkle's own gate (KVO), so the menu item disables itself
    /// mid-check instead of stacking a second session on a click.
    @Published private(set) var canCheck = false

    private init() {
        controller = SPUStandardUpdaterController(
            startingUpdater: true, updaterDelegate: nil, userDriverDelegate: nil)
        controller.updater.publisher(for: \.canCheckForUpdates)
            .receive(on: DispatchQueue.main)
            .assign(to: &$canCheck)
    }

    /// User-initiated check, with Sparkle's own progress and result UI.
    func check() { controller.checkForUpdates(nil) }
}
