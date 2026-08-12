// Open an external URL in the system browser, plus clipboard and save-panel
// helpers.
//
// Only http/https is ever handed to the OS; every other scheme is ignored. A
// failure log carries at most the HOST — mail-derived links routinely carry
// per-recipient tokens. See docs/SECURITY.md §2.

import Foundation
import SwiftUI
import UniformTypeIdentifiers

enum Opener {
    /// True only for http:/https: — the sole schemes we hand to the shell.
    static func isHTTP(_ url: String) -> Bool {
        guard let scheme = URL(string: url)?.scheme?.lowercased() else { return false }
        return scheme == "http" || scheme == "https"
    }

    /// The URL's HOST only — never the path/query.
    private static func safeHost(_ url: String) -> String {
        URL(string: url)?.host ?? "?"
    }

    /// Open `url` externally. A no-op for non-http(s) URLs so callers can pass a
    /// possibly-odd tracking_url through and still be safe here.
    @MainActor
    static func open(_ url: String?) {
        guard let url, !url.isEmpty, isHTTP(url), let parsed = URL(string: url) else { return }
        if !Platform.open(parsed) {
            // A dead button must be diagnosable: static text plus the host only.
            NSLog("openExternal: failed to open external URL (host: %@)", safeHost(url))
        }
    }
}

enum Clip {
    /// How long a "copied" confirmation stays lit.
    static let flashWindow: Duration = .milliseconds(1400)

    /// Copy text to the general pasteboard. Returns whether it landed.
    @MainActor
    @discardableResult
    static func copy(_ text: String) -> Bool {
        Platform.copyToPasteboard(text)
    }

    /// Copy, then hold `flash` true for the confirmation window so a button can
    /// read "copied" and revert itself. Returns whether the copy landed.
    @MainActor
    @discardableResult
    static func copy(_ text: String, flashing flash: Binding<Bool>) -> Bool {
        guard copy(text) else { return false }
        flash.wrappedValue = true
        Task {
            try? await Task.sleep(for: flashWindow)
            flash.wrappedValue = false
        }
        return true
    }
}

enum Downloads {
    enum Result { case saved, cancelled, failed(String) }

    // "Pick a path" is a desktop idea: iOS saves through a document picker or a
    // share sheet, which is a different flow rather than a different call, so
    // there is nothing honest to shim here.
    #if os(macOS)
        /// Save raw bytes to a user-chosen path via the standard save panel.
        @MainActor
        static func saveBytes(_ bytes: Data, filename: String) async -> Result {
            let panel = NSSavePanel()
            panel.nameFieldStringValue = filename
            panel.canCreateDirectories = true
            panel.isExtensionHidden = false
            if let ext = filename.split(separator: ".").last.map(String.init),
                let type = UTType(filenameExtension: ext)
            {
                panel.allowedContentTypes = [type]
            }
            let response = await panel.begin()
            guard response == .OK, let url = panel.url else { return .cancelled }
            do {
                try bytes.write(to: url, options: .atomic)
                return .saved
            } catch {
                return .failed("could not write the file")
            }
        }
    #endif
}
