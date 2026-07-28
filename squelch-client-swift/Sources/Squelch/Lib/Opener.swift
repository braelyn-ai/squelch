// Open an external URL in the user's real system browser, and clipboard writes.
//
// SECURITY: only http/https URLs are ever opened. Anything else (mailto:, tel:,
// javascript:, data:, file:, custom schemes) is ignored — we never hand an
// arbitrary scheme to the OS. A failure log carries at most the HOST, never the
// path/query: unsubscribe links are mail-derived and routinely carry
// per-recipient tokens.
//
// Ported from squelch-desktop/src/lib/{opener,clipboard,download}.ts.

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
            // Never swallow silently: a dead button is not diagnosable. Log a
            // STATIC message plus at most the host.
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
