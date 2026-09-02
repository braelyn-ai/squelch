// The usage page's arithmetic: two sparse, UTC-keyed day series off the wire
// — the model-spend ledger and the mailbox's own traffic — turned into the
// dense, aligned windows the charts draw, plus the totals, the deltas against
// the window before, and the rolling signal share. Foundation only, so it tests
// with the wire types alone and no chart in sight.

import Foundation

/// A "YYYY-MM-DD" day key as the daemon writes it, and its place on a chart:
/// that calendar day at LOCAL midnight. The keys are UTC days — the usage
/// ledger's own — and the chart shows them as the dates they name. Re-anchoring
/// them to the reader's zone would shift a day's mail into the neighbouring
/// column for anyone west of Greenwich, and the day would stop matching the
/// ledger row beside it.
enum DayKey {
    static func date(_ key: String, calendar: Calendar) -> Date? {
        let parts = key.split(separator: "-")
        guard parts.count == 3, let y = Int(parts[0]), let m = Int(parts[1]), let d = Int(parts[2])
        else { return nil }
        return calendar.date(from: DateComponents(year: y, month: m, day: d))
    }

    static func key(_ date: Date, calendar: Calendar) -> String {
        let c = calendar.dateComponents([.year, .month, .day], from: date)
        return String(format: "%04d-%02d-%02d", c.year ?? 0, c.month ?? 0, c.day ?? 0)
    }

    /// `days` consecutive keys ending at `until`, oldest first; nil when `until`
    /// will not parse or the count is not positive.
    static func window(until: String, days: Int, calendar: Calendar) -> [String]? {
        guard days > 0, let end = date(until, calendar: calendar) else { return nil }
        return (0..<days).reversed().compactMap { back in
            calendar.date(byAdding: .day, value: -back, to: end).map { key($0, calendar: calendar) }
        }
    }
}

struct UsageSeries: Equatable {
    /// One calendar day of mailbox traffic, zero-filled.
    struct MailDay: Equatable, Identifiable {
        let key: String
        let date: Date
        var received = 0
        var sent = 0
        var sealed = 0
        var pastDue = 0
        var deadline = 0
        var signal = 0
        var noise = 0
        var id: String { key }

        /// Received mail a verdict exists for: the ratio's denominator. Sealed
        /// mail is structurally outside it (never triaged), and so is mail the
        /// pipeline has not reached.
        var triaged: Int { pastDue + deadline + signal + noise }
        /// The signal side of the ratio: every tier that is not noise.
        var attention: Int { pastDue + deadline + signal }
        var signalShare: Double? {
            triaged > 0 ? Double(attention) / Double(triaged) : nil
        }
    }

    /// The spend chart's series: the pipeline's stages in pipeline order, the
    /// extractors folded into one, and everything else folded into "other".
    /// The ledger is open-ended — every pass writes its own category, and a
    /// build that named them all would colour a ninth one it never heard of —
    /// so the tail is one gray series and the table below names each line.
    enum SpendStage: String, CaseIterable, Identifiable {
        case stage1, stage2, extractors, other
        var id: String { rawValue }
        var label: String {
            switch self {
            case .stage1: "Stage 1"
            case .stage2: "Stage 2"
            case .extractors: "Extractors"
            case .other: "Other"
            }
        }

        /// Which series a ledger category folds into. The two stages are
        /// themselves; every `extract_*` writer is an extractor; the rest
        /// (the revisit pass, the notify fast lane, whatever comes next) is
        /// other.
        static func of(category id: String) -> SpendStage {
            switch id {
            case "stage1": .stage1
            case "stage2": .stage2
            default: id.hasPrefix("extract_") ? .extractors : .other
            }
        }
    }

    /// One calendar day of model spend, folded to stages, zero-filled.
    struct SpendDay: Equatable, Identifiable {
        let key: String
        let date: Date
        var calls = 0
        var inputTokens = 0
        var outputTokens = 0
        /// Dollars by `SpendStage.id`.
        var cost: [String: Double] = [:]
        var id: String { key }
        var tokens: Int { inputTokens + outputTokens }
        var total: Double { cost.values.reduce(0, +) }
    }

