// What "remind me…" resolves to. The `H` palette types into this and picks a
// row; the row's `date` is what gets stamped on the server.
//
// EVERY ROW SHOWS ITS ABSOLUTE TIME. A reminder is a promise about the future,
// and "next week" is a different Monday depending on which day you asked — so
// the label is what you typed and the `detail` is when it actually fires. A
// suggestion nobody can read back is not a suggestion, it is a guess.
//
// THREE SOURCES, IN ORDER OF HOW MUCH WE KNOW. The quick picks are words this
// app owns and resolves exactly; the relative parser handles "in N units"; and
// NSDataDetector is the open world underneath both. The detector is only asked
// when neither of the first two answered — it would otherwise re-guess a word
// we already resolved and hand back a near-duplicate an hour off.
//
// Pure Foundation, network-free, and every date computed through the INJECTED
// `now`/`calendar` so the whole thing is assertable (see RemindTimesTests).
// Times are LOCAL throughout; the conversion to UTC happens at the wire.

import Foundation

/// One offered remind time.
struct RemindHit: Identifiable, Hashable, Sendable {
    /// The words for it — what was typed, or the pick's own name.
    var label: String
    /// When it fires, in local time.
    var date: Date
    /// The same instant, spelled out. Never omitted: see the header.
    var detail: String

    /// Identity is the INSTANT, not the label: two sources naming the same
    /// minute are one row, and dedup upstream relies on that being true.
    var id: String { "\(label)|\(Int(date.timeIntervalSince1970))" }
}

enum RemindTimes {
    // MARK: - quick picks

    /// How a pick's date is built. An enum rather than a closure so the whole
    /// table stays a value — declaration order is the tiebreak, and a table of
    /// closures could not be compared, copied or reasoned about as data.
    private enum Recipe: Sendable {
        /// now + N hours, to the minute — "in an hour" means an hour, not 9am.
        case afterHours(Int)
        /// A wall-clock hour on the day N days from today.
        case dayAt(days: Int, hour: Int)
        /// The next occurrence of a weekday (1 = Sunday) at a wall-clock hour,
        /// searched from TOMORROW: a named weekday never means today.
        case weekdayAt(weekday: Int, hour: Int)
        /// The nearest weekend morning still ahead — its own recipe because the
        /// weekend is TWO days and `weekdayAt` can only name one of them.
        case weekend(hour: Int)
        /// A wall-clock hour on the same day-of-month N months out.
        case monthAt(months: Int, hour: Int)
    }

    private struct QuickPick: Sendable {
        var label: String
        var recipe: Recipe
        /// Extra words that should reach this pick.
        var aliases: [String]
    }

    /// The offered times, in the order an empty query shows them: soonest and
    /// most-asked-for first. 09:00 is the standing "morning" and 18:00 the
    /// standing "evening" — a reminder that fires at 3am is a notification you
    /// will never see land.
    private static let picks: [QuickPick] = [
        QuickPick(
            label: "in an hour", recipe: .afterHours(1),
            aliases: ["hour", "an hour", "1h", "60m", "later"]),
        QuickPick(
            label: "this evening", recipe: .dayAt(days: 0, hour: 18),
            aliases: ["tonight", "evening", "6pm", "later today", "today"]),
        QuickPick(
            label: "tomorrow morning", recipe: .dayAt(days: 1, hour: 9),
            aliases: ["tomorrow", "tmrw", "tmr", "morning", "9am", "am"]),
        QuickPick(
            label: "tomorrow evening", recipe: .dayAt(days: 1, hour: 18),
            aliases: ["tomorrow night", "tmrw evening", "tmrw night"]),
        QuickPick(
            label: "this weekend", recipe: .weekend(hour: 9),
            aliases: ["weekend", "saturday", "sat"]),
        QuickPick(
            label: "next week", recipe: .weekdayAt(weekday: 2, hour: 9),
            aliases: ["monday", "mon", "next monday"]),
        QuickPick(
            label: "in a week", recipe: .dayAt(days: 7, hour: 9),
            aliases: ["a week", "one week", "7 days", "7d", "1w"]),
        QuickPick(
            label: "next month", recipe: .monthAt(months: 1, hour: 9),
            aliases: ["month", "a month", "in a month", "1mo", "30 days"]),
    ]

    // MARK: - matching

    /// Normalize for matching: lowercase, and spaces/underscores/hyphens are
    /// nothing. Same rule as TriageTargets, for the same reason — "tomorrow
    /// morning" and "tomorrowmorning" are the same intent.
    private static func norm(_ s: String) -> String {
        s.lowercased().filter { $0 != " " && $0 != "_" && $0 != "-" }
    }

