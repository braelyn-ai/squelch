// The one `<img>` walker every image pass runs on.
//
// Hand-walked rather than parsed, deliberately: these passes must never
// reserialize the document, because a DOM round-trip would rewrite markup the
// server-side sanitizer already vetted, and re-vetting here would mean two
// sanitizers to keep in step. So the walk is a splice — every byte outside a
// tag the visitor acts on is copied verbatim.

enum HTMLImg {
    /// What a visitor wants done with the `<img …>` tag it was handed.
    enum Action {
        /// Emit the tag unchanged.
        case keep
        /// Omit the tag.
        case drop
        /// Emit this in place of the tag.
        case replace(String)
        /// Emit the tag and every byte after it verbatim, ending the walk.
        case stop
    }

    /// Visit `<img>` tags in document order, splicing in what the visitor asks
    /// for and copying every other byte verbatim.
    static func walk(_ html: String, _ visit: (String) -> Action) -> String {
        var out = ""
        var rest = Substring(html)

        while let open = rest.range(of: "<img", options: [.caseInsensitive]) {
            // Only a tag start if the next character ends the token — otherwise
            // a longer name that merely begins with it (`<imgfoo …>`) would be
            // mistaken for one.
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
                // Unterminated tag: emit it verbatim and stop — never rewrite
                // markup whose extent we could not determine.
                out += rest[open.lowerBound...]
                return out
            }
            let tag = String(rest[open.lowerBound...close])
            rest = rest[rest.index(after: close)...]
            switch visit(tag) {
            case .keep: out += tag
            case .drop: break
            case .replace(let replacement): out += replacement
            case .stop:
                out += tag
                out += rest
                return out
            }
        }
        out += rest
        return out
    }
}
