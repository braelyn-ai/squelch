// THE PHONE'S TOAST HOST — the same two stacks the Mac's ActionLayer renders,
// off the SAME store state: `store.toasts` (auto-dismissing notices) and
// `store.undos` (the five-second window Actions.done/archive/label open).
//
// NO FORKED STATE, and that is the whole design constraint. ActionLayer is
// excluded from this target because it hosts the ask bar, not because its toasts
// are desktop-shaped; this file is its toast half, re-laid-out and nothing else.
// Every id, every label, every tone and every expiry comes from AppStore, so an
// undo fired here is the same call `u` makes there — `store.fireUndo(id)` —
// and the two shells can never disagree about what is pending.
//
// WHAT CHANGES FOR A THUMB. The Mac anchors bottom-LEFT beside the rail and
// leans on `u`; a phone has no rail and no `u`, so the stack centres over the
// tab bar where a thumb already is, and the undo chip's keycap hint becomes a
// real button with a real target.

import SwiftUI

struct MobileToastHost: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        // The Spacers + fixedSize are load-bearing exactly as they are on the
        // Mac: a GlassEffectContainer expands to whatever it is offered, so an
        // unconstrained one in an overlay is an invisible full-screen pane that
        // eats every tap and every scroll on the surface underneath.
        VStack(spacing: 0) {
            Spacer(minLength: 0)
            stack.fixedSize()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .allowsHitTesting(!store.toasts.isEmpty || !store.undos.isEmpty)
    }

    private var stack: some View {
        GlassEffectContainer(spacing: 10) {
            VStack(spacing: 8) {
                // Sender strings reach these labels from mail headers: rendered
                // as Text only, never as markup.
                ForEach(store.toasts) { toast in
                    Button { store.dismissToast(toast.id) } label: {
                        Text(toast.text)
                            .font(Typo.rowSub)
                            .foregroundStyle(tone(toast.tone))
                            .multilineTextAlignment(.leading)
                            .padding(.horizontal, 14)
                            .padding(.vertical, 9)
                    }
                    .buttonStyle(.plain)
                    .glassEffect(
                        .regular.tint(tone(toast.tone).opacity(0.16)).interactive(),
                        in: .capsule)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
                }
                ForEach(store.undos) { undo in
                    undoChip(undo)
                }
            }
            .padding(.horizontal, 16)
        }
        .animation(Motion.toast, value: store.toasts.count)
        .animation(Motion.toast, value: store.undos.count)
    }

    /// The undo chip. Two targets in one pill: the label states what happened,
    /// the button takes it back. Tapping the LABEL does nothing on purpose — the
    /// Mac's whole chip is the undo because a pointer aims, and a thumb landing
    /// anywhere on a five-second chip would undo things it meant to read.
    private func undoChip(_ undo: PendingUndo) -> some View {
        HStack(spacing: 12) {
            Text(undo.label)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.ink)
                .lineLimit(1)
            Button {
                Haptics.commit()
                Task { await store.fireUndo(undo.id) }
            } label: {
                Text("Undo")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Palette.warn)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 5)
                    .contentShape(Capsule())
            }
            .buttonStyle(.plain)
            .glassCapsule(tint: Palette.warn.opacity(0.22))
            .accessibilityLabel("undo \(undo.label)")
        }
        .padding(.leading, 16)
        .padding(.trailing, 6)
        .padding(.vertical, 6)
        .glassEffect(.regular.tint(Palette.warn.opacity(0.18)), in: .capsule)
        .transition(.move(edge: .bottom).combined(with: .opacity))
    }

    private func tone(_ tone: Toast.Tone) -> Color {
        switch tone {
        case .error: Palette.danger
        case .success: Palette.positive
        case .info: Palette.inkDim
        }
    }
}