    /// The score at which a pick is taken to have ANSWERED the query, which is
    /// what silences the detector underneath it.
    private static let exactScore = 90

    /// Rank one pick against what was typed. Higher = better; 0 hides it.
    ///
    /// An exact ALIAS outranks a label PREFIX, which is the one ordering here
    /// that is not obvious: "tomorrow" is an alias of tomorrow morning and a
    /// prefix of tomorrow evening, and typing it must offer the morning first.
    private static func score(_ pick: QuickPick, query: String) -> Int {
        let q = norm(query)
        if q.isEmpty { return 1 }  // empty query: everything, in declaration order

        let label = norm(pick.label)
        if label == q { return 100 }
        for alias in pick.aliases where norm(alias) == q { return exactScore }
        if label.hasPrefix(q) { return 80 }
        for alias in pick.aliases where norm(alias).hasPrefix(q) { return 40 }
        if label.contains(q) { return 20 }
        return 0
    }

    /// The ranked, filtered picks for a query. Stable within equal scores, so
    /// the declaration order above is what breaks every tie.
    private static func ranked(_ query: String) -> [(pick: QuickPick, score: Int)] {
        var out: [(index: Int, pick: QuickPick, score: Int)] = []
        for (index, pick) in picks.enumerated() {
            let s = score(pick, query: query)
            if s > 0 { out.append((index, pick, s)) }
        }
        out.sort { a, b in a.score != b.score ? a.score > b.score : a.index < b.index }
        return out.map { ($0.pick, $0.score) }
    }

    /// Everything `query` could mean, soonest-answer-first, with anything
    /// already in the past dropped and one row per minute.
    ///
    /// PAST CANDIDATES ARE DROPPED, NOT CLAMPED: "this evening" at 9pm is not a
    /// reminder for 9:01pm, it is a phrase that no longer refers to a future
    /// time, and the row simply stops being offered.
    static func match(
        _ query: String, now: Date = Date(), calendar: Calendar = .current
    ) -> [RemindHit] {
        var out: [RemindHit] = []
        var seen = Set<Int>()

        func add(_ hit: RemindHit?) {
            guard let hit, hit.date > now else { return }
            // Bucketed to the minute: two sources naming the same time to the
            // second is luck, naming the same minute is the same answer.
            guard seen.insert(Int(hit.date.timeIntervalSince1970 / 60)).inserted else { return }
            out.append(hit)
        }

        let hits = ranked(query)
        for (pick, _) in hits {
            guard let date = resolve(pick.recipe, now: now, calendar: calendar) else { continue }
            add(
                RemindHit(
                    label: pick.label, date: date,
                    detail: detail(date, now: now, calendar: calendar)))
        }

        let rel = relative(query, now: now, calendar: calendar)
        add(rel)

        // The open world, asked last and only when the closed one had no exact
        // answer. Both guards matter: "mon" is ours, and "in 2 hours" is the
        // relative parser's — letting the detector also answer either would put
        // a second, subtly different row under the one that was already right.
        if rel == nil, !hits.contains(where: { $0.score >= exactScore }) {
            for hit in detected(query, now: now, calendar: calendar) { add(hit) }
        }
        return out
    }

    // MARK: - resolution

    private static func resolve(_ recipe: Recipe, now: Date, calendar: Calendar) -> Date? {
        switch recipe {
        case .afterHours(let hours):
            return calendar.date(byAdding: .hour, value: hours, to: now)
        case .dayAt(let days, let hour):
            guard let day = calendar.date(byAdding: .day, value: days, to: now) else { return nil }
            return at(hour, on: day, calendar: calendar)
        case .weekdayAt(let weekday, let hour):
            return nextWeekday(weekday, hour: hour, now: now, calendar: calendar)
        case .weekend(let hour):
            // THE WEEKEND IS TWO DAYS, and which of them is still ahead depends
            // on where inside it you are asking from. A plain "next Saturday"
            // match cannot say that: asked on a Saturday afternoon it steps
            // clean over the Sunday sitting right there and lands a week out.
            let weekday = calendar.component(.weekday, from: now)
            if weekday == 7 || weekday == 1 {
                // Still before the hour on a weekend day: it is today.
                if let today = at(hour, on: now, calendar: calendar), today > now { return today }
                // Saturday, past it: Sunday is the rest of this weekend.
                if weekday == 7 {
                    guard let tomorrow = calendar.date(byAdding: .day, value: 1, to: now)
                    else { return nil }
                    return at(hour, on: tomorrow, calendar: calendar)
                }
                // Sunday, past it: this weekend is over, so the next one.
            }
            return nextWeekday(7, hour: hour, now: now, calendar: calendar)
        case .monthAt(let months, let hour):
            guard let month = calendar.date(byAdding: .month, value: months, to: now)
            else { return nil }
            return at(hour, on: month, calendar: calendar)
        }
    }

