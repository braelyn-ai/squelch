// THE SEARCH TAB. Full-text search over everything the daemon has ingested,
// with the same contract the Mac's side panel has: 220ms debounce, no refetch
// for a term whose results are already on screen, and pages appended as the
// last row comes into view. All of that state lives in `store.search` rather
// than here, which is what lets a tab hop leave the results exactly where they
// were — a phone switches surfaces constantly, and a search that reset every
// time you glanced at something else would be a search nobody finishes.
//
// AND IT IS ALSO THE AGENT'S FIELD. A phone has one text field per screen, so
// the field a person reaches for when they want to know something is this one,
// whether what they type is "delta receipt" or "when does my flight leave".
// COUNTING SPACES IS THE WHOLE CLASSIFIER: four of them (five words) and the
// surface says it is asking the agent, and return sends it. Nothing about that
// is clever, and that is the point — a model deciding which door you meant cost
// a round trip, spent the user's own key on a keystroke, and was wrong in a way
// nobody could predict. This rule is wrong in a way anybody can see and fix by
// deleting a word. Worst case the search is agentic: a five-word lookup lands
// on an agent that can search mail itself, which is a slower right answer rather
// than a wrong one.
//
// UNDER FOUR SPACES IT IS PLAIN SEARCH, with the agent kept one tap away: the
// first row above the hits offers the typed words to the agent. That row is a
// deliberate tap, so unlike everything else here it DOES send.
//
// WHAT THE MAC'S PANEL HAS THAT THIS DOES NOT: the j/k cursor, the armed row,
// the Enter-to-expand fullscreen mode and the match highlighting. The first
// three are keyboard concepts with no thumb equivalent (a tap opens the mail,
// and that is the whole interaction), and `store.search.index` / `.expanded`
// are simply fields this surface never writes.

import SwiftUI

struct MobileSearchView: View {
    @Environment(AppStore.self) private var store

    @State private var loading = false
    /// A page append is in flight. SEPARATE from `loading`: that one blanks the
    /// list behind "searching…", and an append must leave the read hits alone.
    @State private var loadingMore = false

    /// The question this surface has pushed the agent for. Identity-per-ask
    /// rather than the string itself, so asking the same words twice in one
    /// session pushes twice instead of silently doing nothing the second time.
    @State private var ask: AgentAsk?

    /// Debounce for the search, and the Mac's. Long enough that a normal typing
    /// run makes one request, short enough that a pause feels like an answer
    /// rather than a wait.
    private static let debounce = Duration.milliseconds(220)

    /// Spaces that turn typing into a question. Four means five words, which is
    /// past where a keyword lookup normally stops ("stripe payout receipt") and
    /// into where a sentence starts. Counted RAW, so a double space counts
    /// twice: a rule this cheap is worth more than a rule this exact.
    private static let questionSpaces = 4

    /// How many hits get their thread pulled ahead of a tap. The head of the
    /// page only: search results are read and chosen from, not swept, so the
    /// rest can wait for a real tap, and warming all fifty would be a stampede
    /// for one open.
    private static let warmCount = 5

    /// What the field currently holds, trimmed. Read from the store rather than
    /// mirrored into local state, exactly as the Mac's SearchView reads it: a
    /// local copy would have to be synced back on appear, and a re-mounted view
    /// whose empty mirror won the race against that sync would clear a session
    /// that is still on screen.
    private var term: String { store.search.query.trimmed }

    /// Long enough to be a sentence, so this is a question. Trimmed first: the
    /// space a person leaves before their next word must not flip the surface
    /// one keystroke early.
    private var agentMode: Bool {
        term.count(where: { $0 == " " }) >= Self.questionSpaces
    }

