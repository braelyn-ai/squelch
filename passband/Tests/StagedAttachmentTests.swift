// WHERE ATTACKER BYTES LAND, AND WHAT DRAWS THEM. Everything a stranger controls
// about a staged file goes through `StagedAttachment.safeName`: the mail
// filename arrives from the sender, through a `Content-Disposition` header the
// daemon sanitizes for separators and nothing else, and comes out the other side
// as a real path on this machine AND as the string Quick Look reads its renderer
// choice from.
//
// Two properties are asserted here rather than reasoned about. The write stays
// inside the directory it was given no matter what the name is. And an extension
// whose renderer is a browser engine never survives — that one is the whole
// point, because `isScriptable` is written against the MIME and Quick Look has
// never once looked at a mime.

import Foundation

@main
@MainActor
struct StagedAttachmentTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        namesStayInsideTheDirectory()
        browserExtensionsAreDefused()
        ordinaryNamesAreLeftAlone()
        stagingRoundTrips()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - the path

    private static func namesStayInsideTheDirectory() {
        // A traversal is a NAME here, never a path. The daemon strips `/` and
        // `\` today; this holds whether or not it keeps doing so.
        for name in ["../../evil", "/etc/passwd", "../../../tmp/evil.jpg", "a/b/c.png"] {
            let safe = StagedAttachment.safeName(name)
            equal(safe.contains("/"), false, "\(name) keeps no separator")
            equal(safe.contains(".."), false, "\(name) is not a traversal")
        }
        // The names that are nothing but a path get one of our own.
        for name in ["", ".", "..", "/", "///"] {
            equal(StagedAttachment.safeName(name), "attachment", "\(name.debugDescription) → fallback")
        }
    }

    // MARK: - the renderer

    private static func browserExtensionsAreDefused() {
        // Each of these is drawn by WebKit. The mime the card was gated on says
        // `application/octet-stream` — that is what the daemon serves anything
        // that is not a photo or a PDF as — so the NAME is the only thing left
        // standing between a stranger's script and a browser engine.
        for name in [
            "invoice.html", "invoice.HTML", "note.htm", "page.xhtml", "page.xht",
            "logo.svg", "logo.SVGZ", "feed.xml", "sheet.xsl", "mail.webarchive",
            "mail.mht", "mail.mhtml", "index.shtml", "index.phtml",
        ] {
            let safe = StagedAttachment.safeName(name)
            equal(safe.hasSuffix(".txt"), true, "\(name) is defused")
            equal(
                AttachmentKinds.hasScriptableExtension(safe), false,
                "\(name) does not stay scriptable after defusing")
        }
        // A name that is NOTHING but an extension. Foundation calls this a
        // hidden file with no extension; everything that opens it disagrees.
        equal(StagedAttachment.safeName(".html"), ".html.txt", "a bare extension counts")
        // And the same trick wearing a directory, since the path is dropped
        // before the extension is read and not after.
        equal(StagedAttachment.safeName("../a/b/x.svg"), "x.svg.txt", "a path does not hide it")
    }

    private static func ordinaryNamesAreLeftAlone() {
        // The near-misses. `.docx` is the one that matters — its MIME carries
        // "openxmlformats", and this is the extension half of the same trap.
        for name in [
            "IMG_3480.jpeg", "invoice.pdf", "report.docx", "sheet.xlsx", "notes.txt",
            "archive.zip", "clip.mov", "README", "server.xmlrpc", "a.htmlx", "photo.svgx",
        ] {
            equal(StagedAttachment.safeName(name), name, "\(name) is left alone")
        }
    }

    // MARK: - the write

    private static func stagingRoundTrips() {
        let bytes = Data("hello".utf8)
        guard let file = try? StagedAttachment.stage(id: 7, bytes: bytes, filename: "../x.html")
        else {
            failures += 1
            checks += 1
            print("FAIL: staging threw")
            return
        }
        defer { file.cleanUp() }
        equal(file.id, 7, "the id round-trips")
        equal(file.url.lastPathComponent, "x.html.txt", "the written name is the defused one")
        equal(
            file.url.deletingLastPathComponent().path, file.directory.path,
            "the write landed in the directory it was given")
        equal(try? Data(contentsOf: file.url), bytes, "the bytes are the bytes")

        file.cleanUp()
        equal(
            FileManager.default.fileExists(atPath: file.directory.path), false,
            "cleanUp takes the directory with it")
    }

    // MARK: - helpers

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
