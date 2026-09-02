// The search field's `from:` menu: who has written to you, offered under the
// field while the trailing token is a `from:` operator, the way the composer's
// To field offers contacts. Accepting one completes the operator in the query
// (`from:dan@example.com `) and leaves the caret in the field, because a sender
// is usually the FIRST half of a search, not the whole of it.
//
// WHAT IT SEARCHES is the daemon's sender directory (`/client/senders`), not
// contacts: contacts are the people you write TO, and `from:dan` has to find
// the Dan who has only ever replied. An empty fragment (the reader has typed
// `from:` and nothing else yet) lists the senders with the most mail, so the
// menu means something the instant the operator is typed.
//
// The keymap follows RecipientField's rule: the suggestion set mounts WITH the
// list, so it registers after the panel's own arrows and Enter and wins exactly
// while there is a list to drive; Esc dismisses the list and only the list. The
// owning view decides when this is on screen at all (`FromOperator.fragment`),
// which is what keeps a `from:` earlier in the query from reopening the menu.

import SwiftUI

struct SenderSuggestions: View {
    /// The whole query, bound: accepting a sender rewrites the trailing token in
    /// place, so the panel's debounced search sees the completed operator the
    /// same way it sees any other keystroke.
    @Binding var query: String
    /// The text after `from:` in the trailing token; the owner passes what
    /// `FromOperator.fragment` returned so the two agree about what is open.
    let fragment: String

    @State private var hits: [SenderHit] = []
    @State private var index = 0
    /// Set when the reader dismissed the list with Esc. Cleared by the next
    /// change to the fragment, so Esc means "not this menu" rather than "never
    /// again this session".
    @State private var dismissed = false

    /// Same as RecipientField's: long enough that a typing run makes one
    /// request, short enough that the menu keeps up with the field.
    private static let debounce = Duration.milliseconds(120)

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            ForEach(Array(hits.enumerated()), id: \.element.id) { i, hit in
                Button {
                    accept(hit)
                } label: {
                    HStack(spacing: 8) {
                        // Sender strings are email-derived: rendered as Text
                        // only, never as markup.
                        if let name = hit.display_name, !name.isEmpty {
                            Text(name)
                                .font(.system(size: 11, weight: .medium))
                                .foregroundStyle(Palette.ink)
                                .lineLimit(1)
                        }
                        Text(hit.addr)
                            .font(Typo.mono(11))
                            .foregroundStyle(i == index ? Palette.ink : Palette.inkDim)
                            .lineLimit(1)
                        Spacer(minLength: 8)
                        Text("\(hit.msg_count)×")
                            .font(Typo.num(10))
                            .foregroundStyle(Palette.inkFaintest)
                    }
                    .padding(.horizontal, 9)
                    .padding(.vertical, 5)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .background(
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .fill(i == index ? Palette.accentSoft : .clear)
                )
            }
            if !hits.isEmpty {
                HStack(spacing: 4) {
                    Kbd("↑↓")
                    Text("pick ·").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    Kbd("enter")
                    Text("accept ·").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    Kbd("esc")
                    Text("dismiss").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                }
                .padding(.horizontal, 9)
                .padding(.top, 4)
            }
        }
        .padding(hits.isEmpty ? 0 : 4)
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(hits.isEmpty ? .clear : Palette.canvas.opacity(0.85))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .strokeBorder(
                    hits.isEmpty ? .clear : Palette.hairlineStrong, lineWidth: 0.75))
        // Mounts WITH the list, so these register after the panel's own keys and
        // win exactly while there is a list to drive. An empty binding set is a
        // no-op registration, which is what makes the conditional honest.
        .keyBindings(.modal, hits.isEmpty ? [] : suggestionBindings)
        .task(id: fragment) { await refresh() }
    }

    // MARK: - keymap

    private var suggestionBindings: [KeyBinding] {
        [
            KeyBinding("ArrowDown", "next sender", allowInInput: true) {
                index = min(hits.count - 1, index + 1)
            },
            KeyBinding("ArrowUp", "prev sender", allowInInput: true) {
                index = max(0, index - 1)
            },
            KeyBinding("Enter", "accept sender", allowInInput: true) { acceptSelected() },
            KeyBinding("Tab", "accept sender", allowInInput: true) { acceptSelected() },
            KeyBinding("Escape", "dismiss senders", allowInInput: true) {
                hits = []
                dismissed = true
            },
        ]
    }

    // MARK: - state

    private func refresh() async {
        // A new fragment is a new question; an Esc answered the old one.
        dismissed = false
        // Debounce: a fresh keystroke cancels this task before the request.
        try? await Task.sleep(for: Self.debounce)
        guard !Task.isCancelled else { return }
        let found = (try? await APIClient.shared.senders(fragment)) ?? []
        guard !Task.isCancelled, !dismissed else { return }
        hits = found
        index = 0
    }

    private func acceptSelected() {
        guard let hit = hits[safe: index] else { return }
        accept(hit)
    }

    private func accept(_ hit: SenderHit) {
        query = FromOperator.accepting(hit.addr, in: query)
        // The list goes with the token it was for. The owner unmounts this view
        // on the next render (the trailing token is no longer a `from:`), but
        // the keymap must not survive even one event past the accept.
        hits = []
    }
}
