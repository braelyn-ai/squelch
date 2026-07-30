// EMAILS VIEW — the traditional flat inbox: ALL updates, every tier, newest
// first, one dense list that hides nothing. Owns the "list" KeyContext; e/d drop
// the row optimistically here because the action layer only removes it from the
// sitrep bands. No persistent selection — the highlight renders only while the
// keyboard drives, and action keys are inert without kbActive or a live hover.

import SwiftUI

struct EmailsView: View {
    @Environment(AppStore.self) private var store
    @Namespace private var listGlass

    /// One generous page — the read model is local, this is cheap.
    private static let fetchLimit = 500
    /// How many rows get their thread warmed ahead of any click — a bounded
    /// prefix, because warming all 500 would stampede the daemon for mail nobody
    /// scrolls to. Hover warming picks up anything past it.
    private static let warmRows = 40

    @State private var items: [AttentionUpdate]?
    @State private var error: String?
    @State private var index = 0
    /// True only while the keyboard is driving the cursor.
    @State private var kbActive = false
    /// True only while the cursor is actually over a row.
    @State private var hovering = false

    /// The local snapshot MINUS anything resolved since it was fetched: `items`
    /// only reloads on the 10s `store.lastRefresh` poll, so without this filter
    /// mail finished from the reader sits here visibly undone until the next tick.
    private var rows: [AttentionUpdate] {
        (items ?? []).filter { !store.resolvedIds.contains($0.id) }
    }
    private var selected: AttentionUpdate? { rows[safe: index] }
    /// Action keys are inert unless something is actually highlighted.
    private var actionable: Bool { kbActive || hovering }

