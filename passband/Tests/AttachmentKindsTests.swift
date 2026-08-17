// The attachment buckets. This suite exists because of one shipped bug: the
// thread viewer gated "does a click open this" on `isPDF`, so every photo anyone
// ever sent had a tap target that silently did nothing, and a 1.5MB picture of a
// neon sign rendered as a 38pt chip under a message that said "see photo
// attached". Both answers are asserted here rather than left to a view.
//
// The other half is the browser-engine refusal. svg, html, xhtml and webarchives
// all render through WebKit, and Quick Look will draw every one of them on ask —
// so now that previewing is "whatever the OS has a generator for", the ONLY thing
// standing between a stranger's attachment and a script engine is
// `isScriptable`. It is asserted from both directions: the spellings that must be
// caught, and the near-misses that must not be.

import Foundation

@main
@MainActor
struct AttachmentKindsTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        photosAreInlineAndPreviewable()
        svgIsNeverRenderable()
        scriptableNeverOpens()
        nonImagesStayFiled()
        sizeCeilings()
        undownloadableIsInert()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - the shipped bug

    private static func photosAreInlineAndPreviewable() {
        // The real one, byte-for-byte: rebelneonstudios' "Re: company logo".
        let photo = att(mime: "image/jpeg", size: 1_582_543)
        equal(AttachmentKinds.isInline(photo), true, "a phone photo renders in the column")
        equal(AttachmentKinds.isPreviewable(photo), true, "and a click opens it")
        // That one clears the thumbnail cap with room to spare, so it always had
        // a tile — it was the SIZE of the tile that was the bug.
        equal(
            AttachmentKinds.isThumbnailable(photo.mime, photo.size), true,
            "and it was never the thumbnail cap keeping it small")

        // The case that proves inline cannot be keyed off the thumbnail bucket: a
        // full-resolution photo is past the tile cap and must STILL render.
        let big = att(mime: "image/jpeg", size: 6 * 1024 * 1024)
        equal(
            AttachmentKinds.isThumbnailable(big.mime, big.size), false,
            "a 6MB photo is too big for a 38pt tile")
        equal(AttachmentKinds.isInline(big), true, "but it still renders in the column")

        for mime in ["image/png", "image/heic", "image/gif", "image/webp", "IMAGE/JPEG"] {
            let a = att(mime: mime, size: 40_000)
            equal(AttachmentKinds.isInline(a), true, "\(mime) renders inline")
            equal(AttachmentKinds.isPreviewable(a), true, "\(mime) opens")
        }

        // A PDF still opens; it just does not paste itself across the column.
        let pdf = att(mime: "application/pdf", size: 40_000)
        equal(AttachmentKinds.isPreviewable(pdf), true, "a pdf opens")
        equal(AttachmentKinds.isInline(pdf), false, "a pdf does not render inline")
    }

    // MARK: - svg

    private static func svgIsNeverRenderable() {
        // Including the spellings an exact `== "image/svg+xml"` would wave past.
        for mime in [
            "image/svg+xml", "image/svg", "IMAGE/SVG+XML", "image/svg+xml; charset=utf-8",
            "image/xml", "image/svg+xml ",
        ] {
            equal(AttachmentKinds.isRenderableImage(mime), false, "\(mime) is not renderable")
            let a = att(mime: mime, size: 900)
            equal(AttachmentKinds.isInline(a), false, "\(mime) never renders inline")
            equal(AttachmentKinds.isPreviewable(a), false, "\(mime) never opens inline")
            equal(AttachmentKinds.isThumbnailable(mime, 900), false, "\(mime) gets no tile")
        }
    }

    // MARK: - everything else

    private static func nonImagesStayFiled() {
        for mime in ["text/html", "application/zip", "application/octet-stream", "", "image/"] {
            let a = att(mime: mime, size: 1000)
            equal(AttachmentKinds.isInline(a), false, "\(mime) does not render inline")
        }
        // Filed in the strip, but Quick Look still opens them: previewing stopped
        // being "types this app can draw" when the panel became the system's.
        for mime in [
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/zip", "text/plain", "video/quicktime", "application/octet-stream",
        ] {
            let a = att(mime: mime, size: 20_000)
            equal(AttachmentKinds.isInline(a), false, "\(mime) is filed, not pasted inline")
            equal(AttachmentKinds.isPreviewable(a), true, "\(mime) still opens in Quick Look")
        }
    }

    // MARK: - the browser-engine refusal

    private static func scriptableNeverOpens() {
        // Quick Look renders each of these through WebKit. A stranger's
        // attachment does not get to run script, so the click is withheld even
        // though the panel would happily draw it.
        for mime in [
            "text/html", "TEXT/HTML", "text/html; charset=utf-8", "application/xhtml+xml",
            "application/xml", "text/xml", "application/x-webarchive", "image/svg+xml",
        ] {
            equal(AttachmentKinds.isScriptable(mime), true, "\(mime) is a browser-engine type")
            let a = att(mime: mime, size: 4_000)
            equal(AttachmentKinds.isPreviewable(a), false, "\(mime) does not open")
        }
        // And the near-misses that must NOT be swept up with them. The .docx is
        // the one that matters: its mime carries "openxmlformats", so a rule that
        // searches for the letters anywhere refuses every Word document anyone
        // has ever been sent.
        for mime in [
            "text/plain", "application/pdf", "image/png", "application/json",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ] {
            equal(AttachmentKinds.isScriptable(mime), false, "\(mime) is not a browser type")
        }
    }

    private static func sizeCeilings() {
        equal(
            AttachmentKinds.isThumbnailable("image/jpeg", AttachmentKinds.thumbMaxBytes), true,
            "the thumbnail cap is inclusive")
        equal(
            AttachmentKinds.isThumbnailable("image/jpeg", AttachmentKinds.thumbMaxBytes + 1),
            false, "one byte over gets the glyph")
        equal(
            AttachmentKinds.isInline(att(mime: "image/jpeg", size: AttachmentKinds.inlineMaxBytes)),
            true, "the inline cap is inclusive")
        equal(
            AttachmentKinds.isInline(
                att(mime: "image/jpeg", size: AttachmentKinds.inlineMaxBytes + 1)),
            false, "a huge original is filed, not pasted into the column")
        // The ordering that makes the two caps meaningful at all.
        equal(
            AttachmentKinds.inlineMaxBytes > AttachmentKinds.thumbMaxBytes, true,
            "inline has more headroom than a tile")
    }

    private static func undownloadableIsInert() {
        // Over the ingest cap: metadata exists, the bytes route 410s. Nothing may
        // offer to render or open it.
        let ghost = att(mime: "image/jpeg", size: 400_000, downloadable: false)
        equal(AttachmentKinds.isInline(ghost), false, "unstored bytes do not render")
        equal(AttachmentKinds.isPreviewable(ghost), false, "unstored bytes do not open")
    }

    // MARK: - helpers

    private static func att(mime: String, size: Int, downloadable: Bool = true) -> Attachment {
        Attachment(
            id: 1, filename: "IMG_3480.jpeg", mime: mime, size: size, downloadable: downloadable)
    }

    static func equal<T: Equatable>(
        _ got: T, _ want: T, _ label: String, line: Int = #line
    ) {
        checks += 1
        if got != want {
            failures += 1
            print("FAIL (line \(line)): \(label)\n  want: \(want)\n   got: \(got)")
        }
    }
}
