// Small pure formatters for the read side. Dense output: relative time as
// compact tokens ("2h", "5d", "3w"), deadline chips, importance coloring, and
// age weighting for the STILL OPEN escalation.
//
// Ported from squelch-desktop/src/lib/format.ts.

import Foundation

enum Fmt {
    /// Shared RFC3339 parser. The daemon emits fractional seconds on some
    /// fields and not others, so try both.
    // ISO8601DateFormatter is documented thread-safe for parsing but is not
    // marked Sendable; these are immutable after init, so the unchecked
    // annotation is accurate rather than a suppression.
    nonisolated(unsafe) private static let isoFractional: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()
    nonisolated(unsafe) private static let isoPlain: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime]
        return f
    }()

    /// Parsed timestamps, memoized. Every visible row asks for a relative age
    /// and a deadline chip on every render, and RFC3339 parsing is not cheap —
    /// uncached it was a measurable share of each frame in a long list.
    nonisolated(unsafe) private static var dateCache: [String: Date?] = [:]
    private static let dateCacheLock = NSLock()
    private static let dateCacheCap = 4000

    static func date(_ iso: String?) -> Date? {
        guard let iso, !iso.isEmpty else { return nil }
        dateCacheLock.lock()
        if let hit = dateCache[iso] {
            dateCacheLock.unlock()
            return hit
        }
        dateCacheLock.unlock()

        let parsed = parseISO(iso)

        dateCacheLock.lock()
        if dateCache.count >= dateCacheCap { dateCache.removeAll(keepingCapacity: true) }
        dateCache[iso] = parsed
        dateCacheLock.unlock()
        return parsed
    }

    private static func parseISO(_ iso: String) -> Date? {
        dateCacheLock.lock()
        defer { dateCacheLock.unlock() }
        // The formatters are not thread-safe for concurrent use; the lock that
        // guards the cache guards them too.
        if let d = isoFractional.date(from: iso) { return d }
        if let d = isoPlain.date(from: iso) { return d }
        // Bare "YYYY-MM-DD" (marketing expires_at).
        if iso.count == 10, let d = isoPlain.date(from: iso + "T00:00:00Z") { return d }
        return nil
    }

    /// Compact relative age from an ISO timestamp, e.g. "12m", "4h", "5d", "3w".
    static func relAge(_ iso: String?, now: Date = Date()) -> String {
        guard let then = date(iso) else { return "" }
        let ms = now.timeIntervalSince(then)
        if ms < 0 { return "now" }
        let min = ms / 60
        if min < 1 { return "now" }
        if min < 60 { return "\(Int(min.rounded()))m" }
        let h = min / 60
        if h < 24 { return "\(Int(h.rounded()))h" }
        let d = h / 24
        if d < 14 { return "\(Int(d.rounded()))d" }
        let w = d / 7
        if w < 8 { return "\(Int(w.rounded()))w" }
        let mo = d / 30
        return "\(Int(mo.rounded()))mo"
    }

    /// Human byte size for attachment cards, e.g. "812 B", "34 KB", "2.4 MB".
    static func humanSize(_ bytes: Int?) -> String {
        guard let bytes, bytes >= 0 else { return "—" }
        if bytes < 1024 { return "\(bytes) B" }
        let kb = Double(bytes) / 1024
        if kb < 1024 { return "\(Int(kb.rounded())) KB" }
        let mb = kb / 1024
        return mb < 10 ? String(format: "%.1f MB", mb) : "\(Int(mb.rounded())) MB"
    }

    private static func unit(_ n: Int, _ word: String) -> String {
        "\(n) \(word)\(n == 1 ? "" : "S")"
    }

    /// Louder relative age used in the STILL OPEN band, e.g. "2 WEEKS".
    static func loudAge(_ iso: String?, now: Date = Date()) -> String {
        guard let then = date(iso) else { return "" }
        let ms = now.timeIntervalSince(then)
        if ms < 0 { return "NOW" }
        let min = ms / 60
        if min < 60 { return "\(Int(min.rounded())) MIN" }
        let h = min / 60
        if h < 24 { return unit(Int(h.rounded()), "HOUR") }
        let d = h / 24
        if d < 14 { return unit(Int(d.rounded()), "DAY") }
        return unit(Int((d / 7).rounded()), "WEEK")
    }

    /// Whole hours since an ISO timestamp (0 if missing/invalid/future).
    static func ageHours(_ iso: String?, now: Date = Date()) -> Double {
        guard let then = date(iso) else { return 0 }
        let ms = now.timeIntervalSince(then)
        return ms <= 0 ? 0 : ms / 3600
    }

    /// The 48h threshold where the aging badge earns its place.
    static let agingThresholdH: Double = 48
    static func isAging(_ iso: String?) -> Bool { ageHours(iso) > agingThresholdH }

    /// "last checked: Xh ago" tail for the header.
    static func lastChecked(_ iso: String?) -> String {
        guard iso != nil else { return "never" }
        let a = relAge(iso)
        return a.isEmpty ? "just now" : "\(a) ago"
    }

    /// Age weight 0..1 for the STILL OPEN escalation. Age dominates (this
    /// band's whole point is rot), importance nudges. Saturates at ~30 days.
    static func openWeight(_ iso: String?, importance: Int, now: Date = Date()) -> Double {
        guard let then = date(iso) else { return 0 }
        let ms = now.timeIntervalSince(then)
        if ms <= 0 { return 0 }
        let days = ms / 86400
        let ageComponent = min(1, days / 30)
        let impComponent = min(1, max(0, Double(importance)) / 100)
        return min(1, ageComponent * 0.7 + impComponent * 0.3)
    }

    struct DeadlineChip: Equatable {
        var text: String
        var overdue: Bool
    }

    /// Deadline chip text. Past-due shows the overdue span; upcoming a date.
    static func deadlineChip(_ iso: String?, now: Date = Date()) -> DeadlineChip? {
        guard let t = date(iso) else { return nil }
        if t.timeIntervalSince(now) < 0 {
            return DeadlineChip(text: "\(loudAge(iso, now: now)) PAST DUE", overdue: true)
        }
        return DeadlineChip(text: "due \(shortDate(iso))", overdue: false)
    }

    /// Compact date like "Jul 11" (local).
    static func shortDate(_ iso: String?) -> String {
        guard let d = date(iso) else { return "" }
        return d.formatted(.dateTime.month(.abbreviated).day())
    }

    /// Date + time like "Jul 11, 2:32 PM" for thread messages / audit.
    static func dateTime(_ iso: String?) -> String {
        guard let d = date(iso) else { return "" }
        return d.formatted(.dateTime.month(.abbreviated).day().hour().minute())
    }

    /// Time-of-day like "3:00 PM" (calendar rail).
    static func timeOfDay(_ iso: String?) -> String {
        guard let d = date(iso) else { return "" }
        return d.formatted(.dateTime.hour().minute())
    }

    /// True if an RFC3339 timestamp falls on the current LOCAL calendar day.
    /// Parses defensively: a missing/unparseable stamp returns false.
    static func isToday(_ iso: String?) -> Bool {
        guard let d = date(iso) else { return false }
        return Calendar.current.isDateInToday(d)
    }

    /// Importance (0..100) as a 5-block meter glyph, e.g. "▰▰▰▱▱".
    static let meterSegments = 5
    static func importanceMeter(_ n: Int) -> String {
        let clamped = max(0, min(100, n))
        let filled = min(
            meterSegments, Int((Double(clamped) / 100 * Double(meterSegments)).rounded(.up)))
        return String(repeating: "▰", count: filled)
            + String(repeating: "▱", count: meterSegments - filled)
    }

    /// USD with two decimals + thousands separator ("$3.49"); "—" when absent.
    static func usd(_ amount: Double?, currency: String? = nil) -> String {
        guard let amount, !amount.isNaN else { return "—" }
        var style = FloatingPointFormatStyle<Double>.Currency(code: currency ?? "USD")
        style = style.precision(.fractionLength(2))
        return amount.formatted(style)
    }

    /// Dollars: 2 decimals, but 3 when under a cent so tiny per-day figures survive.
    static func costUSD(_ n: Double) -> String {
        String(format: n < 0.01 ? "$%.3f" : "$%.2f", n)
    }

    /// Compact integer formatting for token counts (1.2M, 34.0k, 812).
    static func compactCount(_ n: Int) -> String {
        if n >= 1_000_000 { return String(format: "%.1fM", Double(n) / 1_000_000) }
        if n >= 1000 { return String(format: "%.1fk", Double(n) / 1000) }
        return String(n)
    }

    /// Usage-page cost: 4 decimals under a dollar so sub-cent spend is visible.
    static func fmtCost(_ n: Double) -> String {
        String(format: n < 1 ? "$%.4f" : "$%.2f", n)
    }

    /// Today's date, "Mon, Jul 27" — the sitrep masthead stamp.
    static func todayStamp(now: Date = Date()) -> String {
        now.formatted(.dateTime.weekday(.abbreviated).month(.abbreviated).day())
    }
}

