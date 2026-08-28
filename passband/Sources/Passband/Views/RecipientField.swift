// The composer's "to" field: committed recipients render as PILLS in the send
// line, with a live text fragment after them. A pill is minted by accepting a
// suggestion, typing a comma, or typing a space after a complete address —
// space WITHOUT an "@" stays literal, because "alice j" is a display-name
// search, not an address. Backspace on an empty fragment is two-stage: first
// press highlights the last pill, second deletes it. Clicking a pill selects
// it the same way.
//
// A SELECTED PILL OPENS THE MOVE BAR — `→ cc`, `→ bcc`, `remove` — wherever
// this field is one of THREE (see `RecipientFields`). Moving somebody between
// headers is what people do halfway through addressing a message, and a
// composer that can only add and delete makes them retype an address they
// already got right. The same verbs hang off a right-click. Selection lands on
// PRESS rather than release, because a pill looks draggable and dragging is the
// first thing people try: the gesture that fails opens the bar offering the one
// that works.
//
// The surfaces that hold ONE list — the share sheet, a group's membership — pass
// no `moves` and get none of that, because there is nowhere to move anybody to.
//
// Autocomplete rides the fragment: every keystroke asks the daemon for
// Sent-derived contacts — people the user has actually written to — and the
// suggestion list opens under the field.
//
// STATE CONTRACT: `text` (ComposeState.to, the wire string) stays the single
// source of truth — pills and fragment are a PARSE of it, and every mutation
// writes back through the same binding, so DraftSaver's autosave hook and the
// send ceremony never learn pills exist. An external write (a draft restore)
// re-parses.
//
// The keymaps register into the pane's "modal" context: the suggestion set
// (arrows / Enter / Tab / Esc) mounts ONLY while a list is showing — within a
// context the latest-registered set wins, so an open list eats Enter before
// the pane's Enter-to-review sees it. The always-mounted set carries only
// Backspace, declining whenever there is fragment text to edit normally.

import SwiftUI

/// What a field can do with one of its pills besides delete it, handed in by
/// whoever owns all three lists.
///
/// A bundle rather than three loose closures because they are one capability:
/// a field either sits among siblings — and can pass somebody to them, and must
/// know who is already addressed anywhere — or it stands alone and can do none
/// of it. The WRITES live with the owner (`RecipientFields`) on purpose, since
/// only it holds the value `Recipients.move` has to operate on.
struct RecipientMoves {
    /// Which header this field is, so the menu offers the other two.
    let slot: RecipientSlot
    let move: (String, RecipientSlot) -> Void
    let remove: (String) -> Void
    /// Is this address already on the message ANYWHERE? Autocomplete asks, so it
    /// never re-offers somebody who is currently blind-copied.
    let addressed: (String) -> Bool
}

struct RecipientField<F: Hashable>: View {
    @Binding var text: String
    var focus: FocusState<F?>.Binding
    let field: F
    /// The caption over the well. Defaults to the composer's "to"; the group
    /// editor reuses this whole field for its membership list and says so.
    var label: String = "to"
    /// Placeholder for the empty field, for the same reason.
    var placeholder: String = "recipient@example.com"
    /// Offer SEND GROUPS alongside contacts as you type. Opt-in: the group
    /// editor reuses this field for its own membership list, and a group that
    /// could contain a group is not a thing this feature means.
    var suggestGroups: Bool = false
    /// Called when a group is accepted from the menu. The composer owns what
    /// happens next, because the answer depends on the group's mode — expand
    /// into this field, expand into the bcc field, or mint one pill — and this
    /// field knows about neither the other field nor the mode.
    var onGroupPicked: ((SendGroup) -> Void)?
    /// The group a `#slug` pill in this field stands for, when the composer has
    /// resolved one. Absent for a token it could not resolve, which is what the
    /// pill renders as a problem.
    var resolvedGroup: (name: String, count: Int)?
    /// WHERE A PILL CAN GO from here. Absent on the one-list surfaces, which is
    /// what turns the move bar and its menu off for them.
    var moves: RecipientMoves?
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
    /// Group matches, always ABOVE the contacts: there are few of them, picking
    /// one is the more deliberate act, and a group buried under eight addresses
    /// would never be found by the person who made it.
    @State private var groupHits: [SendGroup] = []
    @State private var index = 0

