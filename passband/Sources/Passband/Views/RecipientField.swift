// ONE RECIPIENT HEADER — to, cc or bcc — as a row of PILLS with a live text
// fragment after them. A pill is minted by accepting a suggestion, typing a
// comma, or typing a space after a complete address — space WITHOUT an "@"
// stays literal, because "alice j" is a display-name search, not an address.
// Backspace on an empty fragment is two-stage: first press highlights the last
// pill, second deletes it. Clicking a pill selects it the same way.
//
// MOVING SOMEBODY BETWEEN FIELDS HAS TWO DOORS, because it is the thing people
// actually do halfway through addressing a message and a composer that can only
// add and delete makes them retype an address they already got right:
//
// - Click the pill and take the MOVE BAR: `→ cc`, `→ bcc`, `remove`.
// - Right-click it (long-press on a phone) for the same verbs in a menu.
//
// Both land on `Recipients.move`, which takes the address out of every OTHER
// field on the way. An address left in To while the sender believes it went out
// blind is the one outcome worth writing a whole type to prevent.
//
// Autocomplete rides the fragment: every keystroke asks the daemon for
// Sent-derived contacts — people the user has actually written to — and the
// suggestion list opens under the field.
//
// STATE CONTRACT: the bound `Recipients` stays the single source of truth —
// pills and fragment are a PARSE of this field's slice of it, and every
// mutation writes back through the same binding, so DraftSaver's autosave hook
// and the send ceremony never learn pills exist. An external write (a draft
// restore, or a MOVE performed by one of the sibling fields) re-parses.
//
// The keymaps register into the pane's "modal" context: the suggestion set
// (arrows / Enter / Tab / Esc) mounts ONLY while a list is showing — within a
// context the latest-registered set wins, so an open list eats Enter before
// the pane's Enter-to-review sees it. The always-mounted set carries only
// Backspace, declining whenever there is fragment text to edit normally.

import SwiftUI

struct RecipientField<F: Hashable>: View {
    /// ALL THREE HEADERS, not just this one's string: a move writes two fields
    /// at once, and the rule that an address lands in exactly one of them can
    /// only be enforced by something holding all three.
    @Binding var recipients: Recipients
    /// Which header this field edits.
    let slot: RecipientSlot
    var focus: FocusState<F?>.Binding
    let field: F
    /// Drawn in the well instead of the default when the field is empty — the
    /// reply composer uses it to say why a bcc is blank. nil for the ordinary
    /// case.
    var placeholder: String?
    /// Something to hang on the right of the LABEL row. The `to` field carries
    /// the cc/bcc toggles there, so they sit on the line that names the field
    /// they unfold rather than floating loose above the stack.
    var accessory: AnyView?

    /// Committed recipients — the pills.
    @State private var pills: [String] = []
    /// The live fragment being typed after the pills.
    @State private var fragment = ""
    /// Backspace stage two's target (and click-to-select).
    @State private var selectedPill: Int?

    @State private var hits: [ContactHit] = []
    @State private var index = 0

    /// This field's own slice of the bound value.
    private var text: String { recipients[slot] }

