// Settings: a routed main view with a left sub-nav whose section is persisted.
// General (connection, appearance, developer, name), mail, triage (pipeline
// explainer, caps + spend estimate, ranking blend), assistant (BYOK key, model),
// account. Escape is the only key bound here, so global 1..5 / ⌘[ ] nav keeps
// working and the dispatcher's input guard covers the typed fields.

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
                        case .privacy:
                            PrivacySection()
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
        // Escape is exempt from the dispatcher's input guard precisely so a
        // surface this full of text fields can still be left.
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "back to sitrep") { store.setView(.sitrep) }
        ])
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

/// A stored secret: masked at rest, eye to reveal, pencil to edit (prefilled).
/// Plaintext is pulled through `load` only when one of those is pressed, so a
/// secret nobody asked to see never enters view state. Blur is the save — a
/// cancel button is unbuildable here, since pressing it blurs the field and
/// fires the very save it means to prevent. Only first entry uses SecureField.
struct SecretField: View {
    let label: String
    let placeholder: String
    /// Whether a secret is already stored — drives the masked resting state.
    let hasStored: Bool
    /// Fetches the plaintext on demand. May prompt (keychain), so it's async.
    let load: @MainActor () async -> String?
    /// Persist a changed value. Never called with one that matches storage.
    let onCommit: @MainActor (String) async -> Void
    /// The edit buffer. Meaningful to the caller while `editing` is true.
    @Binding var text: String
    @Binding var editing: Bool

    @State private var revealed = false
    @State private var loading = false
    /// Last value read from storage; `text` is diffed against it so opening a
    /// field and clicking straight back out writes nothing.
    @State private var baseline = ""
    @FocusState private var focused: Bool

