// THE OTHER KIND OF EMAIL IMAGE: `<img src="cid:…">`, a reference to a part of
// the message itself. A photo pasted into a body arrives as an attachment part
// plus a Content-ID naming it, and the reading frame could reach neither — `cid:`
// is not a scheme WebKit resolves and the CSP would refuse it anyway — so every
// such reference painted a broken-image outline while the same photo tiled a
// second time in the strip below.
//
// The fix is ImageProxy's trick aimed at LOCAL bytes: a reference this message
// can actually answer becomes `passband-cid://local/<hmac>?a=<id>&n=<name>`,
// minted with a per-launch key and served only by ImageSchemeHandler out of the
// authenticated attachment door (a bearer token cannot ride an `<img src>`, so
// the bytes must come through a handler either way).
//
// EVERY OTHER cid REFERENCE HAS ITS WHOLE `<img>` TAG DROPPED — a Content-ID this
// message does not carry, a part whose bytes were never stored, a non-image, one
// past the inline cap. That is the graceful path and it is deliberately the
// common one: mail synced before the daemon recorded Content-IDs resolves
// nothing at all, and a body with a gap in it reads as a message, while a body
// full of broken glyphs reads as a bug.
//
// THE SIGNATURE IS WHAT KEEPS A REFERENCE INSIDE ITS OWN MESSAGE. Only this
// rewrite holds the key, so a body cannot hand-write
// `passband-cid://local/anything?a=91` and read attachment 91 out of somebody
// else's mail.

import CryptoKit
import Foundation

/// The minting and parsing half: one scheme, one per-launch key, and the two
/// directions between an attachment and a url. Separate from the rewrite so the
/// scheme handler can parse without knowing anything about html.
enum CidProxy {
    /// The custom scheme. Registered on every email configuration alongside
    /// `passband-img` and handled by the same responder.
    static let scheme = "passband-cid"

    /// The url an `<img>` gets pointed at. `n` rides along because the byte door
    /// answers with a `Content-Disposition` we may not be able to parse, and the
    /// name is what the staged file is called — see AttachmentFiles.
    ///
    /// The one `&` in it is safe where these are emitted: an HTML parser consumes
    /// a character reference inside an ATTRIBUTE VALUE only when it ends in `;`
    /// or is not followed by `=`, which is exactly the rule that keeps query
    /// strings intact. These urls never land anywhere else — no `<style>` block,
    /// no raw-text element.
    static func url(for attachment: Attachment) -> String {
        let name = attachment.filename.isEmpty ? "attachment" : attachment.filename
        let encoded =
            name.addingPercentEncoding(withAllowedCharacters: ImageProxy.unreserved) ?? ""
        return "\(scheme)://local/\(signature(id: attachment.id, filename: name))?a=\(attachment.id)&n=\(encoded)"
    }

    /// Recover the attachment a minted url names — nil unless it is ours AND
    /// carries this launch's signature over the same two values it hands back.
    /// The handler's only parser.
    static func attachment(from url: URL) -> (id: Int, filename: String)? {
        guard url.scheme?.lowercased() == scheme,
            let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
            let items = components.queryItems,
            let raw = items.first(where: { $0.name == "a" })?.value, let id = Int(raw),
            let filename = items.first(where: { $0.name == "n" })?.value,
            // Plain comparison, not constant-time, for ImageProxy's reason: the
            // only party presenting a signature is static markup in a frame where
            // script cannot run, so there is no loop for a timing oracle.
            components.path == "/" + signature(id: id, filename: filename)
        else { return nil }
        return (id, filename)
    }

    // MARK: - provenance

    /// Signing key for minted urls: 256 random bits, minted once per process and
    /// never written down. Per-launch is the point — a key on disk would let a
    /// message replay a signature it once observed.
    private static let signingKey = SymmetricKey(size: .bits256)

