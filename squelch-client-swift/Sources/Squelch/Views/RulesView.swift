// RULES MANAGEMENT. The full sender-rule surface: every rule from
// GET /client/rules as a dense table — match pattern, disposition chip,
// want_text (truncated; full on selection), a client-side match count against
// the currently-loaded updates (0 = a likely-dead rule, rendered dim), and a
// relative updated-at.
//
// Keys: j/k select · n new (blank editor) · Enter/e edit · x delete
// (undo-first: the 5s toast recreates the rule).
//
// Ported from squelch-desktop/src/components/{RulesView,RuleEditor}.tsx.

import SwiftUI

struct RulesView: View {
    @Environment(AppStore.self) private var store
    @Namespace private var rulesGlass

    @State private var rules: [SenderRule] = []
    @State private var error: String?
    @State private var loading = true
    @State private var index = 0

    /// Client-side match counts: how many currently-loaded updates each rule
    /// matched. No new endpoint — we read the store's updates.
    private var matchCounts: [Int: Int] {
        var counts: [Int: Int] = [:]
        for u in store.sitrep.standing + store.sitrep.new + store.sitrep.open {
            if let ruleId = u.matched_rule { counts[ruleId, default: 0] += 1 }
        }
        return counts
    }

    var body: some View {
        Group {
            if loading && rules.isEmpty {
                BandNote("loading rules…")
            } else if let error {
                BandNote(error)
            } else if rules.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 4) {
                        Text("no rules yet — press").font(Typo.rowSub)
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
                                    glassNamespace: rulesGlass,
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
                        withAnimation(.easeOut(duration: 0.12)) {
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
        HStack(spacing: 4) {
            Kbd("j"); Kbd("k")
            Text("select").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Text("·").foregroundStyle(Palette.inkFaintest)
            Kbd("n"); Text("new").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Text("·").foregroundStyle(Palette.inkFaintest)
            Kbd("e"); Kbd("↵")
            Text("edit").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Text("·").foregroundStyle(Palette.inkFaintest)
            Kbd("x"); Text("delete").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Spacer()
        }
        .padding(.horizontal, 22)
        .padding(.vertical, 10)
        .overlay(alignment: .top) { Rectangle().fill(Palette.hairline).frame(height: 0.5) }
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
            // Optimistic removal; a re-fetch happens on undo or next open.
            rules.removeAll { $0.id == rule.id }
            // Undo-first: the 5s toast recreates the rule from its cached values.
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
        loading = true
        defer { loading = false }
        do {
            rules = try await APIClient.shared.listRules()
            error = nil
        } catch {
            self.error = errText(error, "rules failed")
        }

    }
}

/// A tiny hook so an undo fired from the global toast stack — which outlives
/// this view's closures, and may fire after the view has gone away — can still
/// ask the rules list to re-pull if it is still on screen.
@MainActor
final class RulesReload {
    static let shared = RulesReload()
    /// Set while a RulesView is mounted; nil otherwise (the undo then simply
    /// has nothing to refresh, which is correct).
    var handler: (@MainActor () async -> Void)?
    private init() {}
    func reload() async { await handler?() }
}

private struct RuleRow: View {
    let glassNamespace: Namespace.ID
    let rule: SenderRule
    let selected: Bool
    let matchCount: Int
    let onSelect: () -> Void
    let onEdit: () -> Void

    @State private var hovering = false

    private var dispositionTone: Color {
        switch rule.disposition {
        case .surface: Palette.positive
        case .squelch: Palette.inkFaint
        case .filtered: Palette.danger
        }
    }

    var body: some View {
        Button(action: onSelect) {
            HStack(spacing: 10) {
                Chip(text: rule.disposition.label, tone: dispositionTone, filled: true)
                    .frame(width: 62, alignment: .leading)
                Text(rule.match_pattern)
                    .font(Typo.mono(11))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                    .frame(width: 190, alignment: .leading)
                Text(rule.want_text.isEmpty ? "—" : rule.want_text)
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
                Text(Fmt.relAge(rule.updated_at).isEmpty ? "—" : Fmt.relAge(rule.updated_at))
                    .font(Typo.num(10))
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(width: 34, alignment: .trailing)
            }
            .padding(.horizontal, 11)
            .padding(.vertical, 7)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .selectionGlass(
            selected, hovering: hovering, cornerRadius: 8,
            id: "rules-selection", in: glassNamespace)
        .onHover { hovering = $0 }
        .simultaneousGesture(TapGesture(count: 2).onEnded { onEdit() })
    }
}

// MARK: - rule editor

/// RULE EDITOR — the `t` (tune sender) modal, and the rules view's create/edit.
///
/// Prefilled with `*@domain` derived from the selected sender, a free want-text
/// field describing the desired behavior, and a disposition cycled with Tab.
///
/// EDIT is create-new THEN delete-old, in that order, so a mid-flight failure
/// can never lose the rule (worst case: a transient duplicate). The server has
/// no PUT /client/rules/{id}; when it grows one this becomes a single call.
struct RuleEditor: View {
    let request: RuleEditorRequest
    let onClose: () -> Void

    @Environment(AppStore.self) private var store

    @State private var pattern: String
    @State private var want: String
    @State private var disposition: Disposition
    @State private var saving = false
    @State private var error: String?
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
        } else {
            _pattern = State(
                initialValue: request.pattern
                    ?? request.sender.map(SenderID.patternFromSender) ?? "")
            _want = State(initialValue: request.want ?? "")
            _disposition = State(initialValue: request.disposition ?? .squelch)
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
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(Typo.sectionLabel)
                        .foregroundStyle(Palette.ink)
                        .textCase(.uppercase)
                    subtitle
                }
                .padding(.bottom, 4)

                Field(label: "match pattern") {
                    TextField("*@example.com", text: $pattern)
                        .textFieldStyle(.plain)
                        .font(Typo.mono(12))
                        .focused($focusedField, equals: .pattern)
                        .autocorrectionDisabled()
                }
                Field(label: "want (what should happen)") {
                    TextField("e.g. only surface if it mentions an invoice", text: $want)
                        .textFieldStyle(.plain)
                        .focused($focusedField, equals: .want)
                }

                VStack(alignment: .leading, spacing: 5) {
                    HStack(spacing: 4) {
                        Text("disposition")
                            .font(Typo.micro).foregroundStyle(Palette.inkFaint)
                        Kbd("tab")
                        Text("to cycle").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    }
                    HStack(spacing: 7) {
                        ForEach(Disposition.allCases, id: \.self) { d in
                            Button {
                                disposition = d
                            } label: {
                                Text(d.label)
                                    .font(Typo.chip)
                                    .padding(.horizontal, 11).padding(.vertical, 4)
                            }
                            .buttonStyle(
                                disposition == d
                                    ? AnyButtonStyle(GlassProminentButtonStyle())
                                    : AnyButtonStyle(GlassButtonStyle())
                            )
                            .tint(Palette.accent)
                            .foregroundStyle(disposition == d ? .white : Palette.inkFaint)
                        }
                    }
                    Text(disposition.hint)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                }

                if let error {
                    Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
                }

                HStack(spacing: 8) {
                    Spacer()
                    Button("esc cancel", action: onClose).buttonStyle(.glass)
                    Button(saving ? "saving…" : (mode == .edit ? "update rule" : "save rule")) {
                        Task { await save() }
                    }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.accent)
                    .disabled(saving)
                }
            }
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "cancel", allowInInput: true) { onClose() },
            KeyBinding("Tab", "cycle disposition", allowInInput: true) { cycle(1) },
            KeyBinding("shift+Tab", "cycle disposition (back)", allowInInput: true) { cycle(-1) },
            KeyBinding("Enter", "save rule", allowInInput: true) { Task { await save() } },
        ])
        .onAppear {
            // From-scratch create starts on the (empty) pattern field;
            // otherwise the pattern is prefilled so focus lands on the want text.
            focusedField = mode == .create ? .pattern : .want
        }
    }

    @ViewBuilder
    private var subtitle: some View {
        switch mode {
        case .tune:
            Text("from \(request.sender ?? "")")
                .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
        case .edit:
            Text("editing \(request.rule?.match_pattern ?? "") · save replaces it")
                .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
        case .create:
            Text("define a sender rule from scratch")
                .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
        }
    }

    private func cycle(_ direction: Int) {
        let all = Disposition.allCases
        guard let i = all.firstIndex(of: disposition) else { return }
        disposition = all[(i + direction + all.count) % all.count]
    }

    private func save() async {
        guard !saving else { return }
        guard !pattern.trimmed.isEmpty else {
            error = "match pattern is empty"
            return
        }
        saving = true
        error = nil
        do {
            try await APIClient.shared.createRule(
                CreateRuleBody(
                    match_pattern: pattern.trimmed, want: want.trimmed, disposition: disposition))
            if let existing = request.rule {
                try await APIClient.shared.deleteRule(existing.id)
            }
            store.pushToast(
                "\(request.rule != nil ? "rule updated" : "rule saved") · \(pattern.trimmed) → \(disposition.rawValue)",
                .success)
            request.onSaved?()
            onClose()
        } catch let apiError as APIError where apiError.kind == .forbidden {
            error = "no write credential — run `squelchd auth --write`"
            saving = false
        } catch {
            self.error = errText(error, "save failed")
            saving = false
        }
    }
}
