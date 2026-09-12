// THE TRAY — what a composer shows of the files it will send — and the three
// doors a file comes in by: the paperclip's open panel, a drop anywhere on the
// composer, and (on the Mac, in MarkdownTextView) a drop or a paste into the
// editor itself. All three end in `ComposeAttach.add`.
//
// One row per file, in the order attached, under the editor where a mail
// client puts its attachments. A PICTURE'S ROW SAYS WHERE IT IS: "inline" when
// the body carries its marker, "attached" when it does not, and clicking that
// word flips it — which is the whole UI for the inline/attached choice, because
// the body's marker is the only state there is (see `ComposeMarkers`). The
// reader's attachment cards are the model for the row's chrome: a 38pt tile
// with the picture or a glyph, the name, the size.
//
// SHARED BY BOTH COMPOSERS and both platforms. What is fenced is the drop
// target (a phone has no pointer to drop with) and the pointer cursor.

import SwiftUI
import UniformTypeIdentifiers

/// The files under a composer's editor. Draws NOTHING when there are none,
/// so both composers mount it unconditionally.
struct AttachmentTray: View {
    let slot: DraftSaver.Slot
    /// Review shows the tray read-only: no remove, no inline flip — review is
    /// for reading what is about to go out.
    var editable = true

    @Environment(AppStore.self) private var store

    private var compose: ComposeState? {
        switch slot {
        case .compose: store.compose
        case .inlineReply: store.inlineReply
        }
    }

