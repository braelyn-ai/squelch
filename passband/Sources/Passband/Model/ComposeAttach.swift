// ATTACHING A FILE, the half with side effects. A file dropped on a composer is
// three things at once: a row in the tray (instantly), a marker in the body if
// it is a picture (instantly), and an upload to the daemon (a round-trip later,
// which is when the row gets the id a draft save and a send name). Every path a
// file can arrive by — the paperclip's open panel, a drop on the pane, a drop on
// the editor, a paste — lands in `add` below, so the three happen together or
// not at all.
//
// THE SLOT IS A VALUE. `store.compose` / `store.inlineReply` can be replaced
// while an upload is out (Escape, then a new composer), so every write after an
// await is keyed to the composer's `ComposeState.id` and lands nowhere if the
// slot moved on — exactly the discipline `ComposePane.fire` keeps. An upload
// whose composer has gone leaves an unclaimed row the daemon sweeps.

import Foundation
import ImageIO

#if os(macOS)
    import AppKit
#else
    import UIKit
#endif

@MainActor
enum ComposeAttach {
    /// Thumbnails by `contentId` — the one identity a tray row has from the
    /// moment it appears (the daemon id lands later, and the key is per
    /// composer). In memory for the session; a restored draft's picture is
    /// fetched back on first sight and lands here too.
    private static var thumbs: [String: PlatformImage] = [:]
    private static var thumbFetches: [String: Task<PlatformImage?, Never>] = [:]

    /// The thumbnail's longest side, in pixels. The tray tile is 38pt; two
    /// and a half times that covers a Retina tile with room for the
    /// `scaledToFill` crop.
    private static let thumbPixels = 96

    // MARK: - adding

    /// Files picked or dropped. Each becomes its own row and its own upload;
    /// one that cannot be read is reported and skipped rather than failing the
    /// rest. `offset` is where an image's marker goes in the body (a character
    /// offset), or nil for "at the end, above the signature".
    static func add(urls: [URL], to slot: DraftSaver.Slot, at offset: Int? = nil) {
        var at = offset
        for url in urls {
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            guard let data = try? Data(contentsOf: url) else {
                AppStore.shared.pushToast("could not read \(url.lastPathComponent)", .error)
                continue
            }
            let placed = add(
                data: data, filename: url.lastPathComponent, mime: ComposeMarkers.mime(for: url),
                to: slot, at: at)
            // Two pictures dropped together go in the order they were dropped,
            // one after the other, rather than the second landing above the
            // first.
            if let placed, let current = at { at = current + placed }
        }
    }

    /// One file's bytes, from wherever they came. Returns how many UTF-16
    /// units the body grew by when a marker was placed (so a caller placing
    /// several can keep their order), nil when nothing was placed.
    @discardableResult
    static func add(
        data: Data, filename: String, mime: String, to slot: DraftSaver.Slot, at offset: Int? = nil
    ) -> Int? {
        // Edit phase only. Review is for reading what goes out, and a file
        // arriving there — a drop that missed, a late paste — would change
        // the mail under the sender's eyes without the ceremony noticing.
        guard var state = read(slot), state.phase == .edit, !state.sending else { return nil }
        guard !data.isEmpty else {
            AppStore.shared.pushToast("\(filename) is empty", .error)
            return nil
        }
        guard data.count <= ComposeMarkers.maxBytes else {
            AppStore.shared.pushToast("\(filename) is over 25 MB", .error)
            return nil
        }
        let attachment = ComposeAttachment(
            filename: filename, mime: mime, size: data.count,
            contentId: ComposeMarkers.mintContentId())
        state.attachments.append(attachment)
        var grew: Int? = nil
        // A PICTURE GOES INLINE BY DEFAULT: the marker is written into the
        // body now, where the drop landed, so the editor shows the placement
        // before the bytes have even left the machine.
        if attachment.isImage {
            let before = state.body.utf16.count
            state.body = ComposeMarkers.insertMarker(
                attachment, into: state.body, at: offset ?? aboveSignature(state.body))
            grew = state.body.utf16.count - before
            if let image = thumbnail(from: data) { thumbs[attachment.contentId] = image }
        }
        write(slot, state)
        DraftSaver.shared.noteChange(slot)

        let composer = state.id
        Task {
            do {
                let staged = try await APIClient.shared.stageAttachment(
                    filename: filename, mime: mime, contentId: attachment.contentId, data: data)
                patch(slot, composer) { next in
                    guard let i = next.attachments.firstIndex(where: { $0.key == attachment.key })
                    else { return }
                    next.attachments[i].id = staged.id
                    // The daemon's view of the name and type is the one the
                    // mail will carry; show that, not the local guess.
                    next.attachments[i].filename = staged.filename
                    next.attachments[i].mime = staged.mime
                }
                // The id is what the draft has to record; arm a save now
                // rather than waiting for the next keystroke.
                DraftSaver.shared.noteChange(slot)
            } catch {
                patch(slot, composer) { next in
                    guard let i = next.attachments.firstIndex(where: { $0.key == attachment.key })
                    else { return }
                    next.attachments[i].failed = true
                }
                AppStore.shared.pushToast(
                    errText(error, "could not attach \(filename)"), .error)
            }
        }
        return grew
    }

