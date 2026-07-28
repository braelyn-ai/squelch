// SETTINGS — a routed main view (bottom rail group), with a left sub-nav whose
// last-active section is persisted so reopening restores it.
//
//   GENERAL   — connection (server URL + token, "Test & Save" re-validates
//               against /client/stats and persists), appearance, developer
//               mode, your display name.
//   MAIL      — remote images.
//   TRIAGE    — the pipeline explainer, the daily caps + spend estimator, and
//               the For-your-eyes ranking blend.
//   ASSISTANT — the BYOK key (write-only) + model.
//   ACCOUNT   — read-only meta + disconnect.
//
// No keys are registered here, so the global 1..5 / ⌘[ ] nav keeps working and
// Esc is a no-op (nothing to close). The dispatch input-guard already prevents
// single-letter binds from firing while a field is focused.
//
// Ported from squelch-desktop/src/views/SettingsView.tsx and
// src/components/TriagePipeline.tsx.

import SwiftUI

struct SettingsView: View {
    @Environment(AppStore.self) private var store
    @Environment(Prefs.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        VStack(spacing: 0) {
            RoutedHeader(title: "Settings")
            HStack(alignment: .top, spacing: 0) {
                nav
                ScrollView {
                    VStack(alignment: .leading, spacing: 16) {
                        switch prefs.settingsSection {
                        case .general:
                            ConnectionSection()
                            AppearanceSection()
                            DeveloperSection()
                            YouSection()
                        case .mail:
                            MailSection()
                        case .triage:
                            TriagePipelineSection()
                            TriageBudgetSection()
                            RankingSection()
                        case .assistant:
                            AssistantSection()
                        case .account:
                            AccountSection()
                        }
                    }
                    .padding(.horizontal, 22)
                    .padding(.vertical, 18)
                    .frame(maxWidth: 720, alignment: .leading)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
        }
    }

    private var nav: some View {
        @Bindable var prefs = prefs
        return VStack(alignment: .leading, spacing: 2) {
            ForEach(SettingsSection.allCases, id: \.self) { section in
                Button {
                    prefs.settingsSection = section
                } label: {
                    Text(section.label)
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(
                            prefs.settingsSection == section ? Palette.accent : Palette.inkFaint
                        )
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 11)
                        .padding(.vertical, 6)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .background(
                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                        .fill(prefs.settingsSection == section ? Palette.accentSoft : .clear)
                )
            }
            Spacer()
        }
        .frame(width: 158)
        .padding(.horizontal, 12)
        .padding(.vertical, 18)
    }
}

// MARK: - shared section chrome

/// An engraved section on one glass pane. Brass (the accent) is used sparingly
/// — the label, the connected dot, and focus. Everything else is ink.
struct SettingsSectionCard<Content: View>: View {
    let label: String
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(label)
                .font(Typo.sectionLabel)
                .foregroundStyle(Palette.accent)
                .textCase(.uppercase)
            content
        }
        .zonePadding()
        .frame(maxWidth: .infinity, alignment: .leading)
        .squelchGlass(.pane, cornerRadius: 18, tint: Palette.glassTint)
    }
}

/// A two-state (or n-state) segmented control drawn in glass.
struct GlassSegmented<T: Hashable>: View {
    let options: [(value: T, label: String)]
    @Binding var selection: T

    var body: some View {
        HStack(spacing: 5) {
            ForEach(options, id: \.value) { option in
                Button {
                    selection = option.value
                } label: {
                    Text(option.label)
                        .font(Typo.chip)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 4)
                }
                .buttonStyle(
                    selection == option.value
                        ? AnyButtonStyle(GlassProminentButtonStyle())
                        : AnyButtonStyle(GlassButtonStyle())
                )
                .tint(Palette.accent)
                .foregroundStyle(selection == option.value ? .white : Palette.inkFaint)
            }
        }
    }
}

/// The small hint line under a control.
struct SettingsHint: View {
    let text: String
    init(_ text: String) { self.text = text }
    var body: some View {
        Text(text)
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaintest)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// A key/control row: label on the left, control on the right.
struct InlineRow<Content: View>: View {
    let key: String
    @ViewBuilder var content: Content

