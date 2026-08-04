// Route every http(s) image reference through our own scheme —
// `passband-img://local/<hmac>?u=<encoded>`, answered only by ImageSchemeHandler
// — so `img-src` is `passband-img: data:` alone and a reference this pass misses
// FAILS CLOSED (a broken-image glyph, never an un-proxied request). The
// per-launch HMAC makes "minted by this rewrite" a checkable claim. cid:, data:
// and protocol-relative are left alone. See docs/SECURITY.md §3.

import CryptoKit
import Foundation

enum ImageProxy {
    /// The custom scheme. Registered on every email configuration and nowhere
    /// else; handled by ImageSchemeHandler.
    static let scheme = "passband-img"

    /// How many distinct urls ONE body contributes to the warm list. The warmer
    /// PINS what it fetches, so an uncapped list is an uncapped claim on the
    /// cache by a single message. Past the cap references still rewrite and
    /// render — they are fetched on demand as ordinary evictable entries.
    static let maxWarmURLs = 64

    /// Rewrite every http(s) image reference to a proxy URL, and report the
    /// originals (de-duped, capped) so the warmer can pre-fetch them.
    ///
    /// Runs LAST in the prepare chain, after tracker stripping and repeat
    /// dedupe: the passes deciding WHICH images survive must still see plain
    /// `<img src="http…">`, and what they dropped is never proxied.
    static func rewrite(_ html: String) -> (html: String, urls: [String]) {
        var urls: [String] = []
        var seen = Set<String>()

        /// Original reference -> proxy URL, collecting the original. nil for
        /// anything not http(s), which leaves the source untouched.
        func proxied(_ raw: String, decodeEntities: Bool) -> String? {
            var candidate = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            if decodeEntities { candidate = unescapeEntities(candidate) }
            let lower = candidate.lowercased()
            guard lower.hasPrefix("http://") || lower.hasPrefix("https://") else { return nil }
            guard
                let encoded = candidate.addingPercentEncoding(
                    withAllowedCharacters: Self.unreserved)
            else { return nil }
            if seen.insert(candidate).inserted, urls.count < maxWarmURLs { urls.append(candidate) }
            // The signature rides in the PATH, not a second query item, so the
            // emitted url contains no `&` — that character needs entity-escaping
            // in the two attribute contexts and must NOT be escaped in the
            // raw-text one.
            return "\(scheme)://local/\(signature(for: candidate))?u=\(encoded)"
        }

        /// The tag-shaped passes. Applied only to the segments BETWEEN raw-text
        /// elements — see the segmentation below.
        func rewriteTags(_ fragment: String) -> String {
            // (a) <img src="http…">. The attribute value is HTML-escaped in the
            // sanitized document, so `&amp;` must come back out before the URL
            // is percent-encoded.
            var out = fragment.replacing(imgSrcDouble) { m in
                guard let url = proxied(String(m.2), decodeEntities: true) else {
                    return String(m.0)
                }
                return "\(m.1)\(url)\(m.3)"
            }
            out = out.replacing(imgSrcSingle) { m in
                guard let url = proxied(String(m.2), decodeEntities: true) else {
                    return String(m.0)
                }
                return "\(m.1)\(url)\(m.3)"
            }
            // (b) url() inside an inline style="" — attribute text, so entities
            // decode here as they do for src.
            out = out.replacing(styleAttrDouble) { m in
                "\(m.1)\(rewriteCSS(String(m.2), decodeEntities: true, proxied))\(m.3)"
            }
            out = out.replacing(styleAttrSingle) { m in
                "\(m.1)\(rewriteCSS(String(m.2), decodeEntities: true, proxied))\(m.3)"
            }
            return out
        }

        // A message can spell our own scheme: ammonia scheme-filters href/src
        // only, so a hand-written `url(passband-img://…)` survives in a kept
        // <style>. The signature check already refuses it; neutering the token
        // demotes it to a scheme the CSP never dispatches. MUST run before the
        // rewrite so it cannot touch what the rewrite mints.
        let source = html.replacing(schemeToken, with: "\(scheme)-blocked:")

        // (c) url() inside a <style> block. <style> is a raw-text element: its
        // CSS is NOT entity-escaped, and it can hold a literal `<img src="…">`
        // no renderer acts on — rewriting that would turn dead text into
        // launch-time fetches, and its long `>`-free runs are what make the tag
        // patterns quadratic. So the tag passes see only what lies BETWEEN style
        // blocks, and each block is rewritten as the CSS it is.
        var out = ""
        var cursor = source.startIndex
        for m in source.matches(of: styleBlock) {
            out += rewriteTags(String(source[cursor..<m.range.lowerBound]))
            out += "\(m.1)\(rewriteCSS(String(m.2), decodeEntities: false, proxied))\(m.3)"
            cursor = m.range.upperBound
        }
        out += rewriteTags(String(source[cursor...]))

        return (out, urls)
    }

