// The composer's "to" field with recipient autocomplete: every keystroke asks
// the daemon for Sent-derived contacts — people the user has actually written
// to — and a suggestion list opens under the field. Comma-aware: only the
// fragment after the last comma is completed, so multiple recipients each get
// their own suggestions.
//
// The suggestion keymap (arrows / Enter / Tab / Esc) registers into the pane's
// own "modal" context and ONLY while suggestions are showing: within a context
// the latest-registered set wins, so an open list eats Enter before the pane's
// Enter-to-review sees it, and closing the list hands every key straight back.

import SwiftUI

struct RecipientField<F: Hashable>: View {
    @Binding var text: String
    var focus: FocusState<F?>.Binding
    let field: F

    @State private var hits: [ContactHit] = []
    @State private var index = 0

    /// The comma-separated token under the caret's end, which is what gets
    /// completed. Suggestions only make sense while the field has focus.
    private var fragment: String? {
        guard focus.wrappedValue == field else { return nil }
        let tail = text.split(separator: ",", omittingEmptySubsequences: false).last.map(String.init) ?? text
        let trimmed = tail.trimmed
        return trimmed.isEmpty ? nil : trimmed
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Field(label: "to") {
                TextField("recipient@example.com", text: $text)
                    .textFieldStyle(.plain)
                    .focused(focus, equals: field)
            }

            if !hits.isEmpty {
                suggestions
            }
        }
        .task(id: fragment) { await refresh() }
        // A click elsewhere defocuses the field; a stale list must not linger.
        .onChange(of: focus.wrappedValue) { _, now in
            if now != field { hits = [] }
        }
    }

    private var suggestions: some View {
        VStack(alignment: .leading, spacing: 1) {
            ForEach(Array(hits.enumerated()), id: \.element.id) { i, hit in
                Button {
                    accept(hit)
                } label: {
                    HStack(spacing: 8) {
                        // Contact strings are email-derived: rendered as Text
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
                        Text("\(hit.sent_count)×")
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
        .padding(4)
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(Palette.canvas.opacity(0.85))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .strokeBorder(Palette.hairlineStrong, lineWidth: 0.75))
        // Mounts WITH the list, so these register after the pane's set and win
        // exactly while there is a list to drive.
        .keyBindings(.modal, suggestionBindings)
    }

    // MARK: - keymap

    private var suggestionBindings: [KeyBinding] {
        [
            KeyBinding("ArrowDown", "next suggestion", allowInInput: true) {
                index = min(hits.count - 1, index + 1)
            },
            KeyBinding("ArrowUp", "prev suggestion", allowInInput: true) {
                index = max(0, index - 1)
            },
            KeyBinding("Enter", "accept suggestion", allowInInput: true) {
                if let hit = hits[safe: index] { accept(hit) }
            },
            KeyBinding("Tab", "accept suggestion", allowInInput: true) {
                if let hit = hits[safe: index] { accept(hit) }
            },
            KeyBinding("Escape", "dismiss suggestions", allowInInput: true) {
                hits = []
            },
        ]
    }

    // MARK: - state

    private func refresh() async {
        guard let fragment else {
            hits = []
            return
        }
        // Debounce: a fresh keystroke cancels this task before the request.
        try? await Task.sleep(for: .milliseconds(120))
        guard !Task.isCancelled else { return }
        let found = (try? await APIClient.shared.contacts(fragment)) ?? []
        guard !Task.isCancelled else { return }
        // The lone hit that IS the fragment is an accepted address, not a
        // suggestion — showing it would nag after every accept.
        hits = (found.count == 1 && found[0].addr == fragment.lowercased()) ? [] : found
        index = 0
    }

    private func accept(_ hit: ContactHit) {
        var tokens = text.split(separator: ",", omittingEmptySubsequences: false)
            .map { String($0).trimmed }
        if tokens.isEmpty {
            tokens = [hit.addr]
        } else {
            tokens[tokens.count - 1] = hit.addr
        }
        text = tokens.joined(separator: ", ")
        hits = []
        // The caret stays in the field: accepting one recipient is usually the
        // middle of addressing, not the end of it.
        focus.wrappedValue = field
    }
}