    var body: some View {
        HStack(spacing: 14) {
            Text(key)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkDim)
                .frame(width: 96, alignment: .leading)
            content
            Spacer(minLength: 0)
        }
    }
}

// MARK: - general

private struct ConnectionSection: View {
    @Environment(AppStore.self) private var store
    @State private var url = ""
    @State private var token = ""
    @State private var busy = false
    @State private var result: Result<Void, String>?

    private enum Result<T, E> { case ok, err(E) }

    var body: some View {
        SettingsSectionCard(label: "Connection") {
            Field(label: "server url") {
                TextField("http://127.0.0.1:8848", text: $url)
                    .textFieldStyle(.plain)
                    .autocorrectionDisabled()
                    .onChange(of: url) { _, _ in result = nil }
            }
            Field(label: "api token") {
                SecureField("SQUELCH_API_TOKEN", text: $token)
                    .textFieldStyle(.plain)
                    .onChange(of: token) { _, _ in result = nil }
            }
            HStack(spacing: 10) {
                Button(busy ? "testing…" : "Test & Save") { Task { await testSave() } }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.accent)
                    .disabled(busy || url.trimmed.isEmpty || token.trimmed.isEmpty)
                switch result {
                case .ok:
                    HStack(spacing: 5) {
                        Circle().fill(Palette.positive).frame(width: 6, height: 6)
                        Text("connected · saved")
                            .font(Typo.micro).foregroundStyle(Palette.positive)
                    }
                case .err(let message):
                    Text(message).font(Typo.micro).foregroundStyle(Palette.danger)
                case nil:
                    EmptyView()
                }
            }
            SettingsHint(
                "The token lives in your macOS keychain and is sent only as a bearer header — never logged."
            )
        }
        .onAppear {
            url = store.settings?.serverURL ?? ""
            token = store.settings?.apiToken ?? ""
        }
    }

    private func testSave() async {
        busy = true
        result = nil
        let outcome = await store.revalidate(
            serverURL: url.trimmed, apiToken: token.trimmed)
        busy = false
        result = outcome.ok ? .ok : .err(outcome.error ?? "failed")
    }
}

private struct AppearanceSection: View {
    @Environment(Prefs.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        SettingsSectionCard(label: "Appearance") {
            InlineRow(key: "theme") {
                GlassSegmented(
                    options: ThemeChoice.allCases.map { ($0, $0.label) },
                    selection: $prefs.theme)
            }
            SettingsHint("Auto follows the system appearance. \\ flips light/dark from anywhere.")
        }
    }
}

/// DEVELOPER — when on, re-triage affordances appear in the sitrep masthead
/// (last 7 days) and the thread viewer (this email): they reset the LLM
/// verdicts so the pipeline re-runs. Rule-decided and sealed rows are never
/// touched.
private struct DeveloperSection: View {
    @Environment(Prefs.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        SettingsSectionCard(label: "Developer") {
            InlineRow(key: "dev mode") {
                GlassSegmented(
                    options: [(false, "Off"), (true, "On")], selection: $prefs.developerMode)
            }
            SettingsHint(
                "Adds re-triage buttons (sitrep masthead + open email) that re-run the triage pipeline. Re-triaging spends model budget."
            )
        }
    }
}

private struct YouSection: View {
    @Environment(Prefs.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        SettingsSectionCard(label: "You") {
            Field(label: "name") {
                TextField("shown in the sitrep greeting", text: $prefs.userName)
                    .textFieldStyle(.plain)
                    .autocorrectionDisabled()
            }
        }
    }
}

// MARK: - mail

/// Remote images: "Always" renders network images automatically in every email;
/// "On demand" blocks them at the frame CSP until the reader opts in per
/// message. Tracking pixels are stripped either way.
private struct MailSection: View {
    @Environment(Prefs.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        SettingsSectionCard(label: "Mail") {
            InlineRow(key: "images") {
                GlassSegmented(
                    options: [(true, "Always"), (false, "On demand")],
                    selection: $prefs.loadRemoteImages)
            }
            SettingsHint(
                "Tracking pixels are removed either way, and images load with no referrer.")
        }
    }
}

