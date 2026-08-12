// EMAILS VIEW — the traditional flat inbox: ALL updates, every tier, newest
// first, one dense list that hides nothing. Owns the "list" KeyContext; e/d drop
// the row optimistically here because the action layer only removes it from the
// sitrep bands. No persistent selection — the highlight renders only while the
// keyboard drives, and action keys are inert without kbActive or a live hover.
//
// TWO PAGES, one list: the inbox and the noise bin (`n`, or the header's noise
// count). Only the DATA SOURCE differs — every verb, the queue handed to the
// reader and the cursor behave identically on noise rows.

import SwiftUI

struct EmailsView: View {
    @Environment(AppStore.self) private var store

    @State private var index = 0
    /// True only while the keyboard is driving the cursor.
    @State private var kbActive = false
    /// True only while the cursor is actually over a row.
    @State private var hovering = false

    private var mode: MailMode { store.mailMode }
    /// The cached page for whichever mode is showing.
    private var page: Loadable<[AttentionUpdate]> { store.mailPage(mode) }

    /// The cached page MINUS anything resolved since it was fetched: the page
    /// only reloads on the 10s `store.lastRefresh` poll, so without this filter
    /// mail finished from the reader sits here visibly undone until the next tick.
    private var rows: [AttentionUpdate] {
        (page.value ?? []).filter { !store.resolvedIds.contains($0.id) }
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
                        // Both notes are gated on having NO rows at all. A reload
                        // keeps the last page on screen, so a revisit — or a
                        // failure while offline — updates underneath what you are
                        // already reading instead of replacing it with a word.
                        if let error = page.error, page.value == nil {
                            BandNote(error)
                        } else if page.value == nil {
                            BandNote(mode == .noise ? "loading noise…" : "loading mail…")
                        } else if rows.isEmpty {
                            // The window the daemon answers with is 30 days, so an
                            // empty noise page says so rather than implying "ever".
                            BandNote(mode == .noise ? "No noise in the last 30 days." : "No mail.")
                        } else {
                            ForEach(Array(rows.enumerated()), id: \.element.id) { i, u in
                                UpdateRow(
                                    update: u,
                                    selected: kbActive && i == index,
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
                    withAnimation(Motion.scrollFollow) { proxy.scrollTo(u.id, anchor: .center) }
                }
            }
            .onContinuousHover { phase in
                if case .ended = phase { hovering = false }
            }
        }
        .keyBindings(.list, bindings)
        // Fires on mount, on each 10s poll AND on a mode switch — only the page
        // being shown is refreshed, so the noise page costs nothing until you go
        // there. The store's short TTL is what makes the mount half free when a
        // tick just landed; the tick half always outruns it and refreshes for real.
        .task(id: RefreshKey(tick: store.lastRefresh, mode: mode)) {
            await store.refreshMail(mode)
        }
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
        // A mode switch replaces every row, so the cursor cannot keep its index —
        // it would land on an unrelated email, and the verbs act on the highlight.
        .onChange(of: mode) { _, _ in
            index = 0
            kbActive = false
            hovering = false
        }
        // Jump to (and highlight) a hand-off target from the sitrep rails.
        .onChange(of: store.selectedId) { _, id in
            guard let id, let i = rows.firstIndex(where: { $0.id == id }) else { return }
            kbActive = true
            index = i
        }
    }

    // MARK: - header

    /// A Mac's page header is also the app's chrome bar: wordmark, counts, the
    /// freshness stamp, and the doors to auth / audit / shortcuts / theme. A
    /// phone has a navigation bar for the name and a tab bar for the doors, so
    /// all that is left up here is the one thing this page owns — which of the
    /// two lists you are looking at, and how much is in the other one.
    @ViewBuilder
    private var header: some View {
        #if os(macOS)
            desktopHeader
        #else
            phoneHeader
        #endif
    }