    /// What an empty well says. ONLY `to` names an example address: three wells
    /// all reading "recipient@example.com" reads as three fields waiting to be
    /// filled, when two of them are ordinarily empty and correct that way. The
    /// label above already says which header this is.
    private var placeholderText: String {
        if let placeholder { return placeholder }
        return slot == .to ? "recipient@example.com" : ""
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            VStack(alignment: .leading, spacing: 5) {
                HStack(spacing: 8) {
                    FieldLabel(slot.label)
                    Spacer(minLength: 0)
                    accessory
                }
                pillRow.fieldWell()
            }

            // The verbs for the pill under the caret. Only with one selected,
            // so an ordinary addressing pass never sees them.
            if let selectedPill, let addr = pills[safe: selectedPill] {
                moveBar(addr)
            }

            if !hits.isEmpty {
                suggestions
            }
        }
        .keyBindings(.modal, fieldBindings)
        .task(id: searchFragment) { await refresh() }
        .onAppear { parse(text) }
        // External writes only — a draft restore, or a sibling field moving an
        // address into (or out of) this one. Our own writes round-trip to
        // exactly `composed`, and re-parsing those would fight the caret.
        .onChange(of: text) { _, now in
            if now != composed { parse(now) }
        }
        // A click elsewhere defocuses the field; a stale list, a half-armed
        // backspace or an open move bar must not linger.
        .onChange(of: focus.wrappedValue) { _, now in
            if now != field {
                hits = []
                selectedPill = nil
            }
        }
    }

    // MARK: - send line

    private var pillRow: some View {
        FlowLine(spacing: 5) {
            ForEach(Array(pills.enumerated()), id: \.offset) { i, addr in
                RecipientPill(
                    addr: addr, selected: i == selectedPill,
                    onTap: {
                        selectedPill = (selectedPill == i) ? nil : i
                        focus.wrappedValue = field
                    },
                    onMove: { move(addr, to: $0) },
                    onRemove: { remove(addr) },
                    slot: slot)
            }
            TextField(pills.isEmpty ? placeholderText : "", text: $fragment)
                .textFieldStyle(.plain)
                .focused(focus, equals: field)
                .frame(minWidth: 120)
                // Commit-on-separator runs off onChange, not a custom Binding
                // setter: handing a method reference into Binding(get:set:)
                // trips an IRGen crash in Swift 6.3 (isolation thunk), and this
                // is the idiomatic shape anyway. Re-entrant sets terminate: an
                // unchanged value never re-fires onChange.
                .onChange(of: fragment) { _, raw in fragmentTyped(raw) }
        }
    }

    /// The selected pill's verbs, in the field's own micro voice. The two
    /// destinations are the OTHER two headers — moving a Cc to Cc is not an
    /// offer worth making.
    private func moveBar(_ addr: String) -> some View {
        HStack(spacing: 6) {
            ForEach(RecipientSlot.allCases.filter { $0 != slot }, id: \.self) { destination in
                Button("→ \(destination.label)") { move(addr, to: destination) }
                    .buttonStyle(.plain)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.accent)
                    .pointingHand()
            }
            Text("·").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Button("remove") { remove(addr) }
                .buttonStyle(.plain)
                .font(Typo.micro)
                .foregroundStyle(Palette.danger)
                .pointingHand()
            Spacer(minLength: 0)
        }
        .padding(.leading, 2)
    }

    /// Every keystroke and paste lands here. Commas ALWAYS commit the token
    /// before them; a trailing space commits only a token carrying "@".
    private func fragmentTyped(_ raw: String) {
        selectedPill = nil
        var rest = raw
        while let comma = rest.firstIndex(of: ",") {
            commit(String(rest[..<comma]))
            rest = String(rest[rest.index(after: comma)...])
        }
        if rest.hasSuffix(" "), rest.trimmed.contains("@") {
            commit(rest)
            rest = ""
        }
        if rest.drop(while: { $0 == " " }).isEmpty { rest = "" }
        if fragment != rest { fragment = rest }
        sync()
    }

    private func commit(_ token: String) {
        let addr = token.trimmed
        guard !addr.isEmpty, !pills.contains(addr) else { return }
        pills.append(addr)
    }

    /// Write the parse back to this field's slice. The fragment rides along raw,
    /// so mid-typing the stored string is what a plain text field would hold.
    private func sync() {
        let next = composed
        guard recipients[slot] != next else { return }
        var updated = recipients
        // Through `set`, not a bare assignment: somebody TYPED into this field
        // who is currently sitting in another one has just been moved here, and
        // leaving the old copy behind is the exact shape this whole feature
        // exists to make impossible — an address in To that the sender believes
        // went out blind.
        updated.set(slot, to: next)
        write(updated)
    }

    private var composed: String {
        var parts = pills
        if !fragment.trimmed.isEmpty { parts.append(fragment.trimmed) }
        return Recipients.join(parts)
    }

    /// An external value, turned back into pills and a live fragment.
    ///
    /// A COMPLETE ADDRESS IS A PILL HOWEVER IT ARRIVED — moved in from another
    /// field, dropped from a drag, restored from a draft, seeded from the
    /// daemon's derivation. Only a trailing token that is not yet an address
    /// (no `@`) stays editable, because that one is somebody mid-keystroke.
    ///
    /// Testing for the trailing COMMA alone was the bug: nothing but typing
    /// puts one there, so every address that arrived any other way landed as
    /// raw text in the well and stayed text until the sender pressed space —
    /// which made "move to cc" look like it had half-worked.
    private func parse(_ value: String) {
        var tokens = Recipients.split(value)
        var trailingPartial = ""
        if !value.trimmed.hasSuffix(","), let last = tokens.last,
            Recipients.key(last).isEmpty
        {
            trailingPartial = last
            tokens.removeLast()
        }
        pills = tokens
        fragment = trailingPartial
        selectedPill = nil
    }

    // MARK: - moves

    /// Hand one addressee to another header. `Recipients.move` is what makes
    /// this safe: the address comes OUT of this field (and any other holding
    /// it) before it goes in anywhere, so it is never in two headers at once.
    private func move(_ addr: String, to destination: RecipientSlot) {
        var updated = recipients
        updated.move(addr, to: destination)
        selectedPill = nil
        write(updated)
        // The caret stays here: moving somebody out is usually the middle of
        // sorting the audience, not the end of it.
        focus.wrappedValue = field
    }

    private func remove(_ addr: String) {
        var updated = recipients
        updated.remove(addr)
        selectedPill = nil
        write(updated)
        focus.wrappedValue = field
    }

    /// The single write path. A move touches TWO fields, so the sibling's
    /// `onChange` re-parses off the same value this one does.
    private func write(_ updated: Recipients) {
        recipients = updated
        // Our own field may have changed underneath the parse (a move out of
        // it), and `onChange` only fires for a value that differs from what we
        // composed. Re-parse eagerly rather than rely on that race.
        if updated[slot] != composed { parse(updated[slot]) }
    }

    // MARK: - suggestions

    /// What autocomplete searches: the fragment, while the field has focus.
    private var searchFragment: String? {
        guard focus.wrappedValue == field else { return nil }
        let trimmed = fragment.trimmed
        return trimmed.isEmpty ? nil : trimmed
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
        // Mounts WITH the list, so these register after everything else and win
        // exactly while there is a list to drive.
        .keyBindings(.modal, suggestionBindings)
    }

    // MARK: - keymaps

    /// Always mounted: the two-stage backspace. Declines whenever the fragment
    /// still has text, so ordinary editing never notices it.
    private var fieldBindings: [KeyBinding] {
        [
            KeyBinding(declining: "Backspace", "remove recipient", allowInInput: true) {
                guard focus.wrappedValue == field, fragment.isEmpty, !pills.isEmpty
                else { return false }
                if let selected = selectedPill {
                    guard let addr = pills[safe: selected] else { return false }
                    remove(addr)
                } else {
                    selectedPill = pills.count - 1
                }
                return true
            }
        ]
    }

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
        guard let searchFragment else {
            hits = []
            return
        }
        // Debounce: a fresh keystroke cancels this task before the request.
        try? await Task.sleep(for: .milliseconds(120))
        guard !Task.isCancelled else { return }
        let found = (try? await APIClient.shared.contacts(searchFragment)) ?? []
        guard !Task.isCancelled else { return }
        // Anyone already addressed — in ANY of the three headers — is a done
        // deal rather than a suggestion. Offering somebody who is currently
        // blind-copied would be offering to un-blind them by accident.
        hits = found.filter { recipients.slot(of: $0.addr) == nil }
        index = 0
    }

    private func accept(_ hit: ContactHit) {
        commit(hit.addr)
        fragment = ""
        hits = []
        sync()
        // The caret stays in the field: accepting one recipient is usually the
        // middle of addressing, not the end of it.
        focus.wrappedValue = field
    }
}

