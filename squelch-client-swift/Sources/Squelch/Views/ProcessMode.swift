// PROCESS MODE — the `p` triage deck.
//
// Card-by-card walk of the NEW + STILL OPEN bands with the same verbs as the
// list (r reply, e archive, d done, t tune, Space skip). archive/done resolve
// the item, drop it from the queue, and advance; the counter ticks down to an
// empty-queue "cleared" state. Reads the live bands from the store so items the
// user resolves elsewhere fall out too.
//
// Ported from squelch-desktop/src/components/ProcessMode.tsx.

import SwiftUI

struct ProcessMode: View {
    let onClose: () -> Void

    @Environment(AppStore.self) private var store

    /// The queue snapshot taken on entry (new + open, in band order).
    @State private var queue: [AttentionUpdate] = []
    @State private var handled: Set<Int> = []
    @State private var index = 0

    /// An item is "still pending" if it hasn't been handled here AND still
    /// exists in a live band (someone may resolve it elsewhere).
    private var pending: [AttentionUpdate] {
        let live = Set((store.sitrep.new + store.sitrep.open).map(\.id))
        return queue.filter { !handled.contains($0.id) && live.contains($0.id) }
    }
    private var current: AttentionUpdate? { pending[safe: index] }
    private var cleared: Int { queue.count - pending.count }

    var body: some View {
        OverlayScrim(onDismiss: onClose) {
            VStack(alignment: .leading, spacing: 10) {
                HStack(spacing: 4) {
                    Text("process mode")
                        .font(Typo.sectionLabel)
                        .foregroundStyle(Palette.inkFaint)
                        .textCase(.uppercase)
                    Text("·").foregroundStyle(Palette.inkFaintest)
                    Kbd("space")
                    Text("skip").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    Text("·").foregroundStyle(Palette.inkFaintest)
                    Kbd("esc")
                    Text("exit").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    Spacer()
                    Text("\(cleared) / \(queue.count) cleared · \(pending.count) left")
                        .font(Typo.num(11))
                        .foregroundStyle(Palette.inkFaint)
                }

                if let current {
                    card(current)
                } else {
                    emptyState
                }
            }
            .frame(width: 660)
        }
        .keyContext(.modal)
        .keyBindings(.modal, bindings)
        .onAppear {
            // Snapshot on entry so the deck has a stable denominator.
            queue = store.sitrep.new + store.sitrep.open
        }
        .onChange(of: pending.count) { _, count in
            if index > count - 1 { index = max(0, count - 1) }
        }
    }

    private func card(_ u: AttentionUpdate) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                Avatar(sender: u.sender, size: 28)
                Text(SenderID.displayName(u.sender))
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Palette.ink)
                Spacer(minLength: 8)
                Text("importance \(u.importance) · \(u.tier.label)")
                    .font(Typo.num(11))
                    .foregroundStyle(Palette.inkFaint)
            }

            Text(u.one_line)
                .font(.system(size: 16))
                .foregroundStyle(Palette.ink)
                .fixedSize(horizontal: false, vertical: true)

            Text(u.reason)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkFaint)
                .fixedSize(horizontal: false, vertical: true)

            if let chip = Fmt.deadlineChip(u.deadline) {
                Chip(
                    text: chip.text, tone: chip.overdue ? Palette.danger : Palette.warn,
                    filled: chip.overdue)
            }

            HStack(spacing: 12) {
                verb("r", "reply")
                verb("e", "archive")
                verb("d", "done")
                verb("t", "tune")
                verb("space", "skip")
                Spacer()
            }
            .padding(.top, 4)
        }
        .padding(22)
        .frame(maxWidth: .infinity, alignment: .leading)
        .squelchGlass(.pane, cornerRadius: 20, tint: Palette.glassTint)
        .shadow(color: .black.opacity(0.3), radius: 44, y: 18)
    }

    private func verb(_ key: String, _ label: String) -> some View {
        HStack(spacing: 4) {
            Kbd(key)
            Text(label).font(Typo.micro).foregroundStyle(Palette.inkFaintest)
        }
    }

    private var emptyState: some View {
        VStack(spacing: 8) {
            Text(queue.isEmpty ? "nothing to process" : "queue cleared")
                .font(Typo.serif(24, weight: .medium))
                .foregroundStyle(Palette.positive)
            Text(
                queue.isEmpty
                    ? "no new or still-open items right now."
                    : "worked through \(queue.count) item\(queue.count == 1 ? "" : "s")."
            )
            .font(Typo.rowSub)
            .foregroundStyle(Palette.inkFaint)
            Button("esc · back to sitrep", action: onClose)
                .buttonStyle(.glass)
                .padding(.top, 6)
        }
        .frame(maxWidth: .infinity)
        .padding(36)
        .squelchGlass(.pane, cornerRadius: 20, tint: Palette.positiveSoft)
        .shadow(color: .black.opacity(0.3), radius: 44, y: 18)
    }

    private var bindings: [KeyBinding] {
        [
            KeyBinding("Escape", "exit process mode") { onClose() },
            KeyBinding("q", "exit") { onClose() },
            KeyBinding("Space", "skip") { skip() },
            KeyBinding("j", "next") { skip() },
            KeyBinding("k", "prev") {
                guard !pending.isEmpty else { return }
                index = (index - 1 + pending.count) % pending.count
            },
            KeyBinding("r", "reply") { if let current { Actions.reply(current) } },
            KeyBinding("e", "archive") {
                guard let current else { return }
                Task { await Actions.archive(current) }
                // Cursor stays put; the handled item drops out and the next
                // slides in.
                handled.insert(current.id)
            },
            KeyBinding("d", "done") {
                guard let current else { return }
                Task { await Actions.done(current) }
                handled.insert(current.id)
            },
            KeyBinding("t", "tune sender") {
                if let current { Actions.tune(sender: current.sender) }
            },
        ]
    }

    private func skip() {
        guard !pending.isEmpty else { return }
        index = (index + 1) % pending.count
    }
}
