// EMAILS VIEW — the traditional flat inbox: ALL updates, every tier, newest
// first, one dense list that hides nothing. Owns the "list" KeyContext; e/d drop
// the row optimistically here because the action layer only removes it from the
// sitrep bands. No persistent selection — the highlight renders only while the
// keyboard drives, and action keys are inert without kbActive or a live hover.
//
// TWO PAGES, one list: the inbox and the noise bin (`n`, or the header's noise
// count). Only the DATA SOURCE differs — every verb, the queue handed to the
// reader and the cursor behave identically on noise rows.
//
// A THIRD PAGE, `sent`, shares the chrome and the cursor but not the verbs: it
// holds outbound mail, which nothing triaged and no verb here can resolve. So
// the rows are SentRows, j/k/Enter work exactly as they do everywhere else, and
// r/e/d/v/f are inert rather than acting on a row they have no meaning for.
//
// AND A LENS OVER THE INBOX: the reminder filter, which swaps the rows for mail
// that is parked (`h`) and waiting to come back. A lens rather than a fourth
// page because it is not a place you navigate to — the first Escape sheds it and
// you are back on the mail you were reading. Its rows come from their own cache
// for one reason worth stating: every one of them is `done`, so the ordinary
// page — which filters through `resolvedIds` — would hide them all.

import SwiftUI

struct EmailsView: View {
    @Environment(AppStore.self) private var store

    @State private var index = 0
    /// True only while the keyboard is driving the cursor.
    @State private var kbActive = false
    /// True only while the cursor is actually over a row.
    @State private var hovering = false

    private var mode: MailMode { store.mailMode }
    /// The reminder lens, which only the inbox page has. Read through here so
    /// every row/verb/key below asks one question rather than two.
    private var reminders: Bool { store.reminderFilter && mode == .inbox }
    /// The cached page for whichever INBOX-shaped mode is showing (sent has its
    /// own wire type and its own cache — see `sentPage`), or the parked list
    /// when the lens is on.
    private var page: Loadable<[AttentionUpdate]> {
        reminders ? store.remindersPage : store.mailPage(mode)
    }
    private var sentPage: Loadable<[SentItem]> { store.sentPage }

    /// The cached page MINUS anything resolved since it was fetched: the page
    /// only reloads on the 10s `store.lastRefresh` poll, so without this filter
    /// mail finished from the reader sits here visibly undone until the next tick.
    ///
    /// The parked rows skip it entirely, and must: setting a reminder resolves
    /// the thread, so every id on that list is in `resolvedIds` by construction
    /// and the filter would empty the page it just filled.
    private var rows: [AttentionUpdate] {
        let cached = page.value ?? []
        return reminders ? cached : cached.filter { !store.resolvedIds.contains($0.id) }
    }
    /// Sent rows are NOT filtered against `resolvedIds`: a resolve is an inbox
    /// verdict, and it must never delete a record of something you sent.
    private var sent: [SentItem] { sentPage.value ?? [] }

    private var selected: AttentionUpdate? { rows[safe: index] }
    private var selectedSent: SentItem? { sent[safe: index] }
    /// However many rows the page on screen has — the cursor's only bound.
    private var rowCount: Int { mode == .sent ? sent.count : rows.count }
    /// The row under the cursor as the two things every page can answer: the
    /// message id (a `.task` key, and the scroll target) and the thread to warm.
    private var cursorRow: (id: Int, threadId: String)? {
        if mode == .sent { return selectedSent.map { ($0.id, $0.thread_id) } }
        return selected.map { ($0.id, $0.thread_id) }
    }
    /// Action keys are inert unless something is actually highlighted.
    private var actionable: Bool { kbActive || hovering }
    /// The triage verbs on top of that: sent mail has no triage to act on.
    private var triageable: Bool { actionable && mode != .sent }

    var body: some View {
        VStack(spacing: 0) {
            header
            #if os(macOS)
                desktopList
            #else
                phoneList
            #endif
        }
        .keyBindings(.list, bindings)
        // Fires on mount, on each 10s poll AND on a mode switch — only the page
        // being shown is refreshed, so the noise page costs nothing until you go
        // there. The store's short TTL is what makes the mount half free when a
        // tick just landed; the tick half always outruns it and refreshes for real.
        .task(id: RefreshKey(tick: store.lastRefresh, mode: mode, reminders: reminders)) {
            if reminders {
                await store.refreshReminders()
            } else if mode == .sent {
                await store.refreshSent()
            } else {
                await store.refreshMail(mode)
            }
        }
        // Pull the thread for the row the cursor rests on, DEBOUNCED so sweeping
        // a 500-row list fires one request for the row you stop on rather than
        // one per row you pass. Covers everything past the bounded head-warm.
        .task(id: cursorRow?.id) {
            guard let row = cursorRow else { return }
            try? await Task.sleep(for: .milliseconds(120))
            guard !Task.isCancelled else { return }
            ThreadPrefetch.shared.prefetch(row.threadId)
        }
        .onChange(of: rowCount) { _, count in
            index = max(0, min(index, count - 1))
        }
        // A mode switch replaces every row, so the cursor cannot keep its index —
        // it would land on an unrelated email, and the verbs act on the highlight.
        // Flipping the lens replaces them just as completely.
        // The lens goes with it: a page switch is a fresh page, and a filter
        // that survived one would silently re-narrow the inbox on the way back.
        .onChange(of: mode) { _, _ in
            store.reminderFilter = false
            resetCursor()
        }
        .onChange(of: reminders) { _, _ in resetCursor() }
        // Jump to (and highlight) a hand-off target from the sitrep rails.
        .onChange(of: store.selectedId) { _, id in
            guard let id, let i = rows.firstIndex(where: { $0.id == id }) else { return }
            kbActive = true
            index = i
        }
    }

