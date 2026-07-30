// Strip tracking pixels from email HTML before it reaches the webview.
//
// The CSP is host-agnostic (img-src is the signed squelch-img: proxy — see
// docs/SECURITY.md §3), so this pass is the only seam that can tell a tracker
// from a hero image. The bias is asymmetric — a missed tracker is one opaque
// referrer-less GET, a false strip visibly breaks the mail — so strip only on
// strong signals: tiny declared size, CSS-hidden, or a known endpoint.

import Foundation

struct StripResult: Sendable {
    var html: String
    var blocked: Int
}

enum Trackers {
    /// Known open-tracking endpoints, matched case-insensitively against the img
    /// src. Deliberately small, and PATH-scoped: a bare host match would strip
    /// legitimate hero images served off the same CDN.
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

    /// Compiled once per attribute name: `attrValue` is hit ~6× per `<img>` and
    /// `strip` runs over every body of every thread. Any other name still
    /// compiles on demand, so matching behaviour is unchanged.
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

    /// Compiled once per property name, as `attrPatterns` is.
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

    /// Tiny only when BOTH declared width and height are present and ≤ 2px.
    /// DECLARED render size only — a 1×1 source stretched to width=600 is a
    /// layout element, not a tracker.
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

    /// Drop the `<img>` elements that meet a strip signal. Every other byte is
    /// preserved verbatim — see `HTMLImg` for why nothing is reserialized.
    static func strip(_ html: String) -> StripResult {
        var blocked = 0
        let out = HTMLImg.walk(html) { tag in
            guard shouldStrip(tag) else { return .keep }
            blocked += 1
            return .drop
        }
        return StripResult(html: out, blocked: blocked)
    }

    /// True when the html still references a network image (http(s) or
    /// protocol-relative). Counts CSS url() too — a mail whose art is all
    /// inline-style backgrounds still needs the opt-in bar.
    static func hasNetworkImages(_ html: String) -> Bool {
        if html.firstMatch(of: /(?i)<img[^>]+src\s*=\s*["']?(?:https?:)?\/\//) != nil { return true }
        if html.firstMatch(of: /(?i)url\(\s*["']?(?:https?:)?\/\//) != nil { return true }
        return false
    }

    /// The first http(s) `<img>` that plausibly isn't chrome: declared width (if
    /// present) ≥ 80px and height ≥ 40px, which skips social icons and spacer
    /// gifs. Protocol-relative srcs resolve to https.
    static func extractHeroSrc(_ html: String) -> String? {
        var hero: String?
        // Read-only pass: the spliced html is discarded, `.stop` just ends the
        // walk at the first hero the way an early `return` used to.
        _ = HTMLImg.walk(strip(html).html) { tag in
            guard var src = attrValue(tag, "src")?.trimmingCharacters(in: .whitespaces),
                !src.isEmpty
            else { return .keep }
            if src.hasPrefix("//") { src = "https:" + src }
            guard src.lowercased().hasPrefix("http") else { return .keep }
            if let w = pxDim(attrValue(tag, "width")), w < 80 { return .keep }
            if let h = pxDim(attrValue(tag, "height")), h < 40 { return .keep }
            hero = src
            return .stop
        }
        return hero
    }

    /// Anchor hrefs in document order, de-duped by href, labelled with the
    /// visible link text (empty text falls back to the host). Only http(s)
    /// survives here, and Opener re-guards it anyway.
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
