// USAGE — what the mailbox did and what the models spent reading it, over one
// window of days. GET /client/mail-activity carries the mail, GET /client/usage
// the spend; both are fetched for TWICE the window so every tile can say how
// this window compares with the one before it. The assistant tally is the one
// client-side number here: a BYOK ask goes straight from this machine to the
// reader's own provider key, so the daemon never sees it.
//
// One crosshair for every plot. Hovering a day in any chart marks that day in
// all of them, so a spike in spend reads against the mail that caused it
// without the pointer moving. Each chart also has a table twin behind the
// "table" action: the plot is the fast read, the table is the exact one.

import Charts
import SwiftUI

/// Both halves of the page, fetched together so they land in one frame and
/// cover one window.
struct UsagePage: Sendable, Equatable {
    var usage: UsageResponse
    var mail: MailActivityResponse
}

struct UsageView: View {
    @Environment(AppStore.self) private var store
    @State private var page: Loadable<UsagePage> = .loading
    @State private var series: UsageSeries?
    @State private var days = 30
    @State private var assistant = AssistantUsage()
    /// The shared crosshair: an instant inside the hovered day, or nothing.
    @State private var scrub: Date?

    private static let windows: [(value: Int, label: String)] = [
        (7, "7 days"), (30, "30 days"), (90, "90 days"),
    ]