    /// Recover the original URL from a proxy URL — nil unless it is ours, was
    /// minted by THIS launch's rewrite, and carries an http(s) target. The
    /// handler's only parser.
    static func original(from url: URL) -> String? {
        guard url.scheme?.lowercased() == scheme,
            let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
            let value = components.queryItems?.first(where: { $0.name == "u" })?.value,
            // Plain comparison, not constant-time: the only party presenting a
            // signature is static markup in a frame where script cannot run, so
            // there is no loop for a timing oracle's millions of trials.
            components.path == "/" + signature(for: value),
            let target = URL(string: value), let targetScheme = target.scheme?.lowercased(),
            targetScheme == "http" || targetScheme == "https"
        else { return nil }
        return value
    }

    // MARK: - provenance

    /// Signing key for proxy urls: 256 random bits, minted once per process and
    /// never written down. Per-launch is the point — a key on disk would let a
    /// message replay a signature it once observed.
    private static let signingKey = SymmetricKey(size: .bits256)

    /// HMAC of the target url, hex. Over the DECODED url — the exact string
    /// `original(from:)` hands to ImageStore, so nothing in between can change
    /// what gets fetched without invalidating the signature.
    private static func signature(for target: String) -> String {
        var mac = HMAC<SHA256>(key: signingKey)
        mac.update(data: Data(target.utf8))
        return mac.finalize().hex
    }

    // MARK: - css

    /// Rewrite every http(s) `url(…)` in a CSS fragment, leaving the rest
    /// verbatim. Emitted UNQUOTED: a proxy URL is percent-encoded to unreserved
    /// characters, and a quote would clash with the surrounding `style=""`.
    ///
    /// `@import` and `@font-face` are SKIPPED as a security rule: both are dead
    /// under `default-src 'none'`, so rewriting them would only hand the launch
    /// warmer live requests the reader never made.
    private static func rewriteCSS(
        _ css: String, decodeEntities: Bool, _ proxied: (String, Bool) -> String?
    ) -> String {
        guard css.range(of: "url(", options: .caseInsensitive) != nil else { return css }

        // Spans a browser fetches NOTHING out of. Extracting from one would fire
        // an invisible beacon for mail the reader never opened.
        var skip: [Range<String.Index>] = []
        if css.contains("/*") { skip += css.matches(of: cssComment).map(\.range) }
        if css.contains("@") { skip += nonImageRules(css) }

        // ONE splice pass over the ORIGINAL fragment: every range here indexes
        // into `css`, so rewriting between passes would invalidate them all.
        // The three quoting patterns overlap (`url("a url(http://x) b")` matches
        // both), so every hit considered CLAIMS its whole span — including one
        // left verbatim — making the outermost match at a position the only one
        // that counts. Without that, a url() that is really text inside someone
        // else's string value gets extracted and fetched.
        var hits: [(range: Range<String.Index>, url: String)] =
            css.matches(of: cssURLDouble).map { ($0.range, String($0.1)) }
            + css.matches(of: cssURLSingle).map { ($0.range, String($0.1)) }
            + css.matches(of: cssURLBare).map { ($0.range, String($0.1)) }
        guard !hits.isEmpty else { return css }
        hits.sort {
            $0.range.lowerBound == $1.range.lowerBound
                ? $0.range.upperBound > $1.range.upperBound
                : $0.range.lowerBound < $1.range.lowerBound
        }

        var out = ""
        var cursor = css.startIndex  // everything before this has been emitted
        var claimed = css.startIndex  // no hit starting before this is considered
        for hit in hits where hit.range.lowerBound >= claimed {
            claimed = hit.range.upperBound
            guard !skip.contains(where: { $0.contains(hit.range.lowerBound) }),
                let url = proxied(hit.url, decodeEntities)
            else { continue }  // left verbatim; the cursor has not moved past it
            out += css[cursor..<hit.range.lowerBound]
            out += "url(\(url))"
            cursor = hit.range.upperBound
        }
        out += css[cursor...]
        return out
    }

    /// Spans of CSS whose `url()`s are not images: `@import …;` statements and
    /// `@font-face { … }` blocks.
    private static func nonImageRules(_ css: String) -> [Range<String.Index>] {
        css.matches(of: atImport).map(\.range) + css.matches(of: atFontFace).map(\.range)
    }