    /// The menu as one flat list, which is what the arrow keys walk.
    private var suggestionCount: Int { groupHits.count + hits.count }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            VStack(alignment: .leading, spacing: 5) {
                HStack(spacing: 8) {
                    FieldLabel(label)
                    Spacer(minLength: 0)
                    accessory
                }
                pillRow.fieldWell()
            }

            // The verbs for the pill under the caret. Only with one selected, so
            // an ordinary addressing pass never sees them.
            if let moves, let selectedPill, let addr = pills[safe: selectedPill] {
                moveBar(addr, moves)
            }

            if suggestionCount > 0 {
                suggestions
            }
        }
        .keyBindings(.modal, fieldBindings)
        .task(id: searchFragment) { await refresh() }
        .onAppear { parse(text) }
        // External writes only (a draft restore): our own writes round-trip to
        // exactly `composed`, and re-parsing those would fight the caret.
        .onChange(of: text) { _, now in
            if now != composed { parse(now) }
        }
        // A click elsewhere defocuses the field; a stale list or a half-armed
        // backspace must not linger.
        .onChange(of: focus.wrappedValue) { _, now in
            if now != field {
                hits = []
                groupHits = []
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
                    group: GroupToken.isToken(addr) ? resolvedGroup : nil,
                    moves: moves,
                    onTap: {
                        selectedPill = (selectedPill == i) ? nil : i
                        focus.wrappedValue = field
                    })
            }
            TextField(pills.isEmpty ? placeholder : "", text: $fragment)
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
    private func moveBar(_ addr: String, _ moves: RecipientMoves) -> some View {
        HStack(spacing: 6) {
            ForEach(RecipientSlot.allCases.filter { $0 != moves.slot }, id: \.self) { destination in
                Button("→ \(destination.label)") {
                    selectedPill = nil
                    moves.move(addr, destination)
                }
                .buttonStyle(.plain)
                .font(Typo.micro)
                .foregroundStyle(Palette.accent)
                .pointingHand()
            }
            Text("·").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            Button("remove") {
                selectedPill = nil
                moves.remove(addr)
            }
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

    /// Write the parse back to the wire string. The fragment rides along raw,
    /// so mid-typing the wire string is what a plain text field would hold.
    private func sync() {
        var parts = pills
        if !fragment.trimmed.isEmpty { parts.append(fragment.trimmed) }
        let next = parts.joined(separator: ", ")
        if text != next { text = next }
    }

    private var composed: String {
        var parts = pills
        if !fragment.trimmed.isEmpty { parts.append(fragment.trimmed) }
        return parts.joined(separator: ", ")
    }

    /// An external value: complete tokens become pills, a trailing partial
    /// (no comma after it) stays editable as the fragment.
    private func parse(_ value: String) {
        var tokens = Recipients.split(value)
        var trailingPartial = ""
        // A COMPLETE ADDRESS IS A PILL HOWEVER IT ARRIVED — moved in from
        // another field, restored from a draft, seeded from the daemon's
        // derivation. Only a trailing token that is not yet an address (no `@`,
        // and a group token is its own kind of complete) stays editable,
        // because that one is somebody mid-keystroke.
        //
        // Testing for the trailing COMMA alone was the bug: nothing but typing
        // puts one there, so every address that arrived any other way landed as
        // raw text in the well and stayed text until the sender pressed space —
        // which made "move to cc" look like it had half-worked.
        if !value.trimmed.hasSuffix(","), let last = tokens.last,
            Recipients.key(last).isEmpty, !GroupToken.isToken(last)
        {
            trailingPartial = last
            tokens.removeLast()
        }
        pills = tokens
        fragment = trailingPartial
        selectedPill = nil
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
            ForEach(Array(groupHits.enumerated()), id: \.element.id) { i, group in
                Button {
                    acceptGroup(group)
                } label: {
                    HStack(spacing: 8) {
                        Image(systemName: group.mode.symbol)
                            .font(.system(size: 9, weight: .semibold))
                            .foregroundStyle(group.mode.tone)
                            .frame(width: 12)
                        Text(group.name)
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(Palette.ink)
                            .lineLimit(1)
                        // The mode is said in the MENU, not only after picking:
                        // it decides whether these addresses are about to become
                        // visible to each other, and finding that out afterwards
                        // is finding out too late.
                        Text(group.mode.blurb)
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaintest)
                            .lineLimit(1)
                        Spacer(minLength: 8)
                        Text("\(group.member_count)")
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
            if !groupHits.isEmpty && !hits.isEmpty {
                Divider().overlay(Palette.hairline).padding(.vertical, 2)
            }
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
                            .foregroundStyle(
                                i + groupHits.count == index ? Palette.ink : Palette.inkDim
                            )
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
                        .fill(i + groupHits.count == index ? Palette.accentSoft : .clear)
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
                    let addr = pills[selected]
                    selectedPill = nil
                    // Through the owner when there is one, so a removal and a
                    // move take the same path out of the value.
                    if let moves {
                        moves.remove(addr)
                    } else {
                        pills.remove(at: selected)
                        sync()
                    }
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
                index = min(suggestionCount - 1, index + 1)
            },
            KeyBinding("ArrowUp", "prev suggestion", allowInInput: true) {
                index = max(0, index - 1)
            },
            KeyBinding("Enter", "accept suggestion", allowInInput: true) { acceptSelected() },
            KeyBinding("Tab", "accept suggestion", allowInInput: true) { acceptSelected() },
            KeyBinding("Escape", "dismiss suggestions", allowInInput: true) {
                hits = []
                groupHits = []
            },
        ]
    }

    // MARK: - state

    private func refresh() async {
        guard let searchFragment else {
            hits = []
            groupHits = []
            return
        }
        // Debounce: a fresh keystroke cancels this task before the request.
        try? await Task.sleep(for: .milliseconds(120))
        guard !Task.isCancelled else { return }
        // Both lookups in flight together, so adding groups to the menu costs a
        // round trip's latency rather than two.
        async let contacts = APIClient.shared.contacts(searchFragment)
        async let groups: [SendGroup] =
            suggestGroups ? APIClient.shared.searchGroups(searchFragment, limit: 4) : []
        let found = (try? await contacts) ?? []
        let foundGroups = (try? await groups) ?? []
        guard !Task.isCancelled else { return }
        // Anyone already addressed is a done deal, not a suggestion — and where
        // this field has siblings, that means addressed in ANY of the three.
        // Offering somebody who is currently blind-copied would be offering to
        // un-blind them by accident.
        hits = found.filter { hit in
            if let moves { return !moves.addressed(hit.addr) }
            return !pills.contains(hit.addr)
        }
        groupHits = foundGroups
        index = 0
    }

    /// Accept whatever the cursor is on, out of the flat list the arrows walk.
    private func acceptSelected() {
        if let group = groupHits[safe: index] {
            acceptGroup(group)
        } else if let hit = hits[safe: index - groupHits.count] {
            accept(hit)
        }
    }

    private func accept(_ hit: ContactHit) {
        commit(hit.addr)
        fragment = ""
        hits = []
        groupHits = []
        sync()
        // The caret stays in the field: accepting one recipient is usually the
        // middle of addressing, not the end of it.
        focus.wrappedValue = field
    }

    /// Hand the group up and clear the menu. What lands in which field is the
    /// composer's call — it depends on the group's mode, and this field knows
    /// about neither the mode nor the bcc row beside it.
    private func acceptGroup(_ group: SendGroup) {
        fragment = ""
        hits = []
        groupHits = []
        sync()
        onGroupPicked?(group)
        focus.wrappedValue = field
    }
}

// MARK: - pill

private struct RecipientPill: View {
    let addr: String
    let selected: Bool
    /// The group this pill stands for, when it is one. Set only by the composer,
    /// and only for a FAN-OUT group: `to`/`bcc` groups are expanded into ordinary
    /// address pills at pick time, so they never reach here.
    var group: (name: String, count: Int)?
    /// Present only where this pill has somewhere else to go.
    var moves: RecipientMoves?
    let onTap: () -> Void

    /// A group pill whose token no longer resolves. Rendered as a PROBLEM rather
    /// than silently dropped: the daemon refuses to send it, and the sender needs
    /// to know which audience went missing rather than watch a send fail.
    private var unresolved: Bool { group == nil && GroupToken.isToken(addr) }

    var body: some View {
        Button(action: onTap) {
            HStack(spacing: 5) {
                if group != nil {
                    Image(systemName: "person.2.fill").font(.system(size: 8, weight: .semibold))
                } else if unresolved {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.system(size: 8, weight: .semibold))
                }
                // Email-derived string: Text only, never markup.
                Text(label)
                    .font(group == nil ? Typo.mono(11) : .system(size: 11, weight: .medium))
                    .lineLimit(1)
                if let group {
                    // The count is the whole point of a fan-out pill: one pill
                    // stands for twelve emails, and the number is the only thing
                    // on screen that says so.
                    Text("· \(group.count) · will send individually")
                        .font(Typo.micro)
                        .opacity(0.75)
                        .lineLimit(1)
                }
            }
            .foregroundStyle(unresolved ? Palette.danger : Palette.ink)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .contentShape(Capsule())
        }
        // PRESS, not release. A pill looks draggable — a small object with a
        // person's name on it — and dragging is the first thing people try. It
        // does not drag (the window would come with it; see `Glass`), so the
        // reach that matters is the one that catches somebody mid-attempt:
        // press, and the move bar is already open under your hand offering the
        // thing you were reaching for. Still a `Button`, which is what keeps a
        // plain click on a pill from dragging the window.
        .buttonStyle(PressToSelect(onPress: onTap))
        .pointingHand()
        .background(Capsule().fill(fill))
        .overlay(
            Capsule().strokeBorder(
                unresolved
                    ? Palette.danger
                    : (selected ? Palette.accent : Palette.accent.opacity(0.25)),
                lineWidth: selected || unresolved ? 1.25 : 0.75)
        )
        .help(unresolved ? "this group could not be found; pick it again" : addr)
        // The same verbs the move bar offers, where a right-click (long-press on
        // a phone) goes looking for them. Two doors to one act, because moving a
        // recipient is the kind of thing people reach for both ways and it has
        // to be findable in either.
        .contextMenu {
            if let moves {
                ForEach(RecipientSlot.allCases.filter { $0 != moves.slot }, id: \.self) { dest in
                    Button("Move to \(dest.label.uppercased())") { moves.move(addr, dest) }
                }
                Divider()
                Button("Remove", role: .destructive) { moves.remove(addr) }
            }
        }
    }

    private var label: String {
        if let group { return group.name }
        if unresolved { return GroupToken.slug(addr) ?? addr }
        return addr
    }

    private var fill: Color {
        if unresolved { return Palette.danger.opacity(0.14) }
        return selected ? Palette.accent.opacity(0.38) : Palette.accentSoft
    }
}

