// Send groups from GET /client/groups: named audiences you address as one.
// Two columns — the groups on the left, the selected one's detail on the right:
// its members, and below them the mail that has already gone to it.
//
// THE HISTORY IS THE POINT OF THE RIGHT COLUMN. A group is made for people you
// have been emailing for a year, so the daemon answers with two sources unioned
// — sends made THROUGH the group, and mail derived by matching stored recipients
// against the membership — and this page does not distinguish them beyond a
// mode chip, because to the reader they are one question: what have I sent these
// people.
//
// Keys: j/k select · n new · e/Enter edit · x delete.
//
// The rail slot is ⌘6, appended below Audit rather than inserted, so no digit
// anybody already has in their fingers moved.

import SwiftUI

struct GroupsView: View {
    @Environment(AppStore.self) private var store

    @State private var groupsState: Loadable<[SendGroup]> = .loading
    @State private var index = 0

    /// The selected group's detail — membership plus history — keyed by id so a
    /// stale response for a group the user has already moved off is dropped
    /// rather than painted over the new one.
    @State private var detail: Loadable<SendGroup> = .idle
    @State private var history: Loadable<[GroupHistoryEntry]> = .idle
    @State private var detailFor: Int?

    private var groups: [SendGroup] { groupsState.value ?? [] }
    private var selected: SendGroup? { groups[safe: index] }

