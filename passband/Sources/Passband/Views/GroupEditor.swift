// Make or edit a send group: a name, the people, and HOW the mail goes out.
//
// The mode picker is the load-bearing control. It is a property of the audience
// rather than of any one message — an investor list is individually-addressed
// every time or it is not one — so it is answered here, once, and the composer
// obeys it instead of re-asking at the moment of sending. Each option carries a
// line saying what it does to the recipients, because the difference between
// them is invisible in the mail that comes out the other end.
//
// The membership field IS `RecipientField`, the composer's own: pills, backspace
// staging, and Sent-derived autocomplete, so building a group is the same
// gesture as addressing an email. Its wire string is comma-joined addresses,
// which is exactly what the daemon's `members` array is built from.

import SwiftUI

struct GroupEditor: View {
    let request: GroupEditorRequest
    let onClose: () -> Void

    @Environment(AppStore.self) private var store

    @State private var name: String
    @State private var mode: GroupMode
    @State private var note: String
    /// The membership as `RecipientField`'s wire string — comma-joined. Held as
    /// text rather than as `[GroupMember]` because that field's whole contract is
    /// that the string is the single source of truth and pills are a parse of it.
    @State private var membersText: String
    @State private var saving = false
    @State private var error: String?
    /// The daemon's cap, fetched rather than compiled in. nil until it lands, and
    /// the count check simply does not fire until then — a limit we have not been
    /// told is not a limit to enforce by guessing.
    @State private var maxMembers: Int?
    @FocusState private var focusedField: FocusTarget?

    private enum FocusTarget { case name, members, note }

    init(request: GroupEditorRequest, onClose: @escaping () -> Void) {
        self.request = request
        self.onClose = onClose
        if let group = request.group {
            _name = State(initialValue: group.name)
            _mode = State(initialValue: group.mode)
            _note = State(initialValue: group.note)
            _membersText = State(
                initialValue: (group.members ?? []).map(\.addr).joined(separator: ", "))
        } else {
            _name = State(initialValue: "")
            // `to` is the default because it is the VISIBLE shape. A group that
            // silently defaulted to bcc would conceal an audience by accident;
            // one that defaults to `to` is at worst a choice you can see and
            // change.
            _mode = State(initialValue: .to)
            _note = State(initialValue: "")
            _membersText = State(
                initialValue: request.seedMembers.map(\.addr).joined(separator: ", "))
        }
    }

    private var editing: Bool { request.group != nil }

    /// The parse the save sends, and what the count below the field reports.
    private var members: [GroupMember] {
        membersText
            .split(separator: ",")
            .map { GroupMember(addr: String($0).trimmed, display_name: nil) }
            .filter { !$0.addr.isEmpty }
    }