    #if !os(macOS)
        private var phoneHeader: some View {
            @Bindable var store = store
            let noise = store.sitrep.stats?.tier_counts["noise"] ?? 0

            return HStack(spacing: 10) {
                GlassSegmented(
                    options: MailMode.allCases.map { ($0, $0.label) },
                    selection: $store.mailMode)
                Spacer(minLength: 8)
                if let err = store.refreshError {
                    Text("offline")
                        .font(Typo.micro).foregroundStyle(Palette.warn)
                        .help(err.message)
                } else if mode == .inbox, noise > 0 {
                    // The count is the door to the page it counts, same as on the
                    // Mac — just without the word "signal" beside it, because the
                    // list underneath is already the signal.
                    Button { store.mailMode = .noise } label: {
                        HStack(spacing: 6) {
                            Text("\(noise)")
                                .font(Typo.num(12, weight: .bold))
                                .foregroundStyle(Palette.inkFaint)
                            Text("noise").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 10)
        }
    #endif

    #if os(macOS)
    private var desktopHeader: some View {
        @Bindable var store = store
        let signal = store.sitrep.standing.count + store.sitrep.new.count + store.sitrep.open.count
        let noise = store.sitrep.stats?.tier_counts["noise"] ?? 0

        return HStack(spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 7) {
                Text("passband")
                    .font(Typo.serif(19, weight: .medium))
                    .foregroundStyle(Palette.ink)
                // The page you are on, not the tab's name.
                Text(mode.label)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .textCase(.uppercase)
            }
            GlassSegmented(
                options: MailMode.allCases.map { ($0, $0.label) },
                selection: $store.mailMode)
            Spacer(minLength: 12)
            HStack(spacing: 8) {
                Text("\(signal)").font(Typo.num(12, weight: .bold)).foregroundStyle(Palette.accent)
                Text("signal").font(Typo.micro).foregroundStyle(Palette.inkFaint)
                // The count is the door to the page it counts. A jump, not a
                // toggle: `n` and the segments are the way back.
                Button { store.mailMode = .noise } label: {
                    HStack(spacing: 8) {
                        Text("\(noise)")
                            .font(Typo.num(12, weight: .bold))
                            .foregroundStyle(mode == .noise ? Palette.accent : Palette.inkFaint)
                        Text("noise").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    }
                    // The number and the word are one target, gap included.
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("the noise page — everything triage filed as noise (n)")
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
                ChromeChip(
                    text: "\(store.sitrep.sealed.count)", icon: "key.fill",
                    tone: Palette.lock,
                    help: "login codes, password resets & sign-in alerts (g)"
                ) { store.setView(.auth) }
            }
            ChromeChip(
                icon: "scroll", font: .system(size: 12),
                help: "audit log — agent & app actions (A)"
            ) { store.setView(.audit) }
            ChromeChip(
                text: "?", font: .system(size: 12, weight: .semibold),
                help: "keyboard shortcuts (?)"
            ) { store.shortcutsOpen = true }
            ThemeToggle()
        }
        // These metrics must match the sitrep masthead's: the rail icon beside
        // this header is aligned to that wordmark's line.
        .padding(.horizontal, 24)
        .padding(.top, 16)
        .padding(.bottom, 12)
        .overlay(alignment: .bottom) { Hairline() }
    }
    #endif

    // MARK: - keymap

    private var bindings: [KeyBinding] {
        [
            KeyBinding("j", "next") { moveByKey(+1) },
            KeyBinding("k", "prev") { moveByKey(-1) },
            KeyBinding("ArrowDown", "next") { moveByKey(+1) },
            KeyBinding("ArrowUp", "prev") { moveByKey(-1) },
            // A LADDER, the same shape as the reader closing over a still-open
            // side panel: from the noise page Escape steps back to the inbox, and
            // only a second press leaves the tab.
            KeyBinding("Escape", "back") {
                if mode == .noise {
                    store.mailMode = .inbox
                } else {
                    store.setView(.sitrep)
                }
            },
            // One key both ways — noise is a page you dip into, not a mode you
            // have to remember you are in.
            KeyBinding("n", "noise / back") { store.mailMode = mode.flipped },
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
            // Reply opens the email and composes in it, so it hands over the same
            // queue Enter does — done + next keeps working from inside the reader.
            KeyBinding("r", "reply") {
                guard actionable, let u = selected else { return }
                Actions.reply(u, queue: rows)
            },
            // `sender` IS the address on this wire type (see AttentionUpdate's
            // SenderStringConvertible note) — the display name would search the
            // body text of unrelated mail.
            KeyBinding("f", "search this sender") {
                guard actionable, let u = selected else { return }
                store.openSearch(seed: "from:\(u.sender)")
            },
            KeyBinding("e", "done") { resolveSelected() },
            KeyBinding("d", "done") { resolveSelected() },
            KeyBinding("a", "browse all") { store.openSide(.browse) },
            KeyBinding("T", "rules") { store.setView(.rules) },
            KeyBinding("A", "audit log") { store.setView(.audit) },
            KeyBinding("g", "auth messages") { store.setView(.auth) },
            // `u` (undo), `\` (theme) and `?` (help) are global bindings, not
            // listed here.
        ]
    }

    /// `.task(id:)` key: a poll tick and a mode switch both have to refetch, and a
    /// tuple of the two cannot be Hashable.
    private struct RefreshKey: Hashable {
        var tick: Date?
        var mode: MailMode
    }

    private func moveByKey(_ delta: Int) {
        kbActive = true
        index = max(0, min(rows.count - 1, index + delta))
    }

    private func resolveSelected() {
        guard actionable, let u = selected else { return }
        Task { await Actions.done(u) }
        // `rows` already filters on resolvedIds, so the row leaves on the next
        // frame regardless; this keeps it out of the cached page too, so the
        // next poll cannot briefly hand it back.
        store.removeFromMail(u.id)
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