    var body: some View {
        VStack(spacing: 0) {
            RoutedHeader(title: "Usage") {
                GlassSegmented(options: Self.windows, selection: $days)
            }
            ScrollView {
                content
                    .padding(.horizontal, 24)
                    .padding(.vertical, 18)
                    .frame(maxWidth: 980, alignment: .leading)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        // The window is the one filter, and it scopes everything below it.
        .task(id: days) { await load() }
        // Esc leaves — a full routed page needs an exit other than the rail.
        .keyContext(.modal)
        .keyBindings(
            .modal,
            [
                KeyBinding("Escape", "back to sitrep") { store.setView(.sitrep) }
            ])
    }

    private func load() async {
        let window = days
        await $page.load("failed to load usage") {
            async let usage = APIClient.shared.getUsage(days: window * 2)
            async let mail = APIClient.shared.getMailActivity(days: window * 2)
            return try await UsagePage(usage: usage, mail: mail)
        }
        if let loaded = page.value {
            series = UsageSeries(usage: loaded.usage, mail: loaded.mail, days: window)
        }
        // Local ledger, read after the server halves land so every section
        // appears in the same frame.
        assistant = AssistantUsageLedger.read()
    }

    // MARK: - layout

    @ViewBuilder private var content: some View {
        if let series {
            VStack(alignment: .leading, spacing: 14) {
                if !page.isLoading, let error = page.error {
                    Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
                }
                masthead
                tiles(series)
                mailCard(series)
                signalCard(series)
                spendCard(series)
                HStack(alignment: .top, spacing: 14) {
                    categoryCard(series)
                    assistantCard.frame(width: 280)
                }
            }
            // A refetch keeps the frame: the last render dims rather than
            // dropping to a spinner, so nothing jumps when the window changes.
            .opacity(page.isLoading ? 0.55 : 1)
            .animation(.smooth(duration: 0.25), value: page.isLoading)
        } else if let error = page.error {
            SectionCard(label: "Usage") {
                Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
            }
        } else {
            SectionCard(label: "Usage") { EmptyNote("loading…") }
        }
    }

    /// The page's one line of voice. Nothing here is load-bearing, and the
    /// masthead says so.
    private var masthead: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("Fun charts")
                .font(.system(size: 20, weight: .semibold))
                .foregroundStyle(Palette.ink)
            Text("We didn't need to build this but graphs are fun ¯\\_(ツ)_/¯")
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkFaint)
        }
        .padding(.bottom, 2)
    }

    // MARK: - tiles

    private func tiles(_ s: UsageSeries) -> some View {
        let versus = "vs prior \(s.days) days"
        return HStack(alignment: .top, spacing: 12) {
            StatTile(
                label: "Received", value: UsageText.count(s.received),
                delta: deltaLabel(s.receivedDelta, upIsGood: nil), versus: versus,
                spark: s.mail.map { Double($0.received) }, tint: Palette.chartIn,
                sub: s.sealed > 0 ? "\(UsageText.count(s.sealed)) sealed" : "none sealed")
            StatTile(
                label: "Sent", value: UsageText.count(s.sent),
                delta: deltaLabel(s.sentDelta, upIsGood: nil), versus: versus,
                spark: s.mail.map { Double($0.sent) }, tint: Palette.chartOut,
                sub: s.sent > 0
                    ? "1 out per \(UsageText.count(Int((Double(s.received) / Double(s.sent)).rounded()))) in"
                    : "nothing sent")
            StatTile(
                label: "Signal share", value: s.signalShare.map(UsageText.percent) ?? "—",
                delta: deltaLabel(s.signalShareDelta, upIsGood: true), versus: versus,
                spark: s.rollingShare.map(\.share), tint: Palette.chartSignal,
                sub: "\(UsageText.count(s.attention)) of \(UsageText.count(s.triaged)) triaged")
            StatTile(
                label: "Triage spend", value: Fmt.fmtCost(s.spendTotal),
                delta: deltaLabel(s.spendDelta, upIsGood: false), versus: versus,
                spark: s.spend.map(\.total), tint: Palette.accent,
                sub: s.costPerEmail.map { "\(Fmt.fmtCost($0)) per email" } ?? "no priced calls")
        }
    }

    /// A tile's delta, toned by direction × whether up is good — and left
    /// neutral when up is neither (mail volume) or the move is a rounding.
    private func deltaLabel(_ d: UsageSeries.Delta?, upIsGood: Bool?) -> StatTile.Delta? {
        guard let d else { return nil }
        let tone: Color
        if d.isFlat || upIsGood == nil {
            tone = Palette.inkFaint
        } else {
            tone = d.isUp == upIsGood ? Palette.positive : Palette.warn
        }
        return StatTile.Delta(text: UsageText.delta(d), tone: tone)
    }

    // MARK: - mail in and out

    private func mailCard(_ s: UsageSeries) -> some View {
        // ONE SCALE, cropped to what is there. Left to itself the axis pads the
        // sent side out to a round number, and a mailbox that sends three a
        // day spends half the plot on air under the baseline.
        let maxIn = Double(s.mail.map(\.received).max() ?? 0)
        let maxOut = Double(s.mail.map(\.sent).max() ?? 0)
        let yDomain = (-max(maxOut, 1) * 1.15)...(max(maxIn, 1) * 1.05)
        return ChartCard(
            title: "Mail in and out", note: "per day",
            legend: [
                LegendKey(label: "received", color: Palette.chartIn),
                LegendKey(label: "sent", color: Palette.chartOut),
            ]
        ) {
            Chart {
                ForEach(s.mail) { d in
                    BarMark(
                        x: .value("Day", d.date, unit: .day),
                        y: .value("Received", Double(d.received)),
                        stacking: .unstacked
                    )
                    .foregroundStyle(Palette.chartIn)
                    .cornerRadius(2)
                    // Sent hangs below the line: in up, out down, one scale.
                    BarMark(
                        x: .value("Day", d.date, unit: .day),
                        y: .value("Sent", -Double(d.sent)),
                        stacking: .unstacked
                    )
                    .foregroundStyle(Palette.chartOut)
                    .cornerRadius(2)
                }
                RuleMark(y: .value("Baseline", 0))
                    .foregroundStyle(Palette.hairlineStrong)
                    .lineStyle(StrokeStyle(lineWidth: 0.5))
                if let day = s.mailDay(at: scrub) {
                    crosshair(day.date) {
                        ChartReadout(
                            title: Self.dayLabel(day.date),
                            rows: [
                                .init(UsageText.count(day.received), "received", Palette.chartIn),
                                .init(UsageText.count(day.sent), "sent", Palette.chartOut),
                            ] + (day.sealed > 0 ? [.init(UsageText.count(day.sealed), "sealed", nil)] : []))
                    }
                }
            }
            .chartYScale(domain: yDomain)
            .chartYAxis {
                AxisMarks(position: .leading, values: .automatic(desiredCount: 4)) { value in
                    AxisGridLine().foregroundStyle(Palette.hairline)
                    AxisValueLabel {
                        if let n = value.as(Double.self) {
                            Text(UsageText.count(Int(abs(n).rounded())))
                        }
                    }
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                }
            }
            .modifier(DayAxis(domain: s.xDomain, days: s.days))
            .chartXSelection(value: $scrub)
            .frame(height: 170)
        } table: {
            DayTable(
                columns: ["day", "in", "out", "sealed"],
                rows: s.mail.reversed().map { d in
                    [d.key, UsageText.count(d.received), UsageText.count(d.sent), UsageText.count(d.sealed)]
                },
                height: 170)
        }
    }

    // MARK: - signal to noise

    private func signalCard(_ s: UsageSeries) -> some View {
        ChartCard(
            title: "Signal to noise", note: "of triaged mail",
            legend: [
                LegendKey(label: "signal", color: Palette.chartSignal),
                LegendKey(label: "noise", color: Palette.chartNoise),
                LegendKey(label: "7-day", color: Palette.ink.opacity(0.75), line: true),
            ]
        ) {
            Chart {
                ForEach(s.mail) { d in
                    if let share = d.signalShare {
                        BarMark(
                            x: .value("Day", d.date, unit: .day),
                            yStart: .value("Signal", 0.0),
                            yEnd: .value("Signal", share)
                        )
                        .foregroundStyle(Palette.chartSignal)
                        .cornerRadius(2)
                        // A hair of pane between the two fills, only when both
                        // exist — the gap is what separates them, not a stroke.
                        BarMark(
                            x: .value("Day", d.date, unit: .day),
                            yStart: .value("Noise", min(1, share + (share > 0 && share < 1 ? 0.015 : 0))),
                            yEnd: .value("Noise", 1.0)
                        )
                        .foregroundStyle(Palette.chartNoise)
                        .cornerRadius(2)
                    }
                }
                ForEach(s.rollingShare) { p in
                    LineMark(
                        x: .value("Day", Self.noon(p.date)),
                        y: .value("7-day share", p.share)
                    )
                    .foregroundStyle(Palette.ink.opacity(0.75))
                    .lineStyle(StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round))
                    .interpolationMethod(.monotone)
                }
                if let day = s.mailDay(at: scrub) {
                    crosshair(day.date) {
                        ChartReadout(title: Self.dayLabel(day.date), rows: signalRows(day, s))
                    }
                }
            }
            .chartYScale(domain: 0...1)
            .chartYAxis {
                AxisMarks(position: .leading, values: [0, 0.5, 1]) { value in
                    AxisGridLine().foregroundStyle(Palette.hairline)
                    AxisValueLabel {
                        if let v = value.as(Double.self) {
                            Text(UsageText.percent(v))
                        }
                    }
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                }
            }
            .modifier(DayAxis(domain: s.xDomain, days: s.days))
            .chartXSelection(value: $scrub)
            .frame(height: 150)
        } table: {
            DayTable(
                columns: ["day", "share", "signal", "past due", "deadline", "noise"],
                rows: s.mail.reversed().map { d in
                    [
                        d.key, d.signalShare.map(UsageText.percent) ?? "—",
                        UsageText.count(d.signal), UsageText.count(d.pastDue),
                        UsageText.count(d.deadline), UsageText.count(d.noise),
                    ]
                },
                height: 150)
        }
    }

    private func signalRows(_ day: UsageSeries.MailDay, _ s: UsageSeries) -> [ChartReadout.Row] {
        var rows: [ChartReadout.Row] = []
        if let share = day.signalShare {
            rows.append(.init(UsageText.percent(share), "signal", Palette.chartSignal))
            var parts = ["\(UsageText.count(day.signal)) signal"]
            if day.pastDue > 0 { parts.append("\(UsageText.count(day.pastDue)) past due") }
            if day.deadline > 0 { parts.append("\(UsageText.count(day.deadline)) deadline") }
            rows.append(.init(UsageText.count(day.attention), parts.joined(separator: " · "), nil))
            rows.append(.init(UsageText.count(day.noise), "noise", Palette.chartNoise))
        } else {
            rows.append(.init("—", "no triaged mail", nil))
        }
        if let rolling = s.rollingShare(at: day.date) {
            rows.append(.init(UsageText.percent(rolling), "7-day share", Palette.ink.opacity(0.75)))
        }
        return rows
    }

    // MARK: - triage spend

    private func spendCard(_ s: UsageSeries) -> some View {
        ChartCard(
            title: "Triage spend", note: "estimated, per day",
            legend: s.stages.map { LegendKey(label: $0.label.lowercased(), color: Self.stageColor($0)) }
        ) {
            Chart {
                ForEach(s.spend) { d in
                    ForEach(s.stages) { stage in
                        BarMark(
                            x: .value("Day", d.date, unit: .day),
                            y: .value("USD", d.cost[stage.id] ?? 0)
                        )
                        .foregroundStyle(by: .value("Stage", stage.label))
                        .cornerRadius(2)
                    }
                }
                if let day = s.spendDay(at: scrub) {
                    crosshair(day.date) {
                        ChartReadout(title: Self.dayLabel(day.date), rows: spendRows(day, s))
                    }
                }
            }
            .chartForegroundStyleScale(
                domain: s.stages.map(\.label), range: s.stages.map(Self.stageColor)
            )
            .chartLegend(.hidden)
            .chartYAxis {
                AxisMarks(position: .leading, values: .automatic(desiredCount: 3)) { value in
                    AxisGridLine().foregroundStyle(Palette.hairline)
                    AxisValueLabel {
                        if let v = value.as(Double.self) {
                            Text(v == 0 ? "$0" : Fmt.fmtCost(v))
                        }
                    }
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                }
            }
            .modifier(DayAxis(domain: s.xDomain, days: s.days))
            .chartXSelection(value: $scrub)
            .frame(height: 150)
        } table: {
            DayTable(
                columns: ["day", "calls", "in", "out", "cost"],
                rows: s.spend.reversed().map { d in
                    [
                        d.key, UsageText.count(d.calls), Fmt.compactCount(d.inputTokens),
                        Fmt.compactCount(d.outputTokens), Fmt.fmtCost(d.total),
                    ]
                },
                height: 150)
        }
    }

    private func spendRows(_ day: UsageSeries.SpendDay, _ s: UsageSeries) -> [ChartReadout.Row] {
        guard day.calls > 0 else { return [.init("—", "no calls", nil)] }
        var rows: [ChartReadout.Row] = [.init(Fmt.fmtCost(day.total), "total", nil)]
        for stage in s.stages {
            if let cost = day.cost[stage.id], cost > 0 {
                rows.append(.init(Fmt.fmtCost(cost), stage.label.lowercased(), Self.stageColor(stage)))
            }
        }
        rows.append(
            .init(
                UsageText.count(day.calls),
                "calls · \(Fmt.compactCount(day.tokens)) tokens", nil))
        return rows
    }

    /// The pipeline's stages on one blue, light to dark, in pipeline order;
    /// the tail in the de-emphasis gray.
    private static func stageColor(_ stage: UsageSeries.SpendStage) -> Color {
        switch stage {
        case .stage1: Palette.chartStages[0]
        case .stage2: Palette.chartStages[1]
        case .extractors: Palette.chartStages[2]
        case .other: Palette.chartNoise
        }
    }

    // MARK: - the table under the spend chart, and the assistant

    private func categoryCard(_ s: UsageSeries) -> some View {
        SectionCard(
            label: "By stage",
            note: "\(UsageText.count(s.calls)) calls · \(Fmt.compactCount(s.tokens)) tokens"
        ) {
            if s.categories.isEmpty {
                EmptyNote("No triage spend in this window.")
            } else {
                Grid(alignment: .trailing, horizontalSpacing: 14, verticalSpacing: 5) {
                    GridRow {
                        Text("").gridColumnAlignment(.leading)
                        Text("model").gridColumnAlignment(.leading)
                        Text("calls")
                        Text("tokens")
                        Text("cost")
                    }
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    ForEach(s.categories) { c in
                        GridRow {
                            HStack(spacing: 6) {
                                RoundedRectangle(cornerRadius: 2, style: .continuous)
                                    .fill(Self.stageColor(c.stage))
                                    .frame(width: 8, height: 8)
                                Text(c.label)
                                    .font(Typo.rowSub)
                                    .foregroundStyle(Palette.ink)
                            }
                            Text(c.model + (c.provider.map { " · \($0)" } ?? ""))
                                .font(Typo.mono(10))
                                .foregroundStyle(Palette.inkFaint)
                                .lineLimit(1)
                            Text(UsageText.count(c.calls))
                                .font(Typo.num(11)).foregroundStyle(Palette.inkDim)
                            Text(Fmt.compactCount(c.tokens))
                                .font(Typo.num(11)).foregroundStyle(Palette.inkDim)
                            Text(Fmt.fmtCost(c.cost))
                                .font(Typo.num(11, weight: .semibold)).foregroundStyle(Palette.ink)
                        }
                    }
                }
                Text(
                    "Stage 1 reads every email and the extractors run on its small model; stage 2 is the capable model, on escalations only. Everything else is a smaller pass."
                )
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
            }
        }
    }

    private var assistantCard: some View {
        SectionCard(label: "Assistant", note: assistant.lastModel) {
            if assistant.asks == 0 {
                EmptyNote("Press ⌘K to ask your inbox a question.")
            } else {
                HStack(spacing: 18) {
                    miniStat("asks", UsageText.count(assistant.asks))
                    miniStat("tokens", Fmt.compactCount(assistant.inputTokens + assistant.outputTokens))
                    miniStat("est cost", Fmt.fmtCost(assistant.estimatedCost), tone: Palette.accent)
                }
                // "Your own key" is only claimed once a BYOK ask is actually in
                // the tally; a relay-only ledger gets the neutral line.
                Text(
                    assistant.relayAsks > 0
                        ? "Relay asks are metered against your plan's assistant budget; the estimate covers only asks made with your own key."
                        : "The ⌘K assistant, tracked on this machine."
                )
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
            }
        }
    }

    private func miniStat(_ key: String, _ value: String, tone: Color = Palette.inkDim) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(key).font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Text(value).font(Typo.num(15, weight: .semibold)).foregroundStyle(tone)
        }
    }

    // MARK: - shared chart bits

    /// The shared crosshair at a day, carrying that chart's readout beside it.
    private func crosshair<Readout: View>(_ day: Date, @ViewBuilder readout: () -> Readout)
        -> some ChartContent
    {
        let view = readout()
        return RuleMark(x: .value("Day", Self.noon(day)))
            .foregroundStyle(Palette.inkFaint.opacity(0.6))
            .lineStyle(StrokeStyle(lineWidth: 1))
            .annotation(
                position: .trailing, alignment: .top, spacing: 8,
                overflowResolution: .init(x: .fit(to: .plot), y: .fit(to: .plot))
            ) { view }
    }

    /// A day's centre: where a point or a rule sits inside a bar's band.
    private static func noon(_ day: Date) -> Date { day.addingTimeInterval(12 * 3600) }

    private static func dayLabel(_ day: Date) -> String {
        day.formatted(.dateTime.weekday(.abbreviated).month(.abbreviated).day())
    }
}

