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
// platform and a document picker on the other. Previewing is Quick Look on both
// — the same renderer, in the shape each platform presents it: a sheet holding a
// QLPreviewController on the phone, and on the Mac the system's own floating
// panel, driven through the responder chain. See
// Sources/PassbandiOS/Views/AttachmentPreviewiOS.swift for the phone's half and
// Views/QuickLookPanel.swift for the Mac's.
//
// Nothing in this file draws a preview any more. Two hand-rolled sheets used to
// live at the bottom of it, one per renderable type, and the gate above them had
// to name every type they could draw; the OS renders all of them and more.

import SwiftUI
import UniformTypeIdentifiers

/// The strip's own coordinate space. FILE-SCOPE so the cards can read it from
/// inside `onGeometryChange`'s Sendable closure; one name serves every strip,
/// since a lookup resolves the nearest ancestor that declares it.
private let stripSpace = "attachment-strip"

struct AttachmentStrip: View {
    let attachments: [Attachment]

    @Environment(AppStore.self) private var store
    #if os(macOS)
        /// The handle onto the system's Quick Look panel, and where each
        /// attachment's clickable rect sits so the panel can zoom out of it.
        @State private var quickLook = QuickLookLauncher()
        @State private var sourceFrames: [Int: CGRect] = [:]
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
            strip
                .padding(.top, 4)
                .accessibilityLabel("attachments")
                #if !os(macOS)
                    // A SHEET, not an overlay: an overlay is laid out against the
                    // strip's 38pt frame deep inside the thread's ScrollView, so
                    // it hangs off the window edge and scrolls away with the
                    // content. The Mac needs none of this — its panel is a window.
                    .sheet(item: $staged) { file in
                        QuickLookPreview(url: file.url)
                            .ignoresSafeArea()
                        // No cleanUp here, deliberately: the file belongs to
                        // AttachmentFiles now, and deleting it behind the cache's
                        // back would leave an entry pointing at a removed
                        // directory. The cache evicts, the account switch wipes,
                        // and launch sweeps the root.
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

    /// The strip itself, plus — on the Mac — the invisible view behind it that
    /// owns the Quick Look panel. Both the coordinate space and that view are
    /// pinned to THIS frame, before the padding, so a rect measured in one is a
    /// rect in the other's bounds and the panel's zoom lands on the card.
    private var strip: some View {
        let content = VStack(alignment: .leading, spacing: 10) {
            // The picture first, the filing second. A message that says "see
            // photo attached" is answered by the photo, and the card below is
            // then just where its name and its download live.
            ForEach(inlineImages) { att in
                InlineImage(attachment: att, onOpen: { openPreview(att) })
                    #if os(macOS)
                        .previewSource(att.id, into: $sourceFrames)
                    #endif
            }
            FlowLayout(spacing: 8) {
                ForEach(attachments) { att in
                    AttachmentCard(
                        attachment: att,
                        onDownload: { Task { await download(att) } },
                        onPreview: AttachmentKinds.isPreviewable(att) ? { openPreview(att) } : nil
                    )
                    #if os(macOS)
                        // An attachment already shown inline registers the
                        // PICTURE as what the panel flies out of, not the chip
                        // beneath it, so there is exactly one source rect per id
                        // and it is the one the human actually clicked.
                        .previewSource(
                            AttachmentKinds.isInline(att) ? nil : att.id, into: $sourceFrames)
                    #endif
                }
            }
        }
        .coordinateSpace(.named(stripSpace))

        #if os(macOS)
            // Nothing is drawn here. QLPreviewPanel is a process-wide singleton
            // that asks the FIRST RESPONDER who is driving it, and SwiftUI has no
            // responder to offer — so the strip plants one behind its own cards.
            return content.background {
                QuickLookHost(
                    attachments: attachments.filter(AttachmentKinds.isPreviewable),
                    sourceFrames: sourceFrames,
                    launcher: quickLook,
                    onError: { store.pushToast($0, .error) })
            }
        #else
            return content
        #endif
    }

    // MARK: - preview

    #if os(macOS)
        /// Straight to the system panel. Fetching, staging and the teardown of
        /// both belong to the host view, which is what the panel talks to.
        private func openPreview(_ att: Attachment) { quickLook.open(att) }
    #else
        /// QuickLook reads a FILE, so the sheet opens on a staged one rather than
        /// on a spinner. Usually there is nothing to wait for: a photo rendered in
        /// the column staged its own original on the way past, and this finds it.
        private func openPreview(_ att: Attachment) {
            if let file = AttachmentFiles.shared.cached(att.id) {
                staged = file
                return
            }
            guard opening == nil else { return }
            opening = att.id
            Task {
                defer { opening = nil }
                do {
                    staged = try await AttachmentFiles.shared.file(for: att)
                } catch is CancellationError {
                    // The account changed under the fetch. Nothing to say.
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
    @State private var warmer: Task<Void, Never>?

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
        .onHover { inside in
            hovering = inside
            warmer?.cancel()
            // The pointer is the earliest honest signal that this one is about to
            // be opened, and it arrives a few hundred milliseconds before the
            // click — which is most of a fetch. The DWELL is what keeps that from
            // becoming a download per card the mouse sweeps across on its way
            // somewhere else.
            guard inside, onPreview != nil else { return }
            warmer = Task {
                try? await Task.sleep(for: .milliseconds(140))
                guard !Task.isCancelled else { return }
                AttachmentFiles.shared.warm(attachment)
            }
        }
        .onDisappear { warmer?.cancel() }
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

#if os(macOS)
    extension View {
        /// Record where this view sits in the strip's space, so Quick Look's panel
        /// can fly out of the thing that was clicked rather than the corner of the
        /// window. `nil` opts a view out: two writers for one id would race, and
        /// the loser would aim the animation at the wrong rectangle.
        ///
        /// Mac-only, because the zoom is. The phone previews into a sheet, and a
        /// sheet has nowhere to fly from.
        fileprivate func previewSource(
            _ id: Int?, into frames: Binding<[Int: CGRect]>
        ) -> some View {
            onGeometryChange(for: CGRect.self) {
                $0.frame(in: .named(stripSpace))
            } action: { rect in
                if let id { frames.wrappedValue[id] = rect }
            }
        }
    }
#endif

