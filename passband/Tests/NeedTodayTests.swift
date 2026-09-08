// THE NUMBER ON THE APP ICON, pinned at its edges.
//
// `NeedToday.count` answers one question — how much is overdue or due by the end
// of today — and three surfaces spend that answer: the sitrep's headline says it
// in words, its masthead says it as a figure, and the badge says it to somebody
// who is looking at neither. The badge is why this is worth a suite. A sentence
// on a dashboard is read WITH the list under it, so an off-by-one is visible and
// self-correcting; a red pill on a home screen carries no such context, and the
// only thing that can catch it being wrong is here.
//
// Every case below is a boundary, because the middle of the range was never in
// doubt. What is in doubt: whether "today" means the end of today or the moment
// of asking, whether an overdue thing still counts, and whether a standing item
// with no date at all can slip into a count of things that are due.

import Foundation

@main
@MainActor
struct NeedTodayTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        overdueStillCounts()
        dueLaterTodayCounts()
        theBoundaryIsEndOfDayNotNow()
        tomorrowDoesNot()
        datelessNeverCounts()
        anUnparseableStampIsNotADeadline()
        nothingDueIsZeroAndNotNil()
        theBandDecodesStraightOffTheWire()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    /// Noon on a fixed day, in the machine's own calendar — the same calendar
    /// the function asks for the end of the day. Fixing the instant is what
    /// makes "tonight" and "tomorrow" mean something in a test that could run
    /// at any hour.
    static let now: Date = {
        var c = DateComponents()
        c.year = 2026
        c.month = 9
        c.day = 8
        c.hour = 12
        return Calendar.current.date(from: c)!
    }()

    /// An obligation due at `offset` from `now`, or with no date when nil.
    static func item(_ offset: TimeInterval?, id: Int = 1) -> AttentionUpdate {
        AttentionUpdate(
            id: id, thread_id: "t\(id)", tier: .deadline, importance: 3,
            sender: "someone@example.com", one_line: "", reason: "",
            deadline: offset.map { stamp(now.addingTimeInterval($0)) }, status: .new)
    }

    /// RFC3339 in UTC, the shape the daemon serves and `Fmt.date` parses.
    static func stamp(_ date: Date) -> String {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.timeZone = TimeZone(identifier: "UTC")
        f.dateFormat = "yyyy-MM-dd'T'HH:mm:ss'Z'"
        return f.string(from: date)
    }

    /// A bill that was due last Tuesday is the most due thing you own. The test
    /// is "by the end of today", so everything behind you satisfies it.
    static func overdueStillCounts() {
        expect(NeedToday.count([item(-6 * 24 * 3600)], now: now) == 1, "six days overdue counts")
        expect(NeedToday.count([item(-60)], now: now) == 1, "a minute overdue counts")
    }

    static func dueLaterTodayCounts() {
        expect(NeedToday.count([item(3600)], now: now) == 1, "due in an hour counts")
    }

    /// THE ONE THAT WOULD SILENTLY SHRINK THE NUMBER. Comparing against `now`
    /// rather than the end of the day would drop everything due later tonight —
    /// a badge that empties out over the afternoon while the work stays on the
    /// list, and nothing on the home screen to say so.
    static func theBoundaryIsEndOfDayNotNow() {
        // 23:58 on the same day: past `now`, inside the day.
        let tonight = 11 * 3600 + 58 * 60
        expect(NeedToday.count([item(TimeInterval(tonight))], now: now) == 1, "23:58 tonight counts")
    }

    static func tomorrowDoesNot() {
        // 00:30 the next day, comfortably past any end-of-day boundary.
        let tomorrow = 12 * 3600 + 30 * 60
        expect(
            NeedToday.count([item(TimeInterval(tomorrow))], now: now) == 0,
            "half past midnight tomorrow does not count")
    }

    /// The standing band is NOT a list of deadlines: it carries live
    /// correspondence too — a thread you have written in, a sender you have
    /// written to — and those arrive with `deadline` nil. Counting them would
    /// put a number on the icon for mail that has no clock on it, which is the
    /// difference between "you owe four things today" and "you have mail".
    static func datelessNeverCounts() {
        expect(NeedToday.count([item(nil)], now: now) == 0, "a dateless obligation does not count")
        expect(
            NeedToday.count([item(nil, id: 1), item(-3600, id: 2), item(nil, id: 3)], now: now) == 1,
            "only the dated one counts among dateless siblings")
    }

    /// A deadline the parser cannot read is not a deadline. It must not count,
    /// and it must not crash the count for the rows beside it.
    static func anUnparseableStampIsNotADeadline() {
        var bad = item(nil, id: 7)
        bad.deadline = "next Tuesday sometime"
        expect(NeedToday.count([bad], now: now) == 0, "an unparseable deadline does not count")
        expect(
            NeedToday.count([bad, item(-3600, id: 8)], now: now) == 1,
            "and does not take its neighbours with it")
    }

    /// Zero is an answer. `Badge.set(0)` clears the icon, and it has to be
    /// reachable — a clear board must be able to say so.
    static func nothingDueIsZeroAndNotNil() {
        expect(NeedToday.count([], now: now) == 0, "an empty band is zero")
    }

    /// THE SEAM THE BADGE'S BACKGROUND HALF HANGS ON, and nobody else checks it.
    ///
    /// The notification service extension refreshes the badge by reading
    /// `/client/updates?band=standing&peek=true` and counting the rows itself.
    /// It has no APIClient — that whole file is off its source list — so it
    /// decodes `Page<AttentionUpdate>` by hand, and every failure there is
    /// SILENT by design: a miss returns nil and the push goes out with the badge
    /// untouched. There is no error to see. The only thing standing between "the
    /// envelope grew a required field" and "the badge stopped updating on pushes
    /// and nobody noticed for a month" is this.
    ///
    /// Verbatim daemon output, `next_cursor` absent exactly as the daemon omits
    /// it, one dated row and one dateless one so the count is doing work.
    static func theBandDecodesStraightOffTheWire() {
        let json = """
            {"items":[\
            {"id":5,"thread_id":"t-2","tier":"past_due","importance":5,\
            "sender":"billing@heliostack.io","one_line":"Invoice 4417 is past due.",\
            "reason":"","deadline":"2026-08-26T04:54:47Z","matched_rule":null,\
            "has_attachments":false,"from_name":"Heliostack Billing","status":"new",\
            "surfaced_at":null,"resolved_at":null,"remind_at":null,"reminded_at":null},\
            {"id":9,"thread_id":"t-9","tier":"signal","importance":2,\
            "sender":"amara@fieldworklabs.com","one_line":"Asked about the deck.",\
            "reason":"","deadline":null,"matched_rule":null,"has_attachments":false,\
            "from_name":"Amara Diallo","status":"new","surfaced_at":null,\
            "resolved_at":null,"remind_at":null,"reminded_at":null}]}
            """
        guard
            let page = try? JSONDecoder().decode(
                Page<AttentionUpdate>.self, from: Data(json.utf8))
        else {
            return expect(false, "the standing band decodes with no next_cursor")
        }
        expect(page.items.count == 2, "both rows survive the decode")
        // Counted at a moment well after that deadline, so the past-due row is
        // in and the dateless one is out.
        expect(NeedToday.count(page.items, now: now) == 1, "one of the two is due")
    }

    static func expect(_ cond: Bool, _ what: String) {
        checks += 1
        if !cond {
            failures += 1
            print("  FAIL: \(what)")
        }
    }
}
