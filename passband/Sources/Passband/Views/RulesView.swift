// Sender rules from GET /client/rules as a dense table: pattern, disposition,
// want_text (full only on the selected row), a client-side match count against
// the loaded updates (0 means likely dead, rendered dim), and updated-at.
// Keys: j/k select · n new · Enter/e edit · x delete, undo-first — the 5s toast
// recreates the deleted rule.

import SwiftUI

struct RulesView: View {
    @Environment(AppStore.self) private var store

    @State private var rulesState: Loadable<[SenderRule]> = .loading
    @State private var index = 0

    /// The loaded rules, which a reload deliberately keeps on screen while the
    /// next fetch is in flight.
    private var rules: [SenderRule] { rulesState.value ?? [] }

    /// How many currently-loaded updates each rule matched, counted client-side
    /// from the store rather than through a new endpoint.
    private var matchCounts: [Int: Int] {
        var counts: [Int: Int] = [:]
        for u in store.sitrep.standing + store.sitrep.new + store.sitrep.open {
            if let ruleId = u.matched_rule { counts[ruleId, default: 0] += 1 }
        }
        return counts
    }

    var body: some View {
        Group {
            if rulesState.isLoading && rules.isEmpty {
                BandNote("loading rules…")
            } else if let error = rulesState.error {
                BandNote(error)
            } else if rules.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 4) {
                        Text("no rules yet. press").font(Typo.rowSub)
                        Kbd("n")
                        Text("to create one, or").font(Typo.rowSub)
                        Kbd("t")
                        Text("on any message.").font(Typo.rowSub)
                    }
                    .foregroundStyle(Palette.inkFaintest)
                }
                .padding(.horizontal, 22)
                .padding(.vertical, 24)
                .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(spacing: 1) {
                            ForEach(Array(rules.enumerated()), id: \.element.id) { i, rule in
                                RuleRow(
                                    rule: rule, selected: i == index,
                                    matchCount: matchCounts[rule.id] ?? 0,
                                    onSelect: { index = i },
                                    onEdit: { edit(rule) })
                                .id(rule.id)
                            }
                        }
                        .padding(.horizontal, 18)
                        .padding(.vertical, 10)
                    }
                    .onChange(of: index) { _, i in
                        guard let rule = rules[safe: i] else { return }
                        withAnimation(Motion.scrollFollow) {
                            proxy.scrollTo(rule.id, anchor: .center)
                        }
                    }
                }
                footer
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .keyBindings(.modal, bindings)
        .task {
            RulesReload.shared.handler = { await load() }
            await load()
        }
        .onDisappear { RulesReload.shared.handler = nil }
        .onChange(of: rules.count) { _, count in
            index = max(0, min(index, max(0, count - 1)))
        }
    }

    private var footer: some View {
        KeyHintBar(hints: [
            KeyHint(["j", "k"], "select"),
            KeyHint("n", "new"),
            KeyHint(["e", "↵"], "edit"),
            KeyHint("x", "delete"),
        ])
    }

    private var bindings: [KeyBinding] {
        [
            KeyBinding("j", "next") { index = min(rules.count - 1, index + 1) },
            KeyBinding("k", "prev") { index = max(0, index - 1) },
            KeyBinding("n", "new rule") { create() },
            KeyBinding("e", "edit rule") { if let r = rules[safe: index] { edit(r) } },
            KeyBinding("Enter", "edit rule") { if let r = rules[safe: index] { edit(r) } },
            KeyBinding("x", "delete rule") {
                if let r = rules[safe: index] { Task { await delete(r) } }
            },
        ]
    }

    private func create() {
        store.openRuleEditor(
            RuleEditorRequest(rule: nil, onSaved: { Task { await load() } }))
    }

    private func edit(_ rule: SenderRule) {
        store.openRuleEditor(
            RuleEditorRequest(rule: rule, onSaved: { Task { await load() } }))
    }

    private func delete(_ rule: SenderRule) async {
        do {
            try await APIClient.shared.deleteRule(rule.id)
            Analytics.capture("rule_deleted", ["disposition": rule.disposition.rawValue])
            // Optimistic removal; a re-fetch happens on undo or next open.
            rulesState.value?.removeAll { $0.id == rule.id }
            // The undo toast recreates the rule from these cached values,
            // disposition included and explicit: an undo restores what was
            // there, it does not re-infer it from the want text. `sweep` is
            // never sent, so restoring a mute rule brings the rule back without
            // the side effect of resolving that sender's open mail — undo puts
            // things back, it does not go clear the inbox.
            store.pushUndo(
                kind: .ruleDelete, messageId: rule.id,
                label: "deleted rule \(rule.match_pattern)"
            ) {
                try await APIClient.shared.createRule(
                    CreateRuleBody(
                        match_pattern: rule.match_pattern, want: rule.want_text,
                        disposition: rule.disposition))
                await RulesReload.shared.reload()
            }
        } catch {
            store.pushToast(errText(error, "delete failed"), .error)
        }
    }

    private func load() async {
        await $rulesState.load("rules failed") {
            try await APIClient.shared.listRules()
        }
    }
}

