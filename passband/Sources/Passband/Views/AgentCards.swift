// THE TWO ROWS EVERY AGENT SURFACE DRAWS: what it is doing (a tool chip) and
// what it found (email cards). Both started life inside AskBar and moved here
// when the search panel grew a lane of its own (docs/SEARCH.md §6.4), because
// the alternative was a second copy that would drift — and the thing that would
// drift is a card the reader clicks to open somebody's mail.
//
// What a card says is the DAEMON'S account of the thread, re-read by
// `show_emails` before the card is built, never the model's description of it.
// That rule lives in AgentTools; this file only draws the result.

import SwiftUI

/// The `show_emails` cards: the agent's answer AS emails, rendered like list
/// rows because they are the result rather than a footnote.
///
/// `onOpen` is the caller's, because opening one means different things in
/// different places: the ⌘K tray opens the thread and closes itself, the search
/// panel opens the thread and stays exactly where it is (the reader is still
/// searching, and the results beside the reader are the point of the strip).
struct EmailCardList: View {
    let cards: [EmailCard]
    let onOpen: (String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(cards) { card in
                Button {
                    onOpen(card.threadId)
                } label: {
                    VStack(alignment: .leading, spacing: 3) {
                        HStack(spacing: 7) {
                            Avatar(sender: card.sender, size: 18)
                            Text(card.sender)
                                .font(.system(size: 12, weight: .semibold))
                                .foregroundStyle(Palette.ink)
                                .lineLimit(1)
                            Spacer(minLength: 6)
                            Text(Fmt.dateTime(card.date))
                                .font(Typo.num(10))
                                .foregroundStyle(Palette.inkFaintest)
                        }
                        Text(card.subject)
                            .font(Typo.rowSub)
                            .foregroundStyle(Palette.inkDim)
                            .lineLimit(1)
                        if !card.snippet.isEmpty {
                            Text(card.snippet)
                                .font(Typo.micro)
                                .foregroundStyle(Palette.inkFaintest)
                                .lineLimit(2)
                                .multilineTextAlignment(.leading)
                        }
                    }
                    .padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .background(
                    RoundedRectangle(cornerRadius: 10, style: .continuous)
                        .fill(Palette.hairline.opacity(0.35))
                )
                .help("Open this thread")
            }
        }
    }
}

/// One tool-activity chip: what the agent is doing, in the smallest type the
/// app has, with its outcome on the trailing edge.
struct ToolChipRow: View {
    let tool: ToolActivity

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: AgentTools.Tool(rawValue: tool.name)?.symbol ?? "wrench")
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(Palette.inkFaint)
                .frame(width: 12)
            Text(tool.summary)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaint)
                .lineLimit(1)
                .truncationMode(.tail)
            Spacer(minLength: 4)
            switch tool.state {
            case .running:
                ProgressView().controlSize(.mini)
            case .ok:
                Image(systemName: "checkmark")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(Palette.positive.opacity(0.8))
            case .failed:
                Image(systemName: "xmark")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(Palette.danger)
            }
        }
    }
}