    var body: some View {
        VStack(spacing: 0) {
            header
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 1) {
                        if let error {
                            BandNote(error)
                        } else if items == nil {
                            BandNote("loading mail…")
                        } else if rows.isEmpty {
                            BandNote("No mail.")
                        } else {
                            ForEach(Array(rows.enumerated()), id: \.element.id) { i, u in
                                UpdateRow(
                                    update: u,
                                    selected: kbActive && i == index,
                                    glassNamespace: listGlass,
                                    onHover: {
                                        // A hover must NOT follow-scroll: a row near
                                        // the viewport edge would jump the list out
                                        // from under the cursor.
                                        hovering = true
                                        kbActive = false
                                        index = i
                                    },
                                    onOpen: { store.openThread(u.thread_id, queue: rows) }
                                )
                                .id(u.id)
                            }
                        }
                    }
                    .padding(.horizontal, 18)
                    .padding(.vertical, 10)
                }
                .onChange(of: index) { _, i in
                    // Follow the KEYBOARD selection only.
                    guard kbActive, let u = rows[safe: i] else { return }
                    withAnimation(.easeOut(duration: 0.12)) { proxy.scrollTo(u.id, anchor: .center) }
                }
            }
            .onContinuousHover { phase in
                if case .ended = phase { hovering = false }
            }
        }
        .keyBindings(.list, bindings)
        .task(id: store.lastRefresh) { await load() }
        // Pull the thread for the row the cursor rests on, DEBOUNCED so sweeping
        // a 500-row list fires one request for the row you stop on rather than
        // one per row you pass. Covers everything past the bounded head-warm.
        .task(id: selected?.id) {
            guard let u = selected else { return }
            try? await Task.sleep(for: .milliseconds(120))
            guard !Task.isCancelled else { return }
            ThreadPrefetch.shared.prefetch(u.thread_id)
        }
        .onChange(of: rows.count) { _, count in
            index = max(0, min(index, count - 1))
        }
        // Jump to (and highlight) a hand-off target from the sitrep rails.
        .onChange(of: store.selectedId) { _, id in
            guard let id, let i = rows.firstIndex(where: { $0.id == id }) else { return }
            kbActive = true
            index = i
        }
    }

    // MARK: - header

    private var header: some View {
        let signal = store.sitrep.standing.count + store.sitrep.new.count + store.sitrep.open.count
        let noise = store.sitrep.stats?.tier_counts["noise"] ?? 0

        return HStack(spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 7) {
                Text("squelch")
                    .font(Typo.serif(19, weight: .medium))
                    .foregroundStyle(Palette.ink)
                Text("all mail")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .textCase(.uppercase)
            }
            Spacer(minLength: 12)
            HStack(spacing: 8) {
                Text("\(signal)").font(Typo.num(12, weight: .bold)).foregroundStyle(Palette.accent)
                Text("signal").font(Typo.micro).foregroundStyle(Palette.inkFaint)
                Text("\(noise)").font(Typo.num(12, weight: .bold)).foregroundStyle(Palette.inkFaint)
                Text("noise").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                if let err = store.refreshError {
                    Text("· offline")
                        .font(Typo.micro).foregroundStyle(Palette.warn)
                        .help(err.message)
                } else {
                    Text("· last checked: \(Fmt.lastChecked(store.sitrep.stats?.last_surfaced_at))")
                        .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                }
            }
            RetriageButton()
            if !store.sitrep.sealed.isEmpty {
                Button { store.setView(.auth) } label: {
                    Label("\(store.sitrep.sealed.count)", systemImage: "key.fill")
                        .font(Typo.micro)
                        .padding(.horizontal, 8).padding(.vertical, 3)
                }
                .buttonStyle(.glass)
                .foregroundStyle(Palette.lock)
                .help("login codes, password resets & sign-in alerts (g)")
            }
            Button { store.setView(.audit) } label: {
                Image(systemName: "scroll").font(.system(size: 12))
                    .padding(.horizontal, 6).padding(.vertical, 3)
            }
            .buttonStyle(.glass)
            .foregroundStyle(Palette.inkFaint)
            .help("audit log — agent & app actions (A)")
            Button { store.shortcutsOpen = true } label: {
                Text("?").font(.system(size: 12, weight: .semibold))
                    .padding(.horizontal, 7).padding(.vertical, 3)
            }
            .buttonStyle(.glass)
            .foregroundStyle(Palette.inkFaint)
            .help("keyboard shortcuts (?)")
            ThemeToggle()
        }
        // These metrics must match the sitrep masthead's: the rail icon beside
        // this header is aligned to that wordmark's line.
        .padding(.horizontal, 24)
        .padding(.top, 16)
        .padding(.bottom, 12)
        .overlay(alignment: .bottom) { Rectangle().fill(Palette.hairline).frame(height: 0.5) }
    }

    // MARK: - keymap

    private var bindings: [KeyBinding] {
        [
            KeyBinding("j", "next") { moveByKey(+1) },
            KeyBinding("k", "prev") { moveByKey(-1) },
            KeyBinding("ArrowDown", "next") { moveByKey(+1) },
            KeyBinding("ArrowUp", "prev") { moveByKey(-1) },
            KeyBinding("Escape", "back to sitrep") { store.setView(.sitrep) },
            KeyBinding("Enter", "drill in") {
                guard actionable, let u = selected else { return }
                // The ordered rows become the viewer's queue, so "done + next"
                // (e/d) can advance in place.
                store.openThread(u.thread_id, queue: rows)
            },
            KeyBinding("v", "fix triage") {
                guard actionable, let u = selected else { return }
                store.openTriageFix(
                    TriageFixTarget(
                        messageId: u.id, sender: u.sender, subject: u.one_line,
                        tier: .some(u.tier.rawValue)))
            },
            KeyBinding("r", "reply") {
                guard actionable, let u = selected else { return }
                Actions.reply(u)
            },
            KeyBinding("e", "done") { resolveSelected() },
            KeyBinding("d", "done") { resolveSelected() },
            KeyBinding("a", "browse all") { store.openSide(.browse) },
            KeyBinding("T", "rules") { store.setView(.rules) },
            KeyBinding("A", "audit log") { store.setView(.audit) },
            KeyBinding("g", "auth messages") { store.setView(.auth) },
            KeyBinding("u", "undo") { Task { await store.fireUndo() } },
            // `\` (theme) and `?` (help) are global bindings, not listed here.
        ]
    }

    private func moveByKey(_ delta: Int) {
        kbActive = true
        index = max(0, min(rows.count - 1, index + delta))
    }

    private func resolveSelected() {
        guard actionable, let u = selected else { return }
        Task { await Actions.done(u) }
        items?.removeAll { $0.id == u.id }
    }

    // MARK: - data

    /// Epoch for "order received": surfaced_at approximates arrival, and items
    /// the triage loop hasn't surfaced yet are the newest mail, so they sort to
    /// the top. Ties (and the nil bucket) break on id, which is ingest order.
    private static func receivedTS(_ u: AttentionUpdate) -> Double {
        guard let s = u.surfaced_at else { return .greatestFiniteMagnitude }
        return Fmt.date(s)?.timeIntervalSince1970 ?? 0
    }

    private func load() async {
        do {
            let page = try await APIClient.shared.getUpdates(
                UpdatesParams(limit: Self.fetchLimit))
            // Done/archived mail leaves the inbox (gmail semantics), which also
            // keeps auto-resolved receipts out — they're rail records, not rows.
            let next =
                page.items
                .filter { $0.status != .done }
                .sorted { a, b in
                    let ta = Self.receivedTS(a)
                    let tb = Self.receivedTS(b)
                    return ta != tb ? ta > tb : a.id > b.id
                }
            // Only touch state when the list actually changed — the 10s poll
            // re-runs this and an identical assignment would re-render 500 rows.
            if next != items { items = next }
            error = nil
            // Pull the head rows' threads before any click, so an open renders
            // from cache.
            ThreadPrefetch.shared.warm(
                (items ?? []).prefix(Self.warmRows).map(\.thread_id), immediate: 5)
        } catch {
            self.error = errText(error, "load failed")
        }
    }
}

/// An inline note inside the list (loading / empty / error).
struct BandNote: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(Typo.rowSub)
            .foregroundStyle(Palette.inkFaintest)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 18)
    }
}
