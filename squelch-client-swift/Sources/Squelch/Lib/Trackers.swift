// Strip tracking pixels from email HTML BEFORE it reaches the webview.
//
// TRADE-OFF (product decision, 2026-07): images load by DEFAULT. Real images
// are the whole point of rendering HTML mail; trackers are collateral we remove
// only when we can identify them with confidence. The webview's CSP is
// host-agnostic (default-src 'none' + img-src http:/https:/data:) so it cannot
// tell a tracker apart from a hero image — this preprocessing pass is the only
// seam where that distinction can be made.
//
// The bias is deliberately ASYMMETRIC: a FALSE-KEEP (a tracker we miss) is
// acceptable — no-referrer means no Referer leaks and links are inert, so the
// blast radius of a missed pixel is one opaque GET with no context. A
// FALSE-STRIP (nuking a real image) is NOT acceptable — it visibly breaks the
// email. So when in doubt we KEEP, and only strip on strong signals: declared
// pixel-tiny render size, CSS-hidden, or a known open-tracking endpoint.
//
// Ported 1:1 from squelch-desktop/src/lib/trackers.ts, including the
// deliberately SHORT known-endpoint list.

import Foundation

struct StripResult: Sendable {
    var html: String
    var blocked: Int
}

enum Trackers {
    /// Known open-tracking endpoints, matched case-insensitively against the
    /// img src URL. Kept deliberately small — the dimension/hidden heuristics
    /// do the heavy lifting; this list only covers a few high-volume,
    /// unambiguous endpoints. These are PATH-scoped on purpose: a bare host
    /// match would strip legitimate hero images served off the same CDN.
    nonisolated(unsafe) private static let knownTrackers: [Regex<AnyRegexOutput>] = {
        let patterns = [
            #"list-manage\.com/track/open"#,       // mailchimp
            #"ct\.sendgrid\.net/wf/open"#,          // sendgrid
            #"email\.mailgun\.net/o/"#,             // mailgun
            #"pstmrk\.it"#,                          // postmark
            #"t\.hubspotemail\.net"#,               // hubspot
            #"track\.hubspot\.com/__ptq\.gif"#,    // hubspot
            #"pi\.pardot\.com/open"#,               // pardot
            #"track\.customer\.io/e/o"#,            // customer.io
            #"google-analytics\.com/collect"#,      // GA measurement protocol
            #"doubleclick\.net"#,                    // doubleclick
        ]
        return patterns.compactMap { try? Regex($0).ignoresCase() }
    }()

    /// True only for srcs we're willing to URL-match trackers against: http(s)
    /// or protocol-relative. data:/cid:/relative srcs are never tracker-matched.
    private static func isNetworkSrc(_ src: String) -> Bool {
        let s = src.trimmingCharacters(in: .whitespaces).lowercased()
        return s.hasPrefix("http://") || s.hasPrefix("https://") || s.hasPrefix("//")
    }

    private static func matchesKnownTracker(_ src: String) -> Bool {
        guard isNetworkSrc(src) else { return false }
        return knownTrackers.contains { src.firstMatch(of: $0) != nil }
    }

    // MARK: - attribute reading

    /// The three quoting shapes one attribute name can arrive in.
    private struct AttrPatterns {
        let quoted: Regex<AnyRegexOutput>?
        let single: Regex<AnyRegexOutput>?
        let bare: Regex<AnyRegexOutput>?

        init(_ name: String) {
            quoted = try? Regex("(?i)\\b\(name)\\s*=\\s*\"([^\"]*)\"")
            single = try? Regex("(?i)\\b\(name)\\s*=\\s*'([^']*)'")
            bare = try? Regex("(?i)\\b\(name)\\s*=\\s*([^\\s>]+)")
        }
    }