    /// One ledger category over the window, for the table under the chart.
    struct CategorySummary: Equatable, Identifiable {
        let id: String
        let label: String
        let model: String
        let provider: String?
        let stage: SpendStage
        var calls = 0
        var tokens = 0
        var cost = 0.0
    }

    /// A point on the rolling signal-share line.
    struct SharePoint: Equatable, Identifiable {
        let date: Date
        let share: Double
        var id: Date { date }
    }

    /// The change from the prior window, shaped by what changed.
    enum Delta: Equatable {
        /// A count or an amount: the fractional change, 0.12 = +12%.
        case ratio(Double)
        /// A share: the change in percentage points, 0.031 = +3.1 pts.
        case points(Double)

        var isUp: Bool {
            switch self {
            case .ratio(let r): r > 0
            case .points(let p): p > 0
            }
        }
        var isFlat: Bool {
            switch self {
            case .ratio(let r): abs(r) < 0.005
            case .points(let p): abs(p) < 0.0005
            }
        }
    }

    /// The trailing window the share line smooths over. Seven, so a quiet
    /// Sunday with two emails does not read as a 50% signal day.
    static let rollingDays = 7

    let days: Int
    let calendar: Calendar
    let mail: [MailDay]
    let priorMail: [MailDay]
    let spend: [SpendDay]
    let priorSpend: [SpendDay]
    /// The stages with any spend in either window, in pipeline order.
    let stages: [SpendStage]
    /// Every ledger category: stages, then extractors, then the rest, each
    /// group by name.
    let categories: [CategorySummary]
    /// Volume-weighted trailing share, one point per day of the window that
    /// had any triaged mail in its trailing week — the prior window leads in,
    /// so the line starts complete instead of climbing out of nothing.
    let rollingShare: [SharePoint]

    private let mailByDate: [Date: MailDay]
    private let spendByDate: [Date: SpendDay]

