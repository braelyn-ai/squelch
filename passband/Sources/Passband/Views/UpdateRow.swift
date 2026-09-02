// One dense update row: importance meter · avatar · sender · one_line ·
// relative time · matched-rule hint · deadline chip. Mouse click selects AND
// opens the thread (gmail semantics); action affordances stay keyboard-first,
// with the verb hint (default [r][e][d]) only on the selected row.

import SwiftUI

struct UpdateRow: View {
    let update: AttentionUpdate
    let selected: Bool
    /// STILL OPEN band: escalating left-rail weight + age note.
    var aging = false
    /// 0..1 escalation weight for the STILL OPEN visual ramp.
    var weight: Double = 0
    /// The keyboard hint on the selected row. A PARAMETER because the spam page
    /// has a different answer: `r` still replies there, but the verb somebody
    /// opened that page to use is `i`, and `v`/`f`/`h` are inert on a row
    /// nothing triaged. A hint that names keys the page ignores is worse than
    /// no hint, and this is the only visible affordance the Mac gives the
    /// rescue — the phone has the swipe rail, the Mac has this.
    var verbHint: String = "[r][e][d]"
    let onHover: () -> Void
    let onOpen: () -> Void

    /// The aging BADGE only earns its place past 48h; under that a STILL OPEN
    /// row shows the plain relative time like any other band.
    private var showAgeBadge: Bool {
        aging && Fmt.isAging(update.surfaced_at ?? update.resolved_at)
    }

    /// Phone rows are taller: a tap target, and two lines instead of one, so the
    /// same padding would crowd them.
    #if os(macOS)
        private static let hPadding: CGFloat = 11
        private static let vPadding: CGFloat = 6
        private static let radius: CGFloat = 8
    #else
        private static let hPadding: CGFloat = 12
        private static let vPadding: CGFloat = 9
        private static let radius: CGFloat = 10
    #endif

    var body: some View {
        let chip = Fmt.deadlineChip(update.deadline)

        // onHover BEFORE onOpen: the click moves the cursor to this row first,
        // so the verb keys act on what was clicked, not on where the keyboard
        // last was. A tap runs the same two calls in the same order — the phone
        // has no hover, but it must still open through the store exactly here.
        ListRow(
            selected: selected, cornerRadius: Self.radius, hPadding: Self.hPadding,
            vPadding: Self.vPadding,
            onHoverChange: { if $0 { onHover() } },
            action: {
                onHover()
                onOpen()
            }
        ) { selected, _ in
            #if os(macOS)
                desktopGuts(chip: chip, selected: selected)
            #else
                phoneGuts(chip: chip)
            #endif
        }
        .overlay(alignment: .leading) {
            if showAgeBadge {
                // Heavier rail as the item rots.
                RoundedRectangle(cornerRadius: 1)
                    .fill(Palette.warn)
                    .frame(width: 3 + weight * 3)
                    .padding(.vertical, 4)
            } else if selected {
                RoundedRectangle(cornerRadius: 1)
                    .fill(Palette.accent)
                    .frame(width: 2)
                    .padding(.vertical, 4)
            }
        }
    }

    // MARK: - phone

    /// ONE ROW, TWO LINES. A 393pt screen cannot hold the Mac's six columns —
    /// the one_line, which carries the whole meaning, is the first thing a
    /// single-line layout truncates away — so the sender and the stamp take the
    /// top line and the summary gets the width of the row to itself.
    #if !os(macOS)
        @ViewBuilder
        private func phoneGuts(chip: Fmt.DeadlineChip?) -> some View {
            HStack(alignment: .top, spacing: 10) {
                Avatar(sender: update.senderString, size: 26)

                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 7) {
                        Text(SenderCache.resolved(update.senderString).displayName)
                            .font(.system(size: 14, weight: .medium))
                            .foregroundStyle(Palette.ink)
                            .lineLimit(1)
                        if update.hasAttachments {
                            Image(systemName: "paperclip")
                                .font(.system(size: 10))
                                .foregroundStyle(Palette.inkFaintest)
                                .accessibilityLabel("has attachments")
                        }
                        Spacer(minLength: 4)
                        Text(Fmt.importanceMeter(update.importance))
                            .font(Typo.mono(10))
                            .foregroundStyle(Palette.importanceColor(update.importance))
                            .accessibilityLabel("importance \(update.importance)")
                        Text(Fmt.relAge(update.surfaced_at))
                            .font(Typo.num(11))
                            .foregroundStyle(Palette.inkFaintest)
                    }

                    Text(update.one_line)
                        .font(Typo.rowSub)
                        .foregroundStyle(
                            showAgeBadge
                                ? Palette.warn.opacity(0.45 + weight * 0.55) : Palette.inkDim
                        )
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)