// MARK: - pill

private struct RecipientPill: View {
    let addr: String
    let selected: Bool
    let onTap: () -> Void
    let onMove: (RecipientSlot) -> Void
    let onRemove: () -> Void
    /// Where this pill currently lives, so the menu offers the other two.
    let slot: RecipientSlot

    /// A `Button`, and deliberately still one. It is also what keeps a click on
    /// a pill from dragging the whole WINDOW: the app hides its titlebar and
    /// moves by its background (see `Glass.WindowConfigurator`), so anything
    /// AppKit reads as background is a window handle — and a bare SwiftUI shape
    /// with a tap gesture is background. A Button is not.
    var body: some View {
        Button(action: onTap) {
            // Email-derived string: Text only, never markup.
            Text(addr)
                .font(Typo.mono(11))
                .foregroundStyle(Palette.ink)
                .lineLimit(1)
                .padding(.horizontal, 8)
                .padding(.vertical, 3)
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .background(
            Capsule().fill(selected ? Palette.accent.opacity(0.38) : Palette.accentSoft)
        )
        .overlay(
            Capsule().strokeBorder(
                selected ? Palette.accent : Palette.accent.opacity(0.25),
                lineWidth: selected ? 1.25 : 0.75)
        )
        .pointingHand()
        // The same verbs the move bar offers, where a right-click (long-press on
        // a phone) goes looking for them. Two doors to one act, because moving a
        // recipient is the kind of thing people reach for both ways and it has
        // to be findable in either.
        .contextMenu {
            ForEach(RecipientSlot.allCases.filter { $0 != slot }, id: \.self) { destination in
                Button("Move to \(destination.label.uppercased())") { onMove(destination) }
            }
            Divider()
            Button("Remove", role: .destructive, action: onRemove)
        }
    }
}

// MARK: - flow layout

/// Minimal wrapping row for the send line: pills flow left to right and wrap,
/// and the LAST subview — the text field — stretches to the end of its line so
/// a click anywhere in the well lands the caret.
private struct FlowLine: Layout {
    var spacing: CGFloat = 5

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let width = proposal.width ?? .infinity
        var x: CGFloat = 0
        var y: CGFloat = 0
        var rowHeight: CGFloat = 0
        for view in subviews {
            let size = view.sizeThatFits(.unspecified)
            if x > 0, x + size.width > width {
                x = 0
                y += rowHeight + spacing
                rowHeight = 0
            }
            x += size.width + spacing
            rowHeight = max(rowHeight, size.height)
        }
        return CGSize(width: proposal.width ?? x, height: y + rowHeight)
    }

