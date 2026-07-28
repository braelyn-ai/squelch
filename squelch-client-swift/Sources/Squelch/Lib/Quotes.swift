// Quoted reply-history detection, for auto-collapsing the tail of a message.
//
// Server-side sanitization (ammonia, defaults) strips class/id attributes, so
// the client never sees Gmail's `.gmail_quote` or Outlook's `#divRplyFwdMsg`
// markers. Detection therefore keys off what DOES survive: <blockquote>
// structure in html bodies, and ">"-prefixed lines / "On … wrote:" attribution
// lines in plain text.
//
// The bias is deliberately conservative: a wrongly-collapsed reply hides real
// content behind a click, so anything ambiguous (inline quotes with substantial
// text after them, messages that are ALL quote) is left fully expanded.
//
// Ported from squelch-desktop/src/lib/quotes.ts. The HTML side (findQuoteNodes)
// runs inside the WKWebView as injected JS — see EmailWebView's collapse script,
// which mirrors this heuristic exactly.

import Foundation

enum Quotes {
    /// Attribution lines that introduce quoted history in plain text and html.
    private static func isAttribution(_ line: String) -> Bool {
        let t = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty else { return false }
        if t.wholeMatch(of: /On .{1,200} wrote:\s*/) != nil { return true }
        if t.wholeMatch(of: /(?i)-{2,}\s*(original|forwarded) message\s*-{0,}/) != nil { return true }
        if t.wholeMatch(of: /(?i)Begin forwarded message:?\s*/) != nil { return true }
        return false
    }

    struct TextSplit {
        var visible: String
        /// nil = collapse nothing.
        var quoted: String?
    }

    /// Split a PLAIN-TEXT body into the visible reply and its trailing quoted
    /// history. Returns `quoted: nil` unless the tail is genuinely history: at
    /// least two quoted lines, and almost nothing that is neither quote,
    /// attribution, nor blank. A message that STARTS quoted is left alone:
    /// collapsing it would blank the whole card.
    static func splitText(_ content: String) -> TextSplit {
        let lines = content.components(separatedBy: "\n")
        var start = -1
        for (i, line) in lines.enumerated() {
            if line.firstMatch(of: /^\s*>/) != nil || isAttribution(line) {
                start = i
                break
            }
        }
        guard start > 0 else { return TextSplit(visible: content, quoted: nil) }

        let tail = Array(lines[start...])
        let quotedCount = tail.filter { $0.firstMatch(of: /^\s*>/) != nil }.count
        let stray = tail.filter {
            !$0.trimmingCharacters(in: .whitespaces).isEmpty
                && $0.firstMatch(of: /^\s*>/) == nil && !isAttribution($0)
        }.count
        if quotedCount < 2 || stray > max(2, Int(Double(tail.count) * 0.2)) {
            return TextSplit(visible: content, quoted: nil)
        }
        return TextSplit(
            visible: lines[0..<start].joined(separator: "\n")
                .replacing(/\s+$/, with: ""),
            quoted: tail.joined(separator: "\n"))
    }
}
