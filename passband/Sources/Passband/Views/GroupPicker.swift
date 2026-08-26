// The composer's group browser: the popover behind the "groups" button beside
// the To line, for addressing an audience you have not remembered the name of.
//
// Autocomplete already handles the case where you HAVE — type "pres" and the
// group is in the menu. This is the other case, and it is why the list shows
// every group with its mode and size rather than being a second search box: you
// are here precisely because you are looking rather than recalling.
//
// Each row states the MODE, because picking is the moment the choice takes
// effect and finding out afterwards is finding out too late. A bcc group and a
// to group produce mail that looks identical in the composer and nothing alike
// in the recipients' inboxes.

import SwiftUI

struct GroupPicker: View {
    let onPick: (SendGroup) -> Void

    @State private var groups: Loadable<[SendGroup]> = .loading

    private var rows: [SendGroup] { groups.value ?? [] }

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("address a group")
                .font(Typo.sectionLabel)
                .foregroundStyle(Palette.inkFaint)
                .textCase(.uppercase)
                .padding(.horizontal, 9)
                .padding(.top, 7)
                .padding(.bottom, 3)

            if groups.isLoading && rows.isEmpty {
                note("loading…")
            } else if let error = groups.error, rows.isEmpty {
                note(error)
            } else if rows.isEmpty {
                note("no groups yet. make one on the Groups page.")
            } else {
                ScrollView {
                    VStack(spacing: 1) {
                        ForEach(rows) { group in
                            row(group)
                        }
                    }
                    .padding(.horizontal, 4)
                    .padding(.bottom, 4)
                }
                .frame(maxHeight: 260)
            }
        }
        .frame(width: 320)
        .task { await $groups.load("groups failed") { try await APIClient.shared.listGroups() } }
    }

    private func note(_ text: String) -> some View {
        Text(text)
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaintest)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 9)
            .padding(.bottom, 9)
    }

    private func row(_ group: SendGroup) -> some View {
        Button { onPick(group) } label: {
            HStack(spacing: 8) {
                Image(systemName: group.mode.symbol)
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(group.mode.tone)
                    .frame(width: 14)
                VStack(alignment: .leading, spacing: 1) {
                    Text(group.name)
                        .font(Typo.row)
                        .foregroundStyle(Palette.ink)
                        .lineLimit(1)
                    Text(group.mode.blurb)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                        .lineLimit(1)
                }
                Spacer(minLength: 8)
                Text("\(group.member_count)")
                    .font(Typo.num(11))
                    .foregroundStyle(Palette.inkFaintest)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(
            RoundedRectangle(cornerRadius: 7, style: .continuous).fill(Palette.accentSoft.opacity(0))
        )
        .hoverHighlight()
    }
}

extension View {
    /// A hover wash for a plain button in a menu-shaped list. The popover's rows
    /// have no selection of their own — the pointer IS the selection — so this is
    /// the only feedback they get.
    fileprivate func hoverHighlight() -> some View {
        modifier(HoverHighlight())
    }
}

private struct HoverHighlight: ViewModifier {
    @State private var hovering = false

    func body(content: Content) -> some View {
        content
            .background(
                RoundedRectangle(cornerRadius: 7, style: .continuous)
                    .fill(hovering ? Palette.accentSoft : .clear)
            )
            .onHover { hovering = $0 }
    }
}