    /// Compiled once per attribute NAME, matching the idiom the rest of the
    /// pipeline uses (ImageProxy's patterns). `attrValue` is hit ~6× per `<img>`
    /// and `strip` runs over every body of every thread, so building three
    /// regexes out of an interpolated pattern PER CALL was pure overhead. Only
    /// these names are ever asked for; anything else still compiles on demand,
    /// so nothing about the matching changes.
    nonisolated(unsafe) private static let attrPatterns: [String: AttrPatterns] =
        Dictionary(
            uniqueKeysWithValues: ["src", "style", "width", "height"].map { ($0, AttrPatterns($0)) }
        )

    /// Read one attribute's value from a raw `<img …>` tag string.
    static func attrValue(_ tag: String, _ name: String) -> String? {
        let patterns = attrPatterns[name] ?? attrPatterns[name.lowercased()] ?? AttrPatterns(name)
        guard let quoted = patterns.quoted else { return nil }
        if let m = tag.firstMatch(of: quoted), let r = m[1].range { return String(tag[r]) }
        guard let single = patterns.single else { return nil }
        if let m = tag.firstMatch(of: single), let r = m[1].range { return String(tag[r]) }
        guard let bare = patterns.bare else { return nil }
        if let m = tag.firstMatch(of: bare), let r = m[1].range { return String(tag[r]) }
        return nil
    }

    /// Parse a declared dimension ("1", "1px", " 2 ") to a number, or nil if it
    /// isn't a plain pixel length (%, auto, em, calc(), missing → not tiny).
    private static func pxDim(_ raw: String?) -> Double? {
        guard let raw else { return nil }
        let s = raw.trimmingCharacters(in: .whitespaces)
        guard !s.isEmpty else { return nil }
        guard let m = s.wholeMatch(of: /(?i)(\d+(?:\.\d+)?)(?:px)?/) else { return nil }
        return Double(m.1)
    }

    /// Compiled once per property name, for the same reason as `attrPatterns`.
    nonisolated(unsafe) private static let stylePatterns: [String: Regex<AnyRegexOutput>] =
        Dictionary(
            uniqueKeysWithValues: ["width", "height", "display", "visibility"].compactMap { prop in
                (try? Regex("(?i)(?:^|;)\\s*\(prop)\\s*:\\s*([^;]+)")).map { (prop, $0) }
            })

    /// Read a property value out of an inline style string (best-effort).
    private static func styleProp(_ style: String, _ prop: String) -> String? {
        let cached = stylePatterns[prop] ?? stylePatterns[prop.lowercased()]
        guard let re = cached ?? (try? Regex("(?i)(?:^|;)\\s*\(prop)\\s*:\\s*([^;]+)")) else {
            return nil
        }
        guard let m = style.firstMatch(of: re), let r = m[1].range else { return nil }
        return String(style[r]).trimmingCharacters(in: .whitespaces)
    }

    /// A pixel is "tiny" only when BOTH declared width and height are present
    /// and ≤ 2px. DECLARED render size only — a 1×1-source image stretched to
    /// width=600 is a layout element, not a tracker.
    private static func isTinyDeclared(_ tag: String) -> Bool {
        let style = attrValue(tag, "style") ?? ""
        let w = pxDim(attrValue(tag, "width")) ?? pxDim(styleProp(style, "width"))
        let h = pxDim(attrValue(tag, "height")) ?? pxDim(styleProp(style, "height"))
        guard let w, let h else { return false }
        return w <= 2 && h <= 2
    }

    /// Inline style hides the element outright.
    private static func isHidden(_ tag: String) -> Bool {
        let style = (attrValue(tag, "style") ?? "").lowercased()
        return styleProp(style, "display") == "none" || styleProp(style, "visibility") == "hidden"
    }

    private static func shouldStrip(_ tag: String) -> Bool {
        let src = attrValue(tag, "src") ?? ""
        return isTinyDeclared(tag) || isHidden(tag) || matchesKnownTracker(src)
    }

    // MARK: - the pass