    /// The window is `days` days ending on the mail report's `until` — the
    /// daemon's today — and the prior window is the same length before it.
    /// Rows outside either are dropped, which is where a future-dated message
    /// (the `Date:` header is untrusted) goes.
    init?(usage: UsageResponse, mail: MailActivityResponse, days: Int, calendar: Calendar = .current) {
        guard days > 0,
            let keys = DayKey.window(until: mail.until, days: days * 2, calendar: calendar),
            keys.count == days * 2
        else { return nil }
        self.days = days
        self.calendar = calendar

        // ---- mail --------------------------------------------------------
        let mailRows = Dictionary(mail.rows.map { ($0.day, $0) }, uniquingKeysWith: { _, b in b })
        var mailDays: [MailDay] = []
        for key in keys {
            guard let date = DayKey.date(key, calendar: calendar) else { continue }
            var day = MailDay(key: key, date: date)
            if let r = mailRows[key] {
                day.received = r.received
                day.sent = r.sent
                day.sealed = r.sealed
                day.pastDue = r.past_due
                day.deadline = r.deadline
                day.signal = r.signal
                day.noise = r.noise
            }
            mailDays.append(day)
        }
        priorMail = Array(mailDays.prefix(days))
        self.mail = Array(mailDays.suffix(days))
        mailByDate = Dictionary(self.mail.map { ($0.date, $0) }, uniquingKeysWith: { _, b in b })

        var rolling: [SharePoint] = []
        for i in days..<mailDays.count {
            let window = mailDays[max(0, i - Self.rollingDays + 1)...i]
            let triaged = window.reduce(0) { $0 + $1.triaged }
            guard triaged > 0 else { continue }
            let attention = window.reduce(0) { $0 + $1.attention }
            rolling.append(SharePoint(date: mailDays[i].date, share: Double(attention) / Double(triaged)))
        }
        rollingShare = rolling

        // ---- spend -------------------------------------------------------
        // The per-stage breakdown when the daemon sends one, else the flat
        // legacy shape as a single Stage-2 category.
        var wire: [(id: String, category: UsageCategory)] = []
        if let map = usage.categories, !map.isEmpty {
            wire = map.keys.sorted().map { ($0, map[$0]!) }
        } else {
            wire = [
                (
                    "stage2",
                    UsageCategory(
                        rows: usage.rows, totals: usage.totals, model: usage.model,
                        provider: usage.provider)
                )
            ]
        }

        var spendDays: [String: SpendDay] = [:]
        for key in keys {
            guard let date = DayKey.date(key, calendar: calendar) else { continue }
            spendDays[key] = SpendDay(key: key, date: date)
        }
        var summaries: [String: CategorySummary] = [:]
        let currentKeys = Set(keys.suffix(days))
        for (id, category) in wire {
            let stage = SpendStage.of(category: id)
            var summary = CategorySummary(
                id: id, label: Self.categoryLabel(id), model: category.model,
                provider: category.provider, stage: stage)
            let totalTokens = category.totals.input_tokens + category.totals.output_tokens
            for row in category.rows {
                guard var day = spendDays[row.day] else { continue }
                let tokens = row.input_tokens + row.output_tokens
                // A daemon older than per-day pricing sends no row cost: share
                // the window total out by tokens, which is the only weight the
                // row carries.
                let cost =
                    row.est_cost_usd
                    ?? (totalTokens > 0
                        ? category.totals.est_cost_usd * Double(tokens) / Double(totalTokens) : 0)
                day.calls += row.calls
                day.inputTokens += row.input_tokens
                day.outputTokens += row.output_tokens
                day.cost[stage.id, default: 0] += cost
                spendDays[row.day] = day
                if currentKeys.contains(row.day) {
                    summary.calls += row.calls
                    summary.tokens += tokens
                    summary.cost += cost
                }
            }
            summaries[id] = summary
        }
        let ordered = keys.compactMap { spendDays[$0] }
        priorSpend = Array(ordered.prefix(days))
        spend = Array(ordered.suffix(days))
        spendByDate = Dictionary(spend.map { ($0.date, $0) }, uniquingKeysWith: { _, b in b })
        stages = SpendStage.allCases.filter { stage in
            ordered.contains { ($0.cost[stage.id] ?? 0) > 0 }
        }
        categories = summaries.values.sorted { a, b in
            // Series in chart order, names within one, so the table reads
            // top-down the way the pipeline runs.
            let ai = SpendStage.allCases.firstIndex(of: a.stage) ?? 0
            let bi = SpendStage.allCases.firstIndex(of: b.stage) ?? 0
            return ai != bi ? ai < bi : a.id < b.id
        }
    }

    // MARK: - lookups

    /// The chart's x range: the first day's start through the last day's end.
    var xDomain: ClosedRange<Date> {
        let first = mail.first?.date ?? Date()
        let last = mail.last?.date ?? first
        let end = calendar.date(byAdding: .day, value: 1, to: last) ?? last
        return first...end
    }

    /// The day under a chart selection — any instant inside it.
    func mailDay(at date: Date?) -> MailDay? {
        guard let date else { return nil }
        return mailByDate[calendar.startOfDay(for: date)]
    }

    func spendDay(at date: Date?) -> SpendDay? {
        guard let date else { return nil }
        return spendByDate[calendar.startOfDay(for: date)]
    }

    /// The trailing-share point for a day, if its week had any triaged mail.
    func rollingShare(at date: Date?) -> Double? {
        guard let date else { return nil }
        let day = calendar.startOfDay(for: date)
        return rollingShare.first { $0.date == day }?.share
    }

    // MARK: - totals

