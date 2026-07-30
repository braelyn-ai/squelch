// Auth mail: login codes, password resets, sign-in alerts, verifications.
// Metadata only — a body is never fetched to render the list. Reveal is always a
// human action and writes a server-side audit row, so this page never reveals on
// load; digits appear immediately only for codes the arrival flow already
// revealed. Revealed codes live in this view's state and are never persisted.

import SwiftUI

struct AuthView: View {
    @Environment(AppStore.self) private var store
    @Namespace private var authGlass

    /// Newer than this counts as live and heads the list.
    private static let liveWindow: TimeInterval = 60 * 60
    /// Cards in the decision rail before it stops growing.
    private static let maxDecisionCards = 6

    enum Filter: String, CaseIterable, Identifiable {
        case all, live, codes, risk
        var id: String { rawValue }
        var label: String {
            switch self {
            case .all: "Everything"
            case .live: "Last hour"
            case .codes: "Codes only"
            case .risk: "Risk"
            }
        }
    }

    @State private var filter: Filter = .all
    @State private var index = 0
    @State private var revealing: SealedMeta?
    @State private var copied = false
    @State private var busy: Int?
    /// Codes revealed this session, by message id; a present key with a nil
    /// value means revealed but unreadable. View state only, never persisted.
    @State private var codes: [Int: String?] = [:]
    /// Ids archived from this page, hidden optimistically (the list is a poll).
    @State private var archived: Set<Int> = []

    private var decisions = AuthDecisions.shared

    private var visible: [SealedMeta] {
        store.sitrep.sealed.filter { !archived.contains($0.id) }
    }
    private var shown: [SealedMeta] { visible.filter { matches($0, filter) } }
    private var focus: SealedMeta? { shown[safe: index] }
    private var live: [SealedMeta] { shown.filter { age($0.received_at) <= Self.liveWindow } }
    private var archive: [SealedMeta] { shown.filter { age($0.received_at) > Self.liveWindow } }
    private var openDecisions: [SealedMeta] {
        visible.filter { AuthDecisions.needsDecision($0.kind) && decisions.decision($0.id) == nil }
    }

    /// Seconds since an ISO stamp; huge when missing/invalid (=> treated old).
    private func age(_ iso: String?) -> TimeInterval {
        guard let d = Fmt.date(iso) else { return .greatestFiniteMagnitude }
        return Date().timeIntervalSince(d)
    }

    private func matches(_ m: SealedMeta, _ f: Filter) -> Bool {
        switch f {
        case .live: age(m.received_at) <= Self.liveWindow
        case .codes: AuthCode.isCodeKind(m.kind)
        // The ones that mean someone may be trying to get in.
        case .risk: AuthDecisions.needsDecision(m.kind)
        case .all: true
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            RoutedHeader(title: "Auth — codes, alerts & resets") {
                HStack(spacing: 12) {
                    StatusDot(
                        color: Palette.lock,
                        label: "\(live.count) live · \(openDecisions.count) awaiting you",
                        tone: Palette.inkFaint, size: 5)
                    HStack(spacing: 3) {
                        Kbd("j")
                        Kbd("k")
                        Text("move").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                        Kbd("c")
                        Text("copy").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                        Kbd("r")
                        Text("reveal").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    }
                }
            }

            HStack(alignment: .top, spacing: 16) {
                mainColumn
                rail
            }
            .padding(.horizontal, 22)
            .padding(.vertical, 16)
        }
        .keyBindings(.modal, bindings)
        .overlay {
            if let revealing {
                RevealPanel(meta: revealing) { self.revealing = nil }
            }
        }
        .onChange(of: store.authQueue.count) { _, _ in seedFromArrivals() }
        .onAppear { seedFromArrivals() }
        .onChange(of: shown.count) { _, count in
            index = max(0, min(index, max(0, count - 1)))
        }
        .onChange(of: focus?.id) { _, _ in copied = false }
    }

    // MARK: - main column