// MARK: - the pieces

/// The x-axis every chart on the page shares: the window's exact span, a
/// handful of dates, no vertical grid.
private struct DayAxis: ViewModifier {
    let domain: ClosedRange<Date>
    let days: Int

    func body(content: Content) -> some View {
        content
            .chartXScale(domain: domain)
            .chartXAxis {
                AxisMarks(values: .automatic(desiredCount: days <= 7 ? 7 : 6)) { _ in
                    AxisValueLabel(format: .dateTime.month(.abbreviated).day())
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                }
            }
    }
}

private struct LegendKey: Identifiable {
    let label: String
    let color: Color
    var line = false
    var id: String { label }
}

/// One chart on its own pane: title, an inline legend, and a table twin behind
/// the "table" action so every number the plot shows is also readable exactly.
private struct ChartCard<Plot: View, Table: View>: View {
    let title: String
    var note: String?
    var legend: [LegendKey] = []
    @ViewBuilder var plot: Plot
    @ViewBuilder var table: Table
    @State private var showTable = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 12) {
                Text(title).font(Typo.zoneTitle).foregroundStyle(Palette.ink)
                if let note {
                    Text(note).font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                }
                Spacer(minLength: 8)
                ForEach(legend) { key in
                    HStack(spacing: 5) {
                        if key.line {
                            Capsule().fill(key.color).frame(width: 10, height: 2)
                        } else {
                            RoundedRectangle(cornerRadius: 2, style: .continuous)
                                .fill(key.color).frame(width: 8, height: 8)
                        }
                        Text(key.label).font(Typo.micro).foregroundStyle(Palette.inkDim)
                    }
                }
                Button(showTable ? "chart" : "table") { showTable.toggle() }
                    .buttonStyle(.textAction)
                    .font(Typo.micro)
            }
            if showTable { table } else { plot }
        }
        .zonePadding()
        .frame(maxWidth: .infinity, alignment: .leading)
        .passbandGlass(.pane, cornerRadius: 18, tint: Palette.glassTint)
    }
}

