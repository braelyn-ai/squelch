// The usage page's arithmetic, pinned. Every number a tile or a tooltip shows
// comes out of UsageSeries, and every way it can be wrong is invisible on the
// page: a day's mail one column over, a delta against the wrong window, a
// share line that climbs out of nothing because the week before was not
// consulted. So the shapes are asserted here, with a fixed calendar.

import Foundation

@main
@MainActor
struct UsageSeriesTests {
    static var failures = 0
    static var checks = 0

    /// A fixed zone so the day keys land where the test says, on any machine.
    static let cal: Calendar = {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "America/Los_Angeles")!
        return c
    }()

    static func main() {
        windowsAreConsecutiveAndEndOnUntil()
        sparseRowsLandOnTheirDaysAndTheRestAreZero()
        totalsAndDeltasReadAgainstThePriorWindow()
        rollingShareIsVolumeWeightedAndLeadsInFromThePriorWindow()
        spendFoldsToStagesInFixedOrderAndPricesUnpricedRows()
        categoriesSumOnlyTheWindowAndReadInPipelineOrder()
        lookupsSnapAnyInstantToItsDay()
        textReadsTheSameEverywhere()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - fixtures

    static func mail(_ day: String, received: Int = 0, sent: Int = 0, sealed: Int = 0,
        pastDue: Int = 0, deadline: Int = 0, signal: Int = 0, noise: Int = 0) -> MailActivityDay
    {
        MailActivityDay(
            day: day, received: received, sent: sent, sealed: sealed, past_due: pastDue,
            deadline: deadline, signal: signal, noise: noise)
    }

    static func activity(until: String, days: Int, _ rows: [MailActivityDay]) -> MailActivityResponse {
        MailActivityResponse(days: days, since: "", until: until, rows: rows)
    }

    static func row(_ day: String, calls: Int = 1, input: Int, output: Int, cost: Double? = nil) -> UsageRow {
        UsageRow(day: day, calls: calls, input_tokens: input, output_tokens: output, est_cost_usd: cost)
    }

    static func category(_ rows: [UsageRow], model: String = "m", cost: Double? = nil) -> UsageCategory {
        let input = rows.reduce(0) { $0 + $1.input_tokens }
        let output = rows.reduce(0) { $0 + $1.output_tokens }
        return UsageCategory(
            rows: rows,
            totals: UsageTotals(
                calls: rows.reduce(0) { $0 + $1.calls }, input_tokens: input, output_tokens: output,
                est_cost_usd: cost ?? rows.reduce(0) { $0 + ($1.est_cost_usd ?? 0) }),
            model: model, provider: nil)
    }

    static func usage(_ categories: [String: UsageCategory]) -> UsageResponse {
        let empty = UsageTotals(calls: 0, input_tokens: 0, output_tokens: 0, est_cost_usd: 0)
        return UsageResponse(rows: [], totals: empty, provider: nil, model: "m", categories: categories)
    }

    static let emptyUsage = usage([:])

    static func series(days: Int, until: String, mail rows: [MailActivityDay], usage u: UsageResponse = emptyUsage) -> UsageSeries? {
        UsageSeries(usage: u, mail: activity(until: until, days: days, rows), days: days, calendar: cal)
    }

    // MARK: - cases

    static func windowsAreConsecutiveAndEndOnUntil() {
        let keys = DayKey.window(until: "2026-09-02", days: 7, calendar: cal)
        expect(
            keys == ["2026-08-27", "2026-08-28", "2026-08-29", "2026-08-30", "2026-08-31", "2026-09-01", "2026-09-02"],
            "seven keys, oldest first, ending on until: \(keys ?? [])")
        // Across a month end and a leap day.
        expect(DayKey.window(until: "2028-03-01", days: 2, calendar: cal) == ["2028-02-29", "2028-03-01"], "leap day")
        expect(DayKey.window(until: "not a day", days: 3, calendar: cal) == nil, "an unparseable until is nil")
        expect(DayKey.window(until: "2026-09-02", days: 0, calendar: cal) == nil, "zero days is nil")
        expect(series(days: 0, until: "2026-09-02", mail: []) == nil, "a zero-day series does not build")
        expect(series(days: 3, until: "garbage", mail: []) == nil, "a series off a bad until does not build")

        guard let s = series(days: 3, until: "2026-09-02", mail: []) else { return expect(false, "builds") }
        expect(s.mail.map(\.key) == ["2026-08-31", "2026-09-01", "2026-09-02"], "current window")
        expect(s.priorMail.map(\.key) == ["2026-08-28", "2026-08-29", "2026-08-30"], "prior window is the 3 days before")
        expect(s.spend.map(\.key) == s.mail.map(\.key), "spend and mail share one calendar")
        expect(s.xDomain.lowerBound == DayKey.date("2026-08-31", calendar: cal), "domain starts at the first day")
        expect(s.xDomain.upperBound == DayKey.date("2026-09-03", calendar: cal), "and ends at the last day's end")
    }

    static func sparseRowsLandOnTheirDaysAndTheRestAreZero() {
        let rows = [
            mail("2026-09-01", received: 5, sent: 1, sealed: 1, signal: 2, noise: 2),
            // Outside the window on both sides: dropped, not shifted.
            mail("2026-08-20", received: 99),
            mail("2026-09-09", received: 99),
        ]
        guard let s = series(days: 3, until: "2026-09-02", mail: rows) else { return expect(false, "builds") }
        expect(s.mail.map(\.received) == [0, 5, 0], "the one row lands on Sept 1: \(s.mail.map(\.received))")
        expect(s.mail[1].sealed == 1 && s.mail[1].signal == 2 && s.mail[1].sent == 1, "every field carried")
        expect(s.received == 5 && s.priorReceived == 0, "future and stale rows count nowhere")
        expect(s.mail[1].triaged == 4 && s.mail[1].attention == 2, "sealed is outside the ratio; signal is its numerator")
        expect(s.mail[1].signalShare == 0.5, "share is attention over triaged")
        expect(s.mail[0].signalShare == nil, "a day with no triaged mail has no share, not 0%")
    }

    static func totalsAndDeltasReadAgainstThePriorWindow() {
        let rows = [
            // Prior window (Aug 28-30): 20 in, 4 out, 5 of 20 signal.
            mail("2026-08-28", received: 10, sent: 2, signal: 3, noise: 7),
            mail("2026-08-30", received: 10, sent: 2, signal: 2, noise: 8),
            // Current window (Aug 31-Sep 2): 25 in, 3 out, 10 of 25 signal.
            mail("2026-08-31", received: 12, sent: 1, pastDue: 1, signal: 4, noise: 7),
            mail("2026-09-02", received: 13, sent: 2, deadline: 2, signal: 3, noise: 8),
        ]
        guard let s = series(days: 3, until: "2026-09-02", mail: rows) else { return expect(false, "builds") }
        expect(s.received == 25 && s.sent == 3, "current totals")
        expect(s.priorReceived == 20 && s.priorSent == 4, "prior totals")
        expect(s.receivedDelta == .ratio(0.25), "received +25%: \(String(describing: s.receivedDelta))")
        expect(s.sentDelta == .ratio(-0.25), "sent -25%")
        expect(s.signalShare == 0.4, "10 of 25")
        expect(s.priorSignalShare == 0.25, "5 of 20")
        if case .points(let p)? = s.signalShareDelta {
            expect(abs(p - 0.15) < 1e-9, "share moved +15 points: \(p)")
        } else {
            expect(false, "share delta is in points")
        }
        expect(s.spendDelta == nil, "no spend either side: no delta, not a division by zero")

        // A window after an empty one is new, not infinitely bigger.
        guard let fresh = series(days: 3, until: "2026-09-02", mail: [mail("2026-09-01", received: 3, noise: 3)])
        else { return expect(false, "builds") }
        expect(fresh.receivedDelta == nil, "no prior received: no delta")
        expect(fresh.signalShareDelta == nil, "no prior share: no delta")
        expect(fresh.signalShare == 0, "all noise reads as 0%, which is an answer")
    }

    static func rollingShareIsVolumeWeightedAndLeadsInFromThePriorWindow() {
        // Prior week: 60 of 70 signal, spread so a quiet day cannot dominate.
        var rows: [MailActivityDay] = []
        for d in 23...29 {
            rows.append(mail("2026-08-\(d)", received: 10, signal: 9, noise: 1))
        }
        // Current 7 days: one loud noisy day, one quiet 1-of-2 day, five empties.
        rows.append(mail("2026-08-30", received: 70, noise: 70))
        rows.append(mail("2026-09-02", received: 2, signal: 1, noise: 1))
        guard let s = series(days: 7, until: "2026-09-02", mail: rows) else { return expect(false, "builds") }

        expect(s.rollingShare.count == 7, "one point per day, the lead-in makes the first complete: \(s.rollingShare.count)")
        // Aug 30 trails Aug 24-30: 54 signal of 130 (60 of the prior six days
        // at 9/10 = 54, plus 70 noise).
        let aug30 = s.rollingShare(at: DayKey.date("2026-08-30", calendar: cal))
        expect(aug30.map { abs($0 - 54.0 / 130.0) < 1e-9 } == true, "Aug 30 is weighted by the 70-noise day: \(String(describing: aug30))")
        // Sep 2 trails Aug 27-Sep 2: 27 + 1 signal of 30 + 70 + 2.
        let sep2 = s.rollingShare(at: DayKey.date("2026-09-02", calendar: cal))
        expect(sep2.map { abs($0 - 28.0 / 102.0) < 1e-9 } == true, "Sep 2 sums volume, it does not average daily shares: \(String(describing: sep2))")
        expect(s.rollingShare(at: DayKey.date("2026-09-09", calendar: cal)) == nil, "outside the window is nil")

        // Nothing at all in the trailing week: no point, not a zero.
        guard let quiet = series(days: 3, until: "2026-09-02", mail: [mail("2026-08-20", received: 5, noise: 5)])
        else { return expect(false, "builds") }
        expect(quiet.rollingShare.isEmpty, "a silent fortnight draws no line")
    }

    static func spendFoldsToStagesInFixedOrderAndPricesUnpricedRows() {
        let u = usage([
            // Arrives out of pipeline order on purpose.
            "extract_shipments": category([row("2026-09-01", input: 1000, output: 100, cost: 0.01)]),
            "stage2": category([row("2026-09-01", input: 2000, output: 200, cost: 0.05), row("2026-08-29", input: 500, output: 50, cost: 0.02)]),
            "extract_banking": category([row("2026-09-02", input: 1000, output: 100, cost: 0.02)]),
            // An OLD daemon: no per-row cost, only the window total.
            "stage1": category(
                [row("2026-09-01", input: 3000, output: 0), row("2026-09-02", input: 1000, output: 0)],
                cost: 0.004),
            // An extractor the app never heard of, present in the ledger.
            "extract_fictional": category([row("2026-09-02", input: 10, output: 0, cost: 0.001)]),
            // Passes that are neither a stage nor an extractor.
            "revisit": category([row("2026-09-02", input: 10, output: 0, cost: 0.002)]),
            "notify": category([row("2026-09-01", input: 10, output: 0, cost: 0.003)]),
        ])
        guard let s = series(days: 3, until: "2026-09-02", mail: [], usage: u) else { return expect(false, "builds") }

        expect(s.stages == [.stage1, .stage2, .extractors, .other], "chart order regardless of arrival: \(s.stages)")
        let sep1 = s.spendDay(at: DayKey.date("2026-09-01", calendar: cal))
        expect(sep1?.cost["extractors"].map { abs($0 - 0.01) < 1e-12 } == true, "shipments folds into extractors")
        expect(sep1?.cost["stage2"].map { abs($0 - 0.05) < 1e-12 } == true, "stage 2 priced from its row")
        // 3000 of 4000 stage-1 tokens fell on Sep 1: 3/4 of $0.004.
        expect(sep1?.cost["stage1"].map { abs($0 - 0.003) < 1e-12 } == true, "an unpriced row is pro-rated by tokens: \(String(describing: sep1?.cost["stage1"]))")
        expect(sep1?.cost["other"].map { abs($0 - 0.003) < 1e-12 } == true, "the notify lane is other, not an extractor")
        expect(sep1?.calls == 4 && sep1?.tokens == 6310, "calls and tokens sum across categories: \(String(describing: sep1?.tokens))")
        let sep2 = s.spendDay(at: DayKey.date("2026-09-02", calendar: cal))
        expect(sep2?.cost["extractors"].map { abs($0 - 0.021) < 1e-12 } == true, "banking and the unknown extractor fold together")
        expect(sep2?.cost["other"].map { abs($0 - 0.002) < 1e-12 } == true, "the revisit pass is other")
        expect(abs(s.spendTotal - (0.01 + 0.05 + 0.02 + 0.004 + 0.001 + 0.002 + 0.003)) < 1e-12, "window total: \(s.spendTotal)")
        expect(abs(s.priorSpendTotal - 0.02) < 1e-12, "Aug 29 is prior spend")
        expect(s.spendDelta.map { if case .ratio(let r) = $0 { return abs(r - (0.09 - 0.02) / 0.02) < 1e-9 }; return false } == true, "spend delta is a ratio against prior")

        // Legacy flat shape: one Stage-2 series.
        let legacy = UsageResponse(
            rows: [row("2026-09-02", input: 100, output: 10, cost: 0.5)],
            totals: UsageTotals(calls: 1, input_tokens: 100, output_tokens: 10, est_cost_usd: 0.5),
            provider: nil, model: "old", categories: nil)
        guard let l = series(days: 3, until: "2026-09-02", mail: [], usage: legacy) else { return expect(false, "builds") }
        expect(l.stages == [.stage2] && abs(l.spendTotal - 0.5) < 1e-12, "the flat shape is stage 2")
        expect(l.categories.map(\.id) == ["stage2"] && l.categories[0].model == "old", "and one category")
    }

    static func categoriesSumOnlyTheWindowAndReadInPipelineOrder() {
        let u = usage([
            "extract_shipments": category([row("2026-09-02", input: 10, output: 1, cost: 0.1)], model: "small"),
            "stage2": category([row("2026-09-02", calls: 2, input: 100, output: 10, cost: 1), row("2026-08-01", calls: 9, input: 900, output: 90, cost: 9)], model: "big"),
            "extract_banking": category([row("2026-08-29", input: 10, output: 1, cost: 0.1)], model: "small"),
            "stage1": category([], model: "small"),
            "revisit": category([], model: "small"),
            "notify": category([], model: "tiny"),
        ])
        guard let s = series(days: 3, until: "2026-09-02", mail: [], usage: u) else { return expect(false, "builds") }
        expect(s.categories.map(\.id) == ["stage1", "stage2", "extract_banking", "extract_shipments", "notify", "revisit"], "stages, extractors, then the rest, each by name: \(s.categories.map(\.id))")
        expect(s.categories.map(\.label) == ["Stage 1", "Stage 2", "Extract banking", "Extract shipments", "Notify lane", "Revisit"], "labels: \(s.categories.map(\.label))")
        let stage2 = s.categories[1]
        expect(stage2.calls == 2 && stage2.tokens == 110 && abs(stage2.cost - 1) < 1e-12, "the Aug 1 row is outside the window and does not count in the table")
        let banking = s.categories[2]
        expect(banking.calls == 0 && banking.cost == 0, "a category that spent only in the prior window shows zero, not absence")
        expect(s.categories[0].model == "small" && s.categories[1].model == "big", "models ride along")
        expect(UsageSeries.categoryLabel("extract_") == "extract_" && UsageSeries.categoryLabel("weird_pass") == "Weird pass", "unknown ids fall through as themselves, capitalised")
    }

    static func lookupsSnapAnyInstantToItsDay() {
        guard let s = series(days: 3, until: "2026-09-02", mail: [mail("2026-09-01", received: 4, noise: 4)]) else { return expect(false, "builds") }
        let noon = DayKey.date("2026-09-01", calendar: cal)!.addingTimeInterval(12 * 3600)
        expect(s.mailDay(at: noon)?.received == 4, "noon resolves to its day")
        let lateNight = DayKey.date("2026-09-02", calendar: cal)!.addingTimeInterval(-1)
        expect(s.mailDay(at: lateNight)?.received == 4, "the last second of the day too")
        expect(s.mailDay(at: DayKey.date("2026-09-02", calendar: cal))?.received == 0, "midnight is the next day, zero-filled")
        expect(s.mailDay(at: nil) == nil, "no selection, no day")
        expect(s.mailDay(at: DayKey.date("2026-08-01", calendar: cal)) == nil, "outside the window")
    }

    static func textReadsTheSameEverywhere() {
        expect(UsageText.count(0) == "0", "zero")
        expect(UsageText.count(999) == "999", "under a thousand")
        expect(UsageText.count(1000) == "1,000", "a thousand")
        expect(UsageText.count(1234567) == "1,234,567", "millions")
        expect(UsageText.count(-1234) == "-1,234", "negative")
        expect(UsageText.percent(0.4) == "40%" && UsageText.percent(0.005) == "1%" && UsageText.percent(1) == "100%", "percent rounds")
        expect(UsageText.delta(.ratio(0.25)) == "+25%" && UsageText.delta(.ratio(-0.084)) == "-8%" && UsageText.delta(.ratio(0)) == "0%", "ratio text")
        expect(UsageText.delta(.points(0.031)) == "+3.1 pts" && UsageText.delta(.points(-0.004)) == "-0.4 pts", "points text")
        expect(UsageSeries.Delta.ratio(0.004).isFlat && !UsageSeries.Delta.ratio(0.02).isFlat, "flat is under half a percent")
        expect(UsageSeries.Delta.points(0.2).isUp && !UsageSeries.Delta.points(-0.2).isUp, "direction")
    }

    static func expect(_ cond: Bool, _ what: String) {
        checks += 1
        if !cond {
            failures += 1
            print("  FAIL: \(what)")
        }
    }
}
