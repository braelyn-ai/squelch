// THE PHONE'S TWO ATTACHMENT VERBS. The strip itself is shared; only these are
// per-platform, because a save panel and a PDFView representable are desktop
// objects with no UIKit spelling.
//
// SAVE is `.fileExporter` — the document picker — which is the honest twin of
// NSSavePanel: the human chooses a destination in Files and the bytes go there.
// The bytes are held in a value the exporter owns and nowhere else.
//
// PREVIEW is QuickLook, the same renderer the Mac drives — only the presentation
// differs, and that difference is the point. QLPreviewController fills a sheet
// and brings the gestures a phone user already has (pinch to zoom, the page
// scrubber, the share sheet); the Mac gets QLPreviewPanel, a floating window a
// 393pt screen has nowhere to put. Staging is shared: Lib/StagedAttachment.swift
// writes the bytes to a file, because QuickLook reads files.

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