/// The hover readout: the day, then every series at it — value first, in ink,
/// keyed by a short stroke of the series colour.
private struct ChartReadout: View {
    struct Row {
        let value: String
        let label: String
        let color: Color?
        init(_ value: String, _ label: String, _ color: Color?) {
            self.value = value
            self.label = label
            self.color = color
        }
    }

    let title: String
    let rows: [Row]

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title).font(Typo.micro).foregroundStyle(Palette.inkFaint)
            ForEach(rows.indices, id: \.self) { i in
                HStack(spacing: 6) {
                    Capsule()
                        .fill(rows[i].color ?? .clear)
                        .frame(width: 8, height: 2.5)
                    Text(rows[i].value)
                        .font(Typo.num(11, weight: .semibold))
                        .foregroundStyle(Palette.ink)
                    Text(rows[i].label)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkDim)
                }
            }
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 7)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Palette.readerBackground)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(Palette.hairlineStrong, lineWidth: 0.5)
        )
        .shadow(color: .black.opacity(0.14), radius: 8, y: 3)
        .fixedSize()
    }
}

/// A headline number with its change against the prior window and the
/// window's own shape underneath.
private struct StatTile: View {
    struct Delta {
        let text: String
        let tone: Color
    }

    let label: String
    let value: String
    var delta: Delta?
    let versus: String
    let spark: [Double]
    let tint: Color
    var sub: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label).font(Typo.micro).foregroundStyle(Palette.inkFaint)
            Text(value)
                .font(.system(size: 22, weight: .semibold))
                .foregroundStyle(Palette.ink)
            HStack(spacing: 4) {
                if let delta {
                    Text(delta.text).font(Typo.num(11, weight: .semibold)).foregroundStyle(delta.tone)
                    Text(versus).font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                } else {
                    Text("no prior window").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                }
            }
            .lineLimit(1)
            Sparkline(values: spark, tint: tint)
                .frame(height: 24)
                .padding(.top, 4)
            if let sub {
                Text(sub).font(Typo.micro).foregroundStyle(Palette.inkFaintest).lineLimit(1)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .passbandGlass(.pane, cornerRadius: 14, tint: Palette.glassTint)
    }
}

