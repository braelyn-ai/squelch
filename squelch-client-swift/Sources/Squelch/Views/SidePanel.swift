// Browse-all + search as a right-hand glass panel. It owns the modal KeyContext
// and Esc-to-close; inner views register their list keys into that same context
// and must not push a second one. Mounted only while a side view is open —
// pushing the modal context unconditionally would gate out the whole "list"
// keymap forever. The thread viewer layers above it, inset by sidePanelWidth.

import AppKit
import SwiftUI

struct SidePanel: View {
    @Environment(AppStore.self) private var store

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
                .padding(.horizontal, 16)
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
            .frame(width: sidePanelWidth)
            .frame(maxHeight: .infinity)
            .squelchGlass(.pane, cornerRadius: 0, tint: Palette.glassTintStrong)
            .shadow(color: .black.opacity(0.24), radius: 40, x: -14)
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "back") { store.closeSide() }
        ])
    }
}

// MARK: - search

/// Search: debounced GET /client/search, j/k selection, click or Enter opens a
/// hit in the reader beside the results. Every durable piece of state lives in
/// `store.search`; only `loading`, which dies with the panel, is local.
struct SearchView: View {
    @Environment(AppStore.self) private var store
    @State private var loading = false
    @FocusState private var focused: Bool

    var body: some View {
        @Bindable var store = store

        VStack(alignment: .leading, spacing: 0) {
            Field(label: "") {
                TextField("search mail…", text: $store.search.query)
                    .textFieldStyle(.plain)
                    .focused($focused)
            }
            .padding(.horizontal, 16)
            .padding(.top, 12)
            .padding(.bottom, 8)

            if loading { BandNote("searching…") }
            if let error = store.search.error { BandNote(error) }
            if !loading && store.search.error == nil && !store.search.query.trimmed.isEmpty
                && store.search.hits.isEmpty
            {
                BandNote("no matches.")
            }

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 6) {
                        ForEach(Array(store.search.hits.enumerated()), id: \.element.id) { i, hit in
                            // One click opens: the reader sits beside this list,
                            // so opening a hit costs the results nothing.
                            HitRow(hit: hit, selected: i == store.search.index) {
                                store.search.index = i
                                store.openThread(hit.thread_id)
                            }
                            .id(hit.id)
                        }
                    }
                    .padding(.horizontal, 14)
                    .padding(.bottom, 14)
                }
                .onChange(of: store.search.index) { _, i in
                    guard let hit = store.search.hits[safe: i] else { return }
                    withAnimation(Motion.scrollFollow) {
                        proxy.scrollTo(hit.id, anchor: .center)
                    }
                }
                // Reopening restores the selection, so restore the scroll too —
                // an index you have to hunt for isn't preserved.
                .onAppear {
                    guard let hit = store.search.hits[safe: store.search.index] else { return }
                    Task { @MainActor in proxy.scrollTo(hit.id, anchor: .center) }
                }
            }
        }
        .keyBindings(.modal, bindings)
        .onAppear { focused = true }
        // The remembered query lands selected, so `/` serves both callers: arrow
        // down into the old results, or type to replace it.
        .onChange(of: focused) { _, on in
            guard on, !store.search.query.isEmpty else { return }
            Task { @MainActor in
                NSApp.sendAction(#selector(NSText.selectAll(_:)), to: nil, from: nil)
            }
        }
        .task(id: store.search.query) { await runSearch() }
    }

    private var bindings: [KeyBinding] {
        [
            KeyBinding("ArrowDown", "next hit", allowInInput: true) { move(1) },
            KeyBinding("ArrowUp", "prev hit", allowInInput: true) { move(-1) },
            KeyBinding("Enter", "open thread", allowInInput: true) { open() },
            // j/k also work when focus is not in the input.
            KeyBinding("j", "next hit") { move(1) },
            KeyBinding("k", "prev hit") { move(-1) },
        ]
    }

    private func move(_ delta: Int) {
        store.search.index = max(
            0, min(store.search.hits.count - 1, store.search.index + delta))
    }

    private func open() {
        if let hit = store.search.hits[safe: store.search.index] {
            store.openThread(hit.thread_id)
        }
    }

    private func runSearch() async {
        let term = store.search.query.trimmed
        guard !term.isEmpty else {
            store.search.hits = []
            store.search.error = nil
            store.search.fetchedQuery = nil
            loading = false
            return
        }
        // Already holding this term's results: reopening must not re-fetch and
        // flash, which is the point of hoisting the session into the store.
        guard term != store.search.fetchedQuery else { return }
        loading = true
        // Debounce: a fresh keystroke cancels this task before the request.
        try? await Task.sleep(for: .milliseconds(220))
        guard !Task.isCancelled else { return }
        do {
            let page = try await APIClient.shared.search(term, limit: 50)
            store.search.hits = page.items
            store.search.index = 0
            store.search.error = nil
            store.search.fetchedQuery = term
        } catch {
            store.search.error = errText(error, "search failed")
            // Leave `fetchedQuery` nil so reopening RETRIES rather than
            // resurrecting a stale error over stale hits.
            store.search.fetchedQuery = nil
        }
        loading = false
    }
}

private struct HitRow: View {
    let hit: SearchHit
    let selected: Bool
    let onOpen: () -> Void

    var body: some View {
        Button(action: onOpen) {
            VStack(alignment: .leading, spacing: 3) {
                HStack {
                    Text(hit.from_name ?? hit.from_addr)
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Palette.ink)
                        .lineLimit(1)
                    Spacer(minLength: 6)
                    Text(Fmt.dateTime(hit.received_at))
                        .font(Typo.num(10))
                        .foregroundStyle(Palette.inkFaintest)
                }
                Text(hit.subject)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkDim)
                    .lineLimit(1)
                Text(hit.snippet)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .lineLimit(2)
                    .multilineTextAlignment(.leading)
            }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(selected ? Palette.accentSoft : Palette.hairline.opacity(0.35))
        )
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