    /// HMAC over BOTH values the parser returns, hex. The id alone would not do:
    /// the filename decides what the staged file is called, and a name is only
    /// ours to trust if it was signed with the id it arrived beside. An id is
    /// digits, so the newline makes the pair unambiguous.
    private static func signature(id: Int, filename: String) -> String {
        var mac = HMAC<SHA256>(key: signingKey)
        mac.update(data: Data("\(id)\n\(filename)".utf8))
        return mac.finalize().hex
    }
}

enum CidImages {
    /// Rewrite every resolvable `cid:` image reference to a minted url and drop
    /// the tags of the rest.
    ///
    /// Runs BETWEEN the dedupe and the image proxy — see EmailWebView.Prepared.
    static func rewrite(html: String, attachments: [Attachment]) -> String {
        run(html, attachments).html
    }

    /// The attachment ids this body renders INLINE, by the same matching the
    /// rewrite does — the strip reads it to avoid tiling a photo the message is
    /// already showing. Same walk, ids kept instead of html, so the two answers
    /// cannot drift.
    static func referencedAttachmentIDs(html: String, attachments: [Attachment]) -> Set<Int> {
        // Nothing to match against, so nothing can resolve. The rewrite still has
        // work in this case (dropping every cid tag); this does not.
        guard !attachments.isEmpty else { return [] }
        return run(html, attachments).ids
    }

    // MARK: - the pass

    /// The one implementation behind both entry points.
    ///
    /// HTMLImg rather than ImageProxy's whole-document regex splice, because the
    /// verdict here is usually DROP: a match anchored on `src=` names the
    /// attribute, not the tag it sits in, and cannot say where the tag ends. The
    /// walker already cut the tag at its `>`, which is also why the patterns
    /// below need no length bound of their own.
    private static func run(_ html: String, _ attachments: [Attachment]) -> (
        html: String, ids: Set<Int>
    ) {
        // Cheap gate on the raw source: most mail carries no `cid:` token at all,
        // and this runs over every body. `passband-cid:` contains it too, so the
        // neutering below is never skipped when it is needed.
        guard html.range(of: "cid:", options: .caseInsensitive) != nil else { return (html, []) }

        // A message can spell our own scheme. `img-src` allows `passband-cid:`
        // UNCONDITIONALLY (attachment bytes are this mail's own, not a remote
        // fetch), so unlike the proxy scheme there is no opt-in gate above the
        // handler here and the signature check is the whole refusal — neutering
        // the token demotes it to a scheme the CSP never dispatches, one layer
        // earlier. MUST run before the rewrite so it cannot touch what we mint.
        let source = html.replacing(schemeToken, with: "\(CidProxy.scheme)-blocked:")

        var ids: Set<Int> = []
        let out = HTMLImg.walk(source) { tag in
            guard let src = srcAttr(tag) else { return .keep }
            guard let wanted = contentID(referencedBy: src.value) else { return .keep }
            guard let hit = match(wanted, attachments) else { return .drop }
            // A quoted value is spliced; an unquoted one is only ever READ. The
            // sanitizer quotes every attribute it emits, so a bare `src=` is
            // either markup we did not produce or a run of text inside somebody
            // else's attribute, and dropping is the answer that cannot mangle it.
            guard src.quoted else { return .drop }
            ids.insert(hit.id)
            var rewritten = tag
            rewritten.replaceSubrange(
                src.range, with: "\(src.open)\(CidProxy.url(for: hit))\(src.close)")
            return .replace(rewritten)
        }
        return (out, ids)
    }