/// The window's daily shape in the de-emphasis gray, its latest day in the
/// tile's own colour. No axes: the tile's number is the scale.
private struct Sparkline: View {
    let values: [Double]
    let tint: Color

    var body: some View {
        Chart {
            ForEach(values.indices, id: \.self) { i in
                AreaMark(x: .value("day", i), y: .value("value", values[i]))
                    .foregroundStyle(Palette.inkFaint.opacity(0.12))
                    .interpolationMethod(.monotone)
                LineMark(x: .value("day", i), y: .value("value", values[i]))
                    .foregroundStyle(Palette.inkFaint)
                    .lineStyle(StrokeStyle(lineWidth: 1.5, lineCap: .round, lineJoin: .round))
                    .interpolationMethod(.monotone)
            }
            if let last = values.indices.last {
                PointMark(x: .value("day", last), y: .value("value", values[last]))
                    .foregroundStyle(tint)
                    .symbolSize(30)
            }
        }
        .chartXAxis(.hidden)
        .chartYAxis(.hidden)
        .chartLegend(.hidden)
        .chartYScale(domain: 0...max(values.max() ?? 1, 0.000_001))
    }
}

/// A chart's exact twin: one row per day, newest first, at the plot's height
/// so switching between the two moves nothing on the page.
private struct DayTable: View {
    let columns: [String]
    let rows: [[String]]
    let height: CGFloat

    var body: some View {
        ScrollView {
            Grid(alignment: .trailing, horizontalSpacing: 14, verticalSpacing: 3) {
                GridRow {
                    ForEach(columns.indices, id: \.self) { c in
                        Text(columns[c])
                            .gridColumnAlignment(c == 0 ? .leading : .trailing)
                    }
                }
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
                ForEach(rows.indices, id: \.self) { r in
                    GridRow {
                        ForEach(rows[r].indices, id: \.self) { c in
                            Text(rows[r][c])
                                .font(c == 0 ? Typo.mono(10) : Typo.num(11))
                                .foregroundStyle(c == 0 ? Palette.inkDim : Palette.inkFaint)
                        }
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(height: height)
    }
}
