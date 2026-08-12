// The `p` triage deck: a card-by-card walk of the new + still-open bands with the
// list's verbs (e archive, d done, t tune, Space skip). Reads the live bands from
// the store, so items resolved elsewhere drop out of the queue too. `r` is the
// one verb that LEAVES: replying happens in the reader, so the deck closes and
// hands the email over.
//
// THE VERBS ARE METHODS, NOT CLOSURES IN THE KEYMAP. A phone drives this deck by
// flinging the card — right resolves, left skips, up tunes — and those gestures
// have to be the same act as the key that names them, not a second
// implementation that happens to agree today. So each verb is a method below and
// both the `bindings` array and the drag physics call it. Adding a verb means
// adding a method; wiring a key or a gesture to something else means it is no
// longer the same deck.

import SwiftUI

struct ProcessMode: View {
    let onClose: () -> Void

    @Environment(AppStore.self) private var store

    /// The queue snapshot taken on entry (new + open, in band order).
    @State private var queue: [AttentionUpdate] = []
    @State private var handled: Set<Int> = []
    @State private var index = 0

    #if !os(macOS)
        /// Live card displacement. Written by the drag, and by the fling that
        /// throws a committed card off screen before the deck advances.
        @State private var drag: CGSize = .zero
    #endif

    /// Pending means not handled here *and* still in a live band — the item may
    /// have been resolved elsewhere.
    private var pending: [AttentionUpdate] {
        let live = Set((store.sitrep.new + store.sitrep.open).map(\.id))
        return queue.filter { !handled.contains($0.id) && live.contains($0.id) }
    }
    private var current: AttentionUpdate? { pending[safe: index] }
    private var cleared: Int { queue.count - pending.count }

    var body: some View {
        surface
            .keyContext(.modal)
            .keyBindings(.modal, bindings)
            .onAppear {
                // Snapshot on entry so the deck has a stable denominator.
                queue = store.sitrep.new + store.sitrep.open
            }
            .onChange(of: pending.count) { _, count in
                if index > count - 1 { index = max(0, count - 1) }
                // The deck just emptied: one completed run, sized by its snapshot.
                if count == 0, !queue.isEmpty {
                    Analytics.capture("process_completed", ["count": queue.count])
                }
            }
    }

    @ViewBuilder
    private var surface: some View {
        #if os(macOS)
            desktopSurface
        #else
            phoneSurface
        #endif
    }

    // MARK: - desktop

