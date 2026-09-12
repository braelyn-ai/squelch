// THE MARKER GRAMMAR, asserted. A picture is inline exactly when the body
// carries `cid:<token>`; the daemon reads the same substring off the rendered
// html. Everything the tray does to the text goes through these three
// functions, so they are pinned rather than reasoned about — and the token
// alphabet is pinned against the daemon's, because a token the daemon refuses
// is a 400 on every upload.

import Foundation

@main
@MainActor
struct ComposeAttachmentsTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        markerShape()
        tokensFitTheDaemonsAlphabet()
        insertionPlacesOnItsOwnLine()
        insertionSpeaksUTF16()
        removalClosesUpTheText()
        removalLeavesOtherMarkersAlone()
        inlineIsTheBodysWord()
        imagesAndFilesBucket()
        wireRestoreCarriesTheId()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    private static func att(_ name: String, _ mime: String = "image/png", cid: String = "tok-1@passband")
        -> ComposeAttachment
    {
        ComposeAttachment(filename: name, mime: mime, size: 3, contentId: cid)
    }

    private static func markerShape() {
        expect(
            ComposeMarkers.marker(for: att("shot.png")) == "![shot.png](cid:tok-1@passband)",
            "the marker is a markdown image whose destination is the part")
        // A name that could close the alt or the link early is defused, and
        // the reference stays the same.
        let tricky = att("a](b).png")
        expect(ComposeMarkers.marker(for: tricky) == "![ab.png](cid:tok-1@passband)", "defused alt")
        expect(ComposeMarkers.reference(tricky) == "cid:tok-1@passband", "reference is the raw cid")
    }

    private static func tokensFitTheDaemonsAlphabet() {
        // MIRRORS `handlers::content_id_ok`: alphanumerics and . _ - @ only,
        // at most 128 long.
        let allowed = Set("abcdefghijklmnopqrstuvwxyz0123456789._-@")
        for _ in 0..<50 {
            let token = ComposeMarkers.mintContentId()
            expect(token.allSatisfy { allowed.contains($0) }, "minted token stays in the alphabet: \(token)")
            expect(token.count <= 128, "minted token fits")
            expect(token.hasSuffix("@passband"), "minted token is scoped")
        }
        expect(ComposeMarkers.mintContentId() != ComposeMarkers.mintContentId(), "tokens differ")
    }

    private static func insertionPlacesOnItsOwnLine() {
        let a = att("shot.png")
        let marker = ComposeMarkers.marker(for: a)
        expect(
            ComposeMarkers.insertMarker(a, into: "", at: nil) == "\(marker)\n",
            "an empty body gets the marker and a line to type on")
        expect(
            ComposeMarkers.insertMarker(a, into: "hello", at: nil) == "hello\n\(marker)\n",
            "appended on its own line")
        expect(
            ComposeMarkers.insertMarker(a, into: "hello\nworld", at: 5) == "hello\n\(marker)\nworld",
            "inserted between lines without doubling the breaks")
        expect(
            ComposeMarkers.insertMarker(a, into: "hello world", at: 5) == "hello\n\(marker)\n world",
            "inserted mid-line splits the line around it")
        // Out of range clamps rather than crashing.
        expect(
            ComposeMarkers.insertMarker(a, into: "hi", at: 99) == "hi\n\(marker)\n",
            "past the end appends")
        expect(
            ComposeMarkers.insertMarker(a, into: "hi", at: -4) == "\(marker)\nhi",
            "before the start prepends")
        // Above the signature is a caller concern, but the seam it relies on
        // holds: an offset at the start of the seed lands the marker before it.
        let body = "note\n\n-- \nme"
        let at = body.utf16.count - "\n\n-- \nme".utf16.count
        expect(
            ComposeMarkers.insertMarker(a, into: body, at: at) == "note\n\(marker)\n\n-- \nme",
            "placed above a signature")
    }

    private static func insertionSpeaksUTF16() {
        // AppKit's insertion index counts UTF-16 units; an emoji is two.
        let a = att("shot.png")
        let marker = ComposeMarkers.marker(for: a)
        let body = "🎉 party\nsoon"
        // After "🎉 party" = 2 + 6 = 8 units.
        expect(
            ComposeMarkers.insertMarker(a, into: body, at: 8) == "🎉 party\n\(marker)\nsoon",
            "offset 8 is the end of the first line, not inside 'soon'")
        // An offset INSIDE the surrogate pair does not split it.
        let split = ComposeMarkers.insertMarker(a, into: body, at: 1)
        expect(split.contains("🎉"), "the emoji survives an offset inside it: \(split)")
    }

    private static func removalClosesUpTheText() {
        let a = att("shot.png")
        let marker = ComposeMarkers.marker(for: a)
        expect(
            ComposeMarkers.removeMarker(a, from: "hello\n\(marker)\nworld") == "hello\nworld",
            "a marker alone on its line takes the line with it")
        expect(
            ComposeMarkers.removeMarker(a, from: "\(marker)\n") == "",
            "an empty body comes back empty")
        expect(
            ComposeMarkers.removeMarker(a, from: "see \(marker) here") == "see  here",
            "a marker inside a line is cut out of it")
        expect(
            ComposeMarkers.removeMarker(a, from: "no marker here") == "no marker here",
            "nothing to remove is a no-op")
        // Placed twice, removed twice.
        expect(
            ComposeMarkers.removeMarker(a, from: "\(marker)\na\n\(marker)\nb") == "a\nb",
            "every copy goes")
        // A bare reference typed by hand (no `![`) loses only the reference.
        expect(
            ComposeMarkers.removeMarker(a, from: "see cid:tok-1@passband ok") == "see  ok",
            "a hand-typed reference is cut without eating the line")
    }

    private static func removalLeavesOtherMarkersAlone() {
        let a = att("a.png", cid: "aaa@passband")
        let b = att("b.png", cid: "bbb@passband")
        let body = "\(ComposeMarkers.marker(for: a))\n\(ComposeMarkers.marker(for: b))\n"
        let out = ComposeMarkers.removeMarker(a, from: body)
        expect(out == "\(ComposeMarkers.marker(for: b))\n", "only a's marker went: \(out)")
        expect(!ComposeMarkers.isInline(a, in: out) && ComposeMarkers.isInline(b, in: out), "b stays inline")
    }

    private static func inlineIsTheBodysWord() {
        let a = att("shot.png")
        expect(!ComposeMarkers.isInline(a, in: "plain"), "no reference, not inline")
        expect(ComposeMarkers.isInline(a, in: "x \(ComposeMarkers.marker(for: a)) y"), "marker => inline")
        // Alt text is free to change; the cid is what counts.
        expect(ComposeMarkers.isInline(a, in: "![anything](cid:tok-1@passband)"), "alt is irrelevant")
        // A different token is a different file.
        expect(!ComposeMarkers.isInline(a, in: "![x](cid:tok-2@passband)"), "another cid is not this one")
    }

    private static func imagesAndFilesBucket() {
        expect(att("a.png", "image/png").isImage, "png is a picture")
        expect(att("a.jpg", "image/jpeg").isImage, "jpeg is a picture")
        expect(!att("a.svg", "image/svg+xml").isImage, "svg is a file")
        expect(!att("a.pdf", "application/pdf").isImage, "pdf is a file")
        expect(ComposeMarkers.mime(for: URL(fileURLWithPath: "/x/y.png")) == "image/png", "png by extension")
        expect(ComposeMarkers.mime(for: URL(fileURLWithPath: "/x/y.pdf")) == "application/pdf", "pdf by extension")
        expect(
            ComposeMarkers.mime(for: URL(fileURLWithPath: "/x/noext")) == "application/octet-stream",
            "no extension is a blob")
        expect(
            ComposeMarkers.mime(for: URL(fileURLWithPath: "/x/y.zzzzqq")) == "application/octet-stream",
            "an unknown extension is a blob")
    }

    private static func wireRestoreCarriesTheId() {
        let wire = OutboundAttachment(
            id: 42, filename: "deck.pdf", mime: "application/pdf", size: 9, content_id: "d@passband")
        let restored = ComposeAttachment(wire)
        expect(restored.id == 42, "a restored file is already staged")
        expect(!restored.uploading && !restored.failed, "nothing to wait for")
        expect(restored.contentId == "d@passband", "the token comes back")
        // A fresh drop has no id yet and is uploading.
        expect(att("x.png").uploading, "fresh => uploading")
        var failed = att("x.png")
        failed.failed = true
        expect(!failed.uploading, "failed is not uploading")
    }

    private static func expect(_ ok: Bool, _ what: String) {
        checks += 1
        if !ok {
            failures += 1
            print("FAIL: \(what)")
        }
    }
}