    private var mainColumn: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                if let focus {
                    FocusPanel(
                        meta: focus,
                        code: codes[focus.id] ?? nil,
                        revealed: codes.index(forKey: focus.id) != nil,
                        busy: busy == focus.id,
                        copied: copied,
                        onCopy: { copy() },
                        onReveal: { Task { await reveal(focus) } },
                        onArchive: { Task { await archiveFocused(focus) } })
                } else {
                    Text(
                        "nothing here — login codes, password resets and sign-in alerts land on this page as they arrive."
                    )
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.vertical, 24)
                }

                HStack(spacing: 6) {
                    ForEach(Filter.allCases) { f in
                        Button {
                            filter = f
                            index = 0
                        } label: {
                            Text(f.label)
                                .font(Typo.micro)
                                .padding(.horizontal, 9)
                                .padding(.vertical, 3)
                        }
                        .buttonStyle(.glass)
                        .foregroundStyle(filter == f ? Palette.accent : Palette.inkFaint)
                    }
                    Spacer(minLength: 6)
                    Text("\(shown.count) of \(visible.count) shown")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                }

                ScrollViewReader { proxy in
                    GlassEffectContainer(spacing: 2) {
                        VStack(spacing: 1) {
                            section("Live · last hour", rows: live)
                            section("Archive", rows: archive)
                        }
                    }
                    .onChange(of: index) { _, i in
                        guard let m = shown[safe: i] else { return }
                        withAnimation(Motion.scrollFollow) {
                            proxy.scrollTo(m.id, anchor: .center)
                        }
                    }
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .top)
    }

    @ViewBuilder
    private func section(_ label: String, rows: [SealedMeta]) -> some View {
        if !rows.isEmpty {
            Text(label)
                .font(Typo.sectionLabel)
                .foregroundStyle(Palette.inkFaintest)
                .textCase(.uppercase)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 10)
                .padding(.bottom, 4)
            ForEach(rows) { m in
                let i = shown.firstIndex(of: m) ?? 0
                AuthRow(
                    glassNamespace: authGlass,
                    meta: m, selected: i == index, code: codes[m.id] ?? nil,
                    onSelect: { index = i },
                    onOpen: { Task { await reveal(m) } })
                .id(m.id)
            }
        }
    }

    // MARK: - rail

    private var rail: some View {
        VStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 9) {
                HStack {
                    Text("Needs a decision")
                        .font(Typo.zoneTitle)
                        .foregroundStyle(Palette.ink)
                    Spacer()
                    Text("\(openDecisions.count) open")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                }
                if openDecisions.isEmpty {
                    EmptyNote(
                        "nothing to rule on. sign-in alerts and reset requests show up here.")
                } else {
                    ForEach(openDecisions.prefix(Self.maxDecisionCards)) { m in
                        DecisionCard(
                            meta: m,
                            onMine: { decisions.set(m.id, .mine) },
                            onNotMine: {
                                // Not a dismissal: record the verdict, then open
                                // the message so the human can act on it.
                                decisions.set(m.id, .notMine)
                                revealing = m
                            })
                    }
                }
            }
            .zonePadding()
            .frame(maxWidth: .infinity, alignment: .leading)
            .squelchGlass(.pane, cornerRadius: 18, tint: Palette.lockSoft.opacity(0.7))

            ShredderCard()
        }
        .frame(width: 306)
    }

    // MARK: - keymap

    private var bindings: [KeyBinding] {
        [
            KeyBinding("j", "next") { index = min(shown.count - 1, index + 1) },
            KeyBinding("k", "prev") { index = max(0, index - 1) },
            KeyBinding("ArrowDown", "next") { index = min(shown.count - 1, index + 1) },
            KeyBinding("ArrowUp", "prev") { index = max(0, index - 1) },
            KeyBinding("r", "reveal") { if let focus { Task { await reveal(focus) } } },
            KeyBinding("Enter", "reveal") { if let focus { Task { await reveal(focus) } } },
            KeyBinding("c", "copy code") { copy() },
        ]
    }

    // MARK: - actions

    /// Code kinds resolve to digits inline; anything whose point is its prose
    /// opens the full one-time reveal panel instead.
    private func reveal(_ m: SealedMeta) async {
        guard AuthCode.isCodeKind(m.kind) else {
            revealing = m
            return
        }
        guard codes.index(forKey: m.id) == nil else { return }
        busy = m.id
        defer { busy = nil }
        do {
            let revealed = try await APIClient.shared.revealSealed(m.id)
            codes[m.id] = AuthCode.extract(revealed.body)
        } catch {
            codes[m.id] = String?.none
            store.pushToast(errText(error, "reveal failed"), .error)
        }
    }

    private func copy() {
        guard let focus, let code = codes[focus.id] ?? nil else { return }
        Clip.copy(code, flashing: $copied)
    }

    private func archiveFocused(_ m: SealedMeta) async {
        busy = m.id
        defer { busy = nil }
        do {
            try await APIClient.shared.actionArchive(m.id)
            archived.insert(m.id)
            store.pushToast("archived", .success)
        } catch {
            store.pushToast(errText(error, "archive failed"), .error)
        }
    }

    private func seedFromArrivals() {
        for entry in store.authQueue where codes.index(forKey: entry.meta.id) == nil {
            codes[entry.meta.id] = entry.code
        }
    }
}