    // MARK: - patterns

    // `\ssrc` rather than `\bsrc` refuses `data-src`/`lowsrc`, which the
    // sanitizer strips and nothing would load. The `style=` patterns anchor to a
    // TAG OPENING, not whitespace: a bare `style="…"` is prose in a mail about
    // HTML, and the renderer treats it as prose.
    //
    // Every lazy run inside a tag is bounded, because unbounded `[^>]*?` is
    // O(k·n) over long `>`-free runs (16KB of `<img ` measured 2.8s). 2048 is
    // far past any real tag, and a longer tag loses its rewrite and its image —
    // the fail-closed direction. The bound is only a backstop: ammonia escapes
    // `<` in text AND attribute values, so every surviving `<img` is a real tag,
    // and the sole raw-text exception (`<style>`) goes to the CSS pass instead.
    nonisolated(unsafe) private static let imgSrcDouble =
        /(?i)(<img\b[^>]{0,2048}?\ssrc\s*=\s*")([^"]*)(")/
    nonisolated(unsafe) private static let imgSrcSingle =
        /(?i)(<img\b[^>]{0,2048}?\ssrc\s*=\s*')([^']*)(')/
    nonisolated(unsafe) private static let styleBlock =
        /(?is)(<style\b[^>]{0,2048}>)(.*?)(<\/style>)/
    nonisolated(unsafe) private static let styleAttrDouble =
        /(?i)(<[a-z][^>]{0,2048}?\sstyle\s*=\s*")([^"]*)(")/
    nonisolated(unsafe) private static let styleAttrSingle =
        /(?i)(<[a-z][^>]{0,2048}?\sstyle\s*=\s*')([^']*)(')/
    nonisolated(unsafe) private static let cssURLDouble = /(?i)url\(\s*"([^"]*)"\s*\)/
    nonisolated(unsafe) private static let cssURLSingle = /(?i)url\(\s*'([^']*)'\s*\)/
    nonisolated(unsafe) private static let cssURLBare = /(?i)url\(\s*([^'"()\s]+)\s*\)/
    nonisolated(unsafe) private static let atImport = /(?is)@import[^;}]*[;}]?/
    nonisolated(unsafe) private static let atFontFace = /(?is)@font-face\s*\{[^}]*\}/
    /// TERMINATED comments only: one stray `/*` inside a legal url
    /// (`url(http://h/a/*b)`) would otherwise silently un-rewrite every image
    /// after it. A miss costs a phantom fetch, a false skip a broken email.
    nonisolated(unsafe) private static let cssComment = /(?s)\/\*.*?\*\//
    nonisolated(unsafe) private static let schemeToken = /(?i)passband-img:/

    /// RFC 3986 unreserved, spelled out rather than taken from
    /// `CharacterSet.alphanumerics` — that set admits non-ASCII letters, which
    /// would ride through unencoded and produce a URL that will not parse.
    private static let unreserved = CharacterSet(
        charactersIn:
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")

    /// The entities an HTML serializer can put in an attribute value, decoded in
    /// ONE pass so `&amp;lt;` yields `&lt;` and not `<`. Internal because this
    /// defines what url a reference FETCHES: any pass identifying a reference
    /// must agree with it (ImageRepeats keys its dedupe here).
    static func unescapeEntities(_ text: String) -> String {
        guard text.contains("&") else { return text }
        var out = ""
        var rest = Substring(text)
        while let amp = rest.firstIndex(of: "&") {
            out += rest[rest.startIndex..<amp]
            let tail = rest[amp...]
            var matched = false
            for (entity, replacement) in entities where tail.hasPrefix(entity) {
                out += replacement
                rest = tail.dropFirst(entity.count)
                matched = true
                break
            }
            if !matched {
                out.append("&")
                rest = tail.dropFirst()
            }
        }
        out += rest
        return out
    }

    private static let entities: [(String, String)] = [
        ("&amp;", "&"), ("&#38;", "&"), ("&lt;", "<"), ("&gt;", ">"),
        ("&quot;", "\""), ("&#34;", "\""), ("&apos;", "'"), ("&#39;", "'"),
    ]
}

/// Lowercase hex — the one spelling this app writes a digest in, shared by the
/// signature above and ImageStore's cache filenames. Both a CryptoKit digest and
/// a MAC are just byte sequences, so this reaches them without unwrapping.
extension Sequence<UInt8> {
    var hex: String { map { String(format: "%02x", $0) }.joined() }
}