    var received: Int { mail.reduce(0) { $0 + $1.received } }
    var sent: Int { mail.reduce(0) { $0 + $1.sent } }
    var sealed: Int { mail.reduce(0) { $0 + $1.sealed } }
    var triaged: Int { mail.reduce(0) { $0 + $1.triaged } }
    var attention: Int { mail.reduce(0) { $0 + $1.attention } }
    var signalShare: Double? { Self.share(attention, of: triaged) }
    var spendTotal: Double { spend.reduce(0) { $0 + $1.total } }
    var calls: Int { spend.reduce(0) { $0 + $1.calls } }
    var tokens: Int { spend.reduce(0) { $0 + $1.tokens } }
    /// What the models cost per email that arrived; nil with nothing received.
    var costPerEmail: Double? { received > 0 ? spendTotal / Double(received) : nil }

    var priorReceived: Int { priorMail.reduce(0) { $0 + $1.received } }
    var priorSent: Int { priorMail.reduce(0) { $0 + $1.sent } }
    var priorSignalShare: Double? {
        Self.share(
            priorMail.reduce(0) { $0 + $1.attention }, of: priorMail.reduce(0) { $0 + $1.triaged })
    }
    var priorSpendTotal: Double { priorSpend.reduce(0) { $0 + $1.total } }

    var receivedDelta: Delta? { Self.ratio(Double(received), over: Double(priorReceived)) }
    var sentDelta: Delta? { Self.ratio(Double(sent), over: Double(priorSent)) }
    var signalShareDelta: Delta? {
        guard let now = signalShare, let before = priorSignalShare else { return nil }
        return .points(now - before)
    }
    var spendDelta: Delta? { Self.ratio(spendTotal, over: priorSpendTotal) }

    private static func share(_ part: Int, of whole: Int) -> Double? {
        whole > 0 ? Double(part) / Double(whole) : nil
    }

    /// Fractional change, or nil when there is nothing to change from: a
    /// window that follows an empty one is new, not infinitely bigger.
    private static func ratio(_ current: Double, over prior: Double) -> Delta? {
        prior > 0 ? .ratio((current - prior) / prior) : nil
    }

    // MARK: - labels

    /// The server enumerates the ledger, so ids arrive that this build has never
    /// heard of. Known ids get a written label; an `extract_*` id is titled from
    /// its own suffix; anything else is shown as itself, capitalised, rather
    /// than hidden.
    static func categoryLabel(_ id: String) -> String {
        switch id {
        case "stage1": return "Stage 1"
        case "stage2": return "Stage 2"
        case "notify": return "Notify lane"
        default: break
        }
        let plain = id.replacingOccurrences(of: "_", with: " ")
        guard id.hasPrefix("extract_") else { return plain.prefix(1).uppercased() + plain.dropFirst() }
        let subject = id.dropFirst("extract_".count).replacingOccurrences(of: "_", with: " ")
        return subject.isEmpty ? id : "Extract \(subject)"
    }
}

/// The page's number formatting, hand-rolled so a test reads the same on every
/// locale and the tiles read the same as the tooltips.
enum UsageText {
    /// "1,234".
    static func count(_ n: Int) -> String {
        let digits = Array(String(abs(n)))
        var out: [Character] = []
        for (i, ch) in digits.reversed().enumerated() {
            if i > 0, i % 3 == 0 { out.append(",") }
            out.append(ch)
        }
        return (n < 0 ? "-" : "") + String(out.reversed())
    }

    /// "68%".
    static func percent(_ share: Double) -> String {
        "\(Int((share * 100).rounded()))%"
    }

    /// "+12%" / "-8%" for a ratio, "+3.1 pts" / "-0.4 pts" for a share.
    static func delta(_ d: UsageSeries.Delta) -> String {
        switch d {
        case .ratio(let r):
            let pct = Int((r * 100).rounded())
            return (pct > 0 ? "+" : "") + "\(pct)%"
        case .points(let p):
            let pts = p * 100
            return String(format: "%@%.1f pts", pts > 0 ? "+" : "", pts)
        }
    }
}