// MARK: - focus panel

/// The blown-up selected message: identity, kind, and — for a code — digits.
private struct FocusPanel: View {
    let meta: SealedMeta
    let code: String?
    let revealed: Bool
    let busy: Bool
    let copied: Bool
    let onCopy: () -> Void
    let onReveal: () -> Void
    let onArchive: () -> Void

    private var isCode: Bool { AuthCode.isCodeKind(meta.kind) }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                Text("In focus")
                    .font(Typo.sectionLabel)
                    .foregroundStyle(Palette.accent)
                    .textCase(.uppercase)
                Spacer()
                Text("arrived \(Fmt.relAge(meta.received_at)) ago")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
            }

            HStack(spacing: 12) {
                Avatar(sender: meta.sender, size: 42)
                VStack(alignment: .leading, spacing: 2) {
                    Text(SenderCache.resolved(meta.sender).displayName)
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(Palette.ink)
                        .help(meta.sender)
                    Text(meta.subject)
                        .font(Typo.rowSub)
                        .foregroundStyle(Palette.inkFaint)
                        .lineLimit(2)
                        .help(meta.subject)
                }
                Spacer(minLength: 8)
                Chip(
                    text: AuthCopy.label(meta.kind), tone: Palette.lock,
                    symbol: AuthCopy.symbol(meta.kind), filled: true)
            }

            if isCode {
                digits
                actions
            } else {
                VStack(alignment: .leading, spacing: 8) {
                    Button(action: onReveal) {
                        HStack(spacing: 6) {
                            Image(systemName: "eye")
                            Text("Open it")
                            Kbd("r")
                        }
                        .font(.system(size: 12, weight: .medium))
                        .padding(.horizontal, 12).padding(.vertical, 5)
                    }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.lock)
                    Text(
                        "This one is a message, not a code. Opening it fetches the body once and records an audit entry."
                    )
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                }
            }
        }
        .zonePadding()
        .frame(maxWidth: .infinity, alignment: .leading)
        .squelchGlass(.pane, cornerRadius: 20, tint: Palette.lockSoft.opacity(0.8))
    }

    private var digits: some View {
        HStack(spacing: 7) {
            if revealed, let code {
                ForEach(Array(code.enumerated()), id: \.offset) { _, ch in
                    Text(String(ch))
                        .font(Typo.mono(26, weight: .semibold))
                        .foregroundStyle(Palette.ink)
                        .frame(width: 40, height: 52)
                        .background(
                            RoundedRectangle(cornerRadius: 10, style: .continuous)
                                .fill(Palette.lockSoft)
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 10, style: .continuous)
                                .strokeBorder(Palette.lock.opacity(0.35), lineWidth: 1))
                }
            } else {
                ForEach(0..<6, id: \.self) { _ in
                    Text("·")
                        .font(Typo.mono(26, weight: .semibold))
                        .foregroundStyle(Palette.inkFaintest)
                        .frame(width: 40, height: 52)
                        .background(
                            RoundedRectangle(cornerRadius: 10, style: .continuous)
                                .fill(Palette.hairline.opacity(0.5)))
                }
            }
        }
        .accessibilityLabel("login code")
    }

    private var actions: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                if revealed, let code, !code.isEmpty {
                    Button(action: onCopy) {
                        Label(
                            copied ? "Copied" : "Copy code",
                            systemImage: copied ? "checkmark" : "doc.on.doc"
                        )
                        .font(.system(size: 12, weight: .medium))
                        .padding(.horizontal, 12).padding(.vertical, 5)
                    }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.lock)

                    Button(action: onArchive) {
                        Label("Used it — archive", systemImage: "archivebox")
                            .font(.system(size: 12))
                            .padding(.horizontal, 10).padding(.vertical, 5)
                    }
                    .buttonStyle(.glass)
                    .foregroundStyle(Palette.inkDim)
                    .disabled(busy)
                } else {
                    Button(action: onReveal) {
                        HStack(spacing: 6) {
                            Image(systemName: "eye")
                            Text(busy ? "Revealing…" : "Reveal code")
                            Kbd("r")
                        }
                        .font(.system(size: 12, weight: .medium))
                        .padding(.horizontal, 12).padding(.vertical, 5)
                    }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.lock)
                    .disabled(busy)
                }
            }
            Text(note)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var note: String {
        if revealed, let code, !code.isEmpty {
            return
                "Held in memory for this screen only — squelch never stores it. Revealing was written to the audit log."
        }
        if revealed {
            return "Revealed, but no code could be read from this one. Open it to read it yourself."
        }
        return
            "Nothing is read until you ask. Revealing fetches the body once and records an audit entry."
    }
}