    /// Where an image goes when nobody said: the end of what was typed, ABOVE
    /// the seeded signature rather than under it.
    private static func aboveSignature(_ body: String) -> Int? {
        let seed = Prefs.shared.signatureSeed
        guard !seed.isEmpty, body.hasSuffix(seed) else { return nil }
        return body.utf16.count - seed.utf16.count
    }

    // MARK: - editing the tray

    /// Take a file out of the tray. Its marker goes with it — a body pointing
    /// at a part that is not there would render as a broken image — and the
    /// staged row is dropped, best-effort: the sweep takes it otherwise.
    static func remove(_ attachment: ComposeAttachment, from slot: DraftSaver.Slot) {
        guard var state = read(slot) else { return }
        state.attachments.removeAll { $0.key == attachment.key }
        state.body = ComposeMarkers.removeMarker(attachment, from: state.body)
        write(slot, state)
        DraftSaver.shared.noteChange(slot)
        thumbs[attachment.contentId] = nil
        if let id = attachment.id {
            Task { try? await APIClient.shared.deleteComposeAttachment(id) }
        }
    }

    /// Flip a picture between inline and attached: place its marker at the end
    /// of the text, or take the marker out. The row stays either way; only
    /// where the picture lands in the mail changes.
    static func toggleInline(_ attachment: ComposeAttachment, in slot: DraftSaver.Slot) {
        guard var state = read(slot) else { return }
        if ComposeMarkers.isInline(attachment, in: state.body) {
            state.body = ComposeMarkers.removeMarker(attachment, from: state.body)
        } else {
            state.body = ComposeMarkers.insertMarker(
                attachment, into: state.body, at: aboveSignature(state.body))
        }
        write(slot, state)
        DraftSaver.shared.noteChange(slot)
    }

    /// Try the upload again for a row whose first attempt failed. The bytes
    /// are not kept — a failed upload of a 20 MB file is not worth 20 MB of
    /// memory on the off chance — so this re-reads nothing and simply tells
    /// the caller to drop and re-add. Kept as a function so the tray's button
    /// has one place to point at.
    static func discardFailed(_ attachment: ComposeAttachment, from slot: DraftSaver.Slot) {
        remove(attachment, from: slot)
    }

    // MARK: - thumbnails

    /// The picture for a tray tile, if this is a picture and its bytes have
    /// been seen. Synchronous and cache-only; `warm` fills it.
    static func thumbnail(for attachment: ComposeAttachment) -> PlatformImage? {
        thumbs[attachment.contentId]
    }

    /// Fetch a restored draft's picture back from the daemon for its tile.
    /// One fetch per file however many tiles ask; nothing for a file whose
    /// bytes were seen at drop time.
    static func warm(_ attachment: ComposeAttachment) async -> PlatformImage? {
        if let hit = thumbs[attachment.contentId] { return hit }
        guard attachment.isImage, let id = attachment.id else { return nil }
        if let running = thumbFetches[attachment.contentId] { return await running.value }
        let key = attachment.contentId
        let task = Task<PlatformImage?, Never> {
            guard let data = try? await APIClient.shared.composeAttachmentBytes(id) else {
                return nil
            }
            return thumbnail(from: data)
        }
        thumbFetches[key] = task
        let image = await task.value
        thumbFetches[key] = nil
        if let image { thumbs[key] = image }
        return image
    }

    private static func thumbnail(from data: Data) -> PlatformImage? {
        guard let cg = Raster.thumbnail(data, maxPixel: thumbPixels) else { return nil }
        #if os(macOS)
            return NSImage(cgImage: cg, size: NSSize(width: cg.width, height: cg.height))
        #else
            return UIImage(cgImage: cg)
        #endif
    }

    /// Account switch: another daemon's ids, another person's pictures.
    static func wipe() {
        thumbs.removeAll()
        thumbFetches.removeAll()
    }

    // MARK: - the slot

    private static func read(_ slot: DraftSaver.Slot) -> ComposeState? {
        switch slot {
        case .compose: AppStore.shared.compose
        case .inlineReply: AppStore.shared.inlineReply
        }
    }

    private static func write(_ slot: DraftSaver.Slot, _ state: ComposeState) {
        switch slot {
        case .compose: AppStore.shared.compose = state
        case .inlineReply: AppStore.shared.inlineReply = state
        }
    }

    /// Patch the slot ONLY IF it still holds composer `id` — every write after
    /// an await goes through here. See the file header.
    private static func patch(
        _ slot: DraftSaver.Slot, _ id: UUID, _ mutate: (inout ComposeState) -> Void
    ) {
        guard var next = read(slot), next.id == id else { return }
        mutate(&next)
        write(slot, next)
    }
}