    var body: some View {
        @Bindable var store = store

        content
            .background(Palette.canvas)
            .navigationTitle("Search")
            .navigationBarTitleDisplayMode(.inline)
            // Bound STRAIGHT to the store, which is what makes a tab hop free:
            // the field, the hits and the cursor all come back exactly as they
            // were left, because none of them were ever this view's to lose.
            .searchable(text: $store.search.query, prompt: "Search or ask")
            // SwiftUI cancels this on the next edit, which IS the debounce.
            .task(id: store.search.query) { await runSearch() }
            // Return is the send, and only in agent mode: under four spaces it
            // is the search that has already run behind the debounce. The
            // submit path CLEARS the field where the ask row does not: there
            // were never results underneath a return-submitted question, so
            // popping back onto the "press return" placeholder would read as
            // the ask never happening — and return would fire it again.
            .onSubmit(of: .search) {
                guard agentMode else { return }
                askAgent(clearingField: true)
            }
            // The chat is a PUSH, not a tab: back lands on the results that were
            // underneath the question, and the transcript survives either way
            // because the session lives on the store.
            .navigationDestination(item: $ask) { ask in
                MobileAgentView(initialQuestion: ask.text)
            }
    }

    // MARK: - the three shapes

    @ViewBuilder
    private var content: some View {
        if agentMode {
            agentState
        } else if store.search.hits.isEmpty {
            // The ask row rides above the empty states too, and especially
            // above "No matches" — that is the exact moment the words are worth
            // handing to something that can go look.
            VStack(spacing: 0) {
                if !term.isEmpty {
                    askRow.padding(.horizontal, 16)
                    Divider().overlay(Palette.hairline)
                }
                emptyState
            }
        } else {
            hitList
        }
    }