    /// The attachment a reference resolves to, or nil for "drop this tag".
    ///
    /// Exact match first, case-insensitive second: a Content-ID is
    /// case-sensitive by RFC, and mailers that fold its case are common enough
    /// that refusing them would leave the outlines this pass exists to remove.
    /// FIRST OCCURRENCE WINS, and the inline gate is then applied to that one
    /// alone — two parts sharing a Content-ID is malformed mail, and picking a
    /// later part because the first failed the gate would be guessing.
    private static func match(_ wanted: String, _ attachments: [Attachment]) -> Attachment? {
        let hit =
            attachments.first { normalized($0.content_id) == wanted }
            ?? attachments.first {
                let stored = normalized($0.content_id)
                return !stored.isEmpty && stored.lowercased() == wanted.lowercased()
            }
        // The same gate the strip renders an image under: stored bytes, an image
        // this app will rasterize, and small enough to be worth pasting into the
        // column. Nothing else may become a fetch.
        guard let hit, AttachmentKinds.isInline(hit) else { return nil }
        return hit
    }

    /// The Content-ID a `src` names, normalized, or nil when the src is not a
    /// cid reference at all (which is a KEEP, not a drop).
    private static func contentID(referencedBy src: String) -> String? {
        // Entity-decoded through the SAME decoder ImageProxy uses, because this
        // value came out of an html attribute and `&amp;` is a `&` in the id.
        var value = ImageProxy.unescapeEntities(
            src.trimmingCharacters(in: .whitespacesAndNewlines))
        guard value.lowercased().hasPrefix("cid:") else { return nil }
        value = String(value.dropFirst("cid:".count))
        // RFC 2392 makes the rest a URI, so a Content-ID holding a character a
        // url cannot spell arrives percent-encoded. An unencodable `%` is left
        // as written rather than dropping the reference over it.
        value = value.removingPercentEncoding ?? value
        let id = normalized(value)
        return id.isEmpty ? nil : id
    }

    /// The two spellings of one Content-ID reconciled: `<x@y>` as the header
    /// writes it and `x@y` as the reference names it. The daemon strips the
    /// brackets on the way in, so this is what makes a body match a row stored by
    /// a daemon that did not — and what lets a reference keep its own brackets.
    private static func normalized(_ raw: String?) -> String {
        var value = (raw ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        if value.count >= 2, value.hasPrefix("<"), value.hasSuffix(">") {
            value = String(value.dropFirst().dropLast())
                .trimmingCharacters(in: .whitespacesAndNewlines)
        }
        return value
    }

    // MARK: - the src attribute

    /// Where a raw `<img …>` tag's src value sits and what surrounds it.
    private struct SrcAttr {
        let range: Range<String.Index>
        let open: String
        let value: String
        let close: String
        /// False for an unquoted value, which is read but never rewritten.
        let quoted: Bool
    }

    /// `\ssrc` and not `\bsrc`, for ImageProxy's reason: the word boundary also
    /// accepts `data-src`, which the sanitizer strips and nothing would load —
    /// and reading a cid out of one would drop a tag whose real src is fine.
    /// Quoted forms are tried first, so a tag carrying both loses nothing.
    private static func srcAttr(_ tag: String) -> SrcAttr? {
        if let m = tag.firstMatch(of: srcDouble) {
            return SrcAttr(
                range: m.range, open: String(m.1), value: String(m.2), close: String(m.3),
                quoted: true)
        }
        if let m = tag.firstMatch(of: srcSingle) {
            return SrcAttr(
                range: m.range, open: String(m.1), value: String(m.2), close: String(m.3),
                quoted: true)
        }
        if let m = tag.firstMatch(of: srcBare) {
            return SrcAttr(
                range: m.range, open: String(m.1), value: String(m.2), close: "", quoted: false)
        }
        return nil
    }

    nonisolated(unsafe) private static let srcDouble = /(?i)(\ssrc\s*=\s*")([^"]*)(")/
    nonisolated(unsafe) private static let srcSingle = /(?i)(\ssrc\s*=\s*')([^']*)(')/
    nonisolated(unsafe) private static let srcBare = /(?i)(\ssrc\s*=\s*)([^\s>"']+)/
    nonisolated(unsafe) private static let schemeToken = /(?i)passband-cid:/
}
