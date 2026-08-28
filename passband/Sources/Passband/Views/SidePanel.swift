// Browse-all + search as a right-hand glass panel. It owns the modal KeyContext
// and Esc-to-close; inner views register their list keys into that same context
// and must not push a second one. Mounted only while a side view is open —
// pushing the modal context unconditionally would gate out the whole "list"
// keymap forever. The thread viewer layers above it, inset by sidePanelWidth.

import SwiftUI

struct SidePanel: View {
    @Environment(AppStore.self) private var store

    /// Fullscreen search takeover (Enter in the bar). Only search expands;
    /// browse is always the strip.
    private var expanded: Bool {
        store.sideView == .search && store.search.expanded
    }

    var body: some View {
        HStack(spacing: 0) {
            Spacer(minLength: 0)
            VStack(alignment: .leading, spacing: 0) {
                HStack {
                    Text(store.sideView.title)
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundStyle(Palette.ink)
                    Spacer()
                    HStack(spacing: 4) {
                        Kbd("Esc")
                        Text("close").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    }
                }
                // As a strip this header sits on the window's right, nowhere
                // near the traffic lights. EXPANDED it spans the whole window
                // and covers the rail, so its leading edge lands in the strip
                // the buttons own and the title draws underneath them.
                .padding(.leading, expanded ? TopBar.dotsClearance : 16)
                .padding(.trailing, 16)
                .padding(.vertical, 13)
                .overlay(alignment: .bottom) { Hairline() }

                Group {
                    switch store.sideView {
                    case .search: SearchView()
                    case .browse: BrowseView()
                    case .none: EmptyView()
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            }
            .frame(width: expanded ? nil : sidePanelWidth)
            .frame(maxWidth: expanded ? .infinity : nil, maxHeight: .infinity)
            .passbandGlass(.pane, cornerRadius: 0, tint: Palette.glassTintStrong)
            .shadow(color: .black.opacity(0.24), radius: 40, x: -14)
        }
        .animation(.smooth(duration: 0.22), value: expanded)
        .keyContext(.modal)
        // Esc unwinds one layer at a time: fullscreen search collapses back to
        // the strip first, a second Esc closes the panel.
        .keyBindings(.modal, [
            KeyBinding("Escape", "back") {
                if expanded {
                    store.search.expanded = false
                } else {
                    store.closeSide()
                }
            }
        ])
    }
}

// MARK: - search

/// Search: debounced GET /client/search, j/k selection, click or Enter opens a
/// hit in the reader beside the results. Enter with NO row armed (index -1)
/// instead expands the panel fullscreen with larger previews — ArrowDown arms
/// a row, so bar-Enter and row-Enter are different verbs. Pages in as you reach
/// the bottom row — there is no "more" button, the strip is too narrow to spend
/// one. Every durable piece of state lives in `store.search`; only the two
/// in-flight flags, which die with the panel, are local.
struct SearchView: View {
    @Environment(AppStore.self) private var store
    @Environment(Prefs.self) private var prefs
    @State private var loading = false
    /// A page append is in flight. SEPARATE from `loading`: that one blanks the
    /// list behind "searching…", and an append must leave the read hits alone.
    @State private var loadingMore = false
    @FocusState private var focused: Bool

    /// The terms the on-screen hits were actually fetched for — the live query
    /// can be mid-edit, and highlighting it would mark text the server never
    /// matched.
    private var terms: [String] {
        (store.search.fetchedQuery ?? "").split(separator: " ").map(String.init)
    }

    var body: some View {
        @Bindable var store = store
        let expanded = store.search.expanded

        VStack(alignment: .leading, spacing: 0) {
            Field(label: "") {
                TextField("search mail…", text: $store.search.query)
                    .textFieldStyle(.plain)
                    .focused($focused)
            }
            .padding(.horizontal, 16)
            .padding(.top, 12)
            .padding(.bottom, 8)

            // THE ORDER, beside the thing that produces it. A sort control is
            // about the answer, so it belongs next to the question and not
            // three screens away — the same preference is in Settings, and the
            // two are one value, so flipping it here is what Settings will say
            // next time it is opened.
            //
            // Shown even with an empty field: a control that only appears once
            // you have results is a control you do not know you have.
            HStack {
                SearchSortPicker()
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 10)

            if loading { BandNote("searching…") }
            if let error = store.search.error { BandNote(error) }
            if !loading && store.search.error == nil && !store.search.query.trimmed.isEmpty
                && store.search.hits.isEmpty
            {
                BandNote("no matches.")
            }

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: expanded ? 10 : 6) {
                        ForEach(Array(store.search.hits.enumerated()), id: \.element.id) { i, hit in
                            // One click opens: the reader sits beside this list,
                            // so opening a hit costs the results nothing.
                            HitRow(
                                hit: hit, terms: terms, selected: i == store.search.index,
                                expanded: expanded
                            ) {
                                store.search.index = i
                                open()
                            }
                            .id(hit.id)
                            // Reaching the last row IS the request for the next
                            // page. On the row rather than a footer sentinel so
                            // it fires in both the strip and fullscreen, where
                            // the column widths (and so the row counts) differ.
                            .onAppear {
                                guard hit.id == store.search.hits.last?.id else { return }
                                Task { await loadMore() }
                            }
                        }
                        // Rows just stopping is indistinguishable from the end
                        // of the results, so the append announces itself.
                        if loadingMore { BandNote("loading more…") }
                    }
                    // Fullscreen keeps a reading-width column: match text in
                    // window-wide rows is a treadmill for the eyes.
                    .frame(maxWidth: expanded ? 780 : .infinity)
                    .frame(maxWidth: .infinity)
                    .padding(.horizontal, expanded ? 24 : 14)
                    .padding(.bottom, 14)
                }
                .onChange(of: store.search.index) { _, i in
                    guard let hit = store.search.hits[safe: i] else { return }
                    withAnimation(Motion.scrollFollow) {
                        proxy.scrollTo(hit.id, anchor: .center)
                    }
                }
            }
        }
        .keyBindings(.modal, bindings)
        .onAppear { focused = true }
        // The reader steals focus while it is up. When it closes and this
        // panel is the surface again, typing must just work — without this the
        // arrows still move the selection but the keyboard is otherwise dead
        // until a mouse click, which reads as the panel being broken.
        .onChange(of: store.threadId) { _, threadId in
            if threadId == nil { focused = true }
        }
        // The remembered query lands selected, so `/` serves both callers: arrow
        // down into the old results, or type to replace it.
        .onChange(of: focused) { _, on in
            guard on, !store.search.query.isEmpty else { return }
            Task { @MainActor in
                // Select-all through the responder chain has no UIKit twin worth
                // shimming; the iOS field selects its text a different way.
                #if os(macOS)
                    NSApp.sendAction(#selector(NSText.selectAll(_:)), to: nil, from: nil)
                #endif
            }
        }
        // KEYED ON THE SORT TOO, or flipping the order leaves the old ranking on
        // screen until the reader edits their query. An array because tuples do
        // not conform to Equatable and `task(id:)` needs one value.
        .task(id: [store.search.query, prefs.searchSort.rawValue]) { await runSearch() }
    }