    #if os(macOS)
        private var desktopSurface: some View {
            OverlayScrim(onDismiss: onClose) {
                VStack(alignment: .leading, spacing: 10) {
                    HStack(spacing: 4) {
                        Text("process mode")
                            .font(Typo.sectionLabel)
                            .foregroundStyle(Palette.inkFaint)
                            .textCase(.uppercase)
                        Text("·").foregroundStyle(Palette.inkFaintest)
                        KeyHint("space", "skip")
                        Text("·").foregroundStyle(Palette.inkFaintest)
                        KeyHint("esc", "exit")
                        Spacer()
                        Text(progressLine)
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
        }
    #endif

    // MARK: - phone

    #if !os(macOS)
        /// How far a card must travel before letting go commits its verb. A
        /// fraction of the screen rather than a constant: the gesture is "throw it
        /// away", and what counts as a throw scales with the thing being thrown.
        private static let commitDistance: CGFloat = 108
        /// Vertical throw for tune. Larger, because the deck lives in a scrollless
        /// full-screen cover and an upward flick is the least deliberate of the
        /// three.
        private static let tuneDistance: CGFloat = 132

        /// THE DECK, FOR A THUMB. The card is the surface, the fling is the verb,
        /// and the buttons underneath exist because a gesture nobody is told about
        /// is a gesture nobody uses. Both paths call the same methods.
        private var phoneSurface: some View {
            VStack(spacing: 0) {
                phoneHeader
                // The card floats between the header and the verbs rather than
                // hanging off the top: it is the only thing on this screen, and a
                // deck you are meant to throw should sit where the thumb is.
                Spacer(minLength: 8)
                if let current {
                    // `.id` so the outgoing card and the one that replaces it are
                    // separate views: the replacement starts centred instead of
                    // inheriting the flung one's offset.
                    card(current)
                        .id(current.id)
                        .offset(drag)
                        .rotationEffect(.degrees(Double(drag.width / 24)), anchor: .bottom)
                        .overlay(alignment: .top) { verdictStamp }
                        .gesture(fling)
                        .padding(.horizontal, 16)
                        .transition(.opacity)
                    Spacer(minLength: 12)
                    phoneButtons(current)
                } else {
                    Spacer(minLength: 0)
                    emptyState.padding(.horizontal, 16)
                    Spacer(minLength: 0)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Palette.canvas.ignoresSafeArea())
        }

        private var phoneHeader: some View {
            HStack(spacing: 8) {
                Text("process mode")
                    .font(Typo.sectionLabel)
                    .foregroundStyle(Palette.inkFaint)
                    .textCase(.uppercase)
                Spacer(minLength: 8)
                Text(progressLine)
                    .font(Typo.num(11))
                    .foregroundStyle(Palette.inkFaint)
                Button(action: onClose) {
                    Image(systemName: "xmark")
                        .font(.system(size: 13, weight: .semibold))
                        .padding(7)
                        .contentShape(Circle())
                }
                .buttonStyle(.plain)
                .foregroundStyle(Palette.inkFaint)
                .glassCapsule()
                .accessibilityLabel("exit process mode")
            }
            .padding(.horizontal, 16)
            .padding(.top, 14)
            .padding(.bottom, 12)
        }

        /// What letting go RIGHT NOW would do, stamped over the card as it moves.
        /// The card is already leaning toward the verb; this names it — pinned to
        /// the edge the card is heading for, so it rides the leading corner
        /// instead of sitting on top of the summary you are reading.
        @ViewBuilder
        private var verdictStamp: some View {
            if let verdict {
                Text(verdict.title)
                    .font(Typo.sectionLabel)
                    .textCase(.uppercase)
                    .foregroundStyle(verdict.tint)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 6)
                    .glassCapsule(tint: verdict.tint.opacity(0.22))
                    .frame(maxWidth: .infinity, alignment: verdict.align)
                    .padding(.horizontal, 14)
                    .padding(.top, -14)
                    .opacity(min(1, verdict.progress))
                    .allowsHitTesting(false)
            }
        }

        /// The pending verdict for the current displacement, or nil while the card
        /// is near enough to home that letting go would just settle it back.
        private var verdict: (title: String, tint: Color, progress: Double, align: Alignment)? {
            if -drag.height > Self.tuneDistance / 2, abs(drag.width) < Self.commitDistance / 2 {
                return ("tune", Palette.accent, Double(-drag.height / Self.tuneDistance), .center)
            }
            if drag.width > 24 {
                return (
                    "done", Palette.positive, Double(drag.width / Self.commitDistance), .trailing
                )
            }
            if drag.width < -24 {
                return (
                    "skip", Palette.inkFaint, Double(-drag.width / Self.commitDistance), .leading
                )
            }
            return nil
        }

        private func phoneButtons(_ u: AttentionUpdate) -> some View {
            HStack(spacing: 10) {
                deckButton("arrowshape.turn.up.left", "reply", Palette.accent) { reply(u) }
                deckButton("archivebox", "archive", Palette.warn) { archive(u) }
                deckButton("slider.horizontal.3", "tune", Palette.accent) { tune(u) }
                deckButton("forward", "skip", Palette.inkFaint) { skip() }
                deckButton("checkmark", "done", Palette.positive) { done(u) }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 22)
        }

        private func deckButton(
            _ symbol: String, _ title: String, _ tint: Color, action: @escaping () -> Void
        ) -> some View {
            Button {
                Haptics.commit()
                action()
            } label: {
                VStack(spacing: 4) {
                    Image(systemName: symbol).font(.system(size: 16, weight: .medium))
                    Text(title).font(Typo.micro)
                }
                .foregroundStyle(tint)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 11)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .glassCapsule(tint: tint.opacity(0.14))
        }

        // MARK: - card physics

        private var fling: some Gesture {
            DragGesture()
                .onChanged { value in
                    drag = value.translation
                }
                .onEnded { value in
                    guard let u = current else {
                        withAnimation(Motion.deckCard) { drag = .zero }
                        return
                    }
                    let t = value.translation
                    if -t.height > Self.tuneDistance, abs(t.width) < Self.commitDistance {
                        // Tune does not clear the card — the rule editor opens over
                        // it and the same email is still waiting underneath.
                        Haptics.commit()
                        withAnimation(Motion.deckCard) { drag = .zero }
                        tune(u)
                    } else if t.width > Self.commitDistance {
                        throwOut(CGSize(width: 900, height: t.height)) { done(u) }
                    } else if t.width < -Self.commitDistance {
                        throwOut(CGSize(width: -900, height: t.height)) { skip() }
                    } else {
                        withAnimation(Motion.deckCard) { drag = .zero }
                    }
                }
        }

        /// Throw the card off screen, THEN advance the deck. The verb fires after
        /// the card has left rather than under it, so the next email never flashes
        /// up wearing the outgoing one's position.
        private func throwOut(_ target: CGSize, run: @escaping () -> Void) {
            Haptics.commit()
            withAnimation(Motion.deckCard) { drag = target }
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(200))
                run()
                drag = .zero
            }
        }
    #endif

    // MARK: - shared card

    private var progressLine: String {
        "\(cleared) / \(queue.count) cleared · \(pending.count) left"
    }

    private func card(_ u: AttentionUpdate) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                Avatar(sender: u.senderString, size: 28)
                Text(SenderID.displayName(u.sender))
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Palette.ink)
                    // A sender with no display name IS its address, and a long
                    // one wraps this header into two ragged lines at phone width.
                    // The Mac's card is 660pt and has never had to care.
                    #if !os(macOS)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    #endif
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

