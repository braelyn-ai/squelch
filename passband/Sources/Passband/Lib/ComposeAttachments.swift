// FILES ON THEIR WAY OUT, the pure half. What a composer holds about each file
// it has attached, and the one rule that decides how the file goes out: A
// PICTURE IS INLINE EXACTLY WHEN THE BODY POINTS AT IT. The body is markdown,
// the pointer is `![name](cid:token)`, and the daemon reads the same rule off
// the rendered html — so there is no second flag to keep in step, and deleting
// the marker from the text is how a picture becomes a plain attachment.
//
// Nothing here touches the network or the store; the upload and the tray live
// in Model/ComposeAttach.swift and Views/AttachmentTray.swift. Kept pure so
// the marker grammar — the thing a stray edit could silently break — compiles
// alone under test.sh.

import Foundation
import UniformTypeIdentifiers

/// One file in a composer's tray. A VALUE inside `ComposeState`, so it copies
/// with the draft and compares with it.
///
/// `id` is the daemon's, and nil while the upload is in flight: the tray shows
/// the file the instant it is dropped, the marker is already in the body, and
/// the id lands a round-trip later. A send refuses to start while any is nil —
/// the mail must carry what the tray shows.
struct ComposeAttachment: Sendable, Equatable, Identifiable, Hashable {
    /// Client-side identity, minted at drop time. THIS is `Identifiable`'s id,
    /// not the daemon's: a row has to be addressable before the upload answers,
    /// and stable across the moment it does.
    let key: UUID
    /// The daemon's id once staged; nil while uploading; nil AND `failed` when
    /// the upload did not land.
    var id: Int?
    var filename: String
    var mime: String
    var size: Int
    /// The `cid:` token, minted HERE so the body can reference the file before
    /// the daemon has seen it. The daemon stores it verbatim.
    var contentId: String
    var failed = false

    init(
        key: UUID = UUID(), id: Int? = nil, filename: String, mime: String, size: Int,
        contentId: String, failed: Bool = false
    ) {
        self.key = key
        self.id = id
        self.filename = filename
        self.mime = mime
        self.size = size
        self.contentId = contentId
        self.failed = failed
    }

    /// A restored draft's file: already staged, so it arrives with its id.
    init(_ wire: OutboundAttachment) {
        self.init(
            id: wire.id, filename: wire.filename, mime: wire.mime, size: wire.size,
            contentId: wire.content_id)
    }

    var uploading: Bool { id == nil && !failed }

    /// Whether this is a picture the composer places INLINE by default. The
    /// reader's own rule (`AttachmentKinds.isRenderableImage`): svg is a file,
    /// because it is a script wearing an image mime and no mail client draws
    /// it inline anyway.
    var isImage: Bool { AttachmentKinds.isRenderableImage(mime) }
}

/// The marker grammar and the mime/token minting — every pure decision the
/// tray and the upload make.
enum ComposeMarkers {
    /// What a `cid:` token may be spelled with. MIRRORS THE DAEMON
    /// (`handlers::content_id_ok`): the token goes into a MIME header and into
    /// html, so it is the address-ish alphabet and nothing else. A UUID's hex
    /// and hyphens are inside it, which is the whole reason a UUID is the mint.
    static func mintContentId() -> String {
        "\(UUID().uuidString.lowercased())@passband"
    }

    /// The body's reference to an inline picture: a markdown image whose
    /// destination is the part. The alt is the filename with the brackets that
    /// would end the alt or the link early taken out, so a name like
    /// `a](b).png` cannot break the marker around itself.
    static func marker(for attachment: ComposeAttachment) -> String {
        let alt = attachment.filename.filter { !"[]()".contains($0) }
        return "![\(alt)](cid:\(attachment.contentId))"
    }

    /// The substring the daemon tests for. Both sides look for the raw
    /// `cid:<token>`, so the marker's alt text is free to change without
    /// changing what is inline.
    static func reference(_ attachment: ComposeAttachment) -> String {
        "cid:\(attachment.contentId)"
    }

    /// Whether the body places this file inline. The single source of truth.
    static func isInline(_ attachment: ComposeAttachment, in body: String) -> Bool {
        body.contains(reference(attachment))
    }

    /// `body` with the marker inserted at `offset` — a UTF-16 offset, which is
    /// what AppKit's `characterIndexForInsertion` and a selection range speak;
    /// nil or out of range appends. Placed on its own line: an image in the
    /// middle of a sentence renders as an inline box in most clients, and a
    /// picture somebody dropped into a mail is a paragraph, not a word.
    static func insertMarker(
        _ attachment: ComposeAttachment, into body: String, at offset: Int?
    ) -> String {
        let marker = marker(for: attachment)
        let count = body.utf16.count
        let at = min(max(offset ?? count, 0), count)
        // Never split a surrogate pair: an offset that lands inside one is
        // nudged to the pair's start, which `String.Index` rounds to anyway.
        let cut = String.Index(utf16Offset: at, in: body)
        let before = String(body[..<cut])
        let after = String(body[cut...])
        var out = before
        if !before.isEmpty && !before.hasSuffix("\n") { out += "\n" }
        out += marker
        if after.isEmpty {
            out += "\n"
        } else if !after.hasPrefix("\n") {
            out += "\n"
        }
        out += after
        return out
    }

    /// `body` with every marker for this file removed — the way a picture is
    /// turned back into a plain attachment, and what happens to the text when
    /// a file is taken out of the tray. Removes the marker's own line break
    /// too when it sits alone on a line, so the text closes up rather than
    /// keeping a blank line where a picture was.
    static func removeMarker(_ attachment: ComposeAttachment, from body: String) -> String {
        let reference = reference(attachment)
        guard body.contains(reference) else { return body }
        var lines = body.components(separatedBy: "\n")
        lines = lines.compactMap { line in
            guard line.contains(reference) else { return line }
            let stripped = stripMarkers(reference, from: line)
            // The whole line was the marker: drop the line.
            return stripped.trimmingCharacters(in: .whitespaces).isEmpty ? nil : stripped
        }
        return lines.joined(separator: "\n")
    }

    /// One line with every `![…](cid:token)` for `reference` cut out of it.
    private static func stripMarkers(_ reference: String, from line: String) -> String {
        var out = line
        while let range = out.range(of: reference) {
            // Walk back to the `![` that opens this marker and forward to the
            // `)` that closes it; a reference not inside a marker (typed by
            // hand) is left alone by cutting only the reference itself.
            let open = out[..<range.lowerBound].range(of: "![", options: .backwards)
            let close = out[range.upperBound...].firstIndex(of: ")")
            if let open, let close,
                !out[open.upperBound..<range.lowerBound].contains(")")
            {
                out.removeSubrange(open.lowerBound...close)
            } else {
                out.removeSubrange(range)
            }
        }
        return out
    }

    /// The mime a file is uploaded under, from its extension. Unknown
    /// extensions are a blob; the daemon would downgrade anything it cannot
    /// name anyway.
    static func mime(for url: URL) -> String {
        let ext = url.pathExtension
        guard !ext.isEmpty, let type = UTType(filenameExtension: ext),
            let mime = type.preferredMIMEType
        else { return "application/octet-stream" }
        return mime
    }

    /// The daemon's own per-file ceiling, refused here before a 25 MB upload
    /// is attempted: the send could never carry it.
    static let maxBytes = 25 * 1024 * 1024
}