    var body: some View {
        Group {
            if groupsState.isLoading && groups.isEmpty {
                BandNote("loading groups…")
            } else if let error = groupsState.error, groups.isEmpty {
                BandNote(error)
            } else if groups.isEmpty {
                empty
            } else {
                HStack(spacing: 0) {
                    groupList
                    Divider().overlay(Palette.hairline)
                    detailColumn
                }
                footer
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .keyBindings(.modal, bindings)
        .task {
            GroupsReload.shared.handler = { await load() }
            await load()
        }
        .onDisappear { GroupsReload.shared.handler = nil }
        .onChange(of: groups.count) { _, count in
            index = max(0, min(index, max(0, count - 1)))
        }
        // Selection drives the right column. Keyed on the ID rather than the
        // index: a delete that shifts everything up must re-fetch, and a reload
        // that leaves the same group selected must not.
        .task(id: selected?.id) { await loadDetail() }
    }

    private var empty: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 4) {
                Text("no groups yet. press").font(Typo.rowSub)
                Kbd("n")
                Text("to make one.").font(Typo.rowSub)
            }
            Text(
                "a group is a set of people you write to together, like your investors or your design partners. name it once and address it by name."
            )
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaintest)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: 420, alignment: .leading)
        }
        .foregroundStyle(Palette.inkFaintest)
        .padding(.horizontal, 22)
        .padding(.vertical, 24)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    // MARK: - left column

    private var groupList: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(spacing: 1) {
                    ForEach(Array(groups.enumerated()), id: \.element.id) { i, group in
                        GroupRow(
                            group: group, selected: i == index,
                            onSelect: { index = i },
                            onEdit: { edit(group) }
                        )
                        .id(group.id)
                    }
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 10)
            }
            .onChange(of: index) { _, i in
                guard let group = groups[safe: i] else { return }
                withAnimation(Motion.scrollFollow) { proxy.scrollTo(group.id, anchor: .center) }
            }
        }
        .frame(width: 250)
    }

    // MARK: - right column

    @ViewBuilder
    private var detailColumn: some View {
        if let group = selected {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    detailHeader(group)
                    membersSection(group)
                    historySection
                }
                .padding(.horizontal, 20)
                .padding(.vertical, 14)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        } else {
            Color.clear.frame(maxWidth: .infinity)
        }
    }

    private func detailHeader(_ group: SendGroup) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Text(group.name)
                    .font(Typo.sectionLabel)
                    .foregroundStyle(Palette.ink)
                Chip(text: group.mode.label, tone: group.mode.tone, filled: true)
                Spacer()
                Button("edit") { edit(group) }
                    .buttonStyle(.glass)
                    .font(Typo.micro)
            }
            // What this group DOES when you address it, in one line. The mode is
            // the single most consequential thing about a group and the least
            // visible in the mail it produces, so it is spelled out rather than
            // left to a three-letter chip.
            Text(group.mode.blurb)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
            if !group.note.isEmpty {
                Text(group.note)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    @ViewBuilder
    private func membersSection(_ group: SendGroup) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            SectionHead("\(group.member_count) \(group.member_count == 1 ? "person" : "people")")
            if let members = detail.value?.members, !members.isEmpty {
                FlowLine(spacing: 5) {
                    ForEach(members) { member in
                        MemberChip(member: member)
                    }
                }
            } else if detail.isLoading {
                Text("loading…").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            } else if let error = detail.error {
                Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
            }
        }
    }

    @ViewBuilder
    private var historySection: some View {
        VStack(alignment: .leading, spacing: 7) {
            SectionHead("sent to this group")
            if history.isLoading && history.value == nil {
                Text("loading…").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            } else if let error = history.error, history.value == nil {
                Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
            } else if (history.value ?? []).isEmpty {
                Text("nothing yet. mail you send this group shows up here, and so does anything you already sent these people.")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: 420, alignment: .leading)
            } else {
                VStack(spacing: 1) {
                    ForEach(history.value ?? []) { entry in
                        GroupHistoryRow(entry: entry) { open(entry) }
                    }
                }
            }
        }
    }

    // MARK: - chrome

    private var footer: some View {
        KeyHintBar(hints: [
            KeyHint(["j", "k"], "select"),
            KeyHint("n", "new"),
            KeyHint(["e", "↵"], "edit"),
            KeyHint("x", "delete"),
        ])
    }

    private var bindings: [KeyBinding] {
        [
            KeyBinding("j", "next") { index = min(groups.count - 1, index + 1) },
            KeyBinding("k", "prev") { index = max(0, index - 1) },
            KeyBinding("n", "new group") { create() },
            KeyBinding("e", "edit group") { if let g = selected { edit(g) } },
            KeyBinding("Enter", "edit group") { if let g = selected { edit(g) } },
            KeyBinding("x", "delete group") {
                if let g = selected { Task { await delete(g) } }
            },
        ]
    }

    // MARK: - actions

    private func create() {
        store.openGroupEditor(GroupEditorRequest(group: nil, onSaved: { Task { await load() } }))
    }

    private func edit(_ group: SendGroup) {
        store.openGroupEditor(
            GroupEditorRequest(group: group, onSaved: { Task { await load() } }))
    }

    /// Delete, undo-first, like every other destructive verb in this app.
    ///
    /// The undo recreates the group from the membership the DETAIL column is
    /// already holding, which is why it refuses when that has not landed: the
    /// list read carries counts only, so an undo built from it would restore an
    /// empty group wearing the old name. Better to decline for the second it
    /// takes to load than to offer an undo that quietly loses everyone.
    ///
    /// The recreated group is a NEW row, so any recorded sends the old one had
    /// no longer point at it. Its derived history — mail matched against the
    /// membership — comes back untouched, which is most of what the page shows.
    private func delete(_ group: SendGroup) async {
        guard let full = detail.value, full.id == group.id, let members = full.members else {
            store.pushToast("still loading this group; try again in a second", .info)
            return
        }
        do {
            try await APIClient.shared.deleteGroup(group.id)
            Analytics.capture("group_deleted", ["mode": group.mode.rawValue])
            groupsState.value?.removeAll { $0.id == group.id }
            let body = GroupBody(
                name: full.name, mode: full.mode, note: full.note, members: members)
            store.pushUndo(
                kind: .groupDelete, messageId: group.id, label: "deleted \(group.name)"
            ) {
                try await APIClient.shared.createGroup(body)
                await GroupsReload.shared.reload()
            }
        } catch {
            store.pushToast(errText(error, "delete failed"), .error)
        }
    }

    /// Open the mail this entry names. A recorded fan-out points at the first
    /// recipient's copy; an entry whose echo never landed points at nothing and
    /// is inert rather than dead-ending in an empty reader.
    private func open(_ entry: GroupHistoryEntry) {
        guard let threadId = entry.thread_id else { return }
        store.openThread(threadId)
    }

    private func load() async {
        await $groupsState.load("groups failed") { try await APIClient.shared.listGroups() }
    }

    /// The right column, both halves, for whichever group is selected.
    ///
    /// `detailFor` is the staleness guard: `.task(id:)` cancels on a change, but
    /// the two awaits below can still land after the user has moved on, and
    /// painting a previous group's members under the current group's name is the
    /// one wrong thing this page could do.
    private func loadDetail() async {
        guard let group = selected else {
            detail = .idle
            history = .idle
            detailFor = nil
            return
        }
        if detailFor != group.id {
            detail = .loading
            history = .loading
            detailFor = group.id
        }
        await $detail.load("members failed") { try await APIClient.shared.group(group.id) }
        guard detailFor == group.id else { return }
        await $history.load("history failed") {
            try await APIClient.shared.groupHistory(group.id).items
        }
    }
}

