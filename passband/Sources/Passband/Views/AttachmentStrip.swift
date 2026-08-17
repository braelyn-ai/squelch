// Attachments for the thread viewer, in two registers. IMAGES RENDER INLINE at
// column width, because "see photo attached" means the photo IS the message and
// every other mail client shows it — a 38pt chip is a filing cabinet, not a
// reading surface. Everything then gets a card in the strip below: image
// thumbnail, PDF page 1, or a file glyph. svg is scriptable so it ALWAYS lands in
// the file bucket, and the SERVER decides Content-Type, so a mislabeled
// attachment stays inert. Art resolves through AttachmentThumbs (a card in a
// LazyVStack would re-download on recycle), authenticated through APIClient.
//
// THE CARDS ARE SHARED; THE TWO VERBS ARE NOT. Saving is a save panel on one
// platform and a document picker on the other, and previewing is a PDFView in a
// modal card here and QuickLook there — different objects, not a different
// spelling of one, so each is fenced and neither pretends to be the other. See
// Sources/PassbandiOS/Views/AttachmentPreviewiOS.swift for the phone's half.

import PDFKit
import SwiftUI
import UniformTypeIdentifiers

struct AttachmentStrip: View {
    let attachments: [Attachment]

    @Environment(AppStore.self) private var store
    #if os(macOS)
        @State private var preview: Attachment?
    #else
        /// The bytes staged on disk for QuickLook, and the export the document
        /// picker is holding. Both are one-at-a-time by construction: a phone
        /// shows one sheet.
        @State private var staged: StagedAttachment?
        @State private var exporting: AttachmentFile?
        @State private var exportName = "attachment"
        @State private var opening: Int?
    #endif

    /// Which rasterizer a card's tile uses, or nil for the glyph. The buckets
    /// themselves live in AttachmentKinds; this is only the mapping from a bucket
    /// to the renderer that draws it.
    static func tileSource(_ att: Attachment) -> AttachmentThumbs.Source? {
        guard att.downloadable else { return nil }
        if AttachmentKinds.isThumbnailable(att.mime, att.size) { return .image }
        if AttachmentKinds.isPDF(att.mime), att.size <= AttachmentKinds.pdfThumbMaxBytes {
            return .pdf
        }
        return nil
    }

    /// The attachments that also render in the column. Order is the server's, so
    /// two photos arrive in the order they were attached.
    private var inlineImages: [Attachment] { attachments.filter(AttachmentKinds.isInline) }

    var body: some View {
        if !attachments.isEmpty {
            VStack(alignment: .leading, spacing: 10) {
                // The picture first, the filing second. A message that says "see
                // photo attached" is answered by the photo, and the card below is
                // then just where its name and its download live.
                ForEach(inlineImages) { att in
                    InlineImage(attachment: att, onOpen: { openPreview(att) })
                }
                FlowLayout(spacing: 8) {
                    ForEach(attachments) { att in
                        AttachmentCard(
                            attachment: att,
                            onDownload: { Task { await download(att) } },
                            onPreview: AttachmentKinds.isPreviewable(att) ? { openPreview(att) } : nil)
                    }
                }
            }
            .padding(.top, 4)
            .accessibilityLabel("attachments")
            // A SHEET, not an overlay: an overlay is laid out against the strip's
            // 38pt frame deep inside the thread's ScrollView, so it hangs off the
            // window edge and scrolls away with the content.
            #if os(macOS)
                .sheet(item: $preview) { att in
                    // Two rasterizers, one card: the scrim, the size and the keys
                    // are the sheet's, and only the middle of it knows a PDF from
                    // a photo.
                    if AttachmentKinds.isPDF(att.mime) {
                        PDFPreview(
                            attachment: att,
                            onDownload: { Task { await download(att) } },
                            onClose: { preview = nil })
                    } else {
                        ImagePreview(
                            attachment: att,
                            onDownload: { Task { await download(att) } },
                            onClose: { preview = nil })
                    }
                }
            #else
                .sheet(item: $staged) { file in
                    QuickLookPreview(url: file.url)
                        .ignoresSafeArea()
                        // The bytes leave the disk with the sheet — nothing an
                        // attachment contained outlives looking at it.
                        .onDisappear { file.cleanUp() }
                }
                .fileExporter(
                    isPresented: Binding(
                        get: { exporting != nil }, set: { if !$0 { exporting = nil } }),
                    document: exporting,
                    contentType: .data,
                    defaultFilename: exportName
                ) { result in
                    switch result {
                    case .success:
                        store.pushToast("saved \(exportName)", .success)
                    case .failure(let error):
                        store.pushToast(errText(error, "save failed"), .error)
                    }
                }
            #endif
        }
    }

    // MARK: - preview

