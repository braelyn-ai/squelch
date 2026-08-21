// REMIND PALETTE — `h` on a focused email: say when, hit Enter, and the mail
// leaves now and comes back then.
//
// EVERY ROW STATES ITS ABSOLUTE TIME. This is the one palette in the app whose
// pick is a promise about the future, and "next week" is a different Monday
// depending on which day you asked — so the words you typed sit on the left and
// the instant they resolve to sits on the right, on every row, always. The
// suggestion engine underneath (RemindTimes) is pure and asserted; this file is
// only the surface.

import SwiftUI

struct RemindPalette: View {
    let target: RemindTarget
    let onClose: () -> Void

    @State private var query = ""
    @State private var selection = 0
    @State private var busy = false
    /// Pinned when the palette opens: every row is scored against ONE clock, so
    /// a list cannot re-rank itself under the cursor between keystrokes.
    @State private var opened = Date()
    @Namespace private var paletteGlass
    @FocusState private var focused: Bool

    /// The rows on screen. STATE, not a computed property: SwiftUI reads a
    /// computed one several times per render, and every read re-ran the whole
    /// engine. One recompute per thing that can actually change the answer —
    /// typing, opening, re-pinning the clock — is the palette's entire typing
    /// latency, so that is all it gets.
    @State private var hits: [RemindHit] = []

    var body: some View {
        surface
            .keyContext(.modal)
            .keyBindings(.modal, bindings)
            .onAppear {
                focused = true
                refreshHits()
            }
    }

    /// Re-run the engine against the pinned clock, keeping the cursor inside
    /// the new list.
    private func refreshHits() {
        hits = RemindTimes.match(query, now: opened)
        selection = max(0, min(selection, max(0, hits.count - 1)))
    }

    @ViewBuilder
    private var surface: some View {
        #if os(macOS)
            // Top-ANCHORED even though it sits near the middle: the list
            // shrinks and grows as typing filters it, and a center-anchored
            // card would breathe in both directions — the input line jumping
            // under the cursor mid-word. The inset, not the anchor, is what
            // puts it at the screen's visual center.
            OverlayScrim(alignment: .top, topInset: 210, onDismiss: onClose) {
                GlassEffectContainer(spacing: 8) {
                    palette
                        .frame(width: 560)
                        .passbandGlass(
                            .pane, cornerRadius: 20, tint: Palette.glassTintStrong,
                            id: "remind", in: paletteGlass)
                        .shadow(color: .black.opacity(0.32), radius: 46, y: 20)
                }
            }
        #else
            palette
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
                .background(Palette.canvas.ignoresSafeArea())
                .presentationDetents([.large])
        #endif
    }

    private var palette: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            input
            Divider().overlay(Palette.hairline)
            list
            footer
        }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: "bell.badge")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Palette.accent)
            Text(rescheduling ? "Move the reminder" : "Remind me")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Palette.ink)
            Text(target.subject)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
                .lineLimit(1)
                .help("\(target.sender) · \(target.subject)")
            Spacer(minLength: 4)
            // WHAT IT IS SET TO NOW, when there is one. A second `h` is a move,
            // not a first booking, and the row you are moving off is the one
            // fact the list itself cannot show.
            if let current = target.remindAt, !current.isEmpty {
                Text("now \(Fmt.remindChip(current))")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkDim)
                    .fixedSize()
            }
        }
        .padding(.horizontal, 16)
        .padding(.top, 14)
        .padding(.bottom, 8)
    }

    private var rescheduling: Bool { !(target.remindAt ?? "").isEmpty }

    private var input: some View {
        TextField("remind when...", text: $query)
            .textFieldStyle(.plain)
            .font(.system(size: 15))
            .foregroundStyle(Palette.ink)
            .focused($focused)
            .autocorrectionDisabled()
            .disabled(busy)
            .padding(.horizontal, 16)
            .padding(.top, 4)
            .padding(.bottom, 12)
            .onChange(of: query) { _, _ in
                selection = 0
                refreshHits()
            }
    }

    private var list: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(spacing: 1) {
                    if hits.isEmpty {
                        Text(
                            "no time in “\(query)”. try a day (\"friday\", \"aug 30\"), a wait (\"in 3 hours\") or leave it empty for the usual ones."
                        )
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(14)
                    } else {
                        ForEach(Array(hits.enumerated()), id: \.element.id) { i, hit in
                            RemindRow(
                                hit: hit, selected: i == selection,
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
            .frame(maxHeight: 230)
            .onChange(of: selection) { _, i in
                guard let hit = hits[safe: i] else { return }
                withAnimation(.easeOut(duration: 0.1)) { proxy.scrollTo(hit.id, anchor: .center) }
            }
        }
    }

    private var footer: some View {
        HStack(spacing: 4) {
            #if os(macOS)
                Kbd("↑"); Kbd("↓")
                Text("pick").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                Text("·").foregroundStyle(Palette.inkFaintest)
                Kbd("↵")
                Text("set").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                Text("·").foregroundStyle(Palette.inkFaintest)
                Kbd("esc")
                Text("cancel").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            #else
                Text("tap to set").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            #endif
            Spacer()
            // The half of this that is not obvious: the mail goes away NOW.
            Text("marks it done until then")
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
            KeyBinding("Enter", "set reminder", allowInInput: true) {
                if let hit = hits[safe: selection] { Task { await apply(hit) } }
            },
        ]
    }

    /// Set it. Then, and only on a stamp that actually landed, close and hand
    /// back to whoever opened us.
    ///
    /// THE PALETTE STAYS OPEN ON FAILURE, with what you typed still in it. The
    /// close and the caller's `onScheduled` (the reader walks to the next email
    /// on it) are the app saying the reminder took, and running that
    /// choreography over a 400 leaves the user watching the mail leave while a
    /// toast says it did not. So both hang off `Actions.remind`'s answer.
    private func apply(_ hit: RemindHit) async {
        guard !busy else { return }
        busy = true
        // THE PINNED CLOCK'S ESCAPE HATCH. `opened` is deliberately frozen so
        // the list cannot re-rank itself under the cursor between keystrokes —
        // which also means a palette left open long enough holds rows that have
        // since gone by, and the daemon 400s a reminder set in the past. Rather
        // than submit that, re-pin the clock and let the list re-resolve: the
        // same words now mean a time that is still ahead.
        guard hit.date > Date() else {
            opened = Date()
            selection = 0
            busy = false
            // `hits` is state now, so moving the clock does nothing until the
            // engine is re-asked.
            refreshHits()
            return
        }
        let ok = await Actions.remind(target.messageId, at: hit.date, label: hit.detail)
        guard ok else {
            busy = false
            return
        }
        onClose()
        target.onScheduled?()
    }
}

private struct RemindRow: View {
    let hit: RemindHit
    let selected: Bool
    let onHover: () -> Void
    let onPick: () -> Void

    var body: some View {
        // hoverFill off for the same reason the triage palette turns it off:
        // the pointer MOVES the selection here, so the selection fill already
        // follows it and a hover wash under it would double up.
        ListRow(
            selected: selected, tint: Palette.accent, hPadding: 10, vPadding: 7,
            hoverFill: false, onHoverChange: { if $0 { onHover() } }, action: onPick
        ) { _, _ in
            HStack(spacing: 10) {
                Text(hit.label)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                // THE ANSWER, on every row without exception.
                Text(hit.detail)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.accent.opacity(0.85))
                    .fixedSize()
            }
        }
    }
}
