// Attachment strip for the thread viewer — one flat card per attachment, shown
// under a message body. Card variants:
//   * image/* (not svg): a real thumbnail of the image.
//   * application/pdf: page 1, rasterized at tile size — a receipt should look
//     like a receipt in the strip. Clicking opens the PDF preview.
//   * everything else, AND image/svg+xml (scriptable — never rendered inline):
//     a file card with filename + human size.
// downloadable=false (bytes over the ingest cap, metadata only) => dimmed card,
// "too large — not stored", no thumbnail/preview/download.
//
// Tiles resolve through AttachmentThumbs, NOT inside the card: these cards live
// in a LazyVStack, and re-downloading the bytes on every recycle is the exact
// bug that cache exists to prevent. Bytes ride the AUTHENTICATED door either
// way — bearer auth can't ride an <img src>, so everything flows through
// APIClient rather than a plain URL.
//
// SECURITY: nothing here trusts the attachment mime for rendering beyond the
// image/pdf/other bucket choice; the SERVER decides what Content-Type it will
// actually serve (svg + html/xml always come back as octet-stream), so a
// mislabeled attachment can't become a scriptable object.
//
// Ported from squelch-desktop/src/components/AttachmentStrip.tsx.

import AppKit
import PDFKit
import SwiftUI

struct AttachmentStrip: View {
    let attachments: [Attachment]

    @Environment(AppStore.self) private var store
    @State private var preview: Attachment?

    /// Images above this skip the auto-fetched thumbnail (a 10MB photo for a
    /// 120px thumb is silly bandwidth) and show the glyph card instead.
    private static let thumbMaxBytes = 2 * 1024 * 1024
    /// PDFs get more headroom than photos: page 1 of a 3MB invoice rasterizes
    /// as cheaply as page 1 of a 30KB one, and receipts/tickets/boarding passes
    /// — the attachments actually worth recognizing at a glance — routinely sit
    /// above the photo cap. Still capped, because pulling a 10MB scanned
    /// brochure down for a 38pt tile is the same silly bandwidth.
    private static let pdfThumbMaxBytes = 4 * 1024 * 1024

    static func isThumbnailable(_ mime: String, _ size: Int) -> Bool {
        mime.hasPrefix("image/") && mime != "image/svg+xml" && size <= thumbMaxBytes
    }
    static func isPDF(_ mime: String) -> Bool { mime == "application/pdf" }

    /// Which rasterizer a card's tile uses, or nil for the glyph. This is the
    /// ONE place the mime buckets are decided — svg lands in the glyph bucket
    /// here and nowhere reconsiders it.
    static func tileSource(_ att: Attachment) -> AttachmentThumbs.Source? {
        guard att.downloadable else { return nil }
        if isThumbnailable(att.mime, att.size) { return .image }
        if isPDF(att.mime), att.size <= pdfThumbMaxBytes { return .pdf }
        return nil
    }

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
            // A SHEET, not an overlay. An overlay here is laid out against the
            // strip's own 38pt-tall frame, deep inside the thread's ScrollView:
            // the 820pt card was centred on the attachment card, hung off the
            // left edge of the window, painted UNDER the messages below it, and
            // scrolled away with the content. A sheet is presented by the
            // window, so it lands where a modal belongs no matter how deep the
            // view that opened it is.
            .sheet(item: $preview) { att in
                PDFPreview(
                    attachment: att,
                    onDownload: { Task { await download(att) } },
                    onClose: { preview = nil })
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
    private var pdf: Bool { stored && AttachmentStrip.isPDF(attachment.mime) }
    private var glyph: String { pdf ? "doc.richtext" : "doc" }

    var body: some View {
        HStack(spacing: 9) {
            ZStack {
                if let source = AttachmentStrip.tileSource(attachment) {
                    ThumbTile(attachment: attachment, source: source, glyph: glyph)
                } else {
                    Image(systemName: glyph)
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
        // A previewable card looks exactly like one that only downloads, so the
        // tooltip is the only place "this opens" is ever said.
        .help(onPreview == nil ? attachment.filename : "\(attachment.filename) — click to preview")
    }
}

/// One attachment's tile. Owns no fetching of its own: AttachmentThumbs resolves
/// (and memoizes) the art, so a card that scrolls out and back paints from cache
/// instead of re-downloading. A failure falls back to the card's file glyph.
private struct ThumbTile: View {
    let attachment: Attachment
    let source: AttachmentThumbs.Source
    let glyph: String

    @State private var resolved: AttachmentThumbs.Tile?

    var body: some View {
        // Read the cache on the way INTO body, not just from `.task`: a
        // recycled card must paint art we already hold on its first frame
        // rather than flash a spinner while the task re-confirms it.
        let tile = resolved ?? AttachmentThumbs.shared.cached(attachment.id)
        Group {
            switch tile {
            case .art(let image)?:
                // A page keeps its aspect (that shape is what reads as "a
                // document"); a photo fills the square.
                Image(nsImage: image)
                    .resizable()
                    .aspectRatio(contentMode: source == .pdf ? .fit : .fill)
            case .blank?:
                Image(systemName: glyph)
                    .font(.system(size: 18, weight: .light))
                    .foregroundStyle(Palette.inkFaintest)
            case nil:
                ProgressView().controlSize(.mini)
            }
        }
        .task { resolved = await AttachmentThumbs.shared.resolve(attachment, as: source) }
    }
}

/// PDF preview. Own "modal" KeyContext so Esc closes it without leaking to the
/// thread keys underneath. Renders natively via PDFKit — no webview and no blob
/// URL, which is strictly stronger than the <embed> the web build used.
private struct PDFPreview: View {
    let attachment: Attachment
    let onDownload: () -> Void
    let onClose: () -> Void

    @State private var document: PDFDocument?
    @State private var error: String?

    /// The scrim, and the card inside it. The gap between them IS the click-off
    /// target, so it has to stay wide enough to hit without aiming.
    ///
    /// Both are FIXED rather than sized to the window, because macOS clamps a
    /// sheet: asking for a 1320x880 sheet on a 1320x880 window gets you 980x640
    /// regardless, so a "cover the window" scrim is not a thing a sheet can be.
    /// These are sized to sit inside that clamp.
    private static let scrimSize = CGSize(width: 940, height: 620)
    private static let cardSize = CGSize(width: 820, height: 520)

    var body: some View {
        ZStack {
            // A sheet brings no scrim of its own, so "click off to close" needs
            // a real view to click. Dimmed the same 14% as every other modal
            // here, with the card centred on it.
            Rectangle()
                .fill(.black.opacity(0.14))
                .contentShape(Rectangle())
                .onTapGesture(perform: onClose)
            card
        }
        .frame(width: Self.scrimSize.width, height: Self.scrimSize.height)
        // Without this the sheet's own backing paints an opaque slab, which
        // both flattens the glass and hides the window the scrim is dimming.
        .presentationBackground(.clear)
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

    private var card: some View {
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
                // A sheet has no scrim to click away, so the Esc hint has to be
                // the button too — otherwise the keyboard is the only exit.
                Button(action: onClose) {
                    HStack(spacing: 4) {
                        Kbd("esc")
                        Text("close").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    }
                }
                .buttonStyle(.plain)
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
        .frame(width: Self.cardSize.width, height: Self.cardSize.height)
        .squelchGlass(.pane, cornerRadius: 13, tint: Palette.glassTint)
        .shadow(color: .black.opacity(0.3), radius: 40, y: 16)
        // The card swallows clicks so they never reach the dismiss scrim under
        // it — otherwise selecting text in the PDF would close the preview.
        .contentShape(Rectangle())
        .onTapGesture {}
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