// MARK: - triage

/// The "How triage works" explainer, mounted above the budget fields as their
/// legend. Stage cards show the LIVE model names + daily caps from
/// GET /client/triage-config. Self-contained: it fetches its own config, so the
/// budget section's logic stays untouched.
private struct TriagePipelineSection: View {
    @State private var config: TriageConfig?

    var body: some View {
        SettingsSectionCard(label: "How triage works") {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 7) {
                    node("mail arrives", note: nil, tint: Palette.inkFaint)
                    arrow
                    node("seal check", note: "auth mail is sealed", tint: Palette.lock)
                    arrow
                    node("sender rules", note: "your rules win", tint: Palette.accent)
                    arrow
                    node(
                        "stage 1", note: config?.stage1.model ?? "—",
                        caps: config.map { "cap \($0.stage1.global_daily_cap)/day" },
                        tint: Palette.accent)
                    arrow
                    node("confident?", note: "escalate if not", tint: Palette.inkFaint)
                    arrow
                    node(
                        "stage 2", note: config?.stage2_model ?? "—",
                        caps: config.map {
                            "\($0.thread_daily_cap)/thread · \($0.sender_daily_cap)/sender · \($0.global_daily_cap)/day"
                        },
                        tint: Palette.warn)
                    arrow
                    node("sitrep / inbox", note: nil, tint: Palette.positive)
                }
                .padding(.vertical, 2)
            }
            SettingsHint(
                "If the API or budget is unavailable, stage 1 falls back to heuristic scores — mail is never stuck."
            )
        }
        .task { config = try? await APIClient.shared.getTriageConfig() }
    }

    private var arrow: some View {
        Image(systemName: "arrow.right")
            .font(.system(size: 9, weight: .semibold))
            .foregroundStyle(Palette.inkFaintest)
    }

    private func node(_ name: String, note: String?, caps: String? = nil, tint: Color)
        -> some View
    {
        VStack(alignment: .leading, spacing: 2) {
            Text(name)
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Palette.ink)
            if let note {
                Text(note)
                    .font(Typo.micro)
                    .foregroundStyle(tint)
                    .lineLimit(1)
            }
            if let caps {
                Text(caps)
                    .font(Typo.mono(9))
                    .foregroundStyle(Palette.inkFaintest)
                    .lineLimit(1)
            }
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 7)
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(tint.opacity(0.10))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .strokeBorder(tint.opacity(0.25), lineWidth: 0.75))
        .fixedSize()
    }
}

/// THE DAILY CAPS for the two triage stages plus a spend estimator computed
/// from trailing-14d usage and configured prices. Stage 1 (a small LLM) runs on
/// EVERY email and has one cap. Stage 2 (a capable LLM) runs only on
/// escalations and keeps the three per-thread / per-sender / global caps.
///
/// Caps are integers 1..=100000; Save posts ONLY changed fields. Values can
/// originate from the built-in default, the server config file, or a live app
/// override — the per-field note reflects that provenance.
private struct TriageBudgetSection: View {
    @Environment(AppStore.self) private var store

    @State private var config: TriageConfig?
    @State private var loadError: String?
    @State private var form: [FormKey: String] = [:]
    @State private var busy = false
    @State private var note: Note?

    private enum Note: Equatable { case saved, error(String) }

    private enum FormKey: String, CaseIterable, Hashable {
        case stage1Global = "stage1_global_daily_cap"
        case thread = "thread_daily_cap"
        case sender = "sender_daily_cap"
        case global = "global_daily_cap"

        var label: String {
            switch self {
            case .stage1Global, .global: "global / day"
            case .thread: "per thread / day"
            case .sender: "per sender / day"
            }
        }
    }

