// What "remind me…" resolves to, asserted against a FIXED now and a fixed
// calendar. This suite exists because a reminder is the one thing in the app
// that is only ever wrong later: the mail leaves the inbox the moment you set
// it, and if "tomorrow morning" quietly meant the day after, or 9am meant
// midnight, nothing says so until the email fails to come back.
//
// Every case pins the calendar to UTC/en_US so the expected instants are
// arithmetic rather than a property of the machine. The one case that cannot —
// NSDataDetector reads the system's own time zone — runs against
// `Calendar.current` on both sides so the two still agree.

import Foundation

@main
@MainActor
struct RemindTimesTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        quickPicksResolve()
        emptyQueryIsThePicksInOrder()
        pastPicksAreDropped()
        tomorrowPrefersTheMorning()
        relativeUnits()
        relativeRejectsNonsense()
        bareDateSnapsToNine()
        detailNamesTheAbsoluteTime()
        duplicatesCollapse()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - fixtures

    /// UTC + en_US: the instants below are then plain arithmetic, and the
    /// rendered detail strings are stable.
    static var cal: Calendar {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "UTC")!
        c.locale = Locale(identifier: "en_US")
        return c
    }

    static func moment(
        _ y: Int, _ mo: Int, _ d: Int, _ h: Int, _ mi: Int = 0, calendar: Calendar? = nil
    ) -> Date {
        let c = calendar ?? cal
        return c.date(
            from: DateComponents(year: y, month: mo, day: d, hour: h, minute: mi, second: 0))!
    }

    /// Wednesday 19 Aug 2026, 14:30 UTC — a mid-week afternoon, so "this
    /// evening" is still ahead and the weekday picks all land in one week.
    static let now = moment(2026, 8, 19, 14, 30)

    static func hit(_ query: String, _ label: String, now: Date = now) -> RemindHit? {
        RemindTimes.match(query, now: now, calendar: cal).first { $0.label == label }
    }

    // MARK: - cases

    /// Every pick, against the same afternoon. 09:00 for morning-ish picks and
    /// 18:00 for evening ones is the whole contract of the table.
    static func quickPicksResolve() {
        equal(hit("", "in an hour")?.date, moment(2026, 8, 19, 15, 30), "in an hour = +1h")
        equal(hit("", "this evening")?.date, moment(2026, 8, 19, 18), "this evening = 18:00 today")
        equal(hit("", "tomorrow morning")?.date, moment(2026, 8, 20, 9), "tomorrow morning = 09:00")
        equal(hit("", "tomorrow evening")?.date, moment(2026, 8, 20, 18), "tomorrow evening = 18:00")
        // Wednesday the 19th → Saturday the 22nd, Monday the 24th.
        equal(hit("", "this weekend")?.date, moment(2026, 8, 22, 9), "weekend = next Saturday 09:00")
        equal(hit("", "next week")?.date, moment(2026, 8, 24, 9), "next week = next Monday 09:00")
        equal(hit("", "in a week")?.date, moment(2026, 8, 26, 9), "in a week = +7d at 09:00")
        equal(hit("", "next month")?.date, moment(2026, 9, 19, 9), "next month = +1mo at 09:00")

        // The gotcha this table is written around: asking a calendar to "set
        // hour 9" on an afternoon searches FORWARD. A morning pick that landed
        // a day late would still look plausible on screen.
        equal(
            cal.dateComponents([.day], from: hit("", "tomorrow morning")!.date).day, 20,
            "morning picks do not slide to the next day")
    }

    /// An empty field is the menu: every pick, in declaration order.
    static func emptyQueryIsThePicksInOrder() {
        let labels = RemindTimes.match("", now: now, calendar: cal).map(\.label)
        equal(
            labels,
            [
                "in an hour", "this evening", "tomorrow morning", "tomorrow evening",
                "this weekend", "next week", "in a week", "next month",
            ], "empty query offers every pick in order")
    }

    /// A phrase that no longer points at a future time stops being offered —
    /// it is not clamped to "one minute from now".
    static func pastPicksAreDropped() {
        let late = moment(2026, 8, 19, 21)
        let labels = RemindTimes.match("", now: late, calendar: cal).map(\.label)
        equal(labels.contains("this evening"), false, "9pm has no 'this evening' left")
        equal(labels.first, "in an hour", "the rest still stand")
        equal(
            hit("tonight", "this evening", now: late) == nil, true,
            "and naming it directly does not resurrect it")
        // Every hit, from any source, is in the future. This is the invariant
        // the server enforces with a 400.
        for h in RemindTimes.match("", now: late, calendar: cal) {
            equal(h.date > late, true, "\(h.label) is in the future")
        }
    }

    /// "tomorrow" is an alias of the morning and a prefix of the evening. The
    /// morning has to come first, or the obvious word picks the wrong row.
    static func tomorrowPrefersTheMorning() {
        let labels = RemindTimes.match("tomorrow", now: now, calendar: cal).map(\.label)
        equal(labels.first, "tomorrow morning", "'tomorrow' means the morning")
        equal(labels.contains("tomorrow evening"), true, "the evening stays on offer under it")
        equal(RemindTimes.match("tmrw", now: now, calendar: cal).first?.label, "tomorrow morning",
            "and so does the abbreviation")
        equal(RemindTimes.match("sat", now: now, calendar: cal).first?.label, "this weekend",
            "a weekday abbreviation reaches its pick")
        equal(RemindTimes.match("mon", now: now, calendar: cal).first?.label, "next week",
            "monday is next week")
    }

    static func relativeUnits() {
        equal(
            RemindTimes.match("in 2 hours", now: now, calendar: cal).first?.date,
            moment(2026, 8, 19, 16, 30), "in 2 hours")
        equal(
            RemindTimes.match("in 2 hours", now: now, calendar: cal).first?.label,
            "in 2 hours", "and it says so")
        equal(
            RemindTimes.match("20m", now: now, calendar: cal).first?.date,
            moment(2026, 8, 19, 14, 50), "bare '20m' is twenty minutes")
        equal(
            RemindTimes.match("in 3 d", now: now, calendar: cal).first?.date,
            moment(2026, 8, 22, 14, 30), "abbreviated days")
        equal(
            RemindTimes.match("2w", now: now, calendar: cal).first?.date,
            moment(2026, 9, 2, 14, 30), "weeks")
        // "mo" and "m" share a letter; the longer one has to win.
        equal(
            RemindTimes.match("2mo", now: now, calendar: cal).first?.date,
            moment(2026, 10, 19, 14, 30), "'2mo' is two months, not two minutes")
        equal(
            RemindTimes.match("in 1 hour", now: now, calendar: cal).first?.label,
            "in 1 hour", "singular stays singular")
    }

    static func relativeRejectsNonsense() {
        equal(RemindTimes.match("0 hours", now: now, calendar: cal).isEmpty, true, "zero is not a wait")
        equal(
            RemindTimes.match("in 5 fortnights", now: now, calendar: cal).contains {
                $0.label.hasPrefix("in 5")
            }, false, "an unknown unit is not guessed at")
    }

    /// A date with no time in it is not midnight. The detector fills the
    /// unstated time in with the start of the day; 09:00 is what was meant.
    ///
    /// Runs on `Calendar.current` on BOTH sides on purpose: NSDataDetector
    /// resolves against the system time zone, so injecting UTC here would
    /// assert that two different clocks agree.
    static func bareDateSnapsToNine() {
        let system = Calendar.current
        let hits = RemindTimes.match("december 25 2099", now: Date(), calendar: system)
        guard let far = hits.first(where: { system.component(.year, from: $0.date) == 2099 }) else {
            equal(false, true, "the detector found a date in 2099")
            return
        }
        let parts = system.dateComponents([.month, .day, .hour, .minute], from: far.date)
        equal(parts.month, 12, "december")
        equal(parts.day, 25, "the 25th")
        equal(parts.hour, 9, "snapped to 09:00")
        equal(parts.minute, 0, "on the hour")

        // A stated time is honoured rather than snapped.
        let timed = RemindTimes.match("december 25 2099 at 3pm", now: Date(), calendar: system)
        if let h = timed.first(where: { system.component(.year, from: $0.date) == 2099 }) {
            equal(system.component(.hour, from: h.date), 15, "a stated time survives")
        } else {
            equal(false, true, "the detector found the timed date")
        }
    }

    /// Every row states when it fires. This is the promise the palette makes.
    static func detailNamesTheAbsoluteTime() {
        equal(hit("", "in an hour")?.detail, "today 3:30 PM", "same day says today")
        equal(hit("", "tomorrow morning")?.detail, "tomorrow 9:00 AM", "the next day says tomorrow")
        // Inside the week: weekday AND date, because a weekday alone repeats.
        equal(hit("", "this weekend")?.detail.contains("Sat"), true, "weekday inside the week")
        equal(hit("", "this weekend")?.detail.contains("Aug 22"), true, "with the date on it")
        equal(hit("", "next month")?.detail, "Sep 19, 9:00 AM", "further out is a date")
        for h in RemindTimes.match("", now: now, calendar: cal) {
            equal(h.detail.isEmpty, false, "\(h.label) states its time")
        }
    }

    /// Two sources naming the same minute are one row.
    static func duplicatesCollapse() {
        // "in a week" (+7d 09:00) and "in 7 days" resolve within a day of each
        // other but not the same minute, so both stand; the real collision is a
        // pick against itself through two aliases.
        let hits = RemindTimes.match("", now: now, calendar: cal)
        let minutes = hits.map { Int($0.date.timeIntervalSince1970 / 60) }
        equal(Set(minutes).count, minutes.count, "no two rows fire in the same minute")
    }

    // MARK: - assert

    static func equal<T: Equatable>(_ got: T?, _ want: T?, _ what: String) {
        checks += 1
        if got != want {
            failures += 1
            print("  FAIL \(what): got \(String(describing: got)), want \(String(describing: want))")
        }
    }
}
