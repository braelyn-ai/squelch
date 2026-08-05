// The composer's markdown scanner: one pass over the source producing styled
// spans, consumed by the live editor (MarkdownTextView) and the review
// preview. SCANNING ONLY — rendering to HTML happens in the daemon
// (squelch-api/src/markdown.rs), from this same source; this side's job is to
// make the formatting visible while the markers stay on screen: `**asdf**`
// keeps its stars and reads bold.
//
// Scope is the basic-formatting set the wire renders: bold, italic, inline
// code, fenced code blocks, links, ATX headings, list markers, blockquotes.
// Unmatched markers style nothing — a lone `*` is just an asterisk. The
// scanner is line-then-inline with a single forward pass per line; nothing
// here backtracks, so pathological input costs linear time.

import Foundation

/// What a span of the source IS. The editor maps each kind to attributes; the
/// delimiters themselves arrive as `.marker` OVERLAYS on top of the styled
/// span, so `**` can fade without breaking the bold run it belongs to.
enum MarkdownKind: Equatable {
    case bold
    case italic
    case code
    case codeBlock
    case heading(Int)
    case listMarker
    case blockquoteMarker
    case linkText
    case linkURL
    /// A syntax delimiter (`**`, `` ` ``, `[`…): kept visible, faded.
    case marker
}

struct MarkdownSpan: Equatable {
    /// UTF-16 range, ready for NSAttributedString / NSTextStorage.
    var range: NSRange
    var kind: MarkdownKind
}

enum Markdown {
    /// Scan the whole source. Spans may overlap (a `.marker` sits inside its
    /// `.bold`); emit order is style-then-marker so a consumer applying them
    /// in order lands the fade on top.
    static func spans(of text: String) -> [MarkdownSpan] {
        var spans: [MarkdownSpan] = []
        let ns = text as NSString
        var inFence = false

        var lineStart = 0
        while lineStart <= ns.length {
            let lineRange = lineStart < ns.length
                ? ns.lineRange(for: NSRange(location: lineStart, length: 0))
                : NSRange(location: lineStart, length: 0)
            if lineRange.length == 0 { break }
            let line = ns.substring(with: lineRange)
            let content = line.hasSuffix("\n") ? String(line.dropLast()) : line
            scanLine(
                content, at: lineRange.location, inFence: &inFence, into: &spans)
            lineStart = lineRange.location + lineRange.length
        }
        return spans
    }

    // MARK: - lines

    private static func scanLine(
        _ line: String, at offset: Int, inFence: inout Bool,
        into spans: inout [MarkdownSpan]
    ) {
        let ns = line as NSString
        let trimmed = line.trimmingCharacters(in: .whitespaces)

        // Fence lines toggle and are themselves markers; fenced content is one
        // undifferentiated code span — no inline scanning inside.
        if trimmed.hasPrefix("```") {
            spans.append(.init(range: NSRange(location: offset, length: ns.length), kind: .marker))
            inFence.toggle()
            return
        }
        if inFence {
            spans.append(
                .init(range: NSRange(location: offset, length: ns.length), kind: .codeBlock))
            return
        }

        var inlineFrom = 0

        // ATX heading: #{1,6} + space. The whole line carries the heading
        // style; the hashes fade as markers.
        let hashes = line.prefix(while: { $0 == "#" }).count
        if hashes > 0, hashes <= 6, line.dropFirst(hashes).first == " " {
            spans.append(
                .init(range: NSRange(location: offset, length: ns.length), kind: .heading(hashes)))
            spans.append(.init(range: NSRange(location: offset, length: hashes), kind: .marker))
            inlineFrom = hashes + 1
        } else if let marker = listMarkerLength(line) {
            spans.append(.init(range: NSRange(location: offset, length: marker), kind: .listMarker))
            inlineFrom = marker
        } else if trimmed.hasPrefix(">") {
            let idx = (line as NSString).range(of: ">").location
            spans.append(
                .init(range: NSRange(location: offset + idx, length: 1), kind: .blockquoteMarker))
            inlineFrom = idx + 1
        }

        scanInline(ns, from: inlineFrom, at: offset, into: &spans)
    }

    /// Leading `- ` / `* ` / `+ ` / `12. ` (after indentation): length of the
    /// marker INCLUDING its trailing space and indentation, or nil.
    private static func listMarkerLength(_ line: String) -> Int? {
        let chars = Array(line)
        var i = 0
        while i < chars.count, chars[i] == " " { i += 1 }
        guard i < chars.count else { return nil }
        if "-*+".contains(chars[i]), i + 1 < chars.count, chars[i + 1] == " " {
            return i + 2
        }
        var j = i
        while j < chars.count, chars[j].isNumber { j += 1 }
        if j > i, j + 1 < chars.count, chars[j] == ".", chars[j + 1] == " " {
            return j + 2
        }
        return nil
    }