/// Lets an undo fired from the global toast stack — which outlives this view's
/// closures — ask the rules list to re-pull if it is still on screen.
@MainActor
final class RulesReload {
    static let shared = RulesReload()
    /// Set only while a RulesView is mounted; nil means the undo has nothing to
    /// refresh, which is correct.
    var handler: (@MainActor () async -> Void)?
    private init() {}
    func reload() async { await handler?() }
}

/// Chip colour per disposition, kept off the wire type so the model layer stays
/// free of SwiftUI.
extension Disposition {
    fileprivate var tone: Color {
        switch self {
        case .surface: Palette.positive
        case .squelch: Palette.inkFaint
        case .filtered: Palette.danger
        }
    }
}

private struct RuleRow: View {
    let rule: SenderRule
    let selected: Bool
    let matchCount: Int
    let onSelect: () -> Void
    let onEdit: () -> Void

    private var dispositionTone: Color { rule.disposition.tone }

    var body: some View {
        ListRow(
            selected: selected, cornerRadius: 8, hPadding: 11, vPadding: 7, action: onSelect
        ) { selected, _ in
            HStack(spacing: 10) {
                Chip(text: rule.disposition.label, tone: dispositionTone, filled: true)
                    .frame(width: 62, alignment: .leading)
                Text(rule.match_pattern)
                    .font(Typo.mono(11))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                    .frame(width: 190, alignment: .leading)
                Text(rule.want_text.isEmpty ? "·" : rule.want_text)
                    .font(Typo.micro)
                    .foregroundStyle(
                        rule.want_text.isEmpty ? Palette.inkFaintest : Palette.inkDim
                    )
                    .lineLimit(selected ? nil : 1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .help(rule.want_text)
                Text("\(matchCount)×")
                    .font(Typo.num(10))
                    .foregroundStyle(matchCount == 0 ? Palette.inkFaintest : Palette.accent)
                    .help(
                        matchCount == 0
                            ? "no currently-loaded updates match this rule"
                            : "\(matchCount) loaded update(s) matched")
                Text(Fmt.relAge(rule.updated_at).isEmpty ? "·" : Fmt.relAge(rule.updated_at))
                    .font(Typo.num(10))
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(width: 34, alignment: .trailing)
            }
        }
        .simultaneousGesture(TapGesture(count: 2).onEnded { onEdit() })
    }
}

// MARK: - rule editor

/// The `t` tune-sender modal, and the rules view's create/edit.
///
/// NL-FIRST: the plain-English want text IS the rule, and it is stored VERBATIM
/// — nothing rewrites it on the way out, because the daemon hands it to Stage 2
/// as the owner's own instruction. The glob that decides WHO the rule covers is
/// derived from the sender on screen and demoted behind `advanced`.
///
/// The disposition is NOT asked for. The daemon reads the same sentence and
/// resolves allow/mute/filter from it, so the simple card has exactly one
/// question on it; advanced carries an override for the times the guess is
/// wrong. That inference sees the OWNER'S RULE TEXT ONLY — never a subject,
/// body or header — so no sender can write themselves an allow rule.
struct RuleEditor: View {
    let request: RuleEditorRequest
    let onClose: () -> Void