    /// The next `weekday` (1 = Sunday) at `hour`, searched from the START OF
    /// TOMORROW.
    ///
    /// Not from `now`, which is the whole point: `nextDate` matches LATER THE
    /// SAME DAY, so "next week" asked on a Monday at 08:30 resolves to 09:00
    /// that same morning — a next week that fires in thirty minutes, on mail
    /// that just left the inbox. Tomorrow is the earliest day a named weekday
    /// can honestly mean.
    private static func nextWeekday(
        _ weekday: Int, hour: Int, now: Date, calendar: Calendar
    ) -> Date? {
        guard let tomorrow = calendar.date(byAdding: .day, value: 1, to: now) else { return nil }
        return calendar.nextDate(
            after: calendar.startOfDay(for: tomorrow),
            matching: DateComponents(hour: hour, minute: 0, second: 0, weekday: weekday),
            matchingPolicy: .nextTime)
    }

    /// A wall-clock hour on the calendar day `day` falls in.
    ///
    /// Rebuilt from components rather than through `date(bySettingHour:of:)`,
    /// which searches FORWARD from the date it is handed: asking it for 9am on
    /// a date that is already 2pm hands back TOMORROW at 9am, silently, and
    /// every "morning" pick would land a day late.
    private static func at(_ hour: Int, on day: Date, calendar: Calendar) -> Date? {
        var parts = calendar.dateComponents([.year, .month, .day], from: day)
        parts.hour = hour
        parts.minute = 0
        parts.second = 0
        return calendar.date(from: parts)
    }

    // MARK: - "in N units"

    /// "in 20 minutes", "in 3 days", "2w". Hand-rolled rather than a regex so
    /// the unit table is readable and the whole thing stays allocation-cheap on
    /// every keystroke. Returns nil for anything that is not exactly this shape.
    private static func relative(_ query: String, now: Date, calendar: Calendar) -> RemindHit? {
        var text = query.trimmingCharacters(in: .whitespaces).lowercased()
        if text.hasPrefix("in ") { text = String(text.dropFirst(3)) }
        text = text.trimmingCharacters(in: .whitespaces)

        var digits = ""
        var rest = Substring(text)
        while let c = rest.first, c.isNumber {
            digits.append(c)
            rest = rest.dropFirst()
        }
        let unit = rest.trimmingCharacters(in: .whitespaces)
        guard let count = Int(digits), count > 0, !unit.isEmpty else { return nil }

        // "mo" before "m": months and minutes share a first letter, and the
        // longer prefix has to win or "2mo" is two minutes.
        let component: Calendar.Component
        let word: String
        if unit.hasPrefix("mo") {
            (component, word) = (.month, "month")
        } else if unit.hasPrefix("mi") || unit == "m" {
            (component, word) = (.minute, "minute")
        } else if unit.hasPrefix("h") {
            (component, word) = (.hour, "hour")
        } else if unit.hasPrefix("d") {
            (component, word) = (.day, "day")
        } else if unit.hasPrefix("w") {
            (component, word) = (.weekOfYear, "week")
        } else {
            return nil
        }

        guard let date = calendar.date(byAdding: component, value: count, to: now) else {
            return nil
        }
        return RemindHit(
            label: "in \(count) \(word)\(count == 1 ? "" : "s")", date: date,
            detail: detail(date, now: now, calendar: calendar))
    }

    // MARK: - the open world

    /// Anything the system's own date parser recognises: "aug 30", "next
    /// tuesday 3pm", "9/2 at noon".
    private static func detected(_ query: String, now: Date, calendar: Calendar) -> [RemindHit] {
        let text = query.trimmingCharacters(in: .whitespaces)
        // Two characters cannot be a date, and asking anyway turns every first
        // keystroke into a detector run.
        guard text.count >= 3,
            let detector = try? NSDataDetector(
                types: NSTextCheckingResult.CheckingType.date.rawValue)
        else { return [] }

        let ns = text as NSString
        // A BARE DATE IS NOT A TIME. "aug 30" names a day, and the detector
        // fills the hour in with a default of its own (noon, as of this
        // writing) — a time nobody said, which is exactly the kind of quiet
        // wrongness a reminder only reveals by firing at the wrong moment. So
        // the TEXT decides: said no time, gets the same 09:00 every morning
        // pick uses. Asked of the query rather than of the parsed result
        // because the detector's own default is undocumented and has moved.
        //
        // (NSTextCheckingResult.timeIsSignificant would be the direct answer,
        // and it is not bridged into Swift.)
        let snap = !hasTimeToken(text)
        var out: [RemindHit] = []
        for match in detector.matches(in: text, range: NSRange(location: 0, length: ns.length)) {
            guard var date = match.date else { continue }
            if snap { date = at(9, on: date, calendar: calendar) ?? date }
            out.append(
                RemindHit(
                    label: ns.substring(with: match.range), date: date,
                    detail: detail(date, now: now, calendar: calendar)))
        }
        return out
    }

