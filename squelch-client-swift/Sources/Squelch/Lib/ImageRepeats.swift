// Repeated-image suppression, scoped to ONE thread: a signature logo repeated
// by every message (and by each reply's quoted history) shows once.
//
// De-duplication only — it runs after Trackers.strip and changes no security
// posture, since dropping a tag can only remove a request, never add one. The
// rules are narrow: exact src equality, network srcs only, and the FIRST
// occurrence always survives, so a src that fails to load takes nothing with it.

enum ImageRepeats {
    /// Only network srcs are deduped: `data:` is a poor key and `cid:` art is
    /// message-specific rather than the repeated chrome this removes.
    static func isDedupable(_ src: String) -> Bool {
        let s = src.lowercased()
        return s.hasPrefix("http://") || s.hasPrefix("https://") || s.hasPrefix("//")
    }

    /// The dedupe key: trimmed and entity-decoded through the SAME decoder
    /// ImageProxy applies, so `?a=1&amp;b=2` and `?a=1&b=2` name one image and
    /// one fetch. Otherwise verbatim — URL paths are case-sensitive. Key
    /// material only; the emitted html is untouched apart from dropped tags.
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

    /// Drop every `<img>` whose src already appeared, in an earlier message
    /// (`alreadySeen`) or earlier in this same html.
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

    /// Walk `<img>` tags in document order, copying every other byte verbatim;
    /// `keep` returns false to drop one. Hand-walked rather than parsed: a DOM
    /// round-trip would reserialize markup the server-side sanitizer vetted,
    /// and re-vetting here would mean two sanitizers to keep in step.
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