    // MARK: - the lists

    /// The three inline notes for the mail / noise pages, shared by both
    /// layouts. Each is gated on having NO rows at all: a reload keeps the last
    /// page on screen, so a revisit — or a failure while offline — updates
    /// underneath what you are already reading instead of replacing it with a
    /// word. The sent page carries its own notes inside `sentList`.
    @ViewBuilder
    private var note: some View {
        if let error = page.error, page.value == nil {
            BandNote(error)
        } else if page.value == nil {
            BandNote(
                reminders ? "loading reminders…" : mode == .noise ? "loading noise…" : "loading mail…"
            )
        } else if rows.isEmpty {
            // The window the daemon answers with is 30 days, so an empty noise
            // page says so rather than implying "ever".
            BandNote(
                reminders
                    ? "No pending reminders."
                    : mode == .noise ? "No noise in the last 30 days." : "No mail.")
        }
    }

    /// The sent rows, same three gated notes. The reader opens with NO queue:
    /// "done + next" walks a triage list, and there is nothing to finish here.
    @ViewBuilder private var sentList: some View {
        if let error = sentPage.error, sentPage.value == nil {
            BandNote(error)
        } else if sentPage.value == nil {
            BandNote("loading sent…")
        } else if sent.isEmpty {
            BandNote("Nothing sent yet.")
        } else {
            ForEach(Array(sent.enumerated()), id: \.element.id) { i, item in
                SentRow(
                    item: item,
                    selected: kbActive && i == index,
                    onHover: {
                        hovering = true
                        kbActive = false
                        index = i
                    },
                    onOpen: { store.openThread(item.thread_id) }
                )
                .id(item.id)
            }
        }
    }

    /// The scroll target for a row position on whichever page is showing.
    private func rowId(at i: Int) -> Int? {
        mode == .sent ? sent[safe: i]?.id : rows[safe: i]?.id
    }

    private var hasRows: Bool { page.value != nil && page.error == nil && !rows.isEmpty }

