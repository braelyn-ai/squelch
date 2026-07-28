// Attachment strip for the thread viewer — one flat card per attachment, shown
// under a message body. Card variants:
//   * image/* (not svg): a real thumbnail, fetched LAZILY on mount over the
//     authenticated door (bearer auth can't ride an <img src>, so bytes flow
//     through the API client into an NSImage).
//   * application/pdf: a document card; clicking opens the PDF preview overlay.
//   * everything else, AND image/svg+xml (scriptable — never rendered inline):
//     a file card with filename + human size.
// downloadable=false (bytes over the ingest cap, metadata only) => dimmed card,
// "too large — not stored", no thumbnail/preview/download.
//
// SECURITY: nothing here trusts the attachment mime for rendering beyond the
// image/pdf/other bucket choice; the SERVER decides what Content-Type it will
// actually serve (svg + html/xml always come back as octet-stream), so a
// mislabeled attachment can't become a scriptable object.
//
// Ported from squelch-desktop/src/components/AttachmentStrip.tsx.

import PDFKit
import SwiftUI

struct AttachmentStrip: View {
    let attachments: [Attachment]

    @Environment(AppStore.self) private var store
    @State private var preview: Attachment?

    /// Images above this skip the auto-fetched thumbnail (a 10MB photo for a
    /// 120px thumb is silly bandwidth) and show the glyph card instead.
    private static let thumbMaxBytes = 2 * 1024 * 1024

    static func isThumbnailable(_ mime: String, _ size: Int) -> Bool {
        mime.hasPrefix("image/") && mime != "image/svg+xml" && size <= thumbMaxBytes
    }
    static func isPDF(_ mime: String) -> Bool { mime == "application/pdf" }

    var body: some View {
        if !attachments.isEmpty {
            FlowLayout(spacing: 8) {
                ForEach(attachments) { att in
                    AttachmentCard(
                        attachment: att,
                        onDownload: { Task { await download(att) } },
                        onPreview: Self.isPDF(att.mime) && att.downloadable
                            ? { preview = att } : nil)
                }
            }
            .padding(.top, 4)
            .accessibilityLabel("attachments")
            .overlay {
                if let preview {
                    PDFPreview(
                        attachment: preview, onDownload: { Task { await download(preview) } },
                        onClose: { self.preview = nil })
                }
            }
        }
    }

    private func download(_ att: Attachment) async {
        do {
            let fetched = try await APIClient.shared.fetchAttachment(
                att.id, fallbackName: att.filename)
            if case .saved = await Downloads.saveBytes(fetched.bytes, filename: fetched.filename) {
                store.pushToast("saved \(fetched.filename)", .success)
            }
        } catch {
            store.pushToast(errText(error, "download failed"), .error)
        }
    }
}

private struct AttachmentCard: View {
    let attachment: Attachment
    let onDownload: () -> Void
    let onPreview: (() -> Void)?

    @State private var hovering = false

    private var stored: Bool { attachment.downloadable }
    private var thumb: Bool {
        stored && AttachmentStrip.isThumbnailable(attachment.mime, attachment.size)
    }
    private var pdf: Bool { stored && AttachmentStrip.isPDF(attachment.mime) }

