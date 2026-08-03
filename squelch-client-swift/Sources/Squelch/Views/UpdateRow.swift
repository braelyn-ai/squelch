// One dense update row: importance meter · avatar · sender · one_line ·
// relative time · matched-rule hint · deadline chip. Mouse click selects AND
// opens the thread (gmail semantics); action affordances stay keyboard-first,
// with the [r][e][d] verb hint only on the selected row.

import SwiftUI

struct UpdateRow: View {
    let update: AttentionUpdate
    let selected: Bool
    /// STILL OPEN band: escalating left-rail weight + age note.
    var aging = false
    /// 0..1 escalation weight for the STILL OPEN visual ramp.
    var weight: Double = 0
    let onHover: () -> Void
    let onOpen: () -> Void

    /// The aging BADGE only earns its place past 48h; under that a STILL OPEN
    /// row shows the plain relative time like any other band.
    private var showAgeBadge: Bool {
        aging && Fmt.isAging(update.surfaced_at ?? update.resolved_at)
    }

    var body: some View {
        let chip = Fmt.deadlineChip(update.deadline)

        // onHover BEFORE onOpen: the click moves the cursor to this row first,
        // so the verb keys act on what was clicked, not on where the keyboard
        // last was.
        ListRow(
            selected: selected, cornerRadius: 8, hPadding: 11, vPadding: 6,
            onHoverChange: { if $0 { onHover() } },
            action: {
                onHover()
                onOpen()
            }
        ) { selected, _ in
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
                        Text("[r][e][d]")
                            .font(Typo.mono(9))
                            .foregroundStyle(Palette.inkFaintest)
                    }
                }
                .layoutPriority(3)
            }
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
}

/// The header sun/moon theme toggle. Mirrors the `\` keybinding, both routed
/// through the same Prefs so every mount stays in sync.
struct ThemeToggle: View {
    @Environment(Prefs.self) private var prefs

    private var isDark: Bool {
        switch prefs.theme {
        case .dark: true
        case .light: false
        case .system:
            NSApp?.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        }
    }

    var body: some View {
        ChromeChip(
            icon: isDark ? "sun.max" : "moon", font: .system(size: 12),
            help: "\(isDark ? "light" : "dark") mode (\\)"
        ) { prefs.flipTheme() }
        .accessibilityLabel("switch to \(isDark ? "light" : "dark") mode")
    }
}
