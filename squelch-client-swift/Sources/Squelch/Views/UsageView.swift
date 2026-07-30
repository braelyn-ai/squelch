// USAGE — server-side triage spend (GET /client/usage) plus the client-side BYOK
// assistant tally. The triage pipeline reports its stages as separate ledger
// categories; this view loops over whatever arrives and falls back to the flat
// single-category shape older servers send. Assistant calls go straight from
// here to the reader's own provider key — the server never sees them.

import SwiftUI

struct UsageView: View {
    @Environment(AppStore.self) private var store
    @State private var usageState: Loadable<UsageResponse> = .loading
    @State private var assistant = AssistantUsage()

    /// A category resolved for rendering: the wire map's KEY promoted to `id`,
    /// plus a display label derived here (the wire carries none).
    private struct ResolvedCategory: Identifiable {
        var id: String
        var label: String
        var subtitle: String
        var rows: [UsageRow]
        var totals: UsageTotals
        var provider: String?
        var model: String
    }

    private static func categoryLabel(_ id: String) -> String {
        switch id {
        case "stage1": "Stage 1 — every email"
        case "stage2": "Stage 2 — escalations"
        default: id
        }
    }

    private static func categorySubtitle(_ id: String) -> String {
        switch id {
        case "stage1":
            "Server-side stage-1 classification (runs on every email; your squelch server's key)."
        case "stage2":
            "Server-side stage-2 classification (escalations; your squelch server's key)."
        default: "Server-side classification spend (your squelch server's key)."
        }
    }

    /// The per-stage `categories` breakdown — an OBJECT MAP keyed by id, so each
    /// key is enumerated and promoted — else the flat legacy shape.
    private var categories: [ResolvedCategory] {
        guard let usage = usageState.value else { return [] }
        if let map = usage.categories, !map.isEmpty {
            return map.keys.sorted().map { id in
                let c = map[id]!
                return ResolvedCategory(
                    id: id, label: Self.categoryLabel(id), subtitle: Self.categorySubtitle(id),
                    rows: c.rows, totals: c.totals, provider: c.provider, model: c.model)
            }
        }
        return [
            ResolvedCategory(
                id: "triage", label: "Triage", subtitle: Self.categorySubtitle("triage"),
                rows: usage.rows, totals: usage.totals, provider: usage.provider,
                model: usage.model)
        ]
    }

    var body: some View {
        VStack(spacing: 0) {
            RoutedHeader(title: "Usage")
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    if usageState.isLoading {
                        SectionCard(label: "Triage") { EmptyNote("loading…") }
                    } else if let error = usageState.error {
                        SectionCard(label: "Triage") {
                            Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
                        }
                    } else {
                        ForEach(categories) { category in
                            categorySection(category)
                        }
                    }
                    assistantSection
                }
                .padding(.horizontal, 24)
                .padding(.vertical, 18)
                .frame(maxWidth: 900, alignment: .leading)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .task { await load() }
        // Esc leaves — a full routed page needs an exit other than the rail.
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "back to sitrep") { store.setView(.sitrep) }
        ])
    }

    private func load() async {
        await $usageState.load("failed to load usage") {
            try await APIClient.shared.getUsage(days: 30)
        }
        // Local ledger, read after the server spend lands so both sections
        // appear in the same frame.
        assistant = AssistantUsageLedger.read()
    }

    // MARK: - sections

    private func categorySection(_ category: ResolvedCategory) -> some View {
        let dayTotals = category.rows.map { $0.input_tokens + $0.output_tokens }
        let peak = max(1, dayTotals.max() ?? 1)

        return SectionCard(
            label: category.label,
            note: category.model + (category.provider.map { " · \($0)" } ?? "")
        ) {
            Text(category.subtitle)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaint)

            if category.rows.isEmpty {
                EmptyNote("No \(category.label.lowercased()) usage yet.")
            } else {
                totalsRow(
                    calls: category.totals.calls,
                    tokens: category.totals.input_tokens + category.totals.output_tokens,
                    cost: category.totals.est_cost_usd)

                VStack(spacing: 0) {
                    HStack(spacing: 10) {
                        Text("day").frame(width: 84, alignment: .leading)
                        Text("calls").frame(width: 48, alignment: .trailing)
                        Text("in").frame(width: 54, alignment: .trailing)
                        Text("out").frame(width: 54, alignment: .trailing)
                        Text("tokens").frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .padding(.vertical, 4)
                    .overlay(alignment: .bottom) { Hairline() }

                    ForEach(Array(category.rows.enumerated()), id: \.element.day) { i, row in
                        let total = dayTotals[i]
                        HStack(spacing: 10) {
                            Text(row.day)
                                .font(Typo.mono(10))
                                .foregroundStyle(Palette.inkDim)
                                .frame(width: 84, alignment: .leading)
                            Text("\(row.calls)")
                                .font(Typo.num(11)).foregroundStyle(Palette.inkDim)
                                .frame(width: 48, alignment: .trailing)
                            Text(Fmt.compactCount(row.input_tokens))
                                .font(Typo.num(11)).foregroundStyle(Palette.inkFaint)
                                .frame(width: 54, alignment: .trailing)
                            Text(Fmt.compactCount(row.output_tokens))
                                .font(Typo.num(11)).foregroundStyle(Palette.inkFaint)
                                .frame(width: 54, alignment: .trailing)
                            GeometryReader { geo in
                                RoundedRectangle(cornerRadius: 2)
                                    .fill(Palette.accent.opacity(0.65))
                                    .frame(
                                        width: geo.size.width * CGFloat(total) / CGFloat(peak),
                                        height: 7)
                                    .frame(maxHeight: .infinity, alignment: .center)
                            }
                            .frame(height: 18)
                            .help("\(total) tokens")
                        }
                        .padding(.vertical, 2)
                    }
                }
            }
        }
    }

    private var assistantSection: some View {
        SectionCard(label: "Assistant", note: assistant.lastModel) {
            Text(
                "The ⌘K \"ask your inbox\" assistant (your own key, tracked on this machine)."
            )
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaint)

            if assistant.asks == 0 {
                EmptyNote("No assistant usage yet. Press ⌘K to ask your inbox a question.")
            } else {
                totalsRow(
                    calls: assistant.asks,
                    tokens: assistant.inputTokens + assistant.outputTokens,
                    cost: assistant.estimatedCost,
                    callsLabel: "asks")
            }
        }
    }

    private func totalsRow(calls: Int, tokens: Int, cost: Double, callsLabel: String = "calls")
        -> some View
    {
        HStack(spacing: 26) {
            stat(callsLabel, "\(calls)", tone: Palette.inkDim)
            stat("in + out tokens", Fmt.compactCount(tokens), tone: Palette.inkDim)
            stat("est cost", Fmt.fmtCost(cost), tone: Palette.accent)
            Spacer()
        }
        .padding(.vertical, 4)
    }

    private func stat(_ key: String, _ value: String, tone: Color) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(key).font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Text(value).font(Typo.num(16, weight: .semibold)).foregroundStyle(tone)
        }
    }
}