    private func value(_ config: TriageConfig, _ key: FormKey) -> Int {
        switch key {
        case .stage1Global: config.stage1.global_daily_cap
        case .thread: config.thread_daily_cap
        case .sender: config.sender_daily_cap
        case .global: config.global_daily_cap
        }
    }

    private func source(_ config: TriageConfig, _ key: FormKey) -> TriageConfigSource {
        switch key {
        case .stage1Global: config.stage1.source
        case .thread: config.sources.thread_daily_cap
        case .sender: config.sources.sender_daily_cap
        case .global: config.sources.global_daily_cap
        }
    }

    var body: some View {
        SettingsSectionCard(label: "Triage budget") {
            if let loadError {
                Text(loadError).font(Typo.micro).foregroundStyle(Palette.danger)
            } else if let config {
                Text("Stage 1 caps")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Palette.ink)
                SettingsHint("Stage 1 — \(config.stage1.model) (every email)")
                capField(.stage1Global, config)

                Text("Stage 2 caps")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Palette.ink)
                    .padding(.top, 6)
                SettingsHint("Stage 2 — \(config.stage2_model) (escalations)")
                capField(.thread, config)
                capField(.sender, config)
                capField(.global, config)

                HStack(spacing: 10) {
                    Button(busy ? "saving…" : "Save") { Task { await save() } }
                        .buttonStyle(.glassProminent)
                        .tint(Palette.accent)
                        .disabled(busy)
                    switch note {
                    case .saved:
                        HStack(spacing: 5) {
                            Circle().fill(Palette.positive).frame(width: 6, height: 6)
                            Text("saved").font(Typo.micro).foregroundStyle(Palette.positive)
                        }
                    case .error(let message):
                        Text(message).font(Typo.micro).foregroundStyle(Palette.danger)
                    case nil:
                        EmptyView()
                    }
                }

                estimator(config)
            } else {
                SettingsHint("loading…")
            }
        }
        .task { await load() }
    }

    private func capField(_ key: FormKey, _ config: TriageConfig) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 5) {
                Text(key.label).font(Typo.micro).foregroundStyle(Palette.inkFaint)
                Text(source(config, key).note)
                    .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            }
            TextField(
                "",
                text: Binding(
                    get: { form[key] ?? String(value(config, key)) },
                    set: {
                        form[key] = $0
                        note = nil
                    })
            )
            .textFieldStyle(.plain)
            .font(Typo.mono(12))
            .frame(width: 120)
            .padding(.horizontal, 9)
            .padding(.vertical, 6)
            .background(
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(Palette.canvas.opacity(0.65))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .strokeBorder(Palette.hairlineStrong, lineWidth: 0.75))
        }
    }

    /// Per-stage cost math from trailing-14d means; nil when a stage has no
    /// token history yet (nothing to price).
    private struct StageEstimate {
        var costPerCall: Double
        var typicalDaily: Double
        var ceilingDaily: Double
    }

    private func estimate(
        tokensIn: Double?, tokensOut: Double?, priceIn: Double, priceOut: Double,
        avgCallsPerDay: Double, cap: Int
    ) -> StageEstimate? {
        guard let tokensIn, let tokensOut else { return nil }
        let costPerCall = tokensIn / 1e6 * priceIn + tokensOut / 1e6 * priceOut
        return StageEstimate(
            costPerCall: costPerCall, typicalDaily: avgCallsPerDay * costPerCall,
            ceilingDaily: Double(cap) * costPerCall)
    }

    /// Text-only estimator. Each stage's "typical" uses its trailing-14d average
    /// call rate × its per-call token means; the combined "hard ceiling" sums
    /// each stage's cap × its per-call cost. Stages with no usage history are
    /// SKIPPED rather than printed as $0.00 — a confident zero would be a lie.
    private func estimator(_ config: TriageConfig) -> some View {
        let s1 = estimate(
            tokensIn: config.stage1.avg_tokens_in_per_call,
            tokensOut: config.stage1.avg_tokens_out_per_call,
            priceIn: config.stage1.price_in_per_mtok,
            priceOut: config.stage1.price_out_per_mtok,
            avgCallsPerDay: config.stage1.avg_calls_per_day,
            cap: config.stage1.global_daily_cap)
        let s2 = estimate(
            tokensIn: config.avg_tokens_in_per_call, tokensOut: config.avg_tokens_out_per_call,
            priceIn: config.price_in_per_mtok, priceOut: config.price_out_per_mtok,
            avgCallsPerDay: config.avg_stage2_calls_per_day, cap: config.global_daily_cap)
        let priced = [s1, s2].compactMap { $0 }
        let totalDaily = priced.reduce(0) { $0 + $1.typicalDaily }
        let totalCeiling = priced.reduce(0) { $0 + $1.ceilingDaily }

        return VStack(alignment: .leading, spacing: 3) {
            SettingsHint("You average ~\(Int(config.avg_inbound_per_day.rounded())) emails/day.")
            SettingsHint(
                s1.map { "Stage 1 typically ~\(Fmt.costUSD($0.typicalDaily))/day." }
                    ?? "Stage 1 typical: not enough usage history yet.")
            SettingsHint(
                s2.map { "Stage 2 typically ~\(Fmt.costUSD($0.typicalDaily))/day." }
                    ?? "Stage 2 typical: not enough usage history yet.")
            if priced.isEmpty {
                SettingsHint("Not enough usage history to estimate a total yet.")
            } else {
                SettingsHint(
                    "typical total ~\(Fmt.costUSD(totalDaily))/day (~\(Fmt.costUSD(totalDaily * 30))/mo)."
                )
                SettingsHint(
                    "combined hard ceiling at your caps: ~\(Fmt.costUSD(totalCeiling))/day.")
            }
        }
        .padding(.top, 4)
    }

    private func load() async {
        do {
            let loaded = try await APIClient.shared.getTriageConfig()
            seed(loaded)
        } catch {
            loadError = errText(error, "could not load")
        }
    }

    private func seed(_ loaded: TriageConfig) {
        config = loaded
        form = Dictionary(
            uniqueKeysWithValues: FormKey.allCases.map { ($0, String(value(loaded, $0))) })
    }

    private func save() async {
        guard let config else { return }
        // Post ONLY fields whose parsed integer differs from the loaded value.
        var patch = TriageConfigPatch()
        var changed = false
        for key in FormKey.allCases {
            let raw = (form[key] ?? "").trimmed
            guard let n = Int(raw), !raw.isEmpty else {
                note = .error("caps must be whole numbers")
                return
            }
            guard n != value(config, key) else { continue }
            changed = true
            switch key {
            case .stage1Global: patch.stage1_global_daily_cap = n
            case .thread: patch.thread_daily_cap = n
            case .sender: patch.sender_daily_cap = n
            case .global: patch.global_daily_cap = n
            }
        }
        guard changed else {
            note = .saved
            return
        }
        busy = true
        note = nil
        do {
            seed(try await APIClient.shared.setTriageConfig(patch))
            note = .saved
        } catch {
            note = .error(errText(error, "save failed"))
        }
        busy = false
    }
}

