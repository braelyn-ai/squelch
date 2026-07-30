// Open an external URL in the system browser, plus clipboard and save-panel
// helpers.
//
// Only http/https is ever handed to the OS; every other scheme is ignored. A
// failure log carries at most the HOST — mail-derived links routinely carry
// per-recipient tokens. See docs/SECURITY.md §2.

import AppKit
import Foundation
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
        if !NSWorkspace.shared.open(parsed) {
            // A dead button must be diagnosable: static text plus the host only.
            NSLog("openExternal: failed to open external URL (host: %@)", safeHost(url))
        }
    }
}

enum Clip {
    /// Copy text to the general pasteboard. Returns whether it landed.
    @MainActor
    @discardableResult
    static func copy(_ text: String) -> Bool {
        let pb = NSPasteboard.general
        pb.clearContents()
        return pb.setString(text, forType: .string)
    }
}

enum Downloads {
    enum Result { case saved, cancelled, failed(String) }

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
}
