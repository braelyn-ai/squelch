// THE DEEPER SEARCH, WHERE THE READER CAN SEE IT (docs/SEARCH.md §6.4). A band
// above the hits in the 460pt strip, the right-hand column beside them when the
// panel is expanded: same content either way, because it is the same
// conversation and only the room it has changes.
//
// What it draws is deliberately thin. One status line while the lane works (the
// tool chips reduced to their summaries: searching "abstract wifi", reading
// "Abstract is today."), then the email cards, then AT MOST one line of prose.
// That is the lane's whole answer shape, and drawing anything more would invite
// a model to write more.
//
// THE ONE LINE IS PLAIN TEXT, never markdown, and that is a security decision
// rather than a typographic one: the model's input is mail somebody else wrote,
// so a link in its prose is a link a stranger asked for. The ⌘K tray renders
// markdown and routes every click through `Opener`; this surface simply has no
// links at all.

import SwiftUI

struct DeeperSearchBand: View {
    /// Whether the panel is expanded. Only the frame differs; the content is
    /// identical, which is the point of one view rather than two.
    let expanded: Bool

    @Environment(AppStore.self) private var store
    @Environment(Prefs.self) private var prefs
    /// Collapsed by the reader, for this panel session. Local state on purpose:
    /// a band folded away while one search was running should not still be
    /// folded away over tomorrow's.
    @State private var collapsed = false

    private var session: AssistantSession { store.searchLane }
    private var search: SearchSession { store.search }

    /// The trigger a not-yet-started band would run under, for the on-request
    /// button. nil once the lane is going: then the trigger that matters is the
    /// one it actually started on.
    private var offeredTrigger: SearchIntent.Trigger? {
        search.laneStarted ? nil : search.lastVerdict?.trigger
    }

    /// One line of small type naming the signal that started this, so a reader
    /// who did not want the lane can see why it ran and shorten the query.
    private var reason: String? {
        if let trigger = search.laneTrigger {
            return SearchIntent.reason(trigger, query: search.laneQuery)
        }
        if let trigger = offeredTrigger {
            return SearchIntent.reason(trigger, query: search.fetchedQuery ?? search.query)
        }
        return nil
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            header
            if !collapsed { content }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(Palette.accentSoft.opacity(0.35))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(Palette.hairline, lineWidth: 0.75)
        )
        .padding(.horizontal, expanded ? 0 : 14)
        .padding(.bottom, expanded ? 0 : 8)
        // STRUCTURE ONLY, for AskBar's reason: rows arriving is a change worth
        // animating, text growing inside one at token rate is a smear.
        .animation(.smooth(duration: 0.25), value: session.transcript.count)
    }

    // MARK: - chrome

    private var header: some View {
        HStack(spacing: 7) {
            Image(systemName: "sparkles")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Palette.accent)
            Text("deeper search")
                .font(Typo.sectionLabel)
                .foregroundStyle(Palette.inkFaint)
                .textCase(.uppercase)
            if let reason {
                Text(reason)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer(minLength: 4)
            if session.isPaused {
                Image(systemName: "pause.circle")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(Palette.inkFaint)
                    .help("Held where it was. It picks up from there, nothing is lost.")
            }
            if search.laneStarted {
                Button("new") { store.resetSearchLane() }
                    .buttonStyle(.textAction)
                    .font(Typo.micro)
                    .help("Forget this conversation and start the deeper search over")
            }
            Button {
                collapsed.toggle()
            } label: {
                Image(systemName: collapsed ? "chevron.down" : "chevron.up")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(Palette.inkFaintest)
            }
            .buttonStyle(.plain)
            .help(collapsed ? "Show what the deeper search found" : "Fold this away")
        }
    }

    // MARK: - the lane's own rows

    @ViewBuilder
    private var content: some View {
        if !search.laneStarted {
            offer
        } else {
            // The status line, while there is work to say something about.
            if session.running, let tool = latestTool {
                ToolChipRow(tool: tool)
            } else if session.running {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.mini)
                    Text("reading your mail…")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaint)
                }
            }
            ForEach(cardRows) { row in
                EmailCardList(cards: row.emails) { threadId in
                    // The panel stays exactly as it is. The reader is still
                    // searching, and the results beside the thread are the
                    // whole reason the strip exists.
                    store.openThread(threadId)
                }
            }
            if let line = answerLine, !line.isEmpty {
                Text(line)
                    .font(.system(size: 13))
                    .foregroundStyle(Palette.ink)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if let failure = errorLine {
                BandNote(failure)
            }
        }
    }

    /// The on-request posture: the band says what it could do and waits to be
    /// asked. Nothing has been spent at this point, and nothing will be until
    /// this is pressed.
    @ViewBuilder
    private var offer: some View {
        HStack(spacing: 8) {
            Text("Read the mail itself and answer this?")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaint)
            Spacer(minLength: 4)
            if let trigger = offeredTrigger {
                Button("ask the agent") { store.startDeeperSearch(trigger: trigger) }
                    .buttonStyle(.glass)
                    .font(Typo.chip)
                    .help("Runs the assistant over these results. It costs a model call.")
            }
        }
    }

    /// The newest tool chip: what the lane is doing right now, reduced to its
    /// own one-line summary.
    private var latestTool: ToolActivity? {
        session.transcript.last { $0.kind == .tool }?.tool
    }

    /// Every batch of cards the lane has shown, oldest first. Identifiable
    /// rows, because a refined search shows a second set beneath the first.
    private var cardRows: [ChatItem] {
        session.transcript.filter { $0.kind == .emails && !$0.emails.isEmpty }
    }

    /// The one line of prose, which is the last thing the lane said. Earlier
    /// assistant text belongs to an earlier narrowing of the same search and is
    /// answered by the line that replaced it.
    private var answerLine: String? {
        session.transcript.last { $0.kind == .assistant }?.text
    }

    /// A failed run, in the panel's own note style.
    private var errorLine: String? {
        session.transcript.last { $0.kind == .error }?.text
    }
}