// MARK: - rows

private struct AuthRow: View {
    let glassNamespace: Namespace.ID
    let meta: SealedMeta
    let selected: Bool
    let code: String?
    let onSelect: () -> Void
    let onOpen: () -> Void

    @State private var hovering = false

    var body: some View {
        Button(action: onSelect) {
            HStack(spacing: 9) {
                Avatar(sender: meta.sender, size: 20)
                Text(SenderCache.resolved(meta.sender).displayName)
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                    .frame(width: 150, alignment: .leading)
                    .help(meta.sender)
                Text(meta.subject)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkDim)
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .help(meta.subject)
                if let code, !code.isEmpty {
                    // Already revealed this session — no reason to ask twice.
                    Text(code)
                        .font(Typo.mono(12, weight: .semibold))
                        .foregroundStyle(Palette.lock)
                } else {
                    Chip(
                        text: AuthCopy.label(meta.kind), tone: Palette.inkFaint,
                        symbol: AuthCopy.symbol(meta.kind))
                }
                Text(Fmt.relAge(meta.received_at))
                    .font(Typo.num(10))
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(width: 30, alignment: .trailing)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .selectionGlass(
            selected, hovering: hovering, tint: Palette.lock, cornerRadius: 8,
            id: "auth-selection", in: glassNamespace)
        .onHover { hovering = $0 }
        .simultaneousGesture(TapGesture(count: 2).onEnded { onOpen() })
    }
}

/// A sign-in alert / reset request awaiting the human's verdict.
private struct DecisionCard: View {
    let meta: SealedMeta
    let onMine: () -> Void
    let onNotMine: () -> Void

    private var isReset: Bool { meta.kind == "password_reset" }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 7) {
                Avatar(sender: meta.sender, size: 18)
                Text(SenderCache.resolved(meta.sender).displayName)
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                Spacer(minLength: 4)
                Text(Fmt.relAge(meta.received_at))
                    .font(Typo.num(10))
                    .foregroundStyle(Palette.inkFaintest)
            }
            // Subject only — the body is sealed and this card never fetches it.
            Text(meta.subject)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkDim)
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 7) {
                Button(isReset ? "I asked" : "That was me", action: onMine)
                    .buttonStyle(.glassProminent)
                    .tint(Palette.accent)
                    .font(Typo.micro)
                Button(isReset ? "I did not" : "Not me", action: onNotMine)
                    .buttonStyle(.glass)
                    .foregroundStyle(Palette.danger)
                    .font(Typo.micro)
            }
            .controlSize(.small)
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(Palette.hairline.opacity(0.5))
        )
    }
}