    var body: some View {
        OverlayScrim(onDismiss: onClose) {
            ModalCard(width: 500) {
                header
                nameField
                membersField
                modePicker
                noteField

                if let error {
                    Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
                }

                footer
            }
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "cancel", allowInInput: true) { onClose() },
            // ⌘Enter, not Enter: the membership field owns Enter for accepting an
            // autocomplete suggestion, and a bare Enter that sometimes saved and
            // sometimes committed a recipient would be the worst key in the app.
            KeyBinding("Enter", "save group", meta: true, allowInInput: true) {
                Task { await save() }
            },
        ])
        .onAppear { focusedField = editing ? .members : .name }
        .task { maxMembers = try? await APIClient.shared.groupLimits().max_members }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(editing ? "edit group" : "new group")
                .font(Typo.sectionLabel)
                .foregroundStyle(Palette.ink)
                .textCase(.uppercase)
            Text("a set of people you write to together, addressed by name.")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
        }
        .padding(.bottom, 2)
    }

    private var nameField: some View {
        Field(label: "name") {
            TextField("preseed investors", text: $name)
                .textFieldStyle(.plain)
                .focused($focusedField, equals: .name)
        }
    }

    private var membersField: some View {
        VStack(alignment: .leading, spacing: 4) {
            RecipientField(
                text: $membersText, focus: $focusedField, field: FocusTarget.members,
                label: "people", placeholder: "name or email")
            HStack(spacing: 5) {
                Text("\(members.count) \(members.count == 1 ? "person" : "people")")
                    .font(Typo.micro)
                    .foregroundStyle(overCap ? Palette.danger : Palette.inkFaintest)
                if let maxMembers, overCap {
                    Text("· at most \(maxMembers)")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.danger)
                }
                Spacer()
            }
        }
    }

    private var overCap: Bool {
        guard let maxMembers else { return false }
        return members.count > maxMembers
    }

    /// Three options, each with the consequence written under it. The picker is
    /// the reason this card exists rather than a rename dialog.
    private var modePicker: some View {
        VStack(alignment: .leading, spacing: 5) {
            FieldLabel("how mail goes out")
            GlassSegmented(
                options: GroupMode.allCases.map { (value: $0, label: $0.label) },
                selection: $mode)
            HStack(spacing: 5) {
                Image(systemName: mode.symbol)
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(mode.tone)
                Text(modeHint)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    /// The consequence, spelled out per mode. Individual says the count out loud
    /// because "one separate email each" is abstract until it is twelve.
    private var modeHint: String {
        switch mode {
        case .to:
            "one email, everyone in To. they can see the whole list and reply to all of it."
        case .bcc:
            "one email, everyone in Bcc. nobody sees who else got it."
        case .individual:
            members.count > 1
                ? "\(members.count) separate emails, one per person. replies come back as private threads."
                : "one separate email per person. replies come back as private threads."
        }
    }

    private var noteField: some View {
        Field(label: "note (optional)") {
            TextField("what this group is for", text: $note)
                .textFieldStyle(.plain)
                .focused($focusedField, equals: .note)
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 9) {
            #if os(macOS)
                HStack(spacing: 4) {
                    KeyHint("⌘↵", "save")
                    Text("·").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    KeyHint("esc", "cancel")
                    Spacer()
                }
            #endif
            HStack(spacing: 8) {
                if let blocked {
                    Text(blocked)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.warn)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 8)
                Button(cancelLabel, action: onClose).buttonStyle(.glass)
                Button(saving ? "saving…" : (editing ? "update group" : "save group")) {
                    Task { await save() }
                }
                .buttonStyle(.glassProminent)
                .tint(Palette.accent)
                .disabled(saving || blocked != nil)
            }
        }
    }

    private var cancelLabel: String {
        #if os(macOS)
            "esc cancel"
        #else
            "cancel"
        #endif
    }

    /// Why save is shut, in the user's terms, or nil when it is open. Checked
    /// BEFORE the round trip so the daemon's own refusals land as hints on a form
    /// still in front of you rather than as errors over one you have left.
    private var blocked: String? {
        if name.trimmed.isEmpty { return "needs a name" }
        if members.isEmpty { return "needs at least one person" }
        if let maxMembers, members.count > maxMembers {
            return "at most \(maxMembers) people in one group"
        }
        return nil
    }

    private func save() async {
        guard blocked == nil, !saving else { return }
        saving = true
        defer { saving = false }
        let body = GroupBody(
            name: name.trimmed, mode: mode, note: note.trimmed, members: members)
        do {
            if let group = request.group {
                _ = try await APIClient.shared.updateGroup(group.id, body)
            } else {
                _ = try await APIClient.shared.createGroup(body)
            }
            // Shape only: how the audience is addressed and how big it is. No
            // address is anywhere near this, at any level.
            Analytics.capture(
                editing ? "group_updated" : "group_created",
                ["mode": mode.rawValue, "members": members.count])
            request.onSaved?()
            onClose()
        } catch {
            // Stays on the card with the error under the fields, so a rejected
            // name or a member the daemon would not accept can be fixed without
            // retyping the rest.
            self.error = errText(error, "save failed")
        }
    }
}
