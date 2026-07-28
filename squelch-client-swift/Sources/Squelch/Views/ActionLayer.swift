// ACTION LAYER — the global action surface, above every other layer.
//
// Renders the always-on overlays and wires the action-side keys:
//   - undo + notice toast stack (bottom-left), 5s undo with `u` / click
//   - the unsubscribe-violation prompt
//   - compose/review send ceremony
//   - rule editor (`t` tune) and process mode (`p`)
//   - the 2FA code modal, the triage-fix palette, the ⌘K ask bar
//   - the shortcuts overlay
//
// The read views own their list keymap; this layer EXTENDS the same "list"
// context with the two action verbs whose overlays live here (t, p). The
// modal-context keys live inside each overlay.
//
// GLASS: the toast stack is its own GlassEffectContainer, so stacked toasts
// merge into one pill of material and separate as they expire — the material
// doing the work a fade would otherwise have to.
//
// Ported from squelch-desktop/src/views/ActionLayer.tsx.

import SwiftUI

struct ActionLayer: View {
    @Environment(AppStore.self) private var store

    @State private var blockPrompt: UnsubscribeRecord?
    @State private var blockBusy = false
    /// Senders the human already blocked/kept THIS session, so a lagging read
    /// model can't re-nag.
    @State private var acted: Set<String> = []

    var body: some View {
        ZStack(alignment: .bottomLeading) {
            // Toast stack — bottom-left, above every fullscreen surface, so an
            // undo is never buried by the email it was fired from.
            //
            // Spacers + fixedSize are load-bearing, not tidiness: a
            // GlassEffectContainer expands to the size it is offered, so an
            // unconstrained one here became an invisible full-window pane that
            // swallowed every scroll and click in the app. Sizing it to its
            // content means only the toasts themselves are hit-testable, and
            // Spacers never take hits.
            VStack(spacing: 0) {
                Spacer(minLength: 0)
                HStack(spacing: 0) {
                    toastStack.fixedSize()
                    Spacer(minLength: 0)
                }
            }
            .padding(.leading, 74)
            .padding(.bottom, 18)

            if store.compose != nil { ComposeReview() }
            if let request = store.ruleEditor {
                RuleEditor(request: request) { store.closeRuleEditor() }
            }
            if store.processModeOpen { ProcessMode { store.processModeOpen = false } }
            if !store.authQueue.isEmpty { AuthCodeModal() }
            if let target = store.triageFix {
                TriageFixPalette(target: target) { store.closeTriageFix() }
            }
            if store.askBarOpen { AskBar { store.askBarOpen = false } }
            if store.shortcutsOpen { ShortcutsOverlay { store.shortcutsOpen = false } }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottomLeading)
        .keyBindings(.list, [
            KeyBinding("t", "tune sender") {
                if let u = store.selectedUpdate { Actions.tune(sender: u.sender) }
            },
            KeyBinding("p", "process mode") { store.processModeOpen = true },
        ])
        // Re-scan for unsubscribe violations whenever the read model ticks.
        .task(id: store.lastRefresh) { await scanViolations() }
    }

    // MARK: - toasts

    private var toastStack: some View {
        GlassEffectContainer(spacing: 10) {
            VStack(alignment: .leading, spacing: 7) {
                // Unsubscribe-violation prompt — an action card above the
                // toasts. Sender strings are email-derived → rendered as TEXT
                // only, never as markup.
                if let prompt = blockPrompt {
                    violationPrompt(prompt)
                }
                ForEach(store.toasts) { toast in
                    Button {
                        store.dismissToast(toast.id)
                    } label: {
                        Text(toast.text)
                            .font(Typo.rowSub)
                            .foregroundStyle(tone(toast.tone))
                            .padding(.horizontal, 12)
                            .padding(.vertical, 7)
                    }
                    .buttonStyle(.plain)
                    .background(
                        RoundedRectangle(cornerRadius: 11, style: .continuous)
                            .fill(.clear)
                    )
                    .glassEffect(
                        .regular.tint(tone(toast.tone).opacity(0.16)).interactive(),
                        in: .rect(cornerRadius: 11, style: .continuous)
                    )
                    .overlay(alignment: .leading) {
                        RoundedRectangle(cornerRadius: 1)
                            .fill(tone(toast.tone))
                            .frame(width: 2)
                            .padding(.vertical, 6)
                    }
                    .transition(.move(edge: .leading).combined(with: .opacity))
                }
                ForEach(store.undos) { undo in
                    Button {
                        Task { await store.fireUndo(undo.id) }
                    } label: {
                        HStack(spacing: 14) {
                            Text(undo.label)
                                .font(Typo.rowSub)
                                .foregroundStyle(Palette.ink)
                            HStack(spacing: 4) {
                                Kbd("u")
                                Text("undo")
                                    .font(Typo.micro)
                                    .foregroundStyle(Palette.inkFaintest)
                            }
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 7)
                    }
                    .buttonStyle(.plain)
                    .glassEffect(
                        .regular.tint(Palette.warn.opacity(0.18)).interactive(),
                        in: .rect(cornerRadius: 11, style: .continuous)
                    )
                    .overlay(alignment: .leading) {
                        RoundedRectangle(cornerRadius: 1)
                            .fill(Palette.warn)
                            .frame(width: 2)
                            .padding(.vertical, 6)
                    }
                    .transition(.move(edge: .leading).combined(with: .opacity))
                }
            }
        }
        .animation(.smooth(duration: 0.24), value: store.toasts.count)
        .animation(.smooth(duration: 0.24), value: store.undos.count)
    }