    #if os(macOS)
        private var desktopList: some View {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 1) {
                        if mode == .sent {
                            sentList
                        } else if !hasRows {
                            note
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
                    // Follow the KEYBOARD selection only. Both pages key their
                    // rows on the message id, so one scroll target serves both.
                    guard kbActive, let id = rowId(at: i) else { return }
                    withAnimation(Motion.scrollFollow) { proxy.scrollTo(id, anchor: .center) }
                }
            }
            .onContinuousHover { phase in
                if case .ended = phase { hovering = false }
            }
        }
    #endif

    #if !os(macOS)
        /// A REAL `List` on the phone, and only on the phone. Everything else in
        /// this app draws its rows into a LazyVStack because the Mac's lists are
        /// keyboard surfaces that need a cursor, an anchor and a scroll-to; none
        /// of that survives contact with a thumb, and what a thumb wants instead —
        /// `swipeActions`, full-swipe commit, the rubber-band, the system's own
        /// timing — is something only `List` can hand over. Reimplementing it
        /// would be a worse copy of a control the OS ships.
        ///
        /// The design skin survives intact: plain style, no separators, clear row
        /// backgrounds, and the app's own `UpdateRow` inside. What the List
        /// contributes is the gesture, not the look.
        private var phoneList: some View {
            List {
                if mode == .sent {
                    // Sent rows carry no triage, so no swipe verbs — the row is
                    // a door to the thread and nothing else (`triageable` says
                    // the same thing to the keyboard).
                    sentList.plainRow()
                } else if !hasRows {
                    note.plainRow()
                } else {
                    ForEach(rows) { u in
                        let verbs = UpdateVerbs(update: u, queue: rows)
                        UpdateRow(
                            update: u,
                            selected: false,
                            onHover: {},
                            onOpen: verbs.open
                        )
                        .plainRow()
                        .swipeVerbs(verbs)
                        .updateContextMenu(verbs)
                    }
                }
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            .environment(\.defaultMinListRowHeight, 0)
        }
    #endif

    // MARK: - header

    /// A Mac's page header is also the app's chrome bar: wordmark, counts, the
    /// freshness stamp, and the door to the process page. The auth / audit /
    /// shortcuts / theme chips that used to line up here are gone, not moved:
    /// every one of them already has a rail button or a global key (g, A, ?, \),
    /// and a second door in the chrome was chrome for its own sake. A phone has
    /// a navigation bar for the name and a tab bar for the doors, so all that is
    /// left up here is the one thing this page owns — which of the two lists you
    /// are looking at, and how much is in the other one.
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
            // NO MODE LABEL BESIDE THE WORDMARK. It said what the segmented
            // control an inch to its right already says, and it said it in a
            // string whose width changed with the answer — so every switch
            // between all mail / noise / sent shoved the control and the whole
            // run of chrome after it sideways. The one thing in this bar that
            // moved was the one thing that was repeating itself.
            Text("passband")
                .font(Typo.serif(19, weight: .medium))
                .foregroundStyle(Palette.ink)
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
            // The reminder lens. Inbox only: `noise` and `sent` are pages, and
            // narrowing either of them to parked mail would answer a question
            // nobody asked from there.
            if mode == .inbox {
                ChromeChip(
                    icon: reminders ? "bell.fill" : "bell",
                    font: .system(size: 12),
                    tone: reminders ? Palette.accent : Palette.inkFaint,
                    help: "mail with pending reminders (h sets one)"
                ) { store.reminderFilter.toggle() }
                .accessibilityLabel(reminders ? "showing pending reminders" : "pending reminders")
            }
            RetriageButton()
            ChromeChip(
                text: "peer-review", font: .system(size: 12),
                help: "the process page: verify how your mail was sorted"
            ) { store.setView(.process) }
        }
        // These metrics must match the sitrep masthead's: every page's bar ends
        // on the line the rail's top edge is cut to.
        .padding(.horizontal, 24)
        .frame(height: TopBar.height)
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
            // side panel: the lens comes off first, then any page that is not
            // the inbox steps back to it, and only then does Escape leave the
            // tab. Each rung undoes the narrowing you did last.
            KeyBinding("Escape", "back") {
                if reminders {
                    store.reminderFilter = false
                } else if mode != .inbox {
                    store.mailMode = .inbox
                } else {
                    store.setView(.sitrep)
                }
            },
            // One key both ways — noise is a page you dip into, not a mode you
            // have to remember you are in. It stays the inbox/noise flip from
            // the sent page too; `sent` is reached by the segments.
            KeyBinding("n", "noise / back") { store.mailMode = mode.flipped },
            KeyBinding("Enter", "drill in") {
                guard actionable else { return }
                if mode == .sent {
                    guard let item = selectedSent else { return }
                    // No queue: "done + next" walks a triage list, and sent mail
                    // has nothing to finish.
                    store.openThread(item.thread_id)
                } else if let u = selected {
                    // The ordered rows become the viewer's queue, so "done + next"
                    // (e/d) can advance in place.
                    store.openThread(u.thread_id, queue: rows)
                }
            },
            KeyBinding("v", "fix triage") {
                guard triageable, let u = selected else { return }
                store.openTriageFix(
                    TriageFixTarget(
                        messageId: u.id, sender: u.sender, subject: u.one_line,
                        tier: .some(u.tier.rawValue)))
            },
            // Reply opens the email and composes in it, so it hands over the same
            // queue Enter does — done + next keeps working from inside the reader.
            KeyBinding("r", "reply") {
                guard triageable, let u = selected else { return }
                Actions.reply(u, queue: rows)
            },
            // `sender` IS the address on this wire type (see AttentionUpdate's
            // SenderStringConvertible note) — the display name would search the
            // body text of unrelated mail. Inert on the sent page, where the
            // sender is the reader and the seed would find their whole archive.
            KeyBinding("f", "search this sender") {
                guard triageable, let u = selected else { return }
                store.openSearch(seed: "from:\(u.sender)")
            },
            KeyBinding("e", "done") { resolveSelected() },
            KeyBinding("d", "done") { resolveSelected() },
            // On the lens it reschedules the row it is already showing — the
            // palette is the same either way.
            KeyBinding("h", "remind me later") {
                guard triageable, let u = selected else { return }
                store.openRemind(
                    RemindTarget(
                        messageId: u.id, sender: u.sender, subject: u.one_line,
                        remindAt: u.remind_at))
            },
            // Cancel a reminder WITHOUT reopening the mail — it was resolved
            // when the reminder was set, and "I do not need to see this again"
            // is a statement about the reminder, not about the email. Bound
            // only on the lens: everywhere else Backspace has no row to mean.
            KeyBinding("Backspace", "cancel reminder") {
                guard reminders, actionable, let u = selected else { return }
                Task { await Actions.cancelReminder(u) }
            },
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
        var reminders: Bool
    }

    private func moveByKey(_ delta: Int) {
        kbActive = true
        index = max(0, min(rowCount - 1, index + delta))
    }

    private func resetCursor() {
        index = 0
        kbActive = false
        hovering = false
    }

    private func resolveSelected() {
        guard triageable, let u = selected else { return }
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