    @Environment(AppStore.self) private var store

    @State private var pattern: String
    @State private var want: String
    /// The OVERRIDE. nil means auto: the request goes out with no disposition and
    /// the daemon resolves one from the want text. Edit opens on the rule's
    /// current value, which is explicit by definition — the rule already is
    /// something, and reopening it must not silently re-roll that.
    @State private var disposition: Disposition?
    /// Whether the raw match pattern is on screen and editable.
    @State private var advanced: Bool
    @State private var saving = false
    @State private var error: String?
    /// Rules whose match_pattern already covers `request.sender`; nil until the
    /// fetch settles, so "none yet" is never claimed before we know.
    @State private var inEffect: [SenderRule]?
    @FocusState private var focusedField: FocusTarget?

    private enum FocusTarget { case pattern, want }

    enum Mode { case tune, create, edit }

    init(request: RuleEditorRequest, onClose: @escaping () -> Void) {
        self.request = request
        self.onClose = onClose
        if let rule = request.rule {
            _pattern = State(initialValue: rule.match_pattern)
            _want = State(initialValue: rule.want_text)
            _disposition = State(initialValue: rule.disposition)
            // Editing IS pattern work: hiding the glob would hide the thing
            // being edited.
            _advanced = State(initialValue: true)
        } else {
            _pattern = State(
                initialValue: request.pattern
                    ?? request.sender.map(SenderID.patternFromSender) ?? "")
            _want = State(initialValue: request.want ?? "")
            // nil = auto, which is what every ordinary create/tune sends. A
            // caller that names one is deliberately overriding on the user's
            // behalf, so its choice travels as an explicit value.
            _disposition = State(initialValue: request.disposition)
            // With no sender there is nothing to derive a glob from, so the
            // glob field is the only way to say who the rule is about.
            _advanced = State(initialValue: request.sender == nil)
        }
    }

    private var mode: Mode {
        if request.rule != nil { return .edit }
        if request.sender != nil { return .tune }
        return .create
    }

    private var title: String {
        switch mode {
        case .edit: "edit rule"
        case .create: "new rule"
        case .tune: "tune sender"
        }
    }