                    // A third line only when there is something to put on it —
                    // most rows are two lines and stay two lines.
                    if chip != nil || update.matched_rule != nil || showAgeBadge
                        || update.hasPendingReminder
                    {
                        HStack(spacing: 7) {
                            if update.hasPendingReminder {
                                Chip(
                                    text: Fmt.remindChip(update.remind_at), tone: Palette.accent,
                                    symbol: "bell.fill")
                            }
                            if let chip {
                                Chip(
                                    text: chip.text,
                                    tone: chip.overdue ? Palette.danger : Palette.warn,
                                    filled: chip.overdue)
                            }
                            if showAgeBadge {
                                Text("← \(Fmt.loudAge(update.surfaced_at ?? update.resolved_at))")
                                    .font(Typo.num(10, weight: .semibold))
                                    .foregroundStyle(Palette.warn)
                            }
                            if update.matched_rule != nil {
                                Text("·rule")
                                    .font(Typo.micro)
                                    .foregroundStyle(Palette.accent.opacity(0.8))
                            }
                        }
                        .padding(.top, 1)
                    }
                }
            }
        }
    #endif

    // MARK: - desktop

    #if os(macOS)
        @ViewBuilder
        private func desktopGuts(chip: Fmt.DeadlineChip?, selected: Bool) -> some View {
            HStack(spacing: 9) {
                Text(Fmt.importanceMeter(update.importance))
                    .font(Typo.mono(10))
                    .foregroundStyle(Palette.importanceColor(update.importance))
                    .accessibilityLabel("importance \(update.importance)")
                    .layoutPriority(2)

                Avatar(sender: update.senderString, size: 20)

                Text(SenderCache.resolved(update.senderString).displayName)
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                    .frame(minWidth: 96, idealWidth: 150, maxWidth: 190, alignment: .leading)
                    .layoutPriority(2)

                if update.hasAttachments {
                    Image(systemName: "paperclip")
                        .font(.system(size: 10))
                        .foregroundStyle(Palette.inkFaintest)
                        .accessibilityLabel("has attachments")
                }

                Text(update.one_line)
                    .font(Typo.rowSub)
                    // Escalation: text leans toward amber as weight climbs.
                    .foregroundStyle(
                        showAgeBadge
                            ? Palette.warn.opacity(0.45 + weight * 0.55) : Palette.inkDim
                    )
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .frame(maxWidth: .infinity, alignment: .leading)

                HStack(spacing: 7) {
                    if update.matched_rule != nil {
                        Text("·rule")
                            .font(Typo.micro)
                            .foregroundStyle(Palette.accent.opacity(0.8))
                            .help("matched a sender rule")
                    }
                    // WHEN IT COMES BACK. Rendered off the row itself rather
                    // than passed in, so a parked row says so on whatever
                    // surface it turns up on. Bands never carry one: a pending
                    // reminder means the thread is done, and they exclude done.
                    if update.hasPendingReminder {
                        Chip(
                            text: Fmt.remindChip(update.remind_at), tone: Palette.accent,
                            symbol: "bell.fill")
                    }
                    if let chip {
                        Chip(
                            text: chip.text, tone: chip.overdue ? Palette.danger : Palette.warn,
                            filled: chip.overdue)
                    }
                    if showAgeBadge {
                        Text("← \(Fmt.loudAge(update.surfaced_at ?? update.resolved_at))")
                            .font(Typo.num(10, weight: .semibold))
                            .foregroundStyle(Palette.warn)
                    } else {
                        Text(Fmt.relAge(update.surfaced_at))
                            .font(Typo.num(10))
                            .foregroundStyle(Palette.inkFaintest)
                            .frame(width: 30, alignment: .trailing)
                    }
                    if selected {
                        Text(verbHint)
                            .font(Typo.mono(9))
                            .foregroundStyle(Palette.inkFaintest)
                    }
                }
                .layoutPriority(3)
            }
        }
    #endif
}