    #if os(macOS)
        private func openPreview(_ att: Attachment) { preview = att }
    #else
        /// QuickLook reads a FILE, so the bytes are fetched and staged before the
        /// sheet opens rather than behind a spinner inside it. The staging
        /// directory belongs to the sheet and is removed on dismiss.
        private func openPreview(_ att: Attachment) {
            guard opening == nil else { return }
            opening = att.id
            Task {
                defer { opening = nil }
                do {
                    let fetched = try await APIClient.shared.fetchAttachment(
                        att.id, fallbackName: att.filename)
                    staged = try StagedAttachment.stage(
                        id: att.id, bytes: fetched.bytes, filename: fetched.filename)
                } catch {
                    store.pushToast(errText(error, "preview failed"), .error)
                }
            }
        }
    #endif

    // MARK: - save

    #if os(macOS)
        private func download(_ att: Attachment) async {
            do {
                let fetched = try await APIClient.shared.fetchAttachment(
                    att.id, fallbackName: att.filename)
                if case .saved = await Downloads.saveBytes(
                    fetched.bytes, filename: fetched.filename)
                {
                    store.pushToast("saved \(fetched.filename)", .success)
                }
            } catch {
                store.pushToast(errText(error, "download failed"), .error)
            }
        }
    #else
        /// Fetch, then hand the bytes to the document picker. The toast waits for
        /// the picker's verdict — a save the human cancelled did not happen.
        private func download(_ att: Attachment) async {
            do {
                let fetched = try await APIClient.shared.fetchAttachment(
                    att.id, fallbackName: att.filename)
                exportName = fetched.filename
                exporting = AttachmentFile(bytes: fetched.bytes)
            } catch {
                store.pushToast(errText(error, "download failed"), .error)
            }
        }
    #endif

}

private struct AttachmentCard: View {
    let attachment: Attachment
    let onDownload: () -> Void
    let onPreview: (() -> Void)?

    @State private var hovering = false

    private var stored: Bool { attachment.downloadable }
    private var pdf: Bool { stored && AttachmentKinds.isPDF(attachment.mime) }
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
        // A previewable card looks exactly like a download-only one, so the
        // tooltip is the only place "this opens" is ever said.
        .help(onPreview == nil ? attachment.filename : "\(attachment.filename) — click to preview")
    }
}

/// One attachment's tile. Owns no fetching: AttachmentThumbs memoizes the art, so
/// a card that scrolls out and back paints from cache instead of re-downloading.
private struct ThumbTile: View {
    let attachment: Attachment
    let source: AttachmentThumbs.Source
    let glyph: String

    @State private var resolved: AttachmentThumbs.Tile?

