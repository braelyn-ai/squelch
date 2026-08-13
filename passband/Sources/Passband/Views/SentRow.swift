// One row of the sent page: who it went to · subject + snippet · relative time,
// plus a quiet mark when a read receipt this send armed has been fetched.
//
// The same envelope and the same metrics as UpdateRow, minus everything that
// would be a lie here. Outbound mail has no importance, no tier, no matched
// rule and no triage verbs, so there is no meter, no rule hint and no [r][e][d]
// hint on the selected row — a key hint for a key that does nothing is worse
// than no hint at all.

import SwiftUI

struct SentRow: View {
    let item: SentItem
    let selected: Bool
    let onHover: () -> Void
    let onOpen: () -> Void

    /// The recipient list, parsed once per render pass and read twice below.
    private var to: [String] { SenderID.recipients(item.to) }

    /// Recipients as people rather than as a header: "Sarah Chen, billing@acme".
    /// Truncated in the MIDDLE, because the ends of a long list are the halves
    /// that identify it — who you wrote to first, and who is on the tail.
    private var names: String {
        to.map { SenderCache.resolved($0).displayName }.joined(separator: ", ")
    }

    var body: some View {
        // onHover BEFORE onOpen, exactly as UpdateRow does it: the click moves
        // the cursor to this row first, so the keyboard picks up where the
        // pointer left it.
        ListRow(
            selected: selected, cornerRadius: 8, hPadding: 11, vPadding: 6,
            onHoverChange: { if $0 { onHover() } },
            action: {
                onHover()
                onOpen()
            }
        ) { _, _ in
            // Neither flag is read: the guts of a sent row are the same
            // selected or not, because there are no verb hints to reveal.
            HStack(spacing: 9) {
                // Stands in for UpdateRow's importance meter, in a slot the same
                // width (five mono glyphs at 10pt) so flipping pages under the
                // segmented control does not shunt the avatar and name column
                // sideways. It earns the space: sent rows name a RECIPIENT, and
                // an unlabelled name here would read as the sender every other
                // list puts in this position.
                Text("to")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .textCase(.uppercase)
                    .frame(width: 30, alignment: .trailing)
                    .layoutPriority(2)

                // The slot is held even when the header carried nothing usable,
                // so a recipient-less row does not shunt the whole list left.
                Group {
                    if let first = to.first {
                        Avatar(sender: first, size: 20)
                    } else {
                        Circle().fill(Palette.hairline)
                    }
                }
                .frame(width: 20, height: 20)

                Text(names.isEmpty ? "no recipient" : names)
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(names.isEmpty ? Palette.inkFaintest : Palette.ink)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .frame(minWidth: 96, idealWidth: 150, maxWidth: 190, alignment: .leading)
                    .layoutPriority(2)

                // Subject first and at priority, snippet in whatever is left:
                // the subject is what a sent row is looked up by, and the
                // snippet is the reminder of which draft it was.
                HStack(spacing: 6) {
                    Text(item.subject.isEmpty ? "(no subject)" : item.subject)
                        .font(Typo.rowSub)
                        .foregroundStyle(Palette.inkDim)
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .layoutPriority(1)
                    if !item.snippet.isEmpty {
                        Text(item.snippet)
                            .font(Typo.rowSub)
                            .foregroundStyle(Palette.inkFaintest)
                            .lineLimit(1)
                            .truncationMode(.tail)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)

                HStack(spacing: 7) {
                    // The opt-in read receipt, kept deliberately quiet: an open
                    // is a pixel fetch and sometimes a proxy warming its cache,
                    // so the row states the count and nothing louder. No opens
                    // means no mark — "not opened" and "never tracked" are the
                    // same silence (see ReadReceipt.swift).
                    if item.opens > 0 {
                        HStack(spacing: 3) {
                            Image(systemName: "eye")
                                .font(.system(size: 9, weight: .semibold))
                            Text("\(item.opens)")
                                .font(Typo.num(10))
                        }
                        .foregroundStyle(Palette.inkFaint)
                        .help(item.opens == 1 ? "opened once" : "opened \(item.opens) times")
                        .accessibilityLabel(
                            item.opens == 1 ? "opened once" : "opened \(item.opens) times")
                    }
                    Text(Fmt.relAge(item.sent_at))
                        .font(Typo.num(10))
                        .foregroundStyle(Palette.inkFaintest)
                        .frame(width: 30, alignment: .trailing)
                }
                .layoutPriority(3)
            }
        }
        .overlay(alignment: .leading) {
            if selected {
                RoundedRectangle(cornerRadius: 1)
                    .fill(Palette.accent)
                    .frame(width: 2)
                    .padding(.vertical, 4)
            }
        }
    }
}