    var body: some View {
        if let compose, !compose.attachments.isEmpty {
            FlowLayout(spacing: 6) {
                ForEach(compose.attachments) { attachment in
                    AttachmentRow(
                        attachment: attachment,
                        inline: ComposeMarkers.isInline(attachment, in: compose.body),
                        editable: editable, slot: slot)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// One file in the tray.
private struct AttachmentRow: View {
    let attachment: ComposeAttachment
    let inline: Bool
    let editable: Bool
    let slot: DraftSaver.Slot

    @State private var thumb: PlatformImage?
    @State private var hovering = false

    private var glyph: String {
        AttachmentKinds.isPDF(attachment.mime) ? "doc.richtext" : "doc"
    }

    var body: some View {
        HStack(spacing: 9) {
            tile
            VStack(alignment: .leading, spacing: 1) {
                Text(attachment.filename)
                    .font(.system(size: 11, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                    .truncationMode(.middle)
                HStack(spacing: 4) {
                    Text(status)
                        .font(Typo.micro)
                        .foregroundStyle(attachment.failed ? Palette.danger : Palette.inkFaintest)
                    if attachment.isImage && editable && !attachment.failed {
                        Text("·").foregroundStyle(Palette.inkFaintest)
                        // THE INLINE SWITCH is the word itself. Two states,
                        // one verb each, and the caption says which the
                        // picture is in right now.
                        Button {
                            ComposeAttach.toggleInline(attachment, in: slot)
                        } label: {
                            Text(inline ? "inline" : "attached")
                                .font(Typo.micro)
                                .foregroundStyle(inline ? Palette.accent : Palette.inkFaint)
                                .underline(hovering, color: Palette.inkFaintest)
                        }
                        .buttonStyle(.plain)
                        .pointingHand()
                        .help(
                            inline
                                ? "shown in the body where its marker is · click to send as a file instead"
                                : "sent as a file · click to place it in the body")
                    } else if attachment.isImage && inline {
                        Text("· inline").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    }
                }
            }
            .frame(maxWidth: 170, alignment: .leading)

            if editable {
                Button {
                    ComposeAttach.remove(attachment, from: slot)
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.system(size: 12))
                        .padding(3)
                }
                .buttonStyle(.plain)
                .foregroundStyle(hovering ? Palette.ink : Palette.inkFaintest)
                .pointingHand()
                .help("remove \(attachment.filename)")
                .accessibilityLabel("remove \(attachment.filename)")
            }
        }
        .padding(6)
        .padding(.trailing, editable ? 0 : 4)
        .background(
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .fill(Palette.hairline.opacity(hovering ? 0.7 : 0.4))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .strokeBorder(
                    attachment.failed ? Palette.danger.opacity(0.5) : .clear, lineWidth: 1)
        )
        .opacity(attachment.uploading ? 0.7 : 1)
        .onHover { hovering = $0 }
        .task(id: attachment.id) {
            thumb = ComposeAttach.thumbnail(for: attachment)
            if thumb == nil { thumb = await ComposeAttach.warm(attachment) }
        }
    }

    /// The tile: the picture, a document glyph, or the upload in progress.
    private var tile: some View {
        ZStack {
            if let thumb {
                Image(platformImage: thumb)
                    .resizable()
                    .scaledToFill()
            } else {
                Image(systemName: glyph)
                    .font(.system(size: 18, weight: .light))
                    .foregroundStyle(Palette.inkFaintest)
            }
            if attachment.uploading {
                ProgressView()
                    .controlSize(.small)
                    .tint(Palette.accent)
            }
        }
        .frame(width: 38, height: 38)
        .background(
            RoundedRectangle(cornerRadius: 7, style: .continuous)
                .fill(Palette.hairline.opacity(0.6))
        )
        .clipShape(RoundedRectangle(cornerRadius: 7, style: .continuous))
    }

    private var status: String {
        if attachment.failed { return "upload failed · remove and try again" }
        if attachment.uploading { return "uploading…" }
        return Fmt.humanSize(attachment.size)
    }
}

// MARK: - the paperclip

/// The button that opens the file picker. One chip in the composer's own
/// register, beside the tracker toggle. Hidden entirely on a daemon that
/// cannot stage files (see `AppStore.composeAttachmentsAvailable`): offering
/// the button there would be offering a mail without its files.
struct AttachButton: View {
    let slot: DraftSaver.Slot

    @Environment(AppStore.self) private var store
    @State private var picking = false

    var body: some View {
        if store.composeAttachmentsAvailable {
            Button { picking = true } label: {
                Chip(text: "attach", tone: Palette.inkFaint, symbol: "paperclip", filled: false)
            }
            .buttonStyle(.plain)
            .pointingHand()
            .help("attach files · or drop them anywhere on the message")
            .fileImporter(
                isPresented: $picking, allowedContentTypes: [.item],
                allowsMultipleSelection: true
            ) { result in
                guard case .success(let urls) = result else { return }
                ComposeAttach.add(urls: urls, to: slot)
            }
        }
    }
}

// MARK: - the drop target

extension View {
    /// Accept files dropped anywhere on a composer. Pictures go inline at the
    /// end of the text; everything else lands in the tray. The editor itself
    /// takes its own drops (MarkdownTextView), at the point of the drop; this
    /// covers the rest of the surface so a file let go over the subject line
    /// or the tray still lands. A phone has nothing to drop with.
    @ViewBuilder
    func composeDropTarget(_ slot: DraftSaver.Slot, targeted: Binding<Bool>) -> some View {
        #if os(macOS)
            self.onDrop(of: [.fileURL, .image], isTargeted: targeted) { providers in
                ComposeDrop.receive(providers, slot: slot, at: nil)
            }
        #else
            self
        #endif
    }
}

#if os(macOS)
    /// Turning an AppKit drop into `ComposeAttach.add` calls. Files come as
    /// `file-url` items; an image dragged out of another app (a browser, Photos)
    /// comes as image DATA with no file behind it, and gets a name from its
    /// type.
    @MainActor
    enum ComposeDrop {
        /// The types an image-data drop is read as, most specific first: a PNG
        /// is kept as PNG, a JPEG as JPEG, and anything else that calls itself
        /// an image is rasterized to PNG by the drop's own conversion.
        private static let imageTypes: [(UTType, String, String)] = [
            (.png, "image/png", "png"), (.jpeg, "image/jpeg", "jpg"),
            (.gif, "image/gif", "gif"), (.heic, "image/heic", "heic"),
            (.tiff, "image/tiff", "tiff"),
        ]

        static func receive(_ providers: [NSItemProvider], slot: DraftSaver.Slot, at offset: Int?)
            -> Bool
        {
            var handled = false
            for provider in providers {
                if provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
                    handled = true
                    provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier) { item, _ in
                        guard let data = item as? Data,
                            let url = URL(dataRepresentation: data, relativeTo: nil)
                        else { return }
                        Task { @MainActor in ComposeAttach.add(urls: [url], to: slot, at: offset) }
                    }
                    continue
                }
                if let (type, mime, ext) = imageTypes.first(where: {
                    provider.hasItemConformingToTypeIdentifier($0.0.identifier)
                }) {
                    handled = true
                    provider.loadDataRepresentation(forTypeIdentifier: type.identifier) { data, _ in
                        guard let data else { return }
                        Task { @MainActor in
                            ComposeAttach.add(
                                data: data, filename: "image.\(ext)", mime: mime, to: slot,
                                at: offset)
                        }
                    }
                }
            }
            return handled
        }

        /// The files on a pasteboard (a drag's, or the general one on paste).
        static func fileURLs(on pasteboard: NSPasteboard) -> [URL] {
            (pasteboard.readObjects(forClasses: [NSURL.self], options: [
                .urlReadingFileURLsOnly: true
            ]) as? [URL]) ?? []
        }

        /// Image bytes on a pasteboard, as PNG: what a screenshot or a copied
        /// picture is. `nil` when there is none — or when there is TEXT too,
        /// because a copied web selection carries both and the words are what
        /// a paste into an editor means.
        static func imagePNG(on pasteboard: NSPasteboard) -> Data? {
            guard pasteboard.string(forType: .string) == nil else { return nil }
            if let png = pasteboard.data(forType: .png) { return png }
            guard let tiff = pasteboard.data(forType: .tiff),
                let rep = NSBitmapImageRep(data: tiff)
            else { return nil }
            return rep.representation(using: .png, properties: [:])
        }
    }
#endif