/// Pulls the first "$1,234.56"-shaped run out of a one-liner.
///
/// Hand-rolled and memoized on purpose: the sitrep evaluates this for every
/// visible row on every render, and a `Regex` there was costing frames.
@MainActor
enum MoneyScan {
    private static var cache: [String: String?] = [:]
    private static let cap = 2000

    static func amount(in text: String) -> String? {
        if let hit = cache[text] { return hit }
        let value = scan(text)
        if cache.count >= cap { cache.removeAll(keepingCapacity: true) }
        cache[text] = value
        return value
    }

    private static func scan(_ text: String) -> String? {
        let chars = Array(text)
        var i = 0
        while i < chars.count {
            guard chars[i] == "$" else {
                i += 1
                continue
            }
            var j = i + 1
            // Tolerate one space after the sigil ("$ 142.00").
            if j < chars.count, chars[j] == " " { j += 1 }
            var digits = ""
            while j < chars.count, chars[j].isNumber || chars[j] == "," || chars[j] == "." {
                digits.append(chars[j])
                j += 1
            }
            // Trim a trailing separator so "$5." reads as "$5".
            while digits.hasSuffix(".") || digits.hasSuffix(",") { digits.removeLast() }
            if digits.contains(where: \.isNumber) { return "$" + digits }
            i = j + 1
        }
        return nil
    }
}