    var body: some View {
        HStack(spacing: 9) {
            ZStack {
                if thumb {
                    ThumbImage(attachment: attachment)
                } else {
                    Image(systemName: pdf ? "doc.richtext" : "doc")
                        .font(.system(size: 18, weight: .light))
                        .foregroundStyle(Palette.inkFaintest)
                }
            }
            .frame(width: 38, height: 38)
            .background(
                RoundedRectangle(cornerRadius: 7, style: .continuous)
                    .fill(Palette.hairline.opacity(0.6))
            )
            .clipShape(RoundedRectangle(cornerRadius: 7, style: .continuous))

            VStack(alignment: .leading, spacing: 1) {
                Text(attachment.filename)
                    .font(.system(size: 11, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                Text(stored ? Fmt.humanSize(attachment.size) : "too large — not stored")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
            }
            .frame(maxWidth: 150, alignment: .leading)

            if stored {
                Button(action: onDownload) {
                    Image(systemName: "arrow.down.circle")
                        .font(.system(size: 13))
                        .padding(3)
                }
                .buttonStyle(.plain)
                .foregroundStyle(hovering ? Palette.accent : Palette.inkFaint)
                .help("download \(attachment.filename)")
            }
        }
        .padding(8)
        .background(
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .fill(Palette.hairline.opacity(hovering ? 0.7 : 0.4))
        )
        .opacity(stored ? 1 : 0.55)
        .contentShape(Rectangle())
        .onTapGesture { onPreview?() }
        .onHover { hovering = $0 }
        .help(attachment.filename)
    }
}

/// A lazily-fetched image thumbnail. Fetches bytes on mount over the
/// authenticated door; a failure falls back to the generic file glyph.
private struct ThumbImage: View {
    let attachment: Attachment
    @State private var image: NSImage?
    @State private var failed = false

    var body: some View {
        Group {
            if let image {
                Image(nsImage: image).resizable().aspectRatio(contentMode: .fill)
            } else if failed {
                Image(systemName: "doc")
                    .font(.system(size: 18, weight: .light))
                    .foregroundStyle(Palette.inkFaintest)
            } else {
                ProgressView().controlSize(.mini)
            }
        }
        .task {
            do {
                let fetched = try await APIClient.shared.fetchAttachment(
                    attachment.id, fallbackName: attachment.filename)
                image = NSImage(data: fetched.bytes)
                if image == nil { failed = true }
            } catch {
                failed = true
            }
        }
    }
}

/// PDF preview overlay. Own "modal" KeyContext so Esc closes it without leaking
/// to the thread keys underneath. Renders natively via PDFKit — no webview and
/// no blob URL, which is strictly stronger than the <embed> the web build used.
private struct PDFPreview: View {
    let attachment: Attachment
    let onDownload: () -> Void
    let onClose: () -> Void

    @State private var document: PDFDocument?
    @State private var error: String?

    var body: some View {
        OverlayScrim(onDismiss: onClose) {
            VStack(spacing: 0) {
                HStack(spacing: 10) {
                    Text(attachment.filename)
                        .font(.system(size: 13, weight: .medium))
                        .foregroundStyle(Palette.ink)
                        .lineLimit(1)
                    Spacer(minLength: 8)
                    Button(action: onDownload) {
                        Label("download", systemImage: "arrow.down.circle")
                            .font(Typo.micro)
                            .padding(.horizontal, 8).padding(.vertical, 3)
                    }
                    .buttonStyle(.glass)
                    .foregroundStyle(Palette.accent)
                    HStack(spacing: 4) {
                        Kbd("esc")
                        Text("close").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    }
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 11)

                Divider().overlay(Palette.hairline)

                Group {
                    if let error {
                        Text(error).font(Typo.rowSub).foregroundStyle(Palette.danger)
                    } else if let document {
                        PDFKitView(document: document)
                    } else {
                        ProgressView().controlSize(.small)
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .frame(width: 820, height: 620)
            .squelchGlass(.pane, cornerRadius: 20, tint: Palette.glassTint)
            .shadow(color: .black.opacity(0.3), radius: 44, y: 20)
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "close preview", allowInInput: true) { onClose() }
        ])
        .task {
            do {
                let fetched = try await APIClient.shared.fetchAttachment(
                    attachment.id, fallbackName: attachment.filename)
                document = PDFDocument(data: fetched.bytes)
                if document == nil { error = "could not read that PDF" }
            } catch {
                self.error = errText(error, "preview failed")
            }
        }
    }
}

private struct PDFKitView: NSViewRepresentable {
    let document: PDFDocument

    func makeNSView(context: Context) -> PDFView {
        let view = PDFView()
        view.autoScales = true
        view.displayMode = .singlePageContinuous
        view.backgroundColor = .clear
        view.document = document
        return view
    }

    func updateNSView(_ nsView: PDFView, context: Context) {
        if nsView.document !== document { nsView.document = document }
    }
}
