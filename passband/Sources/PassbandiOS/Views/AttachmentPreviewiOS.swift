// THE PHONE'S TWO ATTACHMENT VERBS. The strip itself is shared; only these are
// per-platform, because a save panel and a PDFView representable are desktop
// objects with no UIKit spelling.
//
// SAVE is `.fileExporter` — the document picker — which is the honest twin of
// NSSavePanel: the human chooses a destination in Files and the bytes go there.
// The bytes are held in a value the exporter owns and nowhere else.
//
// PREVIEW is QuickLook rather than a PDFView in a card. That is a deliberate
// substitution, not a shortcut: QLPreviewController is what a phone user already
// knows (pinch to zoom, page scrubber, the share sheet) and it needs no chrome
// of our own, whereas the Mac's fixed 820x520 preview card is a shape a 393pt
// screen has no room for. QuickLook wants a file URL, so the fetched bytes are
// written to a per-preview directory under the app's caches and that directory
// is torn down when the sheet closes — the same lifetime the Mac's in-memory
// PDFDocument has.

import QuickLook
import SwiftUI
import UniformTypeIdentifiers

// MARK: - save

/// The raw bytes of one attachment, wrapped just enough for `.fileExporter`.
/// Import is unreachable (the exporter only writes), so it refuses outright
/// rather than inventing a decode this app has no use for.
struct AttachmentFile: FileDocument {
    static let readableContentTypes: [UTType] = [.data]

    var bytes: Data

    init(bytes: Data) { self.bytes = bytes }

    init(configuration: ReadConfiguration) throws {
        throw CocoaError(.fileReadUnsupportedScheme)
    }

    func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper {
        FileWrapper(regularFileWithContents: bytes)
    }
}

// MARK: - preview

/// A fetched attachment staged on disk for QuickLook, and the staging directory
/// it owns. `cleanUp()` is the whole security contract: mail attachments are not
/// left lying in the caches after the sheet that showed them is gone.
struct StagedAttachment: Identifiable {
    let id: Int
    let url: URL
    let directory: URL

    /// Write `bytes` into a directory unique to this preview, named so QuickLook
    /// picks the right renderer from the extension.
    static func stage(id: Int, bytes: Data, filename: String) throws -> StagedAttachment {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("preview-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        // The server-supplied filename is used for its EXTENSION and is never
        // joined as a path: a name carrying separators would otherwise decide
        // where the write lands.
        let safe = (filename as NSString).lastPathComponent
        let url = directory.appendingPathComponent(safe.isEmpty ? "attachment" : safe)
        try bytes.write(to: url, options: .atomic)
        return StagedAttachment(id: id, url: url, directory: directory)
    }

    func cleanUp() {
        try? FileManager.default.removeItem(at: directory)
    }
}

/// QuickLook over one staged file. No navigation of its own — the sheet is the
/// container, and QuickLook brings its own toolbar.
struct QuickLookPreview: UIViewControllerRepresentable {
    let url: URL

    func makeCoordinator() -> Coordinator { Coordinator(url: url) }

    func makeUIViewController(context: Context) -> QLPreviewController {
        let controller = QLPreviewController()
        controller.dataSource = context.coordinator
        return controller
    }

    func updateUIViewController(_ controller: QLPreviewController, context: Context) {
        guard context.coordinator.url != url else { return }
        context.coordinator.url = url
        controller.reloadData()
    }

    final class Coordinator: NSObject, QLPreviewControllerDataSource {
        var url: URL
        init(url: URL) { self.url = url }

        func numberOfPreviewItems(in controller: QLPreviewController) -> Int { 1 }

        func previewController(
            _ controller: QLPreviewController, previewItemAt index: Int
        ) -> QLPreviewItem {
            url as NSURL
        }
    }
}
