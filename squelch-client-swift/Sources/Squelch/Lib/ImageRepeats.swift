// Repeated-image suppression, scoped to ONE thread.
//
// WHY THIS EXISTS: a design thread carried the same three googleusercontent
// signature logos in every message — and again inside each message's quoted
// history, because a reply embeds the message above it — so reading the thread
// meant scrolling past five copies of one logo. Showing it once is the feature.
//
// THIS IS A DE-DUPLICATION PASS AND NOTHING ELSE. It runs AFTER Trackers.strip
// and changes no security posture: tracker stripping, the CSP, the img-src gate
// and the remote-images pref all still decide, unchanged, what is allowed to
// load. Dropping a tag can only ever remove a request, never add one — this
// pass is incapable of widening what the reader fetches.
//
// The rules are deliberately narrow, because a heuristic that hides real
// content is far worse than a logo shown twice:
//   * EXACT src equality. No normalization, no fuzzy "signature-shaped"
//     guessing, no matching on dimensions or position.
//   * The FIRST occurrence ALWAYS survives. An image that appears once is
//     content, so every distinct src still gets exactly one chance to render —
//     which is also why a src that fails to load cannot take its siblings down
//     with it: the copy we kept is the one that tried, and we never suppressed
//     anything that would have rendered in its place.
//   * NETWORK srcs only. A `data:` payload makes a terrible dictionary key, and
//     inline `cid:` art is message-specific rather than the repeated chrome
//     this exists to remove.
//   * Thread-scoped. The caller owns the accumulated set and drops it with the
//     thread; nothing is remembered between threads.

enum ImageRepeats {
    /// Only network images are deduped — see the header.
    static func isDedupable(_ src: String) -> Bool {
        let s = src.lowercased()
        return s.hasPrefix("http://") || s.hasPrefix("https://") || s.hasPrefix("//")
    }

    /// The key an image is deduped by. Trimmed (leading/trailing space in an
    /// attribute is meaningless) and entity-DECODED, through the same decoder
    /// ImageProxy applies before it proxies a src: the attribute value is
    /// HTML-escaped in the sanitized document, so `?a=1&amp;b=2` and `?a=1&b=2`
    /// name ONE image and one fetch, and keying on the raw text would show it
    /// twice. Otherwise verbatim: URL paths are case-sensitive, so folding case
    /// here would merge two distinct images.
    ///
    /// Key material only — the html this pass emits is still byte-for-byte the
    /// html it was handed, minus whole `<img>` tags.
    static func key(_ src: String) -> String {
        ImageProxy.unescapeEntities(src.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    /// Every dedupable `<img src>` in document order.
    static func sources(_ html: String) -> [String] {
        var found: [String] = []
        _ = rewrite(html) { tag in
            if let src = Trackers.attrValue(tag, "src") {
                let k = key(src)
                if isDedupable(k) { found.append(k) }
            }
            return true
        }
        return found
    }

    /// Drop every `<img>` whose src already appeared — either in an earlier
    /// message (`alreadySeen`) or earlier in this same html, which is how a
    /// reply's quoted history re-embeds the signatures stacked above it.
    static func dropRepeats(_ html: String, alreadySeen: Set<String>) -> String {
        var seen = alreadySeen
        return rewrite(html) { tag in
            guard let src = Trackers.attrValue(tag, "src") else { return true }
            let k = key(src)
            guard isDedupable(k) else { return true }
            // `inserted == false` means we have already shown this exact image.
            return seen.insert(k).inserted
        }
    }

    /// Walk `<img>` tags in document order, copying every other byte verbatim.
    /// `keep` returns false to drop the tag.
    ///
    /// Hand-walked rather than parsed, matching Trackers.strip: a DOM
    /// round-trip would reserialize markup the server-side sanitizer already
    /// vetted, and re-vetting it here would mean two sanitizers to keep in step.
    private static func rewrite(_ html: String, keep: (String) -> Bool) -> String {
        var out = ""
        var rest = Substring(html)

        while let open = rest.range(of: "<img", options: [.caseInsensitive]) {
            // Only a tag start if the next character ends the token — otherwise
            // `<images>` would be mistaken for one.
            let afterIdx = open.upperBound
            let isTagStart =
                afterIdx == rest.endIndex
                || rest[afterIdx].isWhitespace || rest[afterIdx] == ">" || rest[afterIdx] == "/"
            guard isTagStart else {
                out += rest[rest.startIndex..<afterIdx]
                rest = rest[afterIdx...]
                continue
            }
            out += rest[rest.startIndex..<open.lowerBound]
            // A sanitized attribute never holds a raw ">".
            guard let close = rest[afterIdx...].firstIndex(of: ">") else {
                out += rest[open.lowerBound...]
                return out
            }
            let tag = String(rest[open.lowerBound...close])
            if keep(tag) { out += tag }
            rest = rest[rest.index(after: close)...]
        }
        out += rest
        return out
    }
}
