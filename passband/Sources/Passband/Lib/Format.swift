// Small pure formatters for the read side. Dense output: relative time as
// compact tokens ("2h", "5d", "3w"), deadline chips, importance coloring, and
// age weighting for the STILL OPEN escalation.

import Foundation

enum Fmt {
    /// Shared RFC3339 parser — the daemon emits fractional seconds on some
    /// fields and not others, so try both.
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

    /// Parsed timestamps, memoized: every visible row parses an age and a
    /// deadline chip per render, and RFC3339 parsing is a measurable frame cost.
    /// A failed parse is cached too — the same bad stamp must not re-parse.
    nonisolated(unsafe) private static var dateCache = LRUMap<String, Date?>(limit: dateCacheCap)
    private static let dateCacheLock = NSLock()
    private static let dateCacheCap = 4000

    static func date(_ iso: String?) -> Date? {
        guard let iso, !iso.isEmpty else { return nil }
        dateCacheLock.lock()
        if let hit = dateCache.get(iso) {
            dateCacheLock.unlock()
            return hit
        }
        dateCacheLock.unlock()

        let parsed = parseISO(iso)

        dateCacheLock.lock()
        dateCache.set(iso, parsed)
        dateCacheLock.unlock()
        return parsed
    }

    private static func parseISO(_ iso: String) -> Date? {
        dateCacheLock.lock()
        defer { dateCacheLock.unlock() }
        // ISO8601DateFormatter is not safe for concurrent use, so the lock that
        // guards the cache guards the formatters too.
        if let d = isoFractional.date(from: iso) { return d }
        if let d = isoPlain.date(from: iso) { return d }
        // Bare "YYYY-MM-DD" (marketing expires_at).
        if iso.count == 10, let d = isoPlain.date(from: iso + "T00:00:00Z") { return d }
        return nil
    }

    /// Compact relative age from an ISO timestamp, e.g. "12m", "4h", "5d", "3w".
    static func relAge(_ iso: String?, now: Date = Date()) -> String {
        guard let then = date(iso) else { return "" }
        return relAge(then, now: now)
    }