    // MARK: - inline

    /// The delimiter code units, as UTF-16 (NSString's alphabet).
    private enum Ch {
        static let backtick: UInt16 = 0x60  // `
        static let star: UInt16 = 0x2A  // *
        static let underscore: UInt16 = 0x5F  // _
        static let lbracket: UInt16 = 0x5B  // [
        static let rbracket: UInt16 = 0x5D  // ]
        static let lparen: UInt16 = 0x28  // (
        static let rparen: UInt16 = 0x29  // )
        static let space: UInt16 = 0x20
        static let tab: UInt16 = 0x09
    }

    /// One forward pass: code spans win (markers inside them are literal),
    /// then bold/italic delimiters, then links.
    private static func scanInline(
        _ ns: NSString, from start: Int, at offset: Int, into spans: inout [MarkdownSpan]
    ) {
        var i = start
        let n = ns.length
        while i < n {
            let c = ns.character(at: i)
            switch c {
            case Ch.backtick:
                // Inline code: to the next backtick on this line, or literal.
                if let close = find(ns, Ch.backtick, from: i + 1) {
                    let range = NSRange(location: offset + i, length: close - i + 1)
                    spans.append(.init(range: range, kind: .code))
                    spans.append(.init(range: NSRange(location: offset + i, length: 1), kind: .marker))
                    spans.append(.init(range: NSRange(location: offset + close, length: 1), kind: .marker))
                    i = close + 1
                } else {
                    i += 1
                }
            case Ch.star, Ch.underscore:
                let double = i + 1 < n && ns.character(at: i + 1) == c
                let markerLen = double ? 2 : 1
                if let close = findDelimiter(ns, c, count: markerLen, from: i + markerLen),
                    close > i + markerLen,  // never an EMPTY emphasis (`****`)
                    // Underscores only at word boundaries: CommonMark (the wire's
                    // renderer) refuses intraword `_`, so snake_case must not
                    // read italic here and then go out literal.
                    c != Ch.underscore
                        || ((i == 0 || !isWord(ns.character(at: i - 1)))
                            && (close + markerLen >= n || !isWord(ns.character(at: close + markerLen))))
                {
                    let full = NSRange(location: offset + i, length: close + markerLen - i)
                    spans.append(.init(range: full, kind: double ? .bold : .italic))
                    spans.append(
                        .init(range: NSRange(location: offset + i, length: markerLen), kind: .marker))
                    spans.append(
                        .init(range: NSRange(location: offset + close, length: markerLen), kind: .marker))
                    // Recurse INSIDE for nesting (`**bold *and italic***` at
                    // least gets its outer run), then continue past it.
                    i = close + markerLen
                } else {
                    i += markerLen
                }
            case Ch.lbracket:
                // [text](url) — all four delimiters present on one line or the
                // bracket is literal.
                if let closeBracket = find(ns, Ch.rbracket, from: i + 1),
                    closeBracket + 1 < n,
                    ns.character(at: closeBracket + 1) == Ch.lparen,
                    let closeParen = find(ns, Ch.rparen, from: closeBracket + 2)
                {
                    spans.append(
                        .init(
                            range: NSRange(location: offset + i + 1, length: closeBracket - i - 1),
                            kind: .linkText))
                    spans.append(
                        .init(
                            range: NSRange(
                                location: offset + closeBracket + 2,
                                length: closeParen - closeBracket - 2),
                            kind: .linkURL))
                    for at in [i, closeBracket, closeBracket + 1, closeParen] {
                        spans.append(
                            .init(range: NSRange(location: offset + at, length: 1), kind: .marker))
                    }
                    i = closeParen + 1
                } else {
                    i += 1
                }
            default:
                i += 1
            }
        }
    }

    /// Letter or digit, for the underscore word-boundary rule. UTF-16 unit —
    /// good enough here: every boundary character that matters is BMP.
    private static func isWord(_ u: UInt16) -> Bool {
        guard let scalar = Unicode.Scalar(u) else { return false }
        return CharacterSet.alphanumerics.contains(scalar)
    }

    private static func find(_ ns: NSString, _ char: UInt16, from: Int) -> Int? {
        var i = from
        while i < ns.length {
            if ns.character(at: i) == char { return i }
            i += 1
        }
        return nil
    }

    /// The next run of exactly-or-more `count` of `char`, returned only when
    /// the character BEFORE it isn't whitespace — `a ** b` is not emphasis.
    private static func findDelimiter(
        _ ns: NSString, _ char: UInt16, count: Int, from: Int
    ) -> Int? {
        var i = from
        while i + count <= ns.length {
            if (0..<count).allSatisfy({ ns.character(at: i + $0) == char }) {
                let before = ns.character(at: i - 1)
                if before != Ch.space && before != Ch.tab {
                    return i
                }
                i += count
            } else {
                i += 1
            }
        }
        return nil
    }
}
