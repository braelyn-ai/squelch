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

    /// The bare type, lowercased, with any `; charset=…` trimmed. Every rule
    /// below is written against this, so a parameterized header cannot smuggle a
    /// subtype past a refusal by not being string-equal to it.
    static func base(_ mime: String) -> String {
        mime.split(separator: ";").first?
            .trimmingCharacters(in: .whitespaces).lowercased() ?? ""
    }

    /// Image mimes this app will rasterize.
    static func isRenderableImage(_ mime: String) -> Bool {
        let base = base(mime)
        guard base.hasPrefix("image/") else { return false }
        let sub = base.dropFirst("image/".count)
        return !sub.isEmpty && !sub.contains("svg") && !sub.contains("xml")
    }

    /// Types whose renderer is a BROWSER ENGINE. Quick Look draws svg, html,
    /// xhtml and webarchives through WebKit, and a stranger's attachment is the
    /// one thing that must not reach it. The svg rule above is this same refusal
    /// wearing an image mime; this is the rest of it. Nothing here is hidden —
    /// the card, the name and the download button all stay. Only the
    /// click-to-open is withheld.
    static func isScriptable(_ mime: String) -> Bool {
        let base = base(mime)
        if base.hasPrefix("image/") {
            let sub = base.dropFirst("image/".count)
            return sub.contains("svg") || sub.contains("xml")
        }
        let sub = base.split(separator: "/").dropFirst().first.map(String.init) ?? ""
        // `+xml` is the structured syntax SUFFIX and is the only place the letters
        // mean the payload really is xml. Searching for them anywhere refuses a
        // .docx, whose mime is `…vnd.openxmlformats-officedocument…` — a vendor's
        // word that happens to contain them.
        return sub == "html" || sub == "xhtml" || sub == "xml" || sub.hasSuffix("+xml")
            || sub.contains("webarchive")
    }

    static func isPDF(_ mime: String) -> Bool { mime == "application/pdf" }

    static func isThumbnailable(_ mime: String, _ size: Int) -> Bool {
        isRenderableImage(mime) && size <= thumbMaxBytes
    }

    /// Rendered in the message column, not merely filed in the strip below it.
    static func isInline(_ att: Attachment) -> Bool {
        att.downloadable && isRenderableImage(att.mime) && att.size <= inlineMaxBytes
    }

    /// What a click OPENS. Both platforms preview through Quick Look, which
    /// renders whatever the OS has a generator for, so this stopped being a list
    /// of the types this app can draw and became the REFUSAL list: a word doc, a
    /// spreadsheet, a text file and a movie all open, and bytes stay shut only
    /// when there are none stored or when their renderer would be a browser.
    static func isPreviewable(_ att: Attachment) -> Bool {
        att.downloadable && !isScriptable(att.mime)
    }
}