    private var bindings: [KeyBinding] {
        [
            KeyBinding("ArrowDown", "next hit", allowInInput: true) { move(1) },
            KeyBinding("ArrowUp", "prev hit", allowInInput: true) { move(-1) },
            // Enter is two verbs: a row armed opens it, the bare bar expands
            // the panel into fullscreen previews. Expanding is UNCONDITIONAL —
            // gating it on results landing would make Enter-right-after-typing
            // (inside the debounce window) silently do nothing.
            KeyBinding(
                "Enter", store.search.index >= 0 ? "open thread" : "expand previews",
                allowInInput: true
            ) {
                if store.search.index >= 0 {
                    open()
                } else {
                    store.search.expanded = true
                }
            },
            // j/k also work when focus is not in the input.
            KeyBinding("j", "next hit") { move(1) },
            KeyBinding("k", "prev hit") { move(-1) },
        ]
    }

    /// Floor -1, not 0: ArrowUp from the top row disarms back to the bar.
    private func move(_ delta: Int) {
        store.search.index = max(
            -1, min(store.search.hits.count - 1, store.search.index + delta))
    }

    private func open() {
        guard let hit = store.search.hits[safe: store.search.index] else { return }
        // openThread itself collapses `expanded` — every path into the reader
        // must, so the collapse lives there rather than here.
        store.openThread(hit.thread_id)
    }

