// Attachment bytes on their way to Quick Look, which reads a FILE and not a
// buffer. Both platforms come through here — the Mac hands the URL to
// QLPreviewPanel, the phone to QLPreviewController — so the staging rules are
// written once.
//
// THE NAME IS THE RENDERER, which is the other half — see `safeName`.
//
// THE DIRECTORY IS THE CONTRACT. Mail attachments do not outlive the looking:
// each preview owns a directory that is torn down when its panel or sheet
// closes, every directory sits under ONE root, and `purgeRoot()` at launch
// clears whatever a crash or a hard quit left behind. A staged file is a
// viewer's scratch copy, never a place bytes accumulate.

import Foundation

struct StagedAttachment: Identifiable, Sendable {
    let id: Int
    let url: URL
    let directory: URL

    /// The single root every staged preview lands under, so one sweep empties
    /// them all.
    private static var root: URL {
        FileManager.default.temporaryDirectory
            .appendingPathComponent("attachment-previews", isDirectory: true)
    }

    /// Write `bytes` into a directory unique to this preview, named so Quick Look
    /// picks the right renderer from the extension.
    static func stage(id: Int, bytes: Data, filename: String) throws -> StagedAttachment {
        let directory = root.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let url = directory.appendingPathComponent(safeName(filename))
        try bytes.write(to: url, options: .atomic)
        return StagedAttachment(id: id, url: url, directory: directory)
    }

    /// THE NAME IS THE RENDERER. Quick Look reads the extension off this string
    /// and nothing else, so two things are settled here rather than trusted:
    ///
    /// WHERE THE WRITE LANDS. The server-supplied filename is used for its
    /// extension and is never joined as a path — a name carrying separators, or
    /// one that IS a traversal, would otherwise choose the destination.
    ///
    /// WHICH RENDERER DRAWS IT. An extension that hands the file to a browser
    /// engine is defused by appending `.txt`, which makes the plain-text
    /// generator the one that opens it. `isPreviewable` already withholds the
    /// click for those, so this should be unreachable from the UI; it is here
    /// because the name written to disk comes from the response's
    /// `Content-Disposition` and the name the gate read came from the message
    /// JSON, and nothing guarantees the two agree.
    static func safeName(_ filename: String) -> String {
        let last = (filename as NSString).lastPathComponent
        // `lastPathComponent` answers "/" for a name that is nothing but
        // separators, which appends to the staging directory as the directory
        // itself — so a separator surviving this far is a fallback, not a name.
        let unusable = last.isEmpty || last.contains("/") || last == "." || last == ".."
        let safe = unusable ? "attachment" : last
        return AttachmentKinds.hasScriptableExtension(safe) ? safe + ".txt" : safe
    }

    func cleanUp() {
        try? FileManager.default.removeItem(at: directory)
    }

    /// Empty the staging root. Called once at launch: the per-preview teardown
    /// is the rule, and this is what makes the rule hold across a crash.
    ///
    /// MOVED ASIDE FIRST, deleted after. The delete is a recursive unlink of
    /// however many megabytes were open when the app died and has no business on
    /// the launch path; the move is one syscall. Doing it in that order is also
    /// what keeps a preview opened while the sweep is still running from staging
    /// its bytes into a directory that is being torn down underneath it.
    static func purgeRoot() {
        let fm = FileManager.default
        let aside = fm.temporaryDirectory
            .appendingPathComponent("attachment-previews-sweeping", isDirectory: true)
        // A sweep the app did not live long enough to finish. One fixed name, so
        // the next launch finds it rather than accumulating a directory per run.
        try? fm.removeItem(at: aside)
        guard (try? fm.moveItem(at: root, to: aside)) != nil else { return }
        Task.detached(priority: .utility) { try? fm.removeItem(at: aside) }
    }
}