/// Lets a save fired from the editor — which outlives this view's closures —
/// ask the list to re-pull if it is still on screen. Mirrors `RulesReload`.
@MainActor
final class GroupsReload {
    static let shared = GroupsReload()
    var handler: (@MainActor () async -> Void)?
    private init() {}
    func reload() async { await handler?() }
}

/// Chip colour per mode, kept off the wire type so the model layer stays free of
/// SwiftUI. Bcc is the WARN tone on purpose: it is the mode whose audience is
/// invisible in the mail itself, and the one whose consequences are hardest to
/// see after the fact.
extension GroupMode {
    var tone: Color {
        switch self {
        case .to: Palette.inkFaint
        case .bcc: Palette.warn
        case .individual: Palette.accent
        }
    }
}

// MARK: - rows

private struct GroupRow: View {
    let group: SendGroup
    let selected: Bool
    let onSelect: () -> Void
    let onEdit: () -> Void

    var body: some View {
        ListRow(selected: selected, cornerRadius: 8, hPadding: 10, vPadding: 7, action: onSelect) {
            _, _ in
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Image(systemName: group.mode.symbol)
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(group.mode.tone)
                        .frame(width: 12)
                    Text(group.name)
                        .font(Typo.row)
                        .foregroundStyle(Palette.ink)
                        .lineLimit(1)
                    Spacer(minLength: 6)
                    Text("\(group.member_count)")
                        .font(Typo.num(10))
                        .foregroundStyle(Palette.inkFaintest)
                }
                if let last = group.last_sent_at {
                    Text("last sent \(Fmt.relAge(last))")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                        .lineLimit(1)
                }
            }
        }
        .onTapGesture(count: 2, perform: onEdit)
    }
}

private struct MemberChip: View {
    let member: GroupMember

    var body: some View {
        HStack(spacing: 5) {
            Avatar(sender: member.addr, size: 16)
            // Contact strings are email-derived: rendered as Text only, never as
            // markup.
            Text(member.label)
                .font(Typo.mono(10))
                .foregroundStyle(Palette.inkDim)
                .lineLimit(1)
        }
        .padding(.horizontal, 7)
        .padding(.vertical, 3)
        .background(
            Capsule(style: .continuous).fill(Palette.accentSoft.opacity(0.5))
        )
        .help(member.addr)
    }
}

private struct GroupHistoryRow: View {
    let entry: GroupHistoryEntry
    let onOpen: () -> Void

    /// "12 of 12" reads as noise on every row that reached everyone, which is
    /// most of them. The count earns its place only when it is a shortfall.
    private var reach: String? {
        if entry.failed > 0 { return "\(entry.reached) of \(entry.group_size)" }
        if entry.reachedEveryone { return nil }
        return "\(entry.reached) of \(entry.group_size)"
    }

    var body: some View {
        ListRow(selected: false, cornerRadius: 8, hPadding: 10, vPadding: 7, action: onOpen) {
            _, _ in
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(entry.subject.isEmpty ? "(no subject)" : entry.subject)
                        .font(Typo.row)
                        .foregroundStyle(Palette.ink)
                        .lineLimit(1)
                    if !entry.snippet.isEmpty {
                        Text(entry.snippet)
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaintest)
                            .lineLimit(1)
                    }
                }
                Spacer(minLength: 8)
                // A PARTLY DELIVERED FAN-OUT gets the loudest thing on the row.
                // Eleven investors got the update and one did not, and the only
                // outcome worse than that is not being told.
                if entry.failed > 0 {
                    Chip(text: "\(entry.failed) failed", tone: Palette.danger, filled: true)
                }
                if let reach {
                    Text(reach)
                        .font(Typo.num(10))
                        .foregroundStyle(entry.failed > 0 ? Palette.danger : Palette.inkFaint)
                }
                if entry.opens > 0 {
                    HStack(spacing: 3) {
                        Image(systemName: "eye").font(.system(size: 8))
                        Text("\(entry.opens)").font(Typo.num(10))
                    }
                    .foregroundStyle(Palette.inkFaintest)
                }
                Text(Fmt.relAge(entry.sent_at))
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(width: 56, alignment: .trailing)
            }
        }
    }
}

/// A small uppercase section label, the same one the detail column uses twice.
private struct SectionHead: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(Typo.sectionLabel)
            .foregroundStyle(Palette.inkFaint)
            .textCase(.uppercase)
    }
}
