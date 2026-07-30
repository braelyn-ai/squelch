// TRIAGE FIX PALETTE — `v` on a focused email: type, hit Enter, the mail moves
// and the correction is recorded as training data. That second half is the
// point. AMBIGUITY IS SHOWN, NOT GUESSED — "bill" matches both Invoice and
// Autopay bill, so the list stays up and the highlighted row is always what
// Enter picks; resolving it silently would poison a ground-truth dataset.

import SwiftUI

struct TriageFixPalette: View {
    let target: TriageFixTarget
    let onClose: () -> Void

    @Environment(AppStore.self) private var store
    @State private var query = ""
    @State private var selection = 0
    @State private var busy = false
    @Namespace private var paletteGlass
    @FocusState private var focused: Bool

    private var hits: [TriageTarget] { TriageTargets.match(query) }

    var body: some View {
        OverlayScrim(alignment: .top, topInset: 110, onDismiss: onClose) {
            GlassEffectContainer(spacing: 8) {
                VStack(alignment: .leading, spacing: 0) {
                    header
                    wasRow
                    input
                    Divider().overlay(Palette.hairline)
                    list
                    footer
                }
                .frame(width: 560)
                .squelchGlass(
                    .pane, cornerRadius: 20, tint: Palette.glassTintStrong,
                    id: "palette", in: paletteGlass)
                .shadow(color: .black.opacity(0.32), radius: 46, y: 20)
            }
        }
        .keyContext(.modal)
        .keyBindings(.modal, bindings)
        .onAppear { focused = true }
        .onChange(of: hits.count) { _, count in
            selection = max(0, min(selection, max(0, count - 1)))
        }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: "wand.and.sparkles")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Palette.accent)
            Text("Fix triage")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Palette.ink)
            Text(target.subject)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
                .lineLimit(1)
                .help("\(target.sender) — \(target.subject)")
            Spacer(minLength: 4)
        }
        .padding(.horizontal, 16)
        .padding(.top, 14)
        .padding(.bottom, 8)
    }

    /// What it is NOW, so the correction reads as a before/after. A dimension the
    /// caller does not know is OMITTED, never shown as "unset" — that would claim
    /// a value we simply never fetched.
    @ViewBuilder
    private var wasRow: some View {
        if target.tier != nil || target.category != nil {
            HStack(spacing: 12) {
                if let tier = target.tier {
                    HStack(spacing: 4) {
                        Text("tier").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                        Text(TriageTargets.label(axis: .tier, value: tier))
                            .font(Typo.micro).bold()
                            .foregroundStyle(Palette.inkDim)
                    }
                }
                if let category = target.category {
                    HStack(spacing: 4) {
                        Text("category").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                        Text(TriageTargets.label(axis: .category, value: category))
                            .font(Typo.micro).bold()
                            .foregroundStyle(Palette.inkDim)
                    }
                }
                Spacer()
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 8)
        }
    }

    private var input: some View {
        TextField("where should this have gone? (important, bill, noise…)", text: $query)
            .textFieldStyle(.plain)
            .font(.system(size: 15))
            .foregroundStyle(Palette.ink)
            .focused($focused)
            .autocorrectionDisabled()
            .disabled(busy)
            .padding(.horizontal, 16)
            .padding(.bottom, 12)
            .onChange(of: query) { _, _ in selection = 0 }
    }

    private var list: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(spacing: 1) {
                    if hits.isEmpty {
                        Text(
                            "nothing matches “\(query)”. these are the only values the triage pipeline itself uses — anything else could not be learned from."
                        )
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(14)
                    } else {
                        ForEach(Array(hits.enumerated()), id: \.element.id) { i, hit in
                            TargetRow(
                                target: hit, selected: i == selection,
                                onHover: { selection = i },
                                onPick: { Task { await apply(hit) } })
                            .id(hit.id)
                            .disabled(busy)
                        }
                    }
                }
                .padding(.horizontal, 8)
                .padding(.vertical, 6)
            }
            .frame(maxHeight: 300)
            .onChange(of: selection) { _, i in
                guard let hit = hits[safe: i] else { return }
                withAnimation(.easeOut(duration: 0.1)) { proxy.scrollTo(hit.id, anchor: .center) }
            }
        }
    }

    private var footer: some View {
        HStack(spacing: 4) {
            Kbd("↑"); Kbd("↓")
            Text("pick").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Text("·").foregroundStyle(Palette.inkFaintest)
            Kbd("↵")
            Text("apply").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Text("·").foregroundStyle(Palette.inkFaintest)
            Kbd("esc")
            Text("cancel").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Spacer()
            Text("stored to refine triage")
                .font(Typo.micro)
                .foregroundStyle(Palette.accent.opacity(0.8))
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .overlay(alignment: .top) { Hairline() }
    }

    /// allowInInput is REQUIRED, not polish: the palette autofocuses its field,
    /// so the `editing && !allowInInput` guard would drop every binding — and
    /// Escape would fall through to whatever binds it underneath, meaning
    /// cancelling would NAVIGATE the app.
    private var bindings: [KeyBinding] {
        [
            KeyBinding("Escape", "cancel", allowInInput: true) { onClose() },
            KeyBinding("ArrowDown", "next", allowInInput: true) {
                selection = min(hits.count - 1, selection + 1)
            },
            KeyBinding("ArrowUp", "prev", allowInInput: true) {
                selection = max(0, selection - 1)
            },
            KeyBinding("Enter", "apply", allowInInput: true) {
                if let hit = hits[safe: selection] { Task { await apply(hit) } }
            },
        ]
    }

    /// Optimistic removal for exactly the moves the server's predicates make
    /// (TriageTarget.exit), then a forced re-read of the bands AND the zones: a
    /// zone row is a different shape than a band row, so an arrival cannot be
    /// synthesized here. A correction that FAILS moves nothing and keeps the
    /// palette up to try again.
    private func apply(_ hit: TriageTarget) async {
        guard !busy else { return }
        busy = true
        do {
            try await APIClient.shared.correctTriage(
                messageId: target.messageId, dimension: hit.axis, toValue: hit.value)
        } catch {
            store.pushToast(errText(error, "could not record the correction"), .error)
            busy = false
            return
        }
        switch hit.exit {
        case .stays: break
        case .standing: store.removeFromStanding(target.messageId)
        case .allBands: _ = store.removeFromBands(target.messageId)
        }
        // Never promise a move the pipeline does not make: a category is not a
        // band, so "moved to Marketing" is a claim the list visibly contradicts.
        store.pushToast(
            hit.lands.map { "\(hit.label) → \($0) · recorded" } ?? "\(hit.label) · recorded",
            .success)
        onClose()
        // Runs on past the dismissal on purpose: the calling Task belongs to the
        // key binding, not to this view, so closing does not cancel it.
        await SitrepPoller.shared.refreshAfterCorrection()
    }
}

private struct TargetRow: View {
    let target: TriageTarget
    let selected: Bool
    let onHover: () -> Void
    let onPick: () -> Void

    private var axisTone: Color {
        switch target.axis {
        case .tier: Palette.warn
        case .category: Palette.accent
        case .sensitivity: Palette.lock
        }
    }

    var body: some View {
        Button(action: onPick) {
            HStack(spacing: 10) {
                Chip(text: target.axis.chipLabel, tone: axisTone, filled: true)
                    .frame(width: 68, alignment: .leading)
                Text(target.label)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .frame(width: 130, alignment: .leading)
                Text(target.hint)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaint)
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                // WHERE IT LANDS, for values whose destination is a server
                // predicate: "For your eyes" is a band over tiers, not a pickable
                // label, so the mapping is the only way to aim at it on purpose.
                if let lands = target.lands {
                    Text("→ \(lands)")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.accent.opacity(0.85))
                        .fixedSize()
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .selectionFill(selected, tint: axisTone, cornerRadius: 9)
        .onHover { if $0 { onHover() } }
    }
}
