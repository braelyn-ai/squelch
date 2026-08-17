// Attachment bytes on their way to Quick Look, which reads a FILE and not a
// buffer. Both platforms come through here — the Mac hands the URL to
// QLPreviewPanel, the phone to QLPreviewController — so the staging rules are
// written once.
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
        // The server-supplied filename is used for its EXTENSION and is never
        // joined as a path: a name carrying separators — or one that IS a
        // traversal — would otherwise decide where the write lands.
        let last = (filename as NSString).lastPathComponent
        let safe = (last.isEmpty || last == "." || last == "..") ? "attachment" : last
        let url = directory.appendingPathComponent(safe)
        try bytes.write(to: url, options: .atomic)
        return StagedAttachment(id: id, url: url, directory: directory)
    }

    func cleanUp() {
        try? FileManager.default.removeItem(at: directory)
    }

    /// Empty the staging root. Called once at launch: the per-preview teardown
    /// is the rule, and this is what makes the rule hold across a crash.
    static func purgeRoot() {
        try? FileManager.default.removeItem(at: root)
    }
}