    func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
    ) {
        var x = bounds.minX
        var y = bounds.minY
        var rowHeight: CGFloat = 0
        for (i, view) in subviews.enumerated() {
            var size = view.sizeThatFits(.unspecified)
            if x > bounds.minX, x + size.width > bounds.maxX {
                x = bounds.minX
                y += rowHeight + spacing
                rowHeight = 0
            }
            // The field takes the rest of its line.
            if i == subviews.count - 1 {
                size.width = max(size.width, bounds.maxX - x)
            }
            view.place(
                at: CGPoint(x: x, y: y),
                proposal: ProposedViewSize(width: size.width, height: size.height))
            x += size.width + spacing
            rowHeight = max(rowHeight, size.height)
        }
    }
}

// MARK: - the three fields together

/// THE ADDRESS BLOCK: `to`, plus `cc` and `bcc` FOLDED AWAY behind their own
/// labels on its line.
///
/// Folded because most mail has neither, and three wells for a message going to
/// one person is three questions where there was one. The labels are the whole
/// affordance — click "cc", the field unfolds with the caret already in it.
///
/// A FIELD HOLDING ADDRESSES IS NEVER FOLDED, whatever the toggle last said.
/// That is not a nicety: a restored draft whose bcc hid itself would be exactly
/// the invisible recipient this feature exists to make visible, and it would be
/// invisible in the one direction that matters — the sender believing they are
/// writing to fewer people than they are. So `shown` ORs in "has content", and
/// clicking a full field's label focuses it instead of hiding it.
struct RecipientFields<F: Hashable>: View {
    @Binding var recipients: Recipients
    var focus: FocusState<F?>.Binding
    /// Maps a slot to the caller's own focus token — the two composers key
    /// their `@FocusState` differently.
    let field: (RecipientSlot) -> F
    /// Per-slot placeholder override. The reply composer uses it on `bcc`.
    var placeholder: (RecipientSlot) -> String? = { _ in nil }

    /// Which optional fields the sender has unfolded this session.
    @State private var revealed: Set<RecipientSlot> = []

    /// The two that fold. `to` is not one of them: a message with no recipient
    /// is not a message.
    private static var optional: [RecipientSlot] { [.cc, .bcc] }

    private func shown(_ slot: RecipientSlot) -> Bool {
        slot == .to || revealed.contains(slot) || !recipients[slot].trimmed.isEmpty
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(RecipientSlot.allCases.filter(shown), id: \.self) { slot in
                RecipientField(
                    recipients: $recipients, slot: slot, focus: focus, field: field(slot),
                    placeholder: placeholder(slot),
                    accessory: slot == .to ? AnyView(toggles) : nil)
            }
        }
    }

    /// The unfold labels, in the field block's own micro voice. Lit while their
    /// field is up, so the row doubles as a statement of what this message
    /// currently carries.
    private var toggles: some View {
        HStack(spacing: 8) {
            ForEach(Self.optional, id: \.self) { slot in
                Button(slot.label) { toggle(slot) }
                    .buttonStyle(.plain)
                    .font(Typo.micro)
                    .foregroundStyle(shown(slot) ? Palette.accent : Palette.inkFaintest)
                    // Two words in the micro voice look exactly like the label
                    // beside them until the pointer says otherwise.
                    .pointingHand()
                    .accessibilityLabel(shown(slot) ? "hide \(slot.label)" : "add \(slot.label)")
            }
        }
    }

    private func toggle(_ slot: RecipientSlot) {
        // A field with people in it does not fold away — it takes the caret, so
        // the click still does something and what it does is visible.
        guard recipients[slot].trimmed.isEmpty else {
            focus.wrappedValue = field(slot)
            return
        }
        if revealed.contains(slot) {
            revealed.remove(slot)
        } else {
            revealed.insert(slot)
            focus.wrappedValue = field(slot)
        }
    }
}