/// The blend that orders the Sitrep "For your eyes" zone. A single slider
/// between time (urgency) and severity (importance). The stored pref is the
/// URGENCY share; the slider is inverted so dragging RIGHT toward "severity"
/// lowers it, matching the label. The sitrep re-ranks live — no save button.
private struct RankingSection: View {
    @Environment(Prefs.self) private var prefs

    var body: some View {
        SettingsSectionCard(label: "For your eyes") {
            Text("Rank For your eyes by: time ←→ severity")
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkDim)
            Slider(
                value: Binding(
                    get: { 1 - prefs.rankWeight },
                    set: { prefs.rankWeight = 1 - $0 }),
                in: 0...1, step: 0.05
            )
            .tint(Palette.accent)
            HStack {
                Text("time").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                Spacer()
                Text("severity").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            }
            SettingsHint(
                "Blends how soon something is due with how important it is. Overdue items rank high; drag toward severity to let important undated mail compete."
            )
        }
    }
}

// MARK: - assistant

/// The BYOK "ask your inbox" (⌘K) settings: paste your OWN Anthropic key
/// (stored in the OS keychain, never sent to the squelch server) and pick the
/// model. The key is WRITE-ONLY from this view's perspective: it can see
/// whether one is set and which provider it routes to, never the value.
private struct AssistantSection: View {
    @Environment(Prefs.self) private var prefs