    var body: some View {
        // Read the cache on the way INTO body, not just from `.task`: a recycled
        // card must paint art we already hold on its first frame rather than
        // flash a spinner while the task re-confirms it.
        let tile = resolved ?? AttachmentThumbs.shared.cached(attachment.id)
        Group {
            switch tile {
            case .art(let image)?:
                // A page keeps its aspect — that shape is what reads as "a
                // document"; a photo fills the square.
                Image(platformImage: image)
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

/// One image attachment, rendered in the message column at the size it was sent
/// to be looked at. Cross-platform on purpose: this is the reading surface, and
/// the two platforms disagree only about what a TAP opens, never about this.
private struct InlineImage: View {
    let attachment: Attachment
    let onOpen: () -> Void

    @State private var resolved: AttachmentThumbs.Tile?
    @State private var hovering = false

    /// Aspect is kept and the height is CAPPED: a portrait photo scaled to the
    /// column's full width would push the rest of the thread off the screen, and
    /// a thread is a conversation before it is a gallery. The tap opens the whole
    /// thing at full size.
    private static let maxHeight: CGFloat = 460
    /// What the placeholder reserves while the bytes are in flight, so the body
    /// above it does not jump when the picture lands.
    private static let placeholderHeight: CGFloat = 160

    var body: some View {
        // Read the cache on the way INTO body for the same reason the tile does:
        // a message scrolled out and back must paint from what we hold.
        let tile = resolved ?? AttachmentThumbs.shared.cachedInline(attachment.id)
        Group {
            switch tile {
            case .art(let image)?:
                Image(platformImage: image)
                    .resizable()
                    .aspectRatio(contentMode: .fit)
                    .frame(maxWidth: .infinity, maxHeight: Self.maxHeight, alignment: .leading)
                    .clipShape(RoundedRectangle(cornerRadius: 9, style: .continuous))
                    .overlay(
                        RoundedRectangle(cornerRadius: 9, style: .continuous)
                            .strokeBorder(Palette.hairline, lineWidth: 1)
                    )
                    .contentShape(Rectangle())
                    .onTapGesture(perform: onOpen)
                    .onHover { hovering = $0 }
                    .opacity(hovering ? 0.93 : 1)
                    .help("\(attachment.filename) — click to open")
                    .accessibilityLabel(attachment.filename)
            // A decode we already know fails is NOT a hole in the column: the
            // card below still names the file and still downloads it.
            case .blank?:
                EmptyView()
            case nil:
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .fill(Palette.hairline.opacity(0.4))
                    .frame(height: Self.placeholderHeight)
                    .overlay(ProgressView().controlSize(.small))
            }
        }
        .task { resolved = await AttachmentThumbs.shared.resolveInline(attachment) }
    }
}

// THE MAC'S PREVIEW, and only the Mac's. A scrim, a fixed card sized to fit
// macOS's sheet clamp, key hints, and a PDFView representable inside it: four
// desktop shapes in one view. The phone's answer to the same tap is QuickLook,
// which brings its own everything — so this stays here rather than growing a
// second layout it would only ever draw on one platform.
#if os(macOS)

/// PDF preview. Own "modal" KeyContext so Esc closes it without leaking to the
/// thread keys underneath. Native PDFKit — no webview, no blob URL.
private struct PDFPreview: View {
    let attachment: Attachment
    let onDownload: () -> Void
    let onClose: () -> Void

    @State private var document: PDFDocument?
    @State private var error: String?

    /// The scrim and the card inside it; the gap between them IS the click-off
    /// target, so it must stay wide enough to hit without aiming. Both are FIXED
    /// rather than window-sized because macOS clamps sheets (~980x640), so these
    /// are sized to sit inside that clamp.
    private static let scrimSize = CGSize(width: 940, height: 620)
    private static let cardSize = CGSize(width: 820, height: 520)

    var body: some View {
        ZStack {
            // A sheet brings no scrim of its own, so "click off to close" needs
            // a real view to click.
            Rectangle()
                .fill(.black.opacity(0.14))
                .contentShape(Rectangle())
                .onTapGesture(perform: onClose)
            card
        }
        .frame(width: Self.scrimSize.width, height: Self.scrimSize.height)
        // Without this the sheet's own backing paints an opaque slab, flattening
        // the glass and hiding the window the scrim is dimming.
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
                ChromeChip(
                    text: "download", icon: "arrow.down.circle", tone: Palette.accent,
                    action: onDownload)
                // The Esc hint has to be a real button too, or the keyboard is
                // the only exit.
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
        .passbandGlass(.pane, cornerRadius: 13, tint: Palette.glassTint)
        .shadow(color: .black.opacity(0.3), radius: 40, y: 16)
        // The card swallows clicks so they never reach the dismiss scrim under it
        // — otherwise selecting text in the PDF would close the preview.
        .contentShape(Rectangle())
        .onTapGesture {}
    }
}

/// The PDFView host. No UIKit twin, deliberately: the phone previews through
/// QLPreviewController, which renders PDFs (and everything else) with a page
/// scrubber, pinch zoom and a share sheet already attached. A hand-built
/// UIViewRepresentable around PDFView would be a strictly worse version of a
/// control the OS ships, and it would exist only to keep this one name
/// cross-platform.
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

/// Image preview — the PDF sheet's twin, sharing its scrim, its clamped card and
/// its Esc. Rasterized through the same ImageIO path everything else here uses
/// rather than handed to `NSImage(data:)`: a 4000px photo decoded at full size to
/// fill an 820pt card is 30MB of bitmap nobody looks at.
private struct ImagePreview: View {
    let attachment: Attachment
    let onDownload: () -> Void
    let onClose: () -> Void

    @State private var image: PlatformImage?
    @State private var error: String?

    private static let scrimSize = CGSize(width: 940, height: 620)
    private static let cardSize = CGSize(width: 820, height: 520)
    /// The card's long edge at 2x. ImageIO does not upscale, so a small logo
    /// opened here is shown at its own size rather than stretched into mush.
    private nonisolated static let maxPixel = 1640

    var body: some View {
        ZStack {
            Rectangle()
                .fill(.black.opacity(0.14))
                .contentShape(Rectangle())
                .onTapGesture(perform: onClose)
            card
        }
        .frame(width: Self.scrimSize.width, height: Self.scrimSize.height)
        .presentationBackground(.clear)
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "close preview", allowInInput: true) { onClose() }
        ])
        .task {
            do {
                let fetched = try await APIClient.shared.fetchAttachment(
                    attachment.id, fallbackName: attachment.filename)
                let bytes = fetched.bytes
                let png = await Task.detached(priority: .userInitiated) { () -> Data? in
                    guard let art = Raster.thumbnail(bytes, maxPixel: Self.maxPixel) else {
                        return nil
                    }
                    return Raster.png(art)
                }.value
                guard let png, let decoded = PlatformImage(data: png) else {
                    error = "could not read that image"
                    return
                }
                image = decoded
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
                ChromeChip(
                    text: "download", icon: "arrow.down.circle", tone: Palette.accent,
                    action: onDownload)
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
                } else if let image {
                    Image(platformImage: image)
                        .resizable()
                        .aspectRatio(contentMode: .fit)
                        .padding(12)
                } else {
                    ProgressView().controlSize(.small)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(width: Self.cardSize.width, height: Self.cardSize.height)
        .passbandGlass(.pane, cornerRadius: 13, tint: Palette.glassTint)
        .shadow(color: .black.opacity(0.3), radius: 40, y: 16)
        .contentShape(Rectangle())
        .onTapGesture {}
    }
}

#endif  // os(macOS) — PDFPreview + ImagePreview + PDFKitView
