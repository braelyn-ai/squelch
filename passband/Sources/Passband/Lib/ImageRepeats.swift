// Repeated-image suppression inside ONE message: quoted history can repeat the
// same signature art several times, and only its first occurrence is useful.
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

    /// Drop every `<img>` whose src appeared earlier in this same html.
    static func dropRepeats(_ html: String) -> String {
        var seen = Set<String>()
        return HTMLImg.walk(html) { tag in
            guard let src = Trackers.attrValue(tag, "src") else { return .keep }
            let k = key(src)
            guard isDedupable(k) else { return .keep }
            // `inserted == false` means we have already shown this exact image.
            return seen.insert(k).inserted ? .keep : .drop
        }
    }
}
