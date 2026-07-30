// Route every remote image in an email through OUR scheme instead of the
// network stack's.
//
// A rewritten body references `squelch-img://local/<sig>?u=<encoded original>`,
// which only ImageSchemeHandler can answer (registered per
// WKWebViewConfiguration, nowhere else in the system). Three things follow from
// that:
//
//   1. The fetch happens in ImageStore — ephemeral, cookie-free, no referrer,
//      image/* only, capped, and cached to disk under our own eviction policy —
//      instead of inside WebKit where we can only observe it.
//   2. `img-src` drops `http: https:` entirely and becomes `squelch-img: data:`.
//      That CLOSES the gap documented in squelch-core/src/sync/html.rs ("KNOWN
//      TRADE-OFF"): the sanitizer keeps `<style>` blocks, and a
//      `background-image: url(...)` in kept CSS is invisible to the
//      `<img>`-scoped tracker stripper. It used to be a live fetch; now the only
//      thing the CSP will load is a proxy URL, and CSS urls go through the same
//      rewrite as `<img src>`.
//   3. ANY reference this pass misses FAILS CLOSED. An `http://` src the
//      rewriter did not catch is no longer in `img-src` at all, so it is blocked
//      by the CSP rather than quietly fetched. A miss costs a broken-image
//      glyph, never an un-proxied request.
//
// `<sig>` is an HMAC of the target url under a key minted once per launch, and
// the handler refuses any url whose signature does not verify. Not secrecy —
// the target is right there in the markup — PROVENANCE: it makes "this
// reference came out of this rewrite" a checkable claim. Without it a message
// writes `url(squelch-img://local/?u=<tracker>)` into a kept <style> block by
// hand and every gate steps aside: ammonia scheme-filters href/src only,
// `proxied` leaves a non-http(s) reference alone, and the tracker stripper is
// `<img>`-scoped — so the pixel fetches while the UI tells the reader the mail
// has no remote content. Signed, it is simply refused.
//
// SCOPE: http(s) only. `cid:`, `data:` and protocol-relative `//host/x` are left
// exactly as they are — the first two are inert, and the third has never loaded
// here (the document has an opaque `about:` origin, so a scheme-relative URL
// resolves to nothing). Rewriting those would ADD fetches the reader does not
// make today, which is not this pass's job.

import CryptoKit
import Foundation

enum ImageProxy {
    /// The custom scheme. Registered on every email configuration and nowhere
    /// else; handled by ImageSchemeHandler.
    static let scheme = "squelch-img"

    /// How many distinct urls ONE body contributes to the warm list. The warmer
    /// PINS what it fetches, and a pin buys an entry a budget of its own, so an
    /// uncapped list is an uncapped claim on the cache by a single message — and
    /// five hundred `<img>` tags is a shape mail has by accident, never mind on
    /// purpose. Past the cap the references are still rewritten and still
    /// render; they are fetched on demand, as ordinary evictable entries,
    /// instead of at launch.
    static let maxWarmURLs = 64

    /// Rewrite every http(s) image reference to a proxy URL, and report the
    /// originals (de-duped, capped) so the warmer can pre-fetch them.
    ///
    /// Runs LAST in the prepare chain — after tracker stripping and repeat
    /// dedupe — so the passes that decide WHICH images survive still see plain
    /// `<img src="http…">`, and everything they dropped is never proxied.
    static func rewrite(_ html: String) -> (html: String, urls: [String]) {
        var urls: [String] = []
        var seen = Set<String>()

        /// Original reference -> proxy URL, collecting the original. Returns nil
        /// for anything that is not http(s), which leaves the source untouched.
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
            // The signature rides in the PATH rather than a second query item so
            // the emitted url still contains no `&`: that character would need
            // entity-escaping in the two attribute contexts and must NOT be
            // escaped in the raw-text one, and there is no reason to buy that
            // distinction. Hex is unreserved, like the rest of the url.
            return "\(scheme)://local/\(signature(for: candidate))?u=\(encoded)"
        }