    private func tone(_ tone: Toast.Tone) -> Color {
        switch tone {
        case .error: Palette.danger
        case .success: Palette.positive
        case .info: Palette.inkDim
        }
    }

    // MARK: - unsubscribe violations

    private func violationPrompt(_ record: UnsubscribeRecord) -> some View {
        VStack(alignment: .leading, spacing: 9) {
            Text(
                "\(SenderCache.resolved(record.sender).displayName) is ignoring your unsubscribe — "
                    + "\(record.violation_count) email\(record.violation_count == 1 ? "" : "s") "
                    + "since you asked \(Fmt.relAge(record.requested_at)) ago."
            )
            .font(Typo.rowSub)
            .foregroundStyle(Palette.ink)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: 360, alignment: .leading)

            HStack(spacing: 8) {
                Button("Block sender") { Task { await block(record) } }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.danger)
                    .disabled(blockBusy)
                Button("Keep") { Task { await keep(record) } }
                    .buttonStyle(.glass)
                    .disabled(blockBusy)
            }
            .controlSize(.small)
        }
        .padding(13)
        .squelchGlass(.pane, cornerRadius: 14, tint: Palette.dangerSoft)
        .transition(.move(edge: .leading).combined(with: .opacity))
    }

    private func scanViolations() async {
        guard let rows = try? await APIClient.shared.getUnsubscribes() else { return }
        // Surface ONE at a time: a stack of these would be a nag, not a prompt.
        blockPrompt = rows.first {
            $0.violation_count > 0 && $0.resolution == nil && !acted.contains($0.sender)
        }
    }

    private func block(_ record: UnsubscribeRecord) async {
        guard !blockBusy else { return }
        blockBusy = true
        // Mark acted immediately so a refresh mid-flight can't re-surface it.
        acted.insert(record.sender)
        do {
            // Squelch rule on the EXACT sender address.
            try await Actions.createBlockRule(sender: record.sender)
            try await APIClient.shared.setUnsubResolution(
                sender: record.sender, resolution: .blocked)
            store.pushToast("blocked \(record.sender)", .success)
        } catch {
            store.pushToast(errText(error, "block failed"), .error)
        }
        blockBusy = false
        blockPrompt = nil
    }

    private func keep(_ record: UnsubscribeRecord) async {
        guard !blockBusy else { return }
        blockBusy = true
        acted.insert(record.sender)
        // Best-effort: the session-level `acted` set already suppresses re-nag.
        _ = try? await APIClient.shared.setUnsubResolution(
            sender: record.sender, resolution: .dismissed)
        blockBusy = false
        blockPrompt = nil
    }
}

// MARK: - shortcuts overlay

/// Keyboard-shortcuts help, opened with '?'. A curated, grouped cheat-sheet —
/// clearer than dumping the raw keymap registry, which carries per-context
/// duplicates.
struct ShortcutsOverlay: View {
    let onClose: () -> Void

    private struct Group: Identifiable {
        var title: String
        var items: [(keys: [String], desc: String)]
        var id: String { title }
    }

    private static let groups: [Group] = [
        Group(
            title: "Sitrep & inbox",
            items: [
                (["j", "k"], "move selection"),
                (["Enter"], "open thread"),
                (["r"], "reply"),
                (["e", "d"], "mark done"),
                (["v"], "fix a wrong triage verdict"),
                (["t"], "tune sender rule"),
                (["p"], "process mode"),
                (["u"], "undo last action"),
            ]),
        Group(
            title: "Thread viewer",
            items: [
                (["j", "k"], "older / newer message"),
                (["h", "l"], "previous / next queued email"),
                (["e", "d"], "done + next"),
                (["u"], "unsubscribe from this sender"),
                (["Esc"], "back"),
            ]),
        Group(
            title: "Navigate",
            items: [
                (["1", "2", "3", "4", "5"], "sitrep · emails · auth · rules · audit"),
                (["⌘["], "back"),
                (["⌘]"], "forward"),
                (["a"], "browse all mail"),
                (["/"], "search"),
                (["⌘K"], "ask your inbox"),
                (["⌘,"], "settings"),
                (["g"], "auth messages"),
                (["T"], "rules"),
                (["A"], "audit log"),
            ]),
        Group(
            title: "App",
            items: [
                (["\\"], "toggle light / dark theme"),
                (["?"], "this help"),
                (["Esc"], "close panel / overlay"),
            ]),
    ]

    var body: some View {
        OverlayScrim(onDismiss: onClose) {
            ModalCard(width: 620) {
                Text("Keyboard shortcuts")
                    .font(Typo.serif(22, weight: .medium))
                    .foregroundStyle(Palette.ink)

                LazyVGrid(
                    columns: [GridItem(.flexible(), spacing: 22), GridItem(.flexible())],
                    alignment: .leading, spacing: 18
                ) {
                    ForEach(Self.groups) { group in
                        VStack(alignment: .leading, spacing: 5) {
                            Text(group.title)
                                .font(Typo.sectionLabel)
                                .foregroundStyle(Palette.accent)
                                .textCase(.uppercase)
                                .padding(.bottom, 2)
                            ForEach(group.items, id: \.desc) { item in
                                HStack(spacing: 6) {
                                    Text(item.desc)
                                        .font(Typo.micro)
                                        .foregroundStyle(Palette.inkDim)
                                    Spacer(minLength: 8)
                                    ForEach(item.keys, id: \.self) { Kbd($0) }
                                }
                            }
                        }
                    }
                }

                HStack(spacing: 4) {
                    Spacer()
                    Kbd("?")
                    Text("or").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    Kbd("Esc")
                    Text("to close").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                }
            }
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "close help") { onClose() },
            KeyBinding("?", "close help") { onClose() },
        ])
    }
}