    private var hitList: some View {
        let hits = store.search.hits
        return List {
            // Guarded on the term rather than assumed from the hits: clearing
            // the field empties them one render later, and an "Ask the agent"
            // row quoting nothing at all is a row in that gap.
            if !term.isEmpty {
                askRow.plainRow()
            }
            // A failed refresh does NOT drop the hits already read, so the
            // error has to ride above them rather than replace them — see
            // the empty state, which carries the same line when there is
            // nothing underneath it to keep.
            if let error = store.search.error {
                BandNote(error).plainRow()
            } else if loading {
                // A refetch leaves the read hits on screen — replacing them
                // with a word would blank the list on every keystroke past
                // the debounce — so the new search says so from above them.
                BandNote("searching…").plainRow()
            }
            ForEach(hits) { hit in
                HitRow(hit: hit) { store.openThread(hit.thread_id) }
                    .plainRow()
                    // Reaching the last row IS the request for the next
                    // page. There is no "more" button: a thumb is already
                    // scrolling, and a button would be one more thing to
                    // reach for at the exact moment the intent is obvious.
                    .onAppear {
                        guard hit.id == hits.last?.id else { return }
                        Task { await loadMore() }
                    }
            }
            // Rows just stopping is indistinguishable from the end of the
            // results, so the append announces itself.
            if loadingMore {
                BandNote("loading more…").plainRow()
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .environment(\.defaultMinListRowHeight, 0)
        // A results list is read with the keyboard up; scrolling it is the
        // gesture that says the typing is done.
        .scrollDismissesKeyboard(.immediately)
    }

    /// The agent's door, sitting where the best-matching email would. It shows
    /// the words back so it is obvious WHAT would be asked, and it sends on tap
    /// rather than filling a field: reaching past every hit for this row is
    /// already the deliberate act, and asking twice for one intention is the
    /// thing that made the old handoff feel like the app taking the wheel.
    private var askRow: some View {
        Button { askAgent() } label: {
            HStack(spacing: 11) {
                Image(systemName: "sparkles")
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(Palette.accent)
                    .frame(width: 18)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Ask the agent")
                        .font(.system(size: 15, weight: .medium))
                        .foregroundStyle(Palette.ink)
                    Text("“\(term)”")
                        .font(Typo.rowSub)
                        .foregroundStyle(Palette.inkFaint)
                        .lineLimit(1)
                }
                Spacer(minLength: 6)
                Image(systemName: "chevron.right")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(Palette.inkFaintest)
            }
            .padding(.vertical, 11)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    /// Past four spaces, and the whole surface says so. NOTHING HERE IS
    /// TAPPABLE, deliberately: a full screen that fires a run on contact would
    /// spend the user's own key on the thumb that was reaching for the keyboard
    /// dismiss. Return sends it, and shortening the line takes it back.
    private var agentState: some View {
        VStack(spacing: 10) {
            Image(systemName: "sparkles")
                .font(.system(size: 28, weight: .light))
                .foregroundStyle(Palette.accent)
            Text("Asking the agent.")
                .font(Typo.serif(24, weight: .medium))
                .foregroundStyle(Palette.ink)
            Text(
                "Press return and it takes the question from here. Shorter than five words goes back to searching."
            )
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkFaint)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 300)
        }
        .padding(.horizontal, 24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    /// The three things that can be true with nothing to show. Ordered by which
    /// the user most needs to hear: a failure first, then the honest "nothing
    /// matched", then the idle explainer that is the tab's resting state.
    @ViewBuilder
    private var emptyState: some View {
        VStack(spacing: 10) {
            if let error = store.search.error {
                Text(error)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.warn)
                    .multilineTextAlignment(.center)
            } else if loading {
                Text("searching…")
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaintest)
            } else if store.search.fetchedQuery != nil {
                Text("No matches.")
                    .font(Typo.serif(22, weight: .medium))
                    .foregroundStyle(Palette.ink)
                Text("Nothing in the ingested mail carries those words.")
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
                    .multilineTextAlignment(.center)
            } else {
                Image(systemName: "magnifyingglass")
                    .font(.system(size: 28, weight: .light))
                    .foregroundStyle(Palette.inkFaintest)
                Text("Search your mail.")
                    .font(Typo.serif(24, weight: .medium))
                    .foregroundStyle(Palette.ink)
                // Says the second thing this field does, once, where a first
                // visit will read it. Nobody discovers a field with two jobs by
                // accident, and the rule is small enough to state outright.
                Text(
                    "A few words find a message. Five or more and the agent takes it as a question."
                )
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: 300)
            }
        }
        .padding(.horizontal, 24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: - the agent

    /// Push the chat with these words. From the ask ROW the field keeps them:
    /// back is a real destination there, and finding an emptied search after
    /// asking a question would mean the results you were reading are gone too.
    /// From RETURN in agent mode the field clears instead — there was nothing
    /// underneath but the placeholder, so back should land on the tab at rest
    /// rather than on a screen mid-sentence about a question already asked.
    private func askAgent(clearingField: Bool = false) {
        let question = term
        guard !question.isEmpty else { return }
        ask = AgentAsk(text: question)
        if clearingField { store.search.query = "" }
    }

    // MARK: - fetching

    /// Ported wholesale from the Mac's SearchView, cancellation handling and
    /// all, because the contract is the store's rather than the panel's.
    private func runSearch() async {
        let query = term
        guard !query.isEmpty else {
            store.search.hits = []
            store.search.error = nil
            store.search.fetchedQuery = nil
            store.search.nextCursor = nil
            loading = false
            return
        }
        // A question is not a search term. Returning BEFORE the sleep is what
        // cancels the debounce: typing past the fourth space stops the network
        // entirely rather than filing a lookup nobody will see. The hits stay
        // in the store, so deleting a word back into search mode finds them.
        guard !agentMode else {
            loading = false
            return
        }
        // Already holding this term's results: coming back to the tab must not
        // re-fetch and flash, which is the point of parking the session in the
        // store.
        guard query != store.search.fetchedQuery else { return }
        loading = true
        // Debounce: a fresh keystroke cancels this task before the request.
        try? await Task.sleep(for: Self.debounce)
        // A cancelled exit MUST clear the flag: the replacing task can
        // early-return on `query == fetchedQuery` without ever touching it
        // (backspace inside the debounce window), and a stuck `loading` both
        // pins the "searching…" note and gates loadMore forever.
        guard !Task.isCancelled else {
            loading = false
            return
        }
        do {
            // NO `mode`, exactly as the Mac passes none: the daemon picks
            // hybrid when it has vectors and keyword when it does not, and a
            // client that pinned one would be choosing worse for half the
            // installs.
            let page = try await APIClient.shared.search(query, limit: 50)
            // Re-check after the await, same as the catch does: a superseded
            // task landing late must not stamp `fetchedQuery` with a term that
            // is no longer in the field — the `query != fetchedQuery` guard
            // above would then refuse to fetch the term that IS.
            guard !Task.isCancelled, term == query else {
                loading = false
                return
            }
            store.search.hits = page.items
            store.search.nextCursor = page.next_cursor
            store.search.error = nil
            store.search.fetchedQuery = query
            for hit in page.items.prefix(Self.warmCount) {
                ThreadPrefetch.shared.prefetch(hit.thread_id)
            }
        } catch {
            // Cancellation surfaces here too (URLError.cancelled mid-request):
            // that is a superseded task, not a failure, and writing an error
            // would stamp the NEW search's state with the old one's obituary.
            guard !Task.isCancelled else {
                loading = false
                return
            }
            store.search.error = errText(error, "search failed")
            // Leave `fetchedQuery` nil so returning RETRIES rather than
            // resurrecting a stale error over stale hits, and drop the cursor
            // with it: it belongs to a page set this view is no longer showing.
            store.search.fetchedQuery = nil
            store.search.nextCursor = nil
        }
        loading = false
    }

    /// Append the page after the one on screen. Cursors are only meaningful
    /// beside the term they were issued for, so this refuses to run while the
    /// field is mid-edit (`query != fetchedQuery`) and re-checks after the
    /// await — a query that turned over in flight would otherwise splice two
    /// different searches into one list.
    private func loadMore() async {
        guard !loading, !loadingMore, let cursor = store.search.nextCursor else { return }
        let query = term
        guard !query.isEmpty, query == store.search.fetchedQuery else { return }
        loadingMore = true
        defer { loadingMore = false }
        do {
            let page = try await APIClient.shared.search(query, limit: 50, cursor: cursor)
            guard query == store.search.fetchedQuery, store.search.nextCursor == cursor else {
                return
            }
            // Deduped because the cursor is an OFFSET: mail arriving between two
            // pages shifts the window, and a repeated id is a normal outcome
            // rather than a server bug. Two rows with one id would also break
            // the ForEach.
            var seen = Set(store.search.hits.map(\.id))
            for hit in page.items where seen.insert(hit.id).inserted {
                store.search.hits.append(hit)
            }
            store.search.nextCursor = page.next_cursor
        } catch {
            // Keep the cursor and stay silent: the hits already read are worth
            // more than an error line, and scrolling the last row back into
            // view retries. The field reports failures for the search itself.
        }
    }
}

// MARK: - one ask

/// One push of the agent, with the words that caused it. Identity is the PUSH
/// rather than the text so `navigationDestination(item:)` treats "ask this
/// again" as a new destination — two identical questions are two questions.
private struct AgentAsk: Hashable, Identifiable {
    let id = UUID()
    let text: String
}

// MARK: - one hit

/// SUBJECT FIRST, and that is the phone's own order: the Mac leads with the
/// sender because its rows sit in a 300pt strip beside a reader that will show
/// you the rest, and here the row is the whole decision. Who it is from and how
/// old it is fit on one micro line under it, and the snippet gets two lines
/// because a snippet cut to one is just a longer subject.
private struct HitRow: View {
    let hit: SearchHit
    let onOpen: () -> Void

    var body: some View {
        Button(action: onOpen) {
            VStack(alignment: .leading, spacing: 3) {
                Text(hit.subject)
                    .font(.system(size: 15, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                HStack(spacing: 6) {
                    Text(hit.from_name ?? hit.from_addr)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaint)
                        .lineLimit(1)
                    Text(Fmt.relAge(hit.received_at))
                        .font(Typo.num(10, weight: .medium))
                        .foregroundStyle(Palette.inkFaintest)
                }
                Text(hit.snippet)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaintest)
                    .lineLimit(2)
                    .multilineTextAlignment(.leading)
            }
            .padding(.vertical, 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}