    /// Nothing stored means nothing to unlock — skip straight to the input.
    private var inputMode: Bool { editing || !hasStored }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            FieldLabel(label)
            HStack(spacing: 6) {
                well
                controls
            }
        }
    }

    @ViewBuilder private var well: some View {
        if inputMode {
            input
                .textFieldStyle(.plain)
                .autocorrectionDisabled()
                .onSubmit { focused = false }
                .onChange(of: focused) { _, nowFocused in
                    if !nowFocused { Task { await commit() } }
                }
                .fieldWell()
        } else {
            Text(resting)
                .font(revealed ? Typo.mono(12) : Font.system(size: 13))
                .foregroundStyle(Palette.inkDim)
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .fieldWell()
        }
    }

    /// Two branches rather than one field with a toggled mask: swapping TextField
    /// for SecureField in place rebuilds it and drops focus — and a dropped focus
    /// here is a save. The swap only happens across a commit, as focus leaves.
    @ViewBuilder private var input: some View {
        if hasStored {
            TextField(placeholder, text: $text).focused($focused)
        } else {
            SecureField(placeholder, text: $text).focused($focused)
        }
    }

    private var resting: String {
        if loading { return "reading keychain…" }
        if revealed { return text.isEmpty ? "—" : text }
        return String(repeating: "•", count: 22)
    }

    @ViewBuilder private var controls: some View {
        if inputMode {
            // Nothing to unmask; this is an explicit "done" that drops focus,
            // which is what saves.
            icon("checkmark", help: "Done") { focused = false }
        } else {
            icon(revealed ? "eye.slash" : "eye", help: revealed ? "Hide" : "Show") {
                if revealed {
                    revealed = false
                } else {
                    Task {
                        await pull()
                        revealed = true
                    }
                }
            }
            icon("pencil", help: "Edit") {
                Task {
                    await pull()
                    editing = true
                    // Focus only lands once the field exists — the render pass
                    // after `editing` flips.
                    DispatchQueue.main.async { focused = true }
                }
            }
        }
    }

    private func icon(_ symbol: String, help: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 12, weight: .medium))
                .frame(width: 26, height: 26)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(Palette.inkFaint)
        .help(help)
        .disabled(loading)
    }

    /// Populate the buffer — and the diff baseline — from storage.
    @MainActor private func pull() async {
        if text.isEmpty, hasStored {
            loading = true
            text = await load() ?? ""
            loading = false
        }
        baseline = text
    }

    /// Blur is the save. Re-masks either way, but only writes on a real change.
    @MainActor private func commit() async {
        guard inputMode else { return }
        let value = text
        editing = false
        revealed = false
        guard value != baseline else { return }
        baseline = value
        await onCommit(value)
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

struct InlineRow<Content: View>: View {
    let key: String
    /// `.top` for controls taller than one line, so the key doesn't float
    /// against the middle of the stack.
    var alignment: VerticalAlignment = .center
    @ViewBuilder var content: Content

    var body: some View {
        HStack(alignment: alignment, spacing: 14) {
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
    @State private var editingToken = false
    @State private var busy = false
    @State private var result: Result<Void, String>?
    @FocusState private var urlFocused: Bool

    private enum Result<T, E> { case ok, err(E) }

    var body: some View {
        SectionCard(label: "Connection") {
            Field(label: "server url") {
                TextField("http://127.0.0.1:8848", text: $url)
                    .textFieldStyle(.plain)
                    .autocorrectionDisabled()
                    .focused($urlFocused)
                    .onSubmit { urlFocused = false }
                    .onChange(of: url) { _, _ in result = nil }
            }
            .onChange(of: urlFocused) { _, nowFocused in
                if !nowFocused { Task { await commitIfChanged() } }
            }
            SecretField(
                label: "api token", placeholder: "SQUELCH_API_TOKEN",
                hasStored: !(store.settings?.apiToken ?? "").isEmpty,
                load: { store.settings?.apiToken },
                onCommit: { _ in await commitIfChanged() },
                text: $token, editing: $editingToken
            )
            .onChange(of: token) { _, _ in result = nil }
            HStack(spacing: 10) {
                // Not "Save" — clicking away already did that. This re-checks a
                // connection that worked yesterday.
                Button(busy ? "testing…" : "Test") { Task { await testSave() } }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.accent)
                    .disabled(busy || url.trimmed.isEmpty || token.trimmed.isEmpty)
                switch result {
                case .ok:
                    StatusDot(color: Palette.positive, label: "connected · saved")
                case .err(let message):
                    Text(message).font(Typo.micro).foregroundStyle(Palette.danger)
                case nil:
                    EmptyView()
                }
            }
            SettingsHint(
                "Saved when you click away. The token lives in your macOS keychain and is sent only as a bearer header — never logged."
            )
        }
        .onAppear {
            url = store.settings?.serverURL ?? ""
            token = store.settings?.apiToken ?? ""
        }
    }

    /// Blur saves the pair, but only on a real change and only through
    /// `revalidate`, which proves the credentials against /client/stats before
    /// writing them — so blur-to-save can't strand you at the connect gate.
    private func commitIfChanged() async {
        let (u, t) = (url.trimmed, token.trimmed)
        guard !u.isEmpty, !t.isEmpty else { return }
        guard u != store.settings?.serverURL || t != store.settings?.apiToken else { return }
        await testSave()
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
        SectionCard(label: "Appearance") {
            InlineRow(key: "theme", alignment: .top) {
                // Three-state, so a Toggle can't hold it. macOS 26 draws
                // segmented Pickers in glass, indistinguishable from our own
                // GlassSegmented; radios read as stock AppKit.
                Picker("theme", selection: $prefs.theme) {
                    ForEach(ThemeChoice.allCases, id: \.self) { choice in
                        Text(choice.label).tag(choice)
                    }
                }
                .pickerStyle(.radioGroup)
                .labelsHidden()
            }
            SettingsHint("Auto follows the system appearance. \\ flips light/dark from anywhere.")
        }
    }
}

/// Dev mode adds re-triage affordances (sitrep masthead, thread viewer) that
/// reset LLM verdicts so the pipeline re-runs. Rule-decided and sealed rows are
/// never touched.
private struct DeveloperSection: View {
    @Environment(Prefs.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        SectionCard(label: "Developer") {
            InlineRow(key: "dev mode") {
                Toggle("dev mode", isOn: $prefs.developerMode)
                    .toggleStyle(.switch)
                    .labelsHidden()
                    .tint(Palette.accent)
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
        SectionCard(label: "You") {
            Field(label: "name") {
                TextField("shown in the sitrep greeting", text: $prefs.userName)
                    .textFieldStyle(.plain)
                    .autocorrectionDisabled()
            }
        }
    }
}

// MARK: - mail

/// Remote images: "Always" renders them in every email, "On demand" blocks them
/// at the frame CSP until the reader opts in per message. Tracking pixels are
/// stripped either way — see docs/SECURITY.md.
private struct MailSection: View {
    @Environment(Prefs.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        SectionCard(label: "Mail") {
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

/// The "how triage works" explainer above the budget fields. Stage cards show
/// live model names and daily caps from GET /client/triage-config, fetched here
/// rather than shared with the budget section.
private struct TriagePipelineSection: View {
    @State private var config: TriageConfig?

    var body: some View {
        SectionCard(label: "How triage works") {
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

/// Daily caps for both triage stages, plus a spend estimate from trailing-14d
/// usage and configured prices. Stage 1 runs on every email (one cap), stage 2
/// only on escalations (thread/sender/global). Save posts only changed fields.
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
        SectionCard(label: "Triage budget") {
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
                        StatusDot(color: Palette.positive, label: "saved")
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

    /// Each stage's "typical" is its trailing-14d call rate × per-call token
    /// means; the ceiling sums cap × per-call cost. Stages with no usage history
    /// are skipped rather than printed as a confident $0.00.
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
        // Post only fields whose parsed integer differs from the loaded value.
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

/// The blend ordering the sitrep "For your eyes" zone: one slider between time
/// and severity. The stored pref is the *urgency* share, and the slider is
/// inverted so dragging right toward "severity" lowers it. No save button.
private struct RankingSection: View {
    @Environment(Prefs.self) private var prefs

    var body: some View {
        SectionCard(label: "For your eyes") {
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

/// BYOK settings for the ⌘K assistant: the user's own Anthropic key, kept in the
/// OS keychain and never sent to the squelch server. It rests masked and is read
/// back only through `AssistantKeyStore.revealAsync`, the one sanctioned way for
/// a view to hold it; the request path never does — `LLMProxy` reads the keychain.
private struct AssistantSection: View {
    @Environment(Prefs.self) private var prefs

    @State private var status = AssistantKeyStatus.absent
    @State private var keyInput = ""
    @State private var editingKey = false
    @State private var busy = false
    @State private var note: Note?

    private enum Note: Equatable {
        case ok(String)
        case err(String)
    }

    var body: some View {
        @Bindable var prefs = prefs
        SectionCard(label: "Assistant") {
            SecretField(
                label: "api key (yours)", placeholder: "sk-ant-…",
                hasStored: status.present,
                load: { await AssistantKeyStore.revealAsync() },
                onCommit: { await saveKey($0) },
                text: $keyInput, editing: $editingKey
            )
            .onChange(of: keyInput) { _, _ in note = nil }
            HStack(spacing: 10) {
                if busy {
                    Text("saving…").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                }
                if status.present {
                    StatusDot(
                        color: Palette.positive,
                        label: "key set" + (status.provider.map { " · \($0.label)" } ?? ""))
                    Button("forget") { Task { await forgetKey() } }
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
                "Saved when you click away. Your key stays on this machine (macOS keychain) and is used only for the ⌘K assistant — never sent to the squelch server."
            )

            InlineRow(key: "model") {
                GlassSegmented(
                    options: AssistantModel.allCases.map { ($0, $0.shortLabel) },
                    selection: $prefs.assistantModel)
            }
            SettingsHint(prefs.assistantModel.label)
        }
        .task { status = await AssistantKeyStore.statusAsync() }
    }

    /// Called by the field on blur. Clearing `keyInput` afterwards keeps the
    /// plaintext out of view state once it is safely in the keychain.
    private func saveKey(_ value: String) async {
        let key = value.trimmed
        guard !key.isEmpty else { return }
        busy = true
        note = nil
        do {
            try AssistantKeyStore.set(key)
            keyInput = ""
            editingKey = false
            status = await AssistantKeyStore.statusAsync()
            note = .ok("key saved")
        } catch {
            note = .err(errText(error, "could not save key"))
        }
        busy = false
    }

    private func forgetKey() async {
        busy = true
        note = nil
        try? AssistantKeyStore.clear()
        keyInput = ""
        editingKey = false
        status = await AssistantKeyStore.statusAsync()
        note = .ok("key forgotten")
        busy = false
    }
}

// MARK: - privacy

/// The developer-telemetry level. Opt-out (Full is the default); the gate
/// itself lives in Analytics, which reads the pref on every event.
private struct PrivacySection: View {
    @Environment(Prefs.self) private var prefs

    var body: some View {
        @Bindable var prefs = prefs
        SectionCard(label: "Developer Telemetry") {
            InlineRow(key: "telemetry") {
                GlassSegmented(
                    options: TelemetryLevel.allCases.map { ($0, $0.label) },
                    selection: $prefs.telemetry)
            }
            SettingsHint(
                "Anonymous usage telemetry that helps improve Squelch. Full never includes email data or anything derived from it — no subjects, senders, bodies, labels, or assistant questions; only which screens and actions are used. Minimal sends app opens and screen views only. None sends nothing."
            )
        }
    }
}

// MARK: - account

private struct AccountSection: View {
    @Environment(AppStore.self) private var store
    @State private var usage: UsageResponse?

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            SectionCard(label: "Account") {
                meta("server", store.settings?.serverURL ?? "—")
                meta("triage model", usage?.model ?? "—")
                meta("provider", usage?.provider ?? "—")
            }
            SectionCard(label: "Danger") {
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