    private func runSearch() async {
        let term = store.search.query.trimmed
        // Read at fetch time, not captured on mount: the panel is often built
        // before a trip to Settings and rebuilt after one.
        let sort = prefs.searchSort
        guard !term.isEmpty else {
            store.search.hits = []
            store.search.error = nil
            store.search.fetchedQuery = nil
            store.search.fetchedSort = nil
            store.search.nextCursor = nil
            loading = false
            return
        }
        // Already holding this term's results UNDER THIS ORDER: reopening must
        // not re-fetch and flash, which is the point of hoisting the session
        // into the store. The sort is half of that test — same words ranked by
        // different rules is a different answer.
        guard term != store.search.fetchedQuery || sort != store.search.fetchedSort else { return }
        loading = true
        // Debounce: a fresh keystroke cancels this task before the request.
        try? await Task.sleep(for: .milliseconds(220))
        // A cancelled exit MUST clear the flag: the replacing task can
        // early-return on `term == fetchedQuery` without ever touching it
        // (backspace inside the debounce window), and a stuck `loading` both
        // pins the "searching…" note and gates loadMore forever.
        guard !Task.isCancelled else {
            loading = false
            return
        }
        do {
            let page = try await APIClient.shared.search(term, limit: 50, sort: sort)
            store.search.hits = page.items
            store.search.nextCursor = page.next_cursor
            // Fresh results land un-armed: Enter straight from the bar means
            // "show me more", not "open whatever floated to the top".
            store.search.index = -1
            store.search.error = nil
            store.search.fetchedQuery = term
            store.search.fetchedSort = sort
            // Warm the head of the page only. Search rows are read and chosen
            // from, not swept, so the rest can wait for a real click — and the
            // whole 50 would be a stampede for one open.
            for hit in page.items.prefix(5) {
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
            // Leave `fetchedQuery` nil so reopening RETRIES rather than
            // resurrecting a stale error over stale hits.
            store.search.fetchedQuery = nil
            store.search.fetchedSort = nil
            // And drop the cursor with it: it belongs to a page set this view
            // is no longer showing.
            store.search.nextCursor = nil
        }
        loading = false
    }

    /// Append the page after the one on screen. Cursors are only meaningful
    /// beside the term they were issued for, so this refuses to run while the
    /// bar is mid-edit (`term != fetchedQuery`) and re-checks after the await —
    /// a query that turned over in flight would otherwise splice two different
    /// searches into one list.
    private func loadMore() async {
        guard !loading, !loadingMore, let cursor = store.search.nextCursor else { return }
        let term = store.search.query.trimmed
        guard !term.isEmpty, term == store.search.fetchedQuery else { return }
        // The cursor is an OFFSET INTO ONE RANKING, so the page after it has to
        // be asked for under the sort the hits on screen were ranked by — not
        // under whatever the preference says now. A sort changed mid-scroll
        // re-ranks from the top through `runSearch`, which is the only honest
        // way to serve it.
        let sort = store.search.fetchedSort
        loadingMore = true
        defer { loadingMore = false }
        do {
            let page = try await APIClient.shared.search(
                term, limit: 50, cursor: cursor, sort: sort)
            guard term == store.search.fetchedQuery, store.search.nextCursor == cursor else {
                return
            }
            // Deduped because the cursor is an OFFSET: mail arriving between
            // two pages shifts the window, and a repeated id is a normal
            // outcome rather than a server bug. Two rows with one id would also
            // break the ForEach.
            var seen = Set(store.search.hits.map(\.id))
            for hit in page.items where seen.insert(hit.id).inserted {
                store.search.hits.append(hit)
            }
            store.search.nextCursor = page.next_cursor
        } catch {
            // Keep the cursor and stay silent: the hits already read are worth
            // more than an error line, and scrolling the last row back into
            // view retries. The bar reports failures for the search itself.
        }
    }
}

private struct HitRow: View {
    let hit: SearchHit
    let terms: [String]
    let selected: Bool
    let expanded: Bool
    let onOpen: () -> Void

