// WHICH ATTACHMENT GETS WHICH TREATMENT — the mime and size buckets, kept out of
// the view that draws them so they can be asserted rather than reasoned about.
// Three questions: does it render inline in the message column, does its card get
// a thumbnail, and does a click open it.
//
// svg is why this is ONE place and not three. Any svg/xml-ish image subtype is
// scriptable, so it has to fall out of every renderable bucket, and a rule spelled
// three times is a rule that will be spelled two ways. The daemon's
// `safe_content_type` refuses to serve those as `image/*` at all; this is the
// client agreeing with it rather than trusting an exact string the server never
// promised to send.

import Foundation

enum AttachmentKinds {
    /// Thumbnails above this show the glyph card instead — 10MB of transfer for a
    /// 38pt tile is silly.
    static let thumbMaxBytes = 2 * 1024 * 1024

    /// PDFs get more headroom than photos: page 1 rasterizes as cheaply for a 3MB
    /// invoice as for a 30KB one, and receipts/tickets — the attachments worth
    /// recognizing at a glance — routinely sit above the photo cap.
    static let pdfThumbMaxBytes = 4 * 1024 * 1024

    /// The INLINE ceiling is far higher than the thumbnail's, deliberately: 2MB
    /// spent on a 38pt tile is waste, but the same 2MB spent showing the photo
    /// somebody wrote the email about IS the email. A phone photo clears the
    /// thumbnail cap routinely and must still render.
    static let inlineMaxBytes = 12 * 1024 * 1024

    /// Image mimes this app will rasterize. Matched on the bare type with any
    /// `; charset=…` trimmed, so a parameterized header cannot smuggle a subtype
    /// past the svg refusal by not being string-equal to it.
    static func isRenderableImage(_ mime: String) -> Bool {
        let base =
            mime.split(separator: ";").first?
            .trimmingCharacters(in: .whitespaces).lowercased() ?? ""
        guard base.hasPrefix("image/") else { return false }
        let sub = base.dropFirst("image/".count)
        return !sub.isEmpty && !sub.contains("svg") && !sub.contains("xml")
    }

    static func isPDF(_ mime: String) -> Bool { mime == "application/pdf" }

    static func isThumbnailable(_ mime: String, _ size: Int) -> Bool {
        isRenderableImage(mime) && size <= thumbMaxBytes
    }

    /// Rendered in the message column, not merely filed in the strip below it.
    static func isInline(_ att: Attachment) -> Bool {
        att.downloadable && isRenderableImage(att.mime) && att.size <= inlineMaxBytes
    }

    /// What a click OPENS. Both platforms have a real viewer for images, so this
    /// is no longer "is it a PDF" — that gate is what left every photo in the
    /// mailbox with a tap target that did nothing.
    static func isPreviewable(_ att: Attachment) -> Bool {
        att.downloadable && (isPDF(att.mime) || isRenderableImage(att.mime))
    }
}