// MARK: - shredder

/// Shredder panel. Every number comes from the server's `shred_log` ledger,
/// nothing is estimated. This switch is the only way automatic deletion turns
/// on, and shredding means Gmail Trash (recoverable 30 days), never a purge.
private struct ShredderCard: View {
    @Environment(AppStore.self) private var store
    @State private var stats: ShredStats?
    @State private var busy = false

    var body: some View {
        Group {
            if let stats {
                VStack(alignment: .leading, spacing: 9) {
                    HStack(spacing: 8) {
                        Image(systemName: "trash")
                            .font(.system(size: 12))
                            .foregroundStyle(Palette.inkFaint)
                        Text("Auto-shredder")
                            .font(Typo.zoneTitle)
                            .foregroundStyle(Palette.ink)
                        Spacer()
                        Toggle(
                            "",
                            isOn: Binding(
                                get: { stats.enabled },
                                set: { _ in Task { await toggle() } })
                        )
                        .labelsHidden()
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .tint(Palette.accent)
                        .disabled(busy || !stats.write_ready)
                        .help(
                            stats.write_ready
                                ? "move auth mail older than the window to Gmail Trash"
                                : "needs the write credential")
                    }

                    if !stats.write_ready {
                        Label {
                            Text(
                                "Needs the write credential — run `squelchd auth --write`. Nothing can be shredded until then."
                            )
                        } icon: {
                            Image(systemName: "exclamationmark.shield")
                        }
                        .font(Typo.micro)
                        .foregroundStyle(Palette.warn)
                        .fixedSize(horizontal: false, vertical: true)
                    } else if stats.enabled {
                        Text(
                            "Auth mail older than \(stats.after_days) days moves to Gmail Trash automatically, where it stays recoverable for 30 days."
                                + (stats.pending > 0 ? " \(stats.pending) eligible right now." : "")
                        )
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaint)
                        .fixedSize(horizontal: false, vertical: true)
                    } else {
                        Text(
                            "Off. Turn it on and auth mail older than \(stats.after_days) days moves to Gmail Trash — recoverable there for 30 days, never deleted outright."
                                + (stats.pending > 0
                                    ? " \(stats.pending) would go on the next pass." : "")
                        )
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaint)
                        .fixedSize(horizontal: false, vertical: true)
                    }

                    Text(
                        "\(stats.shredded_recent) shredded in the last 30 days"
                            + (stats.shredded_total > stats.shredded_recent
                                ? " · \(stats.shredded_total) all time" : "")
                    )
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                }
                .zonePadding()
                .frame(maxWidth: .infinity, alignment: .leading)
                .squelchGlass(.pane, cornerRadius: 18, tint: Palette.glassTint)
            }
        }
        .task { await load() }
    }

    private func load() async {
        // Apply the policy now instead of waiting out the server's hourly timer;
        // it is a server-side no-op when the shredder is off or can't run.
        if let run = try? await APIClient.shared.runShredder() {
            stats = run.stats
        } else {
            // Fall back to a plain read — an older daemon may lack the route.
            stats = try? await APIClient.shared.getShredder()
        }
    }

    private func toggle() async {
        guard let current = stats, !busy else { return }
        busy = true
        defer { busy = false }
        do {
            let next = try await APIClient.shared.setShredder(
                ShredderPatch(enabled: !current.enabled))
            stats = next
            if next.enabled {
                let run = try await APIClient.shared.runShredder()
                stats = run.stats
                if run.shredded > 0 {
                    store.pushToast("shredded \(run.shredded) old auth message(s)", .success)
                }
            }
        } catch {
            store.pushToast(errText(error, "could not change the shredder"), .error)
        }
    }
}