            // The keycaps are the Mac's affordance and the phone's would be a lie;
            // there, the buttons under the deck say the same thing in thumbs.
            #if os(macOS)
                HStack(spacing: 12) {
                    KeyHint("r", "reply in reader")
                    KeyHint("e", "archive")
                    KeyHint("d", "done")
                    KeyHint("t", "tune")
                    KeyHint("space", "skip")
                    Spacer()
                }
                .padding(.top, 4)
            #else
                Text("swipe right to finish · left to skip · up to tune")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .padding(.top, 2)
            #endif
        }
        .padding(22)
        .frame(maxWidth: .infinity, alignment: .leading)
        .passbandGlass(.pane, cornerRadius: 20, tint: Palette.glassTint)
        .shadow(color: .black.opacity(0.3), radius: 44, y: 18)
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
            Button(exitLabel, action: onClose)
                .buttonStyle(.glass)
                .padding(.top, 6)
        }
        .frame(maxWidth: .infinity)
        .padding(36)
        .passbandGlass(.pane, cornerRadius: 20, tint: Palette.positiveSoft)
        .shadow(color: .black.opacity(0.3), radius: 44, y: 18)
    }

    private var exitLabel: String {
        #if os(macOS)
            "esc · back to sitrep"
        #else
            "back to sitrep"
        #endif
    }

    // MARK: - verbs

    // One method per verb, called by the keymap below AND by the phone's card
    // physics. Nothing else in this file acts on an update.

    /// `r`. The deck steps ASIDE for the reader rather than stacking a composer
    /// over its own overlay; exiting first also keeps the process-mode keymap
    /// from sitting above the reader's.
    private func reply(_ u: AttentionUpdate) {
        onClose()
        Actions.reply(u)
    }

    /// `e`. Cursor stays put: the handled item drops out, the next slides in.
    private func archive(_ u: AttentionUpdate) {
        Task { await Actions.archive(u) }
        handled.insert(u.id)
    }

    /// `d`, and the phone's swipe right.
    private func done(_ u: AttentionUpdate) {
        Task { await Actions.done(u) }
        handled.insert(u.id)
    }

    /// `t`, and the phone's swipe up.
    private func tune(_ u: AttentionUpdate) {
        Actions.tune(sender: u.sender)
    }

    /// Space / `j`, and the phone's swipe left.
    private func skip() {
        guard !pending.isEmpty else { return }
        index = (index + 1) % pending.count
    }

    /// `k`.
    private func back() {
        guard !pending.isEmpty else { return }
        index = (index - 1 + pending.count) % pending.count
    }

    // MARK: - keymap

    private var bindings: [KeyBinding] {
        [
            KeyBinding("Escape", "exit process mode") { onClose() },
            KeyBinding("q", "exit") { onClose() },
            KeyBinding("Space", "skip") { skip() },
            KeyBinding("j", "next") { skip() },
            KeyBinding("k", "prev") { back() },
            KeyBinding("r", "reply") {
                guard let current else { return }
                reply(current)
            },
            KeyBinding("e", "archive") {
                guard let current else { return }
                archive(current)
            },
            KeyBinding("d", "done") {
                guard let current else { return }
                done(current)
            },
            KeyBinding("t", "tune sender") {
                guard let current else { return }
                tune(current)
            },
        ]
    }
}