    var body: some View {
        Button(action: onOpen) {
            VStack(alignment: .leading, spacing: expanded ? 5 : 3) {
                HStack {
                    Text(highlight(hit.from_name ?? hit.from_addr))
                        .font(.system(size: expanded ? 13 : 11, weight: .semibold))
                        .foregroundStyle(Palette.ink)
                        .lineLimit(1)
                    Spacer(minLength: 6)
                    Text(Fmt.dateTime(hit.received_at))
                        .font(Typo.num(expanded ? 11 : 10))
                        .foregroundStyle(Palette.inkFaintest)
                }
                Text(highlight(hit.subject))
                    .font(expanded ? .system(size: 13) : Typo.rowSub)
                    .foregroundStyle(Palette.inkDim)
                    .lineLimit(expanded ? 2 : 1)
                    .multilineTextAlignment(.leading)
                Text(highlight(hit.snippet))
                    .font(expanded ? .system(size: 12) : Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .lineLimit(expanded ? 6 : 2)
                    .multilineTextAlignment(.leading)
            }
            .padding(expanded ? 16 : 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(selected ? Palette.accentSoft : Palette.hairline.opacity(0.35))
        )
    }

    /// Paint every case-insensitive occurrence of each query term. Best effort
    /// by design: the stored snippet is the message HEAD, so a body-deep match
    /// can produce a legitimately unpainted row.
    private func highlight(_ text: String) -> AttributedString {
        var attr = AttributedString(text)
        for term in terms {
            var from = attr.startIndex
            while from < attr.endIndex,
                let range = attr[from...].range(of: term, options: .caseInsensitive)
            {
                attr[range].backgroundColor = Palette.accentSoft
                attr[range].foregroundColor = Palette.accentInk
                from = range.upperBound
            }
        }
        return attr
    }
}

// MARK: - browse

/// Browse-all (`a`) — the "radio console" survivor. Fetches ALL updates incl.
/// below-the-line (no band filter), tier-colored, ranked by importance. A
/// client-side noise-filter knob hides the noise below the line without
/// re-fetching. j/k selects, Enter opens the thread.
struct BrowseView: View {
    @Environment(AppStore.self) private var store

    @State private var browseState: Loadable<[AttentionUpdate]> = .loading
    /// Client-side min importance — the squelch knob.
    @State private var squelch: Double = 0
    @State private var index = 0

    private var all: [AttentionUpdate] { browseState.value ?? [] }

    private var visible: [AttentionUpdate] {
        all.filter { Double($0.importance) >= squelch }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 10) {
                Text("Noise filter: \(Int(squelch))")
                    .font(Typo.num(11))
                    .foregroundStyle(Palette.inkDim)
                    .frame(width: 104, alignment: .leading)
                Slider(value: $squelch, in: 0...100, step: 5)
                    .tint(Palette.accent)
                Text("\(all.count - visible.count) below line")
                    .font(Typo.num(10))
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(width: 92, alignment: .trailing)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)

            if browseState.isLoading {
                BandNote("loading all mail…")
            } else if let error = browseState.error {
                BandNote(error)
            } else if visible.isEmpty {
                BandNote("nothing above the noise line.")
            } else {
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(spacing: 1) {
                            ForEach(Array(visible.enumerated()), id: \.element.id) { i, u in
                                BrowseRow(
                                    update: u, selected: i == index
                                ) { index = i } open: {
                                    store.openThread(u.thread_id)
                                }
                                .id(u.id)
                            }
                        }
                        .padding(.horizontal, 12)
                        .padding(.bottom, 14)
                    }
                    .onChange(of: index) { _, i in
                        guard let u = visible[safe: i] else { return }
                        withAnimation(Motion.scrollFollow) {
                            proxy.scrollTo(u.id, anchor: .center)
                        }
                    }
                }
            }
        }
        .keyBindings(.modal, bindings)
        .task { await load() }
        .onChange(of: visible.count) { _, count in
            index = min(index, max(0, count - 1))
        }
    }

    private var bindings: [KeyBinding] {
        [
            KeyBinding("j", "next") { index = min(visible.count - 1, index + 1) },
            KeyBinding("k", "prev") { index = max(0, index - 1) },
            KeyBinding("Enter", "open thread") {
                if let u = visible[safe: index] { store.openThread(u.thread_id) }
            },
            KeyBinding("+", "raise noise filter") { squelch = min(100, squelch + 5) },
            KeyBinding("=", "raise noise filter") { squelch = min(100, squelch + 5) },
            KeyBinding("-", "lower noise filter") { squelch = max(0, squelch - 5) },
        ]
    }

    private func load() async {
        await $browseState.load("load failed") {
            let page = try await APIClient.shared.getUpdates(UpdatesParams(limit: 500))
            // Highest importance first — the ranked board.
            return page.items.sorted { $0.importance > $1.importance }
        }
    }
}

private struct BrowseRow: View {
    let update: AttentionUpdate
    let selected: Bool
    let onSelect: () -> Void
    let open: () -> Void

    var body: some View {
        // hoverFill off: this list has never washed on hover, and 500 rows of
        // tracking area is not the place to start.
        ListRow(
            selected: selected, cornerRadius: 7, tint: Palette.tierColor(update.tier),
            hPadding: 9, vPadding: 5, hoverFill: false, action: onSelect
        ) { _, _ in
            HStack(spacing: 8) {
                Circle()
                    .fill(Palette.tierColor(update.tier))
                    .frame(width: 6, height: 6)
                Text("\(update.importance)")
                    .font(Typo.num(11, weight: .semibold))
                    .foregroundStyle(Palette.importanceColor(update.importance))
                    .frame(width: 24, alignment: .trailing)
                Text(SenderCache.resolved(update.senderString).displayName)
                    .font(.system(size: 11, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                    .frame(width: 116, alignment: .leading)
                Text(update.one_line)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkDim)
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Text(Fmt.relAge(update.surfaced_at))
                    .font(Typo.num(10))
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(width: 28, alignment: .trailing)
            }
        }
        .overlay(alignment: .leading) {
            if selected {
                RoundedRectangle(cornerRadius: 1)
                    .fill(Palette.tierColor(update.tier))
                    .frame(width: 2)
            }
        }
        .simultaneousGesture(TapGesture(count: 2).onEnded { open() })
    }
}