        /// The tag-shaped passes. Applied only to the segments BETWEEN raw-text
        /// elements — see the segmentation below.
        func rewriteTags(_ fragment: String) -> String {
            // (a) <img src="http…">. The attribute value is HTML-escaped in the
            // sanitized document, so `&amp;` has to come back out before the URL
            // is percent-encoded — the browser used to do that decode for us,
            // and a CDN url with two query params is the common case, not the
            // exotic one.
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
            // (b) url() inside an inline style="" attribute — attribute text, so
            // entities are decoded here as they are for src.
            out = out.replacing(styleAttrDouble) { m in
                "\(m.1)\(rewriteCSS(String(m.2), decodeEntities: true, proxied))\(m.3)"
            }
            out = out.replacing(styleAttrSingle) { m in
                "\(m.1)\(rewriteCSS(String(m.2), decodeEntities: true, proxied))\(m.3)"
            }
            return out
        }

        // A message can spell our own scheme itself: ammonia scheme-filters
        // href/src and nothing else, so `url(squelch-img://…)` written by hand
        // survives verbatim in a kept <style>. The signature check refuses such
        // a reference on arrival; neutering the token here demotes it further,
        // from "a request the handler refuses" to "an unknown scheme the CSP
        // never dispatches". Runs before the rewrite so it cannot touch what the
        // rewrite goes on to mint.
        let source = html.replacing(schemeToken, with: "\(scheme)-blocked:")

        // (c) url() inside a <style> block. html5ever serializes <style> as a
        // raw-text element, so its CSS is NOT entity-escaped — and, the other
        // way round, its content can hold a literal `<img src="…">` or
        // `style="…"` that no renderer will ever act on. Rewriting THOSE would
        // turn dead text into launch-time fetches, and the long `>`-free runs
        // they arrive in are exactly what makes the tag patterns quadratic. So
        // the tag passes see only what lies between the style blocks, and each
        // block's content is rewritten as the CSS it actually is.
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

    /// Recover the original URL from a proxy URL, or nil if it is not one of
    /// ours / was not minted by this launch's rewrite / does not carry an
    /// http(s) target. The handler's only parser.
    static func original(from url: URL) -> String? {
        guard url.scheme?.lowercased() == scheme,
            let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
            let value = components.queryItems?.first(where: { $0.name == "u" })?.value,
            // A plain comparison rather than a constant-time one: the only party
            // that can present a signature here is static markup in a frame
            // where script cannot run, so there is no loop in which to run the
            // millions of trials a timing oracle needs.
            components.path == "/" + signature(for: value),
            let target = URL(string: value), let targetScheme = target.scheme?.lowercased(),
            targetScheme == "http" || targetScheme == "https"
        else { return nil }
        return value
    }

    // MARK: - provenance

    /// Signing key for proxy urls: 256 random bits, minted once per process and
    /// never written down. Per-launch is the point — a key on disk would let a
    /// message replay a signature it once observed, and a proxy url has no
    /// meaning outside the render that produced it.
    private static let signingKey = SymmetricKey(size: .bits256)

    /// HMAC of the target url, hex. Over the DECODED url, which is the exact
    /// string `original(from:)` hands to ImageStore — so nothing between the two
    /// can change what gets fetched without invalidating the signature.
    private static func signature(for target: String) -> String {
        var mac = HMAC<SHA256>(key: signingKey)
        mac.update(data: Data(target.utf8))
        return mac.finalize().map { String(format: "%02x", $0) }.joined()
    }

    // MARK: - css

