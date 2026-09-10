// THE SEARCH PANEL'S EMPTY STATE: the last few queries, offered where the hits
// would be, so opening `/` on a fresh field is a list of shortcuts rather than
// a blank rectangle (docs/SEARCH.md §4.2).
//
// THE INTERACTION IS THE `from:` MENU'S (SenderSuggestions): a short list under
// the field, arrows arm a row, Enter takes the armed one into the field. And
// the verb is that menu's verb too — accepting FILLS THE BAR and runs the
// search, it does not open anything. A remembered query is the first half of
// looking something up, the same way a sender is; the reader may well want to
// narrow it before they are done.
//
// THE KEYS ARE NOT ITS OWN, and that is the one place this deliberately parts
// company with the sender menu. That menu mounts LATER than the panel — the
// reader has to type `from:` first — so registering its own arrows puts them
// above the panel's by simple arrival order. This list mounts WITH the panel,
// in the same render, where "who registered last" is SwiftUI's business and not
// ours. So the panel keeps one keymap and one armed index across both lists it
// can draw, and `SearchView.move` is what walks whichever one is on screen.

import SwiftUI

struct RecentSearches: View {
    /// Newest first, as the store keeps them.
    let queries: [String]
    /// The armed row, -1 for none. Owned by the panel (`store.search.index`),
    /// which is the same index that arms hit rows — there is only ever one of
    /// these lists on screen, so there is only ever one selection.
    let armed: Int
    let onRun: (String) -> Void
    let onClear: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            header
            ForEach(Array(queries.enumerated()), id: \.offset) { i, query in
                Button {
                    onRun(query)
                } label: {
                    HStack(spacing: 8) {
                        // The reader's own words, rendered as Text and never as
                        // markup — the same rule the hit rows follow, and this
                        // string has been round-tripped through UserDefaults.
                        Text(query)
                            .font(.system(size: 12))
                            .foregroundStyle(i == armed ? Palette.ink : Palette.inkDim)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Spacer(minLength: 8)
                    }
                    .padding(.horizontal, 9)
                    .padding(.vertical, 5)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .background(
                    RoundedRectangle(cornerRadius: 7, style: .continuous)
                        .fill(i == armed ? Palette.accentSoft : .clear)
                )
            }
            HStack(spacing: 4) {
                Kbd("↑↓")
                Text("pick ·").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                Kbd("enter")
                Text("search").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            }
            .padding(.horizontal, 9)
            .padding(.top, 5)
        }
        .padding(4)
    }

    private var header: some View {
        HStack(spacing: 6) {
            Image(systemName: "clock.arrow.circlepath")
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(Palette.inkFaintest)
            Text("recent")
                .font(Typo.sectionLabel)
                .foregroundStyle(Palette.inkFaint)
                .textCase(.uppercase)
            Spacer(minLength: 8)
            // No confirmation: there is nothing here to lose that a search does
            // not put back, and a modal over ten strings would be heavier than
            // the thing it guards.
            Button(action: onClear) {
                Text("clear")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
            }
            .buttonStyle(.plain)
            .help("Forget these searches")
        }
        .padding(.horizontal, 9)
        .padding(.bottom, 4)
    }
}
