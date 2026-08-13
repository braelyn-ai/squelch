// THE PLATFORM SHIM. The handful of AppKit calls the SHARED code makes, each
// with a UIKit twin behind it, so a model or a view never has to know which OS
// it is running on.
//
// Every macOS branch here is exactly the call its call sites made before this
// file existed. A shim that quietly "improves" one of them is a shim that
// changes the Mac app.
//
// What deliberately stays OUT: anything with no honest twin — windows, the save
// panel, the key-event source, the AppKit view representables. Those are fenced
// where they are used, because a cross-platform name over a macOS-only concept
// only moves the lie one level down.

import SwiftUI

#if os(macOS)
    import AppKit

    typealias PlatformImage = NSImage
    typealias PlatformFont = NSFont
    /// The concrete color class `NSAttributedString.Key.foregroundColor` wants.
    /// SwiftUI's `Color` is not it on either platform, and both spell the
    /// conversion `init(_: Color)` — so shared attribute code says
    /// `PlatformColor(Palette.ink)` and stops caring which OS it is on.
    typealias PlatformColor = NSColor
    typealias PlatformSound = NSSound
#else
    import UIKit

    typealias PlatformImage = UIImage
    typealias PlatformFont = UIFont
    typealias PlatformColor = UIColor

    /// The notification chime is the only sound this app plays, and it plays it
    /// by holding the object alive for the duration. UIKit has no equivalent
    /// one-liner (AVAudioPlayer is the eventual answer), so this stand-in keeps
    /// that one call site compiling and stays silent.
    final class PlatformSound {
        init?(contentsOf url: URL, byReference: Bool) { return nil }
        func play() {}
        func stop() {}
    }
#endif

enum Platform {
    // MARK: - shell

    /// Hand a URL to the system. Returns whether the OS took it.
    static func open(_ url: URL) -> Bool {
        #if os(macOS)
            return NSWorkspace.shared.open(url)
        #else
            // UIKit answers asynchronously through a completion handler; nothing
            // here waits on that verdict, so the ask itself is the answer.
            UIApplication.shared.open(url)
            return true
        #endif
    }

    /// Replace the general pasteboard with `text`. Returns whether it landed.
    static func copyToPasteboard(_ text: String) -> Bool {
        #if os(macOS)
            let pasteboard = NSPasteboard.general
            pasteboard.clearContents()
            return pasteboard.setString(text, forType: .string)
        #else
            UIPasteboard.general.string = text
            return true
        #endif
    }

    // MARK: - appearance

    /// Whether the system is drawing DARK right now. Only for the places that
    /// resolve a `system` theme choice themselves — a view inside SwiftUI reads
    /// the `colorScheme` environment instead.
    @MainActor
    static var isDarkAppearance: Bool {
        #if os(macOS)
            NSApp?.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        #else
            UITraitCollection.current.userInterfaceStyle == .dark
        #endif
    }

    /// A color that RE-RESOLVES per appearance instead of being sampled once, so
    /// a theme flip repaints without the palette being rebuilt.
    static func dynamicColor(light: Color, dark: Color) -> Color {
        #if os(macOS)
            Color(
                nsColor: NSColor(name: nil) { appearance in
                    let isDark = appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                    return NSColor(isDark ? dark : light)
                })
        #else
            Color(
                uiColor: UIColor { traits in
                    UIColor(traits.userInterfaceStyle == .dark ? dark : light)
                })
        #endif
    }

    // MARK: - lifecycle

    #if os(macOS)
        static let didBecomeActiveNotification = NSApplication.didBecomeActiveNotification
        static let didResignActiveNotification = NSApplication.didResignActiveNotification
        static let willTerminateNotification = NSApplication.willTerminateNotification
    #else
        static let didBecomeActiveNotification = UIApplication.didBecomeActiveNotification
        /// UIKit posts `willResignActive` where AppKit posts `didResignActive`.
        /// Same moment for every use here: the app is leaving the foreground.
        static let didResignActiveNotification = UIApplication.willResignActiveNotification
        static let willTerminateNotification = UIApplication.willTerminateNotification
    #endif
}

extension Image {
    /// One spelling for `Image(nsImage:)` / `Image(uiImage:)`.
    init(platformImage: PlatformImage) {
        #if os(macOS)
            self.init(nsImage: platformImage)
        #else
            self.init(uiImage: platformImage)
        #endif
    }
}