    @State private var status = AssistantKeyStatus.absent
    @State private var keyInput = ""
    @State private var busy = false
    @State private var note: Note?

    private enum Note: Equatable {
        case ok(String)
        case err(String)
    }

    var body: some View {
        @Bindable var prefs = prefs
        SettingsSectionCard(label: "Assistant") {
            Field(label: "api key (yours)") {
                SecureField(
                    status.present ? "•••••• set — paste to replace" : "sk-ant-…", text: $keyInput
                )
                .textFieldStyle(.plain)
                .onChange(of: keyInput) { _, _ in note = nil }
            }
            HStack(spacing: 10) {
                Button(busy ? "saving…" : "Save key") { saveKey() }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.accent)
                    .disabled(busy || keyInput.trimmed.isEmpty)
                if status.present {
                    HStack(spacing: 5) {
                        Circle().fill(Palette.positive).frame(width: 6, height: 6)
                        Text(
                            "key set"
                                + (status.provider.map { " · \($0.label)" } ?? "")
                        )
                        .font(Typo.micro).foregroundStyle(Palette.positive)
                    }
                    Button("forget") { forgetKey() }
                        .buttonStyle(.plain)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.danger)
                }
                switch note {
                case .ok(let text):
                    Text(text).font(Typo.micro).foregroundStyle(Palette.positive)
                case .err(let text):
                    Text(text).font(Typo.micro).foregroundStyle(Palette.danger)
                case nil:
                    EmptyView()
                }
            }
            SettingsHint(
                "Your key stays on this machine (macOS keychain) and is used only for the ⌘K assistant — never sent to the squelch server."
            )

            InlineRow(key: "model") {
                GlassSegmented(
                    options: AssistantModel.allCases.map { ($0, $0.shortLabel) },
                    selection: $prefs.assistantModel)
            }
            SettingsHint(prefs.assistantModel.label)
        }
        .onAppear { status = AssistantKeyStore.status() }
    }

    private func saveKey() {
        busy = true
        note = nil
        do {
            try AssistantKeyStore.set(keyInput.trimmed)
            keyInput = ""
            status = AssistantKeyStore.status()
            note = .ok("key saved")
        } catch {
            note = .err(errText(error, "could not save key"))
        }
        busy = false
    }

    private func forgetKey() {
        busy = true
        note = nil
        try? AssistantKeyStore.clear()
        status = AssistantKeyStore.status()
        note = .ok("key forgotten")
        busy = false
    }
}

// MARK: - account

private struct AccountSection: View {
    @Environment(AppStore.self) private var store
    @State private var usage: UsageResponse?

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            SettingsSectionCard(label: "Account") {
                meta("server", store.settings?.serverURL ?? "—")
                meta("triage model", usage?.model ?? "—")
                meta("provider", usage?.provider ?? "—")
            }
            SettingsSectionCard(label: "Danger") {
                HStack(spacing: 12) {
                    Button("Disconnect") { store.disconnect() }
                        .buttonStyle(.glassProminent)
                        .tint(Palette.danger)
                    Text("clears saved settings and returns to the connect screen")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                }
            }
        }
        // Usage is decorative here; errors are ignored.
        .task { usage = try? await APIClient.shared.getUsage(days: 1) }
    }

    private func meta(_ key: String, _ value: String) -> some View {
        HStack(spacing: 14) {
            Text(key)
                .font(Typo.rowSub).foregroundStyle(Palette.inkFaint)
                .frame(width: 110, alignment: .leading)
            Text(value)
                .font(Typo.mono(11)).foregroundStyle(Palette.inkDim)
                .textSelection(.enabled)
            Spacer()
        }
    }
}