/// A button style that fires on PRESS instead of on release, and draws nothing
/// of its own — the pill paints its own capsule.
///
/// `Button`'s own action waits for mouse-up inside, which is correct for
/// anything that should be abandonable by dragging away. A recipient pill is
/// the opposite case: dragging away IS the gesture worth catching, because
/// somebody trying to drag a pill is somebody looking for the move verbs.
private struct PressToSelect: ButtonStyle {
    let onPress: () -> Void

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            // Only the leading edge: `isPressed` falls back to false on release
            // (or on a drag away), and firing there would toggle the selection
            // straight off again.
            .onChange(of: configuration.isPressed) { _, pressed in
                if pressed { onPress() }
            }
    }
}

// MARK: - flow layout

/// Minimal wrapping row for the send line: pills flow left to right and wrap,
/// and the LAST subview — the text field — stretches to the end of its line so
/// a click anywhere in the well lands the caret.
///
/// Not private: the Groups page flows member chips through the same layout, and
/// two wrapping rows that disagreed by a point would be visible side by side.
struct FlowLine: Layout {
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
/// the invisible recipient this exists to make visible, and invisible in the one
/// direction that matters — the sender believing they are writing to fewer
/// people than they are. So `shown` ORs in "has content", and clicking a full
/// field's label focuses it instead of hiding it.
///
/// THIS is what owns the value the three fields share, which is why the move
/// verbs live here: `Recipients.move` needs all three lists to guarantee an
/// address lands in exactly one of them.
struct RecipientFields<F: Hashable>: View {
    @Binding var recipients: Recipients
    var focus: FocusState<F?>.Binding
    /// Maps a slot to the caller's own focus token — the two composers key their
    /// `@FocusState` differently.
    let field: (RecipientSlot) -> F
    /// Per-slot placeholder override. The reply composer uses it on `bcc`.
    var placeholder: (RecipientSlot) -> String? = { _ in nil }
    /// Group affordances, which belong to the `to` field alone: a group is an
    /// audience, and an audience is who the mail is TO.
    var suggestGroups = false
    var onGroupPicked: ((SendGroup) -> Void)?
    var resolvedGroup: (name: String, count: Int)?

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
                    text: binding(slot), focus: focus, field: field(slot),
                    label: slot.label,
                    // Only `to` names an example address: three wells all
                    // reading "recipient@example.com" reads as three fields
                    // waiting to be filled, when two are ordinarily empty and
                    // correct that way.
                    placeholder: placeholder(slot) ?? (slot == .to ? "recipient@example.com" : ""),
                    suggestGroups: slot == .to && suggestGroups,
                    onGroupPicked: slot == .to ? onGroupPicked : nil,
                    resolvedGroup: slot == .to ? resolvedGroup : nil,
                    moves: RecipientMoves(
                        slot: slot,
                        move: { addr, destination in
                            var next = recipients
                            next.move(addr, to: destination)
                            recipients = next
                            revealed.insert(destination)
                            focus.wrappedValue = field(slot)
                        },
                        remove: { addr in
                            var next = recipients
                            next.remove(addr)
                            recipients = next
                            focus.wrappedValue = field(slot)
                        },
                        addressed: { recipients.slot(of: $0) != nil }),
                    accessory: slot == .to ? AnyView(toggles) : nil)
            }
        }
    }

    /// One field's slice of the shared value. Writes go through `Recipients.set`,
    /// not a bare assignment: somebody TYPED into this field who was sitting in
    /// another one has just been moved here — typing an address out is as
    /// explicit as clicking it there — and leaving the old copy behind is the
    /// exact shape this whole block exists to make impossible: an address in To
    /// that the sender believes went out blind.
    private func binding(_ slot: RecipientSlot) -> Binding<String> {
        Binding(
            get: { recipients[slot] },
            set: { value in
                guard recipients[slot] != value else { return }
                var next = recipients
                next.set(slot, to: value)
                recipients = next
            })
    }

    /// The unfold labels, in the block's own micro voice. Lit while their field
    /// is up, so the row doubles as a statement of what this message carries.
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