    /// Rewrite every http(s) `url(…)` in a CSS fragment, leaving the rest of the
    /// declaration verbatim. The replacement is emitted UNQUOTED: a proxy URL is
    /// percent-encoded down to unreserved characters plus the fixed prefix, so
    /// it contains no quote, paren or space that would need one — and quoting it
    /// would break whichever quote character the surrounding `style=""`
    /// attribute is already using.
    ///
    /// `@import` and `@font-face` are SKIPPED, and that is a security rule, not
    /// a formatting one: neither loads an image, both fall under
    /// `default-src 'none'` (there is no font-src, and style-src is
    /// `'unsafe-inline'` with no host), so both are dead in the frame either
    /// way. Rewriting them would only add a stylesheet and a font url to the
    /// list the launch warmer fetches — turning two references the reader's
    /// machine has never requested into two live hits on someone's server.
    private static func rewriteCSS(
        _ css: String, decodeEntities: Bool, _ proxied: (String, Bool) -> String?
    ) -> String {
        guard css.range(of: "url(", options: .caseInsensitive) != nil else { return css }

        // Spans a browser fetches NOTHING out of. Extracting from one hands the
        // launch warmer a live request for markup the reader's machine would
        // never have made — an invisible beacon, fired for mail never opened.
        var skip: [Range<String.Index>] = []
        if css.contains("/*") { skip += css.matches(of: cssComment).map(\.range) }
        if css.contains("@") { skip += nonImageRules(css) }

        // ONE splice pass over the ORIGINAL fragment rather than three chained
        // `replacing` calls: every range here — the matches and the skip spans —
        // is an index into `css`, and rewriting between passes would invalidate
        // them all. The three quoting patterns DO overlap: a bare url() nested
        // inside a quoted one (`url("a url(http://x) b")`) is matched by both.
        // So position order alone is not enough — every hit considered CLAIMS
        // its whole span, including a hit left verbatim, which makes the
        // outermost match at a position the only one that counts. Without that,
        // a url() that is really just text inside someone else's string value
        // gets extracted and fetched.
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

    // Immutable, built once, matching Trackers' idiom. `\ssrc` (rather than
    // `\bsrc`) is deliberate: it refuses to match `data-src`/`lowsrc`, which the
    // sanitizer strips anyway and which nothing would ever load.
    //
    // Every lazy run INSIDE a tag is bounded. Unbounded, `[^>]*?` is O(k·n) over
    // a document with k `<img` prefixes and long `>`-free runs: 16KB of nothing
    // but `<img ` measured at 2.8s, quadrupling per doubling, which puts 1MB in
    // the hours. The bound is the BACKSTOP, not the fix — the fix is that this
    // pattern no longer sees the only text that can be shaped that way. Ammonia
    // escapes `<` in text content AND in attribute values (verified against
    // ammonia 4), so every `<img` that survives sanitization is a real tag whose
    // `>`-free run is the tag itself; the sole exception is `<style>` raw text,
    // which the segmentation above hands to the CSS pass instead. Same 1MB
    // input, through `rewrite`: 0.5s.
    //
    // 2048 is far past any real tag. A tag longer than that loses its rewrite
    // and therefore its image, which is the fail-closed direction.
    //
    // The `style=` patterns are anchored to a TAG OPENING rather than to
    // whitespace. A bare `style="…"` is ordinary prose in a mail that talks
    // about HTML, the renderer treats it as prose, and so must we.
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
    /// TERMINATED comments only. An unterminated `/*` also kills the rest of a
    /// real stylesheet, but treating it as a skip span would mean one stray `/*`
    /// inside a url — `url(http://h/a/*b)` is legal — silently un-rewriting
    /// every image after it. A miss here costs a phantom fetch; a false skip
    /// costs a visibly broken email, and that is the worse of the two.
    nonisolated(unsafe) private static let cssComment = /(?s)\/\*.*?\*\//
    nonisolated(unsafe) private static let schemeToken = /(?i)squelch-img:/

    /// RFC 3986 unreserved, spelled out in ASCII rather than taken from
    /// `CharacterSet.alphanumerics` — that set includes non-ASCII letters, which
    /// would then ride through unencoded and produce a URL that will not parse.
    private static let unreserved = CharacterSet(
        charactersIn:
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")

    /// The five entities an HTML serializer can put in an attribute value, in
    /// ONE pass so `&amp;lt;` decodes to `&lt;` and not to `<`.
    private static func unescapeEntities(_ text: String) -> String {
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