    var body: some View {
        OverlayScrim(onDismiss: onClose) {
            ModalCard(width: 470) {
                header
                alreadyInEffect
                wantField
                appliesTo
                advancedSection

                if let error {
                    Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
                }

                footer
            }
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "cancel", allowInInput: true) { onClose() },
            // Tab only means something while the override is on screen. With
            // advanced closed it DECLINES, so the key falls through to AppKit
            // and moves focus the way it does in every other card.
            KeyBinding(declining: "Tab", "cycle disposition", allowInInput: true) {
                guard advanced else { return false }
                cycle(1)
                return true
            },
            KeyBinding(declining: "shift+Tab", "cycle disposition (back)", allowInInput: true) {
                guard advanced else { return false }
                cycle(-1)
                return true
            },
            // ⌘-chorded because the want field owns every bare letter, and the
            // hint row below teaches it.
            KeyBinding("d", "advanced", meta: true, allowInInput: true) { toggleAdvanced() },
            // BOTH SPELLINGS SAVE. Bare Enter is this card's own convention and
            // the hint row has taught it since the card existed. ⌘Enter is the
            // one every OTHER multi-line surface in the app uses — the group
            // editor, the composer, the inline reply — so a hand arriving from
            // any of them tries it first and, until now, got nothing. They
            // coexist because dispatch matches `meta` exactly in both
            // directions, so neither binding can answer for the other.
            KeyBinding("Enter", "save rule", allowInInput: true) { Task { await save() } },
            KeyBinding("Enter", "save rule", meta: true, allowInInput: true) {
                Task { await save() }
            },
        ])
        .onAppear {
            // The want text is the rule, so it takes focus everywhere except
            // create-from-scratch, where there is no sender and the empty glob
            // is the first thing that has to be written.
            focusedField = mode == .create ? .pattern : .want
        }
        .task { await loadInEffect() }
    }

    /// Title plus WHO the rule is about. The sender is email-derived, so it is
    /// rendered as Text only, never as markup.
    private var header: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title)
                .font(Typo.sectionLabel)
                .foregroundStyle(Palette.ink)
                .textCase(.uppercase)
            if let sender = request.sender {
                HStack(spacing: 8) {
                    Avatar(sender: sender, size: 26)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(SenderCache.resolved(sender).displayName)
                            .font(Typo.row)
                            .foregroundStyle(Palette.ink)
                            .lineLimit(1)
                        if SenderCache.resolved(sender).displayName.lowercased()
                            != SenderID.address(sender)
                        {
                            Text(SenderID.address(sender))
                                .font(Typo.mono(10))
                                .foregroundStyle(Palette.inkFaintest)
                                .lineLimit(1)
                        }
                    }
                }
            } else {
                subtitle
            }
        }
        .padding(.bottom, 2)
    }

    /// The rule itself, in the user's own words, and now the ONLY question on
    /// the simple card: the sentence decides both what Stage 2 does with the
    /// mail and which disposition the rule lands on. Multi-line because a real
    /// one is a sentence, not a keyword. The examples stay polarity-diverse on
    /// purpose, since a card that only ever showed "i dont care about X" would
    /// teach people this box is a mute button.
    private var wantField: some View {
        Field(label: wantLabel) {
            TextField(
                "in plain english. e.g. always show these, or: only order confirmations, or: i dont care about the approval emails",
                text: $want, axis: .vertical
            )
            .textFieldStyle(.plain)
            .lineLimit(2...5)
            .focused($focusedField, equals: .want)
        }
    }

    private var wantLabel: String {
        mode == .tune ? "what do you want from this sender?" : "what do you want?"
    }

    /// The two things the card decided FOR you, both dim and neither editable
    /// here: the glob it derived, and where the outcome is coming from. Visible
    /// because nothing about a rule should be a surprise; demoted because
    /// neither one is the question being asked.
    @ViewBuilder
    private var appliesTo: some View {
        if !advanced {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 5) {
                    Text("applies to").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    Text(pattern)
                        .font(Typo.mono(10))
                        .foregroundStyle(Palette.inkFaint)
                        .lineLimit(1)
                    Spacer()
                }
                Text(outcomeNote)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
            }
        }
    }

    /// Where the disposition is coming from, said once, in the collapsed card.
    private var outcomeNote: String {
        guard let disposition else { return "the outcome is read from what you wrote." }
        return "outcome: \(disposition.label) · change it under advanced"
    }

    /// The override. Lives in advanced because the honest default is now "let
    /// the daemon read the sentence", and a control on the front of the card
    /// would re-ask a question that has already been answered above it.
    private var dispositionOverride: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 4) {
                Text("disposition").font(Typo.micro).foregroundStyle(Palette.inkFaint)
                Kbd("tab")
                Text("to cycle").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                Spacer()
            }
            GlassSegmented(options: dispositionOptions, selection: $disposition)
            Text(disposition?.hint ?? "auto: resolved from the line you wrote above.")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
        }
    }

    /// "auto" IS one of the values, not the absence of one: nil is the default
    /// state, so the travelling pane parks on it and the control never reads as
    /// unanswered.
    private var dispositionOptions: [(value: Disposition?, label: String)] {
        var options: [(value: Disposition?, label: String)] = [(value: nil, label: "auto")]
        options.append(
            contentsOf: Disposition.allCases.map { (value: Optional($0), label: $0.label) })
        return options
    }

    /// The cancel button doubles as the Escape hint on a Mac; a phone dismisses
    /// by tapping off the card and has no key to name.
    private var cancelLabel: String {
        #if os(macOS)
            "esc cancel"
        #else
            "cancel"
        #endif
    }

    @ViewBuilder
    private var advancedSection: some View {
        Button(action: toggleAdvanced) {
            HStack(spacing: 5) {
                Image(systemName: advanced ? "chevron.down" : "chevron.right")
                    .font(.system(size: 8, weight: .semibold))
                Text("advanced").font(Typo.micro)
                #if os(macOS)
                    Kbd("⌘d")
                #endif
                Spacer()
            }
            .foregroundStyle(Palette.inkFaint)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)

        if advanced {
            Field(label: "match pattern") {
                TextField("*@example.com", text: $pattern)
                    .textFieldStyle(.plain)
                    .font(Typo.mono(12))
                    .focused($focusedField, equals: .pattern)
                    .autocorrectionDisabled()
            }
            dispositionOverride
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 9) {
            // A row of keycaps for four keys a phone does not have. The editor is
            // reachable there now (tune is a row swipe), so the hints are fenced
            // rather than shown as instructions nobody can follow.
            #if os(macOS)
                HStack(spacing: 4) {
                    // Taught only while it does something: tab is a focus key
                    // everywhere else in the app.
                    if advanced {
                        KeyHint("tab", "disposition")
                        Text("·").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    }
                    KeyHint("⌘d", "advanced")
                    Text("·").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    KeyHint("⌥↵", "new line")
                    Text("·").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    KeyHint(["↵", "⌘↵"], "save")
                    Spacer()
                }
            #endif
            HStack(spacing: 8) {
                if let blocked {
                    Text(blocked)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.warn)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 8)
                Button(cancelLabel, action: onClose).buttonStyle(.glass)
                Button(saving ? "saving…" : (mode == .edit ? "update rule" : "save rule")) {
                    Task { await save() }
                }
                .buttonStyle(.glassProminent)
                .tint(Palette.accent)
                .disabled(saving || blocked != nil)
            }
        }
    }

    /// Why save is shut, in the user's terms, or nil when it is open. Checked
    /// BEFORE the round trip so the filtered-needs-want rule the store enforces
    /// lands as a hint rather than as an error over a form you already left.
    ///
    /// The want text is what holds save shut now: on auto it is the only input
    /// the disposition can be resolved from, so an empty one asks the daemon to
    /// guess from nothing. An explicit allow/mute override says the outcome out
    /// loud, so it may save wantless — which is also what keeps a legacy
    /// block-created rule (empty want, explicit mute) editable.
    private var blocked: String? {
        if pattern.trimmed.isEmpty { return "needs a match pattern" }
        if want.trimmed.isEmpty {
            if disposition == .filtered {
                return "filter needs a line saying what you do or do not want"
            }
            if disposition == nil { return "say what you want in plain english" }
        }
        return nil
    }

    /// ⌘d and the disclosure row are one gesture. Collapsing never discards the
    /// glob, it only stops showing it, so a hand-written pattern survives a
    /// stray toggle.
    private func toggleAdvanced() {
        withAnimation(Motion.disclose) { advanced.toggle() }
        focusedField = advanced ? .pattern : .want
    }

    /// What already governs this sender, above the field that would add another,
    /// scoped by the same glob the triage pipeline matches with. Tune mode only:
    /// create has no address to scope by, edit already shows the rule it edits.
    /// One line per rule now that the card leads with the sender and the want
    /// text; the full want stays in the tooltip.
    @ViewBuilder
    private var alreadyInEffect: some View {
        if mode == .tune, let inEffect, !inEffect.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                Text("already in effect")
                    .font(Typo.sectionLabel)
                    .foregroundStyle(Palette.inkFaint)
                    .textCase(.uppercase)
                ScrollView {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(inEffect) { rule in
                            HStack(spacing: 9) {
                                Chip(
                                    text: rule.disposition.label,
                                    tone: rule.disposition.tone, filled: true
                                )
                                .frame(width: 54, alignment: .leading)
                                Text(rule.match_pattern)
                                    .font(Typo.mono(11))
                                    .foregroundStyle(Palette.ink)
                                    .lineLimit(1)
                                Text(rule.want_text.isEmpty ? "·" : rule.want_text)
                                    .font(Typo.micro)
                                    .foregroundStyle(
                                        rule.want_text.isEmpty
                                            ? Palette.inkFaintest : Palette.inkDim
                                    )
                                    .lineLimit(1)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .help(rule.want_text)
                            }
                        }
                    }
                }
                // A sender with a pile of rules must not push the field that
                // writes the next one off the card.
                .frame(maxHeight: 66)
            }
            .padding(.bottom, 2)
        }
    }

    /// Best-effort: a failed lookup leaves the strip absent rather than claiming
    /// the sender is unruled, which is the one wrong answer.
    private func loadInEffect() async {
        guard mode == .tune, let sender = request.sender else { return }
        guard let all = try? await APIClient.shared.listRules() else { return }
        let address = SenderID.address(sender)
        inEffect = all.filter {
            Newsletters.ruleMatches(pattern: $0.match_pattern, address: address)
        }
    }

    @ViewBuilder
    private var subtitle: some View {
        switch mode {
        case .tune:
            Text("from \(request.sender ?? "")")
                .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
        case .edit:
            Text("editing \(request.rule?.match_pattern ?? "")")
                .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
        case .create:
            Text("define a sender rule from scratch")
                .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
        }
    }

    /// Walks the override ring, auto included, so handing the decision back is
    /// as cheap as taking it — shift+tab from auto lands straight on filter.
    private func cycle(_ direction: Int) {
        var ring: [Disposition?] = [nil]
        ring.append(contentsOf: Disposition.allCases.map(Optional.init))
        let i = ring.firstIndex(of: disposition) ?? 0
        disposition = ring[(i + direction + ring.count) % ring.count]
    }

    /// Create POSTs (the store upserts on match_pattern, so re-tuning a sender
    /// edits its rule instead of stacking another); edit PUTs the id, so a
    /// pattern change moves the row it is looking at rather than orphaning it.
    /// The want text goes out exactly as typed, trimmed at the ends and nothing
    /// else.
    ///
    /// `disposition` rides only when the user overrode it; otherwise the field
    /// is absent and the RESPONSE tells us what the rule became. That answer is
    /// what the toast and the analytics report, because the client no longer
    /// knows it up front.
    private func save() async {
        guard !saving, blocked == nil else { return }
        let body = CreateRuleBody(
            match_pattern: pattern.trimmed, want: want.trimmed, disposition: disposition)
        // A DEMONSTRATION SAVE stops here: the editor still validates and still
        // builds the body, so the ceremony is the real one, but nothing leaves
        // the machine and no rule_created event is claimed for a rule that does
        // not exist. Only the onboarding tour sets this.
        if let intercept = request.intercept {
            intercept(body)
            // The glob reads as a domain in the toast; nobody demonstrating a
            // rule needs to see the star.
            let shown =
                body.match_pattern.hasPrefix("*@")
                ? String(body.match_pattern.dropFirst(2)) : body.match_pattern
            // The outcome clause only when the user said the outcome out loud.
            // On auto it is the daemon that resolves the disposition, and this
            // save never asked it — so naming one here would be a guess.
            let outcome = body.disposition.map { " → \($0.label)" } ?? ""
            store.pushToast("rule saved · \(shown)\(outcome) (demo)", .success)
            request.onSaved?()
            onClose()
            return
        }
        saving = true
        error = nil
        do {
            let saved: CreatedRule
            if let existing = request.rule {
                saved = try await APIClient.shared.updateRule(existing.id, body)
            } else {
                saved = try await APIClient.shared.createRule(body)
            }
            // A daemon too old to answer with the resolved value leaves us with
            // whatever we sent — nil there means we genuinely do not know, and
            // the toast says less rather than guessing.
            let resolved = saved.disposition ?? disposition
            var properties: [String: Any] = [
                "has_want": !want.trimmed.isEmpty,
                "edit": request.rule != nil,
            ]
            if let resolved { properties["disposition"] = resolved.rawValue }
            Analytics.capture("rule_created", properties)
            let outcome = resolved.map { " → \($0.label)" } ?? ""
            store.pushToast(
                "\(request.rule != nil ? "rule updated" : "rule saved") · \(pattern.trimmed)\(outcome)",
                .success)
            request.onSaved?()
            onClose()
        } catch let apiError as APIError where apiError.kind == .forbidden {
            error = "no write credential · run squelchd auth --write"
            saving = false
        } catch {
            self.error = errText(error, "save failed")
            saving = false
        }
    }
}