    /// Walk the html, dropping only `<img>` elements that meet a strip signal.
    /// Every other byte is preserved verbatim (no reserialization — a DOM
    /// round-trip would rewrite markup the sanitizer already vetted).
    static func strip(_ html: String) -> StripResult {
        var out = ""
        var blocked = 0
        var rest = Substring(html)

        while let open = rest.range(of: "<img", options: [.caseInsensitive]) {
            // Only treat it as a tag start if the next char ends the token.
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
            // Find the tag's closing ">" — a sanitized attribute never holds a raw ">".
            guard let close = rest[afterIdx...].firstIndex(of: ">") else {
                out += rest[open.lowerBound...]
                return StripResult(html: out, blocked: blocked)
            }
            let tag = String(rest[open.lowerBound...close])
            if shouldStrip(tag) { blocked += 1 } else { out += tag }
            rest = rest[rest.index(after: close)...]
        }
        out += rest
        return StripResult(html: out, blocked: blocked)
    }

    /// True when the html still references at least one NETWORK image (http(s)
    /// or protocol-relative), counting CSS url() as well as <img src> — a mail
    /// whose art is all inline-style backgrounds still needs the opt-in bar.
    static func hasNetworkImages(_ html: String) -> Bool {
        if html.firstMatch(of: /(?i)<img[^>]+src\s*=\s*["']?(?:https?:)?\/\//) != nil { return true }
        if html.firstMatch(of: /(?i)url\(\s*["']?(?:https?:)?\/\//) != nil { return true }
        return false
    }

    /// Pick the HERO image src from a newsletter's sanitized html: the first
    /// http(s) `<img>` that plausibly isn't chrome — declared width (when
    /// present) must be >= 80px and height >= 40px, which skips social icons
    /// and spacer gifs. Protocol-relative srcs resolve to https.
    static func extractHeroSrc(_ html: String) -> String? {
        var rest = Substring(strip(html).html)
        while let open = rest.range(of: "<img", options: [.caseInsensitive]) {
            guard let close = rest[open.upperBound...].firstIndex(of: ">") else { return nil }
            let tag = String(rest[open.lowerBound...close])
            rest = rest[rest.index(after: close)...]

            guard var src = attrValue(tag, "src")?.trimmingCharacters(in: .whitespaces),
                !src.isEmpty
            else { continue }
            if src.hasPrefix("//") { src = "https:" + src }
            guard src.lowercased().hasPrefix("http") else { continue }
            if let w = pxDim(attrValue(tag, "width")), w < 80 { continue }
            if let h = pxDim(attrValue(tag, "height")), h < 40 { continue }
            return src
        }
        return nil
    }

    /// Pull http/https anchor hrefs out of sanitized html, in document order,
    /// de-duped by href. The visible link text is captured for a readable
    /// label; empty text falls back to the href host. Only http/https survive —
    /// everything else is dropped here and re-guarded in Opener.
    static func extractLinks(_ html: String) -> [EmailLink] {
        var out: [EmailLink] = []
        var seen = Set<String>()
        for m in html.matches(of: /(?is)<a\b[^>]*\bhref\s*=\s*["']([^"']+)["'][^>]*>(.*?)<\/a>/) {
            let href = String(m.1).trimmingCharacters(in: .whitespaces)
            guard href.lowercased().hasPrefix("http"), !seen.contains(href) else { continue }
            seen.insert(href)
            let text =
                String(m.2)
                .replacing(/<[^>]+>/, with: " ")
                .replacing(/&nbsp;/, with: " ")
                .replacing(/\s+/, with: " ")
                .trimmingCharacters(in: .whitespaces)
            let label = text.isEmpty ? (URL(string: href)?.host ?? href) : text
            out.append(EmailLink(href: href, text: label))
        }
        return out
    }
}

/// An extracted, de-duped outbound link: the http(s) href + its visible text.
struct EmailLink: Identifiable, Hashable, Sendable {
    var href: String
    var text: String
    var id: String { href }
}
