// THE SEARCH TAB. Full-text search over everything the daemon has ingested,
// with the same contract the Mac's side panel has: 220ms debounce, no refetch
// for a term whose results are already on screen, and pages appended as the
// last row comes into view. All of that state lives in `store.search` rather
// than here, which is what lets a tab hop leave the results exactly where they
// were — a phone switches surfaces constantly, and a search that reset every
// time you glanced at the agent would be a search nobody finishes.
//
// AND IT IS ALSO A DOOR TO THE AGENT. A phone has one text field per screen, so
// the field a person reaches for when they want to know something is this one,
// whether what they type is "delta receipt" or "when does my flight leave".
// QueryClassifier decides which they meant, and a question quietly carries them
// to the Agent tab with their words already in its composer. Nothing is sent on
// their behalf, nothing is cleared here: coming back to this tab finds the term
// still in the field and whatever it matched still under it. The intended feel
// is that the search tab noticed what they were asking and opened the right
// door, not that the app took the wheel.
//
// WHAT THE MAC'S PANEL HAS THAT THIS DOES NOT: the j/k cursor, the armed row,
// the Enter-to-expand fullscreen mode and the match highlighting. The first
// three are keyboard concepts with no thumb equivalent (a tap opens the mail,
// and that is the whole interaction), and `store.search.index` / `.expanded`
// are simply fields this surface never writes.

import SwiftUI

struct MobileSearchView: View {
    @Environment(AppStore.self) private var store

    /// The shell's tab switch, with the words to carry over. Passed in rather
    /// than reached for: this view has no business knowing there is a tab bar,
    /// only that there is somewhere else a question can go.
    let switchToAgent: (String) -> Void

    @State private var loading = false
    /// A page append is in flight. SEPARATE from `loading`: that one blanks the
    /// list behind "searching…", and an append must leave the read hits alone.
    @State private var loadingMore = false

    /// The session's verdicts. Held here so the memo (and the single in-flight
    /// call) live as long as the tab does rather than as long as one keystroke.
    @State private var classifier = QueryClassifier()

    /// The last term handed to the agent, so one string bounces the user out of
    /// this tab exactly once. Without it, editing a question and undoing the
    /// edit would fire the handoff a second time while they were still typing —
    /// being carried somewhere twice for one sentence reads as a bug even when
    /// the verdict is right.
    @State private var handedOff: String?

    /// Debounce shared by both reactions below, and by the Mac. Long enough that
    /// a normal typing run makes one request, short enough that a pause feels
    /// like an answer rather than a wait.
    private static let debounce = Duration.milliseconds(220)

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

    var body: some View {
        @Bindable var store = store

        results
            .background(Palette.canvas)
            .navigationTitle("Search")
            .navigationBarTitleDisplayMode(.inline)
            // Bound STRAIGHT to the store, which is what makes a tab hop free:
            // the field, the hits and the cursor all come back exactly as they
            // were left, because none of them were ever this view's to lose.
            .searchable(text: $store.search.query, prompt: "Search mail")
            // Two independent debounced reactions to the same keystroke, and
            // deliberately not one: the classification can take a network round
            // trip, and folding it into the search task would hold the results
            // hostage behind a call that is only deciding which tab to be on.
            // SwiftUI cancels each on the next edit, which IS the debounce.
            .task(id: store.search.query) { await runSearch() }
            .task(id: store.search.query) { await runHandoff() }
    }

    // MARK: - the list

    @ViewBuilder
    private var results: some View {
        let hits = store.search.hits

        if hits.isEmpty {
            emptyState
        } else {
            List {
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
                // visit will read it. Nobody discovers a handoff by accident —
                // and because telling the two apart can spend the user's own
                // key, the sentence owns up to that here rather than nowhere.
                Text(
                    "Type words to find a message, or ask a question and the agent takes it. Telling the two apart can use your Anthropic key."
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

    // MARK: - the handoff

    /// Decide, after the same pause the search takes, whether these words were
    /// meant for the agent — and if they were, carry them over. Runs beside the
    /// search rather than instead of it, so a misread question still leaves
    /// results waiting here when they come back.
    private func runHandoff() async {
        let query = term
        guard !query.isEmpty, query != handedOff else { return }
        try? await Task.sleep(for: Self.debounce)
        guard !Task.isCancelled else { return }
        guard await classifier.verdict(for: query) == .question else { return }
        // The field kept typing while the verdict was in flight: those are
        // different words now, and hauling the user off for a sentence they
        // have already edited is the one way this feature becomes annoying.
        guard !Task.isCancelled, term == query else { return }
        handedOff = query
        switchToAgent(query)
    }
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
