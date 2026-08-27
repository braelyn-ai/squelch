// iOS entry point. The same AppStore, the same Prefs, the same pair-link
// handler as the Mac — the only thing this file owns is the fact that a phone
// has one full-screen scene instead of a window with a backdrop behind it.
//
// Deliberately NOT here: the AppKit activation policy, the window configurator,
// the menu commands, Sparkle. Those are the Mac shell, and the Mac keeps them.

import SwiftUI
import UserNotifications

@main
struct PassbandiOSApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var store = AppStore.shared
    @State private var prefs = Prefs.shared

    init() {
        Typo.registerBundledFonts()
    }

    var body: some Scene {
        WindowGroup {
            MobileRootView()
                .environment(store)
                .environment(prefs)
                .preferredColorScheme(prefs.theme.colorScheme)
                // passband://pair links, through the SAME store entry point the
                // Mac uses: parked for the Connect gate rather than acted on, so
                // an install that already holds a device identity ignores them.
                .onOpenURL { store.receivePairLink($0) }
        }
    }
}

/// The launch hook. UIKit's delegate exists for one reason here — the
/// notification centre hands a tap made while the app was dead to whatever
/// delegate is installed by the time launch finishes, and drops it if there is
/// none.
final class AppDelegate: NSObject, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        // The same launch sweep the Mac does, and for the same reason: staged
        // attachment bytes are torn down when the sheet closes, and a phone that
        // was jetsammed mid-preview closed nothing. Without this the decrypted
        // plaintext accumulates in the container across launches.
        StagedAttachment.purgeRoot()
        MainActor.assumeIsolated {
            Notifier.shared.install()
            PushRegistration.shared.start()
            Analytics.start()
            // The boot view never passes through route(to:), so its screen event
            // is recorded here or not at all.
            Analytics.screen(AppStore.shared.activeView.rawValue)
        }
        return true
    }

    func applicationDidBecomeActive(_ application: UIApplication) {
        Task { @MainActor in await PushRegistration.shared.registerAndSync() }
    }

    func application(
        _ application: UIApplication,
        didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) {
        Task { @MainActor in await PushRegistration.shared.received(deviceToken) }
    }

    func application(
        _ application: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: Error
    ) {
        Task { @MainActor in PushRegistration.shared.registrationFailed() }
    }
}
