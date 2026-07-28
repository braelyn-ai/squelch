// SIDE VIEWS — browse-all + search, as a right-hand glass panel.
//
// Renders whichever side view store.sideView selects and owns the modal
// KeyContext + Esc-to-close for the whole panel. Each inner view registers its
// own list-style keys into this same modal context; they must NOT push a
// second context.
//
// CRITICAL: this panel is mounted ONLY while a side view is open. If the modal
// context were pushed unconditionally it would sit on the stack forever,
// permanently gating out the entire "list" keymap and leaving Escape as the
// only working key — the exact bug the desktop client's header warns about.
//
// The thread drill-in is NOT a side view: it's the fullscreen viewer, layered
// ABOVE this panel, so opening a thread from search keeps the results mounted
// underneath and Esc returns to them.
//
// Ported from squelch-desktop/src/views/SideViews.tsx and
// src/components/{SearchView,BrowseView}.tsx.

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
                .overlay(alignment: .bottom) {
                    Rectangle().fill(Palette.hairline).frame(height: 0.5)
                }

                Group {
                    switch store.sideView {
                    case .search(let query): SearchView(initialQuery: query)
                    case .browse: BrowseView()
                    case .none: EmptyView()
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            }
            .frame(width: 460)
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

/// Search side view. Debounced to GET /client/search; results with j/k
/// selection; Enter opens the selected hit fullscreen (the viewer layers above
/// this panel, which stays mounted underneath).
struct SearchView: View {
    let initialQuery: String

    @Environment(AppStore.self) private var store
    @State private var query: String
    @State private var hits: [SearchHit] = []
    @State private var error: String?
    @State private var loading = false
    @State private var index = 0
    @FocusState private var focused: Bool

    init(initialQuery: String) {
        self.initialQuery = initialQuery
        _query = State(initialValue: initialQuery)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Field(label: "") {
                TextField("search mail…", text: $query)
                    .textFieldStyle(.plain)
                    .focused($focused)
            }
            .padding(.horizontal, 16)
            .padding(.top, 12)
            .padding(.bottom, 8)

            if loading { BandNote("searching…") }
            if let error { BandNote(error) }
            if !loading && error == nil && !query.trimmed.isEmpty && hits.isEmpty {
                BandNote("no matches.")
            }

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 6) {
                        ForEach(Array(hits.enumerated()), id: \.element.id) { i, hit in
                            HitRow(hit: hit, selected: i == index, onSelect: { index = i }) {
                                store.openThread(hit.thread_id)
                            }
                            .id(hit.id)
                        }
                    }
                    .padding(.horizontal, 14)
                    .padding(.bottom, 14)
                }
                .onChange(of: index) { _, i in
                    guard let hit = hits[safe: i] else { return }
                    withAnimation(.easeOut(duration: 0.12)) {
                        proxy.scrollTo(hit.id, anchor: .center)
                    }
                }
            }
        }
        .keyBindings(.modal, bindings)
        .onAppear { focused = true }
        .task(id: query) { await runSearch() }
    }

    private var bindings: [KeyBinding] {
        [
            KeyBinding("ArrowDown", "next hit", allowInInput: true) {
                index = min(hits.count - 1, index + 1)
            },
            KeyBinding("ArrowUp", "prev hit", allowInInput: true) {
                index = max(0, index - 1)
            },
            KeyBinding("Enter", "open thread", allowInInput: true) {
                if let hit = hits[safe: index] { store.openThread(hit.thread_id) }
            },
            // j/k also work when focus is not in the input.
            KeyBinding("j", "next hit") { index = min(hits.count - 1, index + 1) },
            KeyBinding("k", "prev hit") { index = max(0, index - 1) },
        ]
    }

    private func runSearch() async {
        let term = query.trimmed
        guard !term.isEmpty else {
            hits = []
            error = nil
            loading = false
            return
        }
        loading = true
        // Debounce: a fresh keystroke cancels this task before the request.
        try? await Task.sleep(for: .milliseconds(220))
        guard !Task.isCancelled else { return }
        do {
            let page = try await APIClient.shared.search(term, limit: 50)
            hits = page.items
            index = 0
            error = nil
        } catch {
            self.error = errText(error, "search failed")
        }
        loading = false
    }
}

private struct HitRow: View {
    let hit: SearchHit
    let selected: Bool
    let onSelect: () -> Void
    let onOpen: () -> Void

    var body: some View {
        Button(action: onSelect) {
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
        .simultaneousGesture(TapGesture(count: 2).onEnded { onOpen() })
    }
}

// MARK: - browse

/// Browse-all (`a`) — the "radio console" survivor. Fetches ALL updates incl.
/// below-the-line (no band filter), tier-colored, ranked by importance. A
/// client-side noise-filter knob hides the noise below the line without
/// re-fetching. j/k selects, Enter opens the thread.
struct BrowseView: View {
    @Environment(AppStore.self) private var store
    @Namespace private var browseGlass

    @State private var all: [AttentionUpdate] = []
    @State private var error: String?
    @State private var loading = true
    /// Client-side min importance — the squelch knob.
    @State private var squelch: Double = 0
    @State private var index = 0

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

            if loading {
                BandNote("loading all mail…")
            } else if let error {
                BandNote(error)
            } else if visible.isEmpty {
                BandNote("nothing above the noise line.")
            } else {
                ScrollViewReader { proxy in
                    ScrollView {
                        GlassEffectContainer(spacing: 2) {
                        LazyVStack(spacing: 1) {
                            ForEach(Array(visible.enumerated()), id: \.element.id) { i, u in
                                BrowseRow(
                                    update: u, selected: i == index, glassNamespace: browseGlass
                                ) { index = i } open: {
                                    store.openThread(u.thread_id)
                                }
                                .id(u.id)
                            }
                        }
                        }
                        .padding(.horizontal, 12)
                        .padding(.bottom, 14)
                    }
                    .onChange(of: index) { _, i in
                        guard let u = visible[safe: i] else { return }
                        withAnimation(.easeOut(duration: 0.12)) {
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
        loading = true
        defer { loading = false }
        do {
            let page = try await APIClient.shared.getUpdates(UpdatesParams(limit: 500))
            // Highest importance first — the ranked board.
            all = page.items.sorted { $0.importance > $1.importance }
            error = nil
        } catch {
            self.error = errText(error, "load failed")
        }
    }
}

private struct BrowseRow: View {
    let update: AttentionUpdate
    let selected: Bool
    let glassNamespace: Namespace.ID
    let onSelect: () -> Void
    let open: () -> Void

    var body: some View {
        Button(action: onSelect) {
            HStack(spacing: 8) {
                Circle()
                    .fill(Palette.tierColor(update.tier))
                    .frame(width: 6, height: 6)
                Text("\(update.importance)")
                    .font(Typo.num(11, weight: .semibold))
                    .foregroundStyle(Palette.importanceColor(update.importance))
                    .frame(width: 24, alignment: .trailing)
                Text(SenderID.displayName(update.sender))
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
            .padding(.horizontal, 9)
            .padding(.vertical, 5)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .selectionGlass(
            selected, tint: Palette.tierColor(update.tier), cornerRadius: 7,
            id: "browse-selection", in: glassNamespace)
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