    /// Same, for a Date already in hand — no round trip through a formatter.
    static func relAge(_ then: Date, now: Date = Date()) -> String {
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

    /// When a pending reminder comes back, at row scale: "in 40m", "in 3h",
    /// "tomorrow 9am", "Sat 9am", "aug 30".
    ///
    /// The scale changes with the distance because the useful fact does. Inside
    /// the day what matters is how long you have; past it, which day; past the
    /// week, the date — a countdown of "in 214h" answers nothing anybody asked.
    static func remindChip(_ iso: String?, now: Date = Date()) -> String {
        guard let then = date(iso) else { return "" }
        let secs = then.timeIntervalSince(now)
        // A reminder whose moment has passed but whose sweep has not run yet.
        if secs <= 0 { return "due" }
        if secs < 12 * 3600 {
            let mins = Int((secs / 60).rounded())
            return mins < 60 ? "in \(max(1, mins))m" : "in \(Int((secs / 3600).rounded()))h"
        }
        if Calendar.current.isDateInTomorrow(then) { return "tomorrow \(clock(then))" }
        if secs < 7 * 86400 { return "\(weekday(iso)) \(clock(then))" }
        return shortDate(iso).lowercased()
    }

    /// Compact wall clock: "9am", "6:30pm". Hand-built rather than run through a
    /// date style because at chip size ":00 AM" is three characters of nothing —
    /// and because 12-hour is what every other string in this app assumes.
    private static func clock(_ then: Date, calendar: Calendar = .current) -> String {
        let parts = calendar.dateComponents([.hour, .minute], from: then)
        let hour24 = parts.hour ?? 0
        let minute = parts.minute ?? 0
        let suffix = hour24 < 12 ? "am" : "pm"
        let hour = hour24 % 12 == 0 ? 12 : hour24 % 12
        return minute == 0
            ? "\(hour)\(suffix)" : "\(hour):\(String(format: "%02d", minute))\(suffix)"
    }

    /// Rendered stamps, memoized like the parsed dates above and for the same
    /// reason: `Date.formatted` builds a format style and runs ICU on every call,
    /// and these run for every visible row on every render — a deadline chip
    /// alone pays it once per for-your-eyes row. Only ABSOLUTE formats belong
    /// here; anything relative to `now` (see `relAge`) must never be cached.
    nonisolated(unsafe) private static var textCache = LRUMap<String, String>(limit: dateCacheCap)

    /// `kind` namespaces the key, so the same stamp can hold a short date and a
    /// date-time without one answering for the other.
    private static func rendered(_ iso: String?, _ kind: String, _ make: (Date) -> String) -> String
    {
        // Parse FIRST: `date` takes the same lock, so holding it here would
        // deadlock on the very first miss.
        guard let iso, let d = date(iso) else { return "" }
        let key = kind + iso
        dateCacheLock.lock()
        if let hit = textCache.get(key) {
            dateCacheLock.unlock()
            return hit
        }
        dateCacheLock.unlock()

        let value = make(d)

        dateCacheLock.lock()
        textCache.set(key, value)
        dateCacheLock.unlock()
        return value
    }

    /// Compact date like "Jul 11" (local).
    static func shortDate(_ iso: String?) -> String {
        rendered(iso, "d|") { $0.formatted(.dateTime.month(.abbreviated).day()) }
    }

    /// Date + time like "Jul 11, 2:32 PM" for thread messages / audit.
    static func dateTime(_ iso: String?) -> String {
        rendered(iso, "dt|") { $0.formatted(.dateTime.month(.abbreviated).day().hour().minute()) }
    }

    /// Same, for a Date already in hand — read-tracking opens arrive as unix
    /// seconds rather than the RFC3339 text the rest of the wire carries.
    /// Uncached on purpose: a Date is not a stable cache key the way the
    /// server's stamp string is.
    static func dateTime(_ then: Date) -> String {
        then.formatted(.dateTime.month(.abbreviated).day().hour().minute())
    }

    /// Weekday alone, like "Tue". Only unambiguous inside a week of today, so
    /// the caller owns the window and falls back to `shortDate` outside it.
    static func weekday(_ iso: String?) -> String {
        rendered(iso, "w|") { $0.formatted(.dateTime.weekday(.abbreviated)) }
    }

    /// Time-of-day like "3:00 PM" (calendar rail).
    static func timeOfDay(_ iso: String?) -> String {
        rendered(iso, "t|") { $0.formatted(.dateTime.hour().minute()) }
    }

    /// True if the stamp falls on the current LOCAL calendar day; nil → false.
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

    /// Truncate into a copy budget of `n` characters, the ellipsis included, so
    /// a cut announces itself rather than landing mid-word the way the system's
    /// own truncation does.
    static func truncate(_ s: String, _ n: Int) -> String {
        guard n > 0 else { return "" }
        guard s.count > n else { return s }
        return String(s.prefix(n - 1)).trimmingCharacters(in: .whitespaces) + "…"
    }

    /// Uppercase the first character, leaving the rest alone — for text that
    /// starts a sentence after a leading label was stripped off it.
    static func capitalizingFirst(_ s: String) -> String {
        s.prefix(1).uppercased() + s.dropFirst()
    }
}

/// The first "$1,234.56"-shaped run in a one-liner. Hand-rolled and memoized on
/// purpose: the sitrep runs this per visible row per render, and a `Regex` there
/// was costing frames.
@MainActor
enum MoneyScan {
    /// "no amount in this line" is a verdict worth keeping, so the value is
    /// itself optional: a hit is an answer, a miss is an unscanned line.
    private static var cache = LRUMap<String, String?>(limit: cap)
    private static let cap = 2000

    static func amount(in text: String) -> String? {
        if let hit = cache.get(text) { return hit }
        let value = scan(text)
        cache.set(text, value)
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