    /// Whether the text names a time of day at all. Token-wise, not substring:
    /// "sam" and "campaign" both contain "am", and a false positive here costs
    /// the 09:00 snap above.
    private static let timeWords: Set<String> = [
        "noon", "midnight", "midday", "morning", "afternoon", "evening", "night",
        "tonight", "oclock",
    ]

    private static func hasTimeToken(_ text: String) -> Bool {
        if text.contains(":") { return true }
        // Apostrophes are ERASED rather than treated as separators: splitting on
        // one turns "o'clock" into "o" and "clock", and neither half is a time
        // word. Both the typed one and the one autocorrect substitutes.
        let flat =
            text.lowercased()
            .replacingOccurrences(of: "'", with: "")
            .replacingOccurrences(of: "\u{2019}", with: "")
        let tokens = flat.split(whereSeparator: { !$0.isLetter && !$0.isNumber }).map(String.init)
        for (i, token) in tokens.enumerated() {
            if timeWords.contains(token) { return true }
            if token == "am" || token == "pm" { return true }
            // "3pm", "1130pm" — a NUMBER carrying the marker. A word that
            // merely ends in one ("spam", "program") does not.
            if token.count > 2, token.hasSuffix("am") || token.hasSuffix("pm"),
                token.dropLast(2).allSatisfy(\.isNumber)
            {
                return true
            }
            // "at 8", "at 17" — the preposition is what makes a bare number an
            // hour, and the detector reads it that way too. Without this the
            // 09:00 snap overwrites an hour the user typed in so many words,
            // while the row's label still shows the words. The number has to
            // stand alone and be a real hour: "at 2099" is a year.
            if token == "at", i + 1 < tokens.count {
                let next = tokens[i + 1]
                if next.count <= 2, next.allSatisfy(\.isNumber), let hour = Int(next),
                    hour >= 0, hour <= 23
                {
                    return true
                }
            }
        }
        return false
    }

    // MARK: - rendering

    /// The absolute time, at the coarsest scale that is still unambiguous:
    /// today and tomorrow by name, the rest of the week by weekday AND date
    /// (a weekday alone means two different days once you are eight days out),
    /// and anything further by date. The year appears only when it differs.
    static func detail(_ date: Date, now: Date = Date(), calendar: Calendar = .current) -> String {
        let time = format(date, template: "jmm", calendar: calendar)
        if calendar.isDate(date, inSameDayAs: now) { return "today \(time)" }
        if let tomorrow = calendar.date(byAdding: .day, value: 1, to: now),
            calendar.isDate(date, inSameDayAs: tomorrow)
        {
            return "tomorrow \(time)"
        }
        let days =
            calendar.dateComponents(
                [.day], from: calendar.startOfDay(for: now), to: calendar.startOfDay(for: date)
            ).day ?? 0
        if days > 0, days < 7 {
            return "\(format(date, template: "EEEMMMd", calendar: calendar)), \(time)"
        }
        let sameYear =
            calendar.component(.year, from: date) == calendar.component(.year, from: now)
        let template = sameYear ? "MMMd" : "MMMdyyyy"
        return "\(format(date, template: template, calendar: calendar)), \(time)"
    }

    /// Locale-shaped rendering of a skeleton, through the INJECTED calendar —
    /// a formatter reading `Calendar.current` would make every assertion here
    /// depend on the machine it ran on.
    private static func format(_ date: Date, template: String, calendar: Calendar) -> String {
        let locale = calendar.locale ?? .current
        let formatter = DateFormatter()
        formatter.calendar = calendar
        formatter.timeZone = calendar.timeZone
        formatter.locale = locale
        formatter.dateFormat =
            DateFormatter.dateFormat(fromTemplate: template, options: 0, locale: locale) ?? template
        // ICU separates the AM/PM marker with a NARROW NO-BREAK SPACE. It is
        // invisible, it is not the space anyone types, and it makes two
        // identical-looking strings compare unequal — so it never leaves here.
        return formatter.string(from: date).replacingOccurrences(of: "\u{202F}", with: " ")
    }
}
