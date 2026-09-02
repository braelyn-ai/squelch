// SITREP VIEW — the abstracted dashboard and default surface on launch: ranked
// standing items, newsletters, a status strip and the records rail. Mail still
// landing is a spinner in the masthead (IngestIndicator), not a board zone.
// Owns the "sitrep" KeyContext. No persistent selection — the focus fill renders
// only while the keyboard drives, and hover must NOT drag it.

import SwiftUI

/// How many ranked standing items show before the quiet "{n} more" expander.
private let eyesVisible = 10

/// THE FOR-YOUR-EYES WALK: the queue the reader is handed when an email is
/// opened from this band, so `E` (done + next) can step through it without
/// coming back here.
///
/// The FULL ranked list, not the visible slice — the cursor stops at the
/// collapse cutoff because arrowing past it is how you ASK for the rest, but a
/// walk that quietly ended at row ten would strand exactly the mail the band is
/// hiding. And the same ranking `body` renders, so the order you see is the
/// order you get.
///
/// Computed at KEY-PRESS AND CLICK TIME ONLY. It re-ranks the standing list,
/// which is the cost `SitrepCursor` exists to keep out of the render path.
@MainActor
private func eyesWalk(_ store: AppStore, weight: Double) -> [AttentionUpdate] {
    Ranking.rank(store.sitrep.standing, weight: weight)
}

/// The sitrep's hover + keyboard cursor, in an `@Observable` box rather than
/// `SitrepView`'s `@State`.
///
/// WHY IT IS NOT `@State`: a pointer resting anywhere over the page fires hover
/// enter/exit CONTINUOUSLY while a scroll drags rows under it, and a `@State`
/// write at the top of the tree re-renders the whole dashboard for every one of
/// them — re-ranking the standing list, rebuilding every row, re-measuring every
/// newsletter card. Held here, a write only invalidates views that actually READ
/// that field, and the keymap reads all of it at KEY-PRESS time rather than at
/// render time, so most of these writes reach no observer at all.
@MainActor
@Observable
final class SitrepCursor {
    /// Index into the VISIBLE rows only — collapsed-away rows are unreachable.
    var index = 0
    /// True only while the keyboard is driving; the focus fill renders on this.
    var kbActive = false
    /// True only while the pointer is over a row.
    var hovering = false
    /// Whether "for your eyes" is showing past the first `eyesVisible`.
    var expanded = false
    /// Hovered newsletter address — `e` marks that sender's whole window done,
    /// deferring to the For-your-eyes handler when nothing hovers.
    var newsletter: String?

    /// Point the cursor at a row the POINTER is over. EVERY WRITE IS GUARDED:
    /// `@Observable` notifies on assignment, not on change, so an unguarded
    /// `kbActive = false` here would invalidate every row on every mouse-move —
    /// which is exactly the cost this whole type exists to avoid.
    func hover(_ index: Int) {
        if !hovering { hovering = true }
        if kbActive { kbActive = false }
        if self.index != index { self.index = index }
    }
}

struct SitrepView: View {
    @Environment(AppStore.self) private var store
    @Environment(Prefs.self) private var prefs

    /// Hover + keyboard cursor. A reference, deliberately — see `SitrepCursor`.
    @State private var cursor = SitrepCursor()

    /// Below this page width the records rail stops being a pinned column and
    /// stacks under the left content, the whole page scrolling as one — a
    /// squeezed side-by-side starves the left column first, and that column is
    /// the work surface. At the window's minimum the page measures ~920, so
    /// the smallest windows always stack.
    private static let railBreakpoint: CGFloat = 960

    /// Measured page width the breakpoint is judged against. Starts infinite
    /// so the first pass lays out side-by-side rather than flashing stacked.
    @State private var pageWidth: CGFloat = .infinity

    /// The rows the cursor can reach, recomputed at KEY-PRESS time. The render
    /// path does NOT come through here — `body` ranks once into a `let` and
    /// passes the result down.
    private var reachable: [AttentionUpdate] {
        let ranked = Ranking.rank(store.sitrep.standing, weight: prefs.rankWeight)
        return cursor.expanded ? ranked : Array(ranked.prefix(eyesVisible))
    }

    var body: some View {
        // ONE rank per render. This was a computed property read FOUR times a
        // pass — twice through `visibleEyes`, and once more by each `onChange`,
        // whose value expression is re-evaluated on every body evaluation — and
        // every read re-sorted the entire standing list.
        let ranked = Ranking.rank(store.sitrep.standing, weight: prefs.rankWeight)
        let visible = cursor.expanded ? ranked : Array(ranked.prefix(eyesVisible))
        let overflow = ranked.count - eyesVisible
        let threadIds = ranked.map(\.thread_id)

        return VStack(spacing: 0) {
            masthead
            // ABOVE THE HERO, because it outranks it. The hero's question is
            // "what needs you today", and its answer is worthless while the
            // mailbox behind it has been frozen since Tuesday. This is the
            // landing page, so it is where somebody wondering why nothing has
            // arrived actually looks.
            if store.gmailDisconnected {
                GmailDisconnectedBanner()
                    .padding(.horizontal, 28)
                    .padding(.top, 14)
            }
            // THE HERO STAYS PUT. It sits above BOTH columns, so it cannot scroll
            // with one of them and hold still for the other; and "one item needs
            // you today" is the page's standing answer, which is worth keeping on
            // screen rather than scrolling away first.
            // The 18px here is a hard standoff: scrollShadowRoom's top fade
            // reaches zero at the viewport top, so this padding is the band
            // where scrolling content NEVER renders — without it the fade tail
            // touches the headline. The fade zone itself is the matching top
            // padding inside each scroll view's content.
            DashHero(standing: store.sitrep.standing)
                .padding(.horizontal, 28)
                .padding(.bottom, 18)

            if pageWidth < Self.railBreakpoint {
                // NARROW: one scroll, records under the work surface. The rail
                // zones are reference material, so they yield the fold to the
                // items that need acting on — but the status strip stays the
                // page's last line either way.
                ScrollView(.vertical) {
                    VStack(spacing: 16) {
                        leftZones(visible: visible, overflow: overflow)
                        // Same key as the pinned rail below: whichever layout is
                        // on screen is the one the tour's ring finds.
                        railCards.tourTarget(.records)
                        StatusStrip()
                    }
                    .padding(.top, 14)
                    .padding(.bottom, 28)
                }
                .scrollIndicators(.hidden)
                .scrollShadowRoom()
                .padding(.horizontal, 24)
            } else {
                HStack(alignment: .top, spacing: 18) {
                    // THE ONLY THING THAT SCROLLS WITH THE PAGE.
                    ScrollView(.vertical) {
                        VStack(spacing: 16) {
                            leftZones(visible: visible, overflow: overflow)
                            StatusStrip()
                        }
                        .padding(.top, 14)
                        .padding(.bottom, 28)
                    }
                    // No bar on either column. Two of them side by side read as a
                    // split pane rather than one page, and the left one rides the
                    // column boundary instead of the window edge.
                    .scrollIndicators(.hidden)
                    .scrollShadowRoom()
                    .frame(maxWidth: .infinity, alignment: .top)

                    // THE RECORDS RAIL IS PINNED. These are reference columns you
                    // read WHILE working the left side, so they must not leave with
                    // it. Its own scroll view rather than a plain stack so a rail
                    // taller than the window is still reachable —
                    // `.basedOnSize` is what keeps it completely inert until then,
                    // instead of rubber-banding against nothing.
                    ScrollView(.vertical) {
                        VStack(spacing: 14) {
                            railZones
                        }
                        .padding(.top, 14)
                        .padding(.bottom, 28)
                    }
                    .scrollBounceBehavior(.basedOnSize)
                    .scrollIndicators(.hidden)
                    .scrollShadowRoom()
                    .frame(width: 306)
                    // The COLUMN, not its content: a rail taller than the window
                    // would put the ring's bottom edge off screen.
                    .tourTarget(.records)
                }
                .padding(.horizontal, 24)
            }
        }
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { pageWidth = $0 }
        .keyContext(.sitrep)
        .keyBindings(.sitrep, bindings)
        // Refreshes underneath what is already on screen — rows are never
        // cleared first, so a revisit paints instantly and updates in place.
        .task { await store.refreshZones() }
        // Warm the visible top-10 immediately and trickle the collapsed
        // remainder, so a long list never stampedes the daemon. Prefetch dedupes
        // in-flight and fresh entries, so re-runs are near-free.
        .onChange(of: threadIds) { _, ids in
            ThreadPrefetch.shared.warm(ids, immediate: eyesVisible, spacing: .milliseconds(150))
        }
        .onAppear {
            ThreadPrefetch.shared.warm(
                threadIds, immediate: eyesVisible, spacing: .milliseconds(150))
        }
        .onChange(of: visible.count) { _, count in
            cursor.index = max(0, min(cursor.index, max(0, count - 1)))
        }
    }

    // MARK: - columns

    /// The work-surface zones, shared by both layouts. The status strip is NOT
    /// here — each layout places it last on the page itself.
    @ViewBuilder
    private func leftZones(visible: [AttentionUpdate], overflow: Int) -> some View {
        if !store.sitrep.standing.isEmpty {
            forYourEyes(visible: visible, overflow: overflow)
                .tourTarget(.eyes)
        }
        NewslettersZone(
            newsletters: Newsletters.prune(
                store.zones.newsletters, resolved: store.resolvedIds),
            cursor: cursor
        )
        .tourTarget(.newsletters)
    }

    /// The records zones as the pinned rail shows them: full-width rows.
    @ViewBuilder
    private var railZones: some View {
        CalendarZone()
        ShipmentsZone()
        BankingZone()
        ReceiptsZone()
    }

    /// The records as HALF-WIDTH cards for the stacked layout. Two top-aligned
    /// columns rather than a LazyVGrid: a grid row takes its tallest cell's
    /// height, and these cards are never the same height, so a grid opens
    /// gaps that independent columns simply pack.
    private var railCards: some View {
        HStack(alignment: .top, spacing: 16) {
            VStack(spacing: 16) {
                CalendarZone()
                BankingZone()
            }
            .frame(maxWidth: .infinity)
            VStack(spacing: 16) {
                ShipmentsZone()
                ReceiptsZone()
            }
            .frame(maxWidth: .infinity)
        }
    }

    // MARK: - masthead

    private var masthead: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 7) {
                Text("passband")
                    .font(Typo.serif(19, weight: .medium))
                    .foregroundStyle(Palette.ink)
                Text("sitrep")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .textCase(.uppercase)
            }
            Spacer(minLength: 12)
            IngestIndicator()
            RetriageButton()
            if needNow > 0 {
                HStack(spacing: 5) {
                    Circle().fill(Palette.danger).frame(width: 5, height: 5)
                    Text("\(needNow) need you now")
                }
                .font(Typo.chip)
                .foregroundStyle(Palette.danger)
                .padding(.horizontal, 9)
                .padding(.vertical, 3)
                .glassCapsule(tint: Palette.danger.opacity(0.18), interactive: false)
            }
            Text(Fmt.todayStamp())
                .font(Typo.num(11, weight: .medium))
                .foregroundStyle(Palette.inkFaint)
            SyncLabel()
        }
        .padding(.horizontal, 24)
        // THE TOP BAR. The wordmark sits on the traffic lights' line rather than
        // under it, which is why this is a fixed height and not padding: the
        // rail beside it starts at the same line, and the two only read as one
        // bar if neither can drift.
        .frame(height: TopBar.height)
    }

    /// Obligations that are overdue or due by end of today — the "need you" set.
    private var needNow: Int { Self.needTodayCount(store.sitrep.standing) }


    static func needTodayCount(_ items: [AttentionUpdate], now: Date = Date()) -> Int {
        let endOfDay = Calendar.current.date(
            bySettingHour: 23, minute: 59, second: 59, of: now) ?? now
        return items.filter { u in
            guard let t = Fmt.date(u.deadline) else { return false }
            return t <= endOfDay
        }.count
    }

    // MARK: - (a) for your eyes

    /// The standing band, ranked: dated obligations AND live correspondence
    /// (threads the reader has written in, senders they have written to). A row
    /// here is therefore no promise of a deadline — `Ranking` scores a dateless
    /// row at urgency 0, which is what keeps the real dates at the top.
    private func forYourEyes(visible: [AttentionUpdate], overflow: Int) -> some View {
        ZoneCard(
            symbol: "eye", title: "For your eyes", count: store.sitrep.standing.count
        ) {
            VStack(spacing: 1) {
                ForEach(Array(visible.enumerated()), id: \.element.id) { i, u in
                    // No closures passed down: a stored closure is never equal to
                    // last render's, so handing rows their actions that way meant
                    // SwiftUI could not skip a single one when the parent redrew.
                    ObligationRow(update: u, index: i, cursor: cursor)
                }
                if overflow > 0 { expander(overflow) }
            }
            // Only LEAVING the zone ends the hover — crossing between rows never
            // fires a parent's hover, so a sweep still can't flicker actionable
            // off and on. This was `.onContinuousHover`, which delivers every
            // mouse-moved event over the whole zone; it only ever needed .ended,
            // and the rest was a stream of hit-tests during the exact scroll
            // this zone is supposed to stay out of the way of.
            .onHover { over in
                if !over, cursor.hovering { cursor.hovering = false }
            }
        }
    }

    /// Shared by the button and by arrowing off the end of the collapsed list,
    /// so both openings look like the same gesture.
    private static let expandAnimation: Animation = .smooth(duration: 0.28)

    private func expander(_ overflow: Int) -> some View {
        Button {
            withAnimation(Self.expandAnimation) { cursor.expanded.toggle() }
        } label: {
            Text(cursor.expanded ? "show less" : "\(overflow) more")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaint)
                .padding(.horizontal, 11)
                .padding(.vertical, 4)
        }
        .buttonStyle(.glass)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, 6)
    }


    // MARK: - keymap

    /// Action keys are inert unless something is actually highlighted — either
    /// the keyboard cursor or a live hover. Read from HANDLERS only, never from
    /// `body`, which is what keeps these fields free to change on hover.
    private var eyesActionable: Bool { cursor.kbActive || cursor.hovering }

    private var bindings: [KeyBinding] {
        [
            KeyBinding("j", "next item") { moveEyes(+1) },
            KeyBinding("k", "prev item") { moveEyes(-1) },
            KeyBinding("ArrowDown", "next item") { moveEyes(+1) },
            KeyBinding("ArrowUp", "prev item") { moveEyes(-1) },
            KeyBinding("d", "mark done") {
                guard eyesActionable, let u = reachable[safe: cursor.index] else { return }
                Task { await Actions.done(u) }
            },
            // `e` first tries the hovered newsletter card; with nothing hovered
            // it DECLINES so the for-your-eyes done handler runs instead.
            KeyBinding(declining: "e", "mark done") {
                if let addr = cursor.newsletter,
                    let nl = store.zones.newsletters.first(where: { $0.address == addr })
                {
                    Task { await markNewsletterDone(nl) }
                    return true
                }
                guard eyesActionable, let u = reachable[safe: cursor.index] else { return false }
                Task { await Actions.done(u) }
                return true
            },
            KeyBinding("Enter", "open email") {
                guard eyesActionable, let u = reachable[safe: cursor.index] else { return }
                store.openThread(u.thread_id, queue: eyesWalk(store, weight: prefs.rankWeight))
            },
            // Same guard as every other verb here — inert unless a row is
            // actually highlighted. Reply opens the email and composes inside it,
            // and hands over the same walk Enter does, so `E` keeps working from
            // inside the reader once the reply is away.
            KeyBinding("r", "reply") {
                guard eyesActionable, let u = reachable[safe: cursor.index] else { return }
                Actions.reply(u, queue: eyesWalk(store, weight: prefs.rankWeight))
            },
            KeyBinding("v", "fix triage") {
                guard eyesActionable, let u = reachable[safe: cursor.index] else { return }
                store.openTriageFix(
                    TriageFixTarget(
                        messageId: u.id, sender: u.sender, subject: u.one_line,
                        tier: .some(u.tier.rawValue)))
            },
            KeyBinding("h", "remind me later") {
                guard eyesActionable, let u = reachable[safe: cursor.index] else { return }
                store.openRemind(
                    RemindTarget(
                        messageId: u.id, sender: u.sender, subject: u.one_line,
                        remindAt: u.remind_at))
            },
            // The one verb here that is NOT about a highlighted row: a new message
            // needs nothing selected, so it skips the `eyesActionable` guard.
            KeyBinding("c", "new message") { store.openComposeNew() },
        ]
    }

    /// j/k hand the cursor back to the keyboard, which is what makes the focus
    /// glass reappear.
    private func moveEyes(_ delta: Int) {
        cursor.kbActive = true
        // Ranked once here rather than through `reachable` twice: the expander
        // check needs the full list and the cursor needs the visible one.
        let ranked = Ranking.rank(store.sitrep.standing, weight: prefs.rankWeight)
        let rows = cursor.expanded ? ranked : Array(ranked.prefix(eyesVisible))
        let next = cursor.index + delta

        // DOWN OFF THE END OF A COLLAPSED LIST OPENS IT and keeps going, landing
        // on the first newly-revealed row. Without this the expander is the one
        // control in the zone that only a mouse can reach, and the cursor just
        // stalls on the last row with no indication there is more beneath it.
        if delta > 0, next >= rows.count, !cursor.expanded, ranked.count > rows.count {
            withAnimation(Self.expandAnimation) { cursor.expanded = true }
            cursor.index = rows.count
            return
        }
        cursor.index = max(0, min(rows.count - 1, next))
    }

    private func markNewsletterDone(_ nl: Newsletter) async {
        // Bulk-resolve every aggregated update; one toast, optimistic drop.
        store.zones.newsletters.removeAll { $0.address == nl.address }
        do {
            for item in nl.items {
                try await APIClient.shared.setStatus(item.id, .done)
                // Record it as resolved, not just gone from this zone: the same
                // message can be sitting in a band or on the mail page, and this
                // path never went through removeFromBands.
                store.noteResolved(item.id)
                // Unpin as each one lands, not at the end: a throw partway
                // through still leaves the resolved ones released. No undo on
                // this path, so nothing re-pins.
                await ImageStore.shared.release(messageId: item.id)
            }
            store.pushToast(
                "done: \(SenderCache.resolved(nl.sender).displayName) (\(nl.items.count))", .info)
        } catch {
            store.pushToast("some marks failed; refresh to re-sync", .error)
        }
    }

}

/// MAIL LANDING, as a spinner in the masthead rather than a card on the board.
///
/// The signal is the `new` band — `surfaced_at IS NULL` — and "surfaced" means
/// the client has FETCHED the row: `get_updates` stamps the very page it
/// returns. So a freshly triaged message sits here for about one poll and is
/// then gone, having moved to wherever its tier belongs.
///
/// That one-poll window is exactly why this must not be a zone card. A card
/// announces arriving mail as anonymous sender chips a beat BEFORE the same
/// mail takes its real place in For-your-eyes — the same item, twice, under a
/// name derived from its sending domain. It is a transient STATUS, not a
/// destination, so it reads as one: no count of things to go look at, no click
/// target that vanishes while you reach for it.
private struct IngestIndicator: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        let count = store.sitrep.new.count
        if count > 0 {
            HStack(spacing: 5) {
                Image(systemName: "arrow.triangle.2.circlepath")
                    .font(.system(size: 10, weight: .semibold))
                    .symbolEffect(.rotate, isActive: true)
                Text(count == 1 ? "ingesting" : "ingesting \(count)")
            }
            .font(Typo.chip)
            .foregroundStyle(Palette.accent)
            .padding(.horizontal, 9)
            .padding(.vertical, 3)
            .glassCapsule(tint: Palette.accent.opacity(0.18), interactive: false)
            .help(
                count == 1
                    ? "a new email is being triaged; it lands in its band on the next refresh"
                    : "\(count) new emails are being triaged; they land in their bands on the next refresh"
            )
        }
    }
}

/// The masthead's freshness stamp, isolated ON PURPOSE: `lastRefresh` changes on
/// every 10s poll, so read inline in the dashboard body it would invalidate the
/// entire sitrep. Scoped here, a poll re-renders one line of text.
private struct SyncLabel: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        Text(label)
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaintest)
    }

    private var label: String {
        guard let last = store.lastRefresh else { return "syncing…" }
        let rel = Fmt.relAge(last)
        return (rel == "now" || rel.isEmpty) ? "synced just now" : "synced \(rel) ago"
    }
}

// MARK: - GREETING

/// "GOOD MORNING, BRAELYN" — the one line on either client that says the human's
/// own name, and the one place they can set it without going to Settings.
///
/// THE NAME IS A GUESS UNTIL IT ISN'T. A fresh install has no name at all, and
/// nobody opens Settings to introduce themselves, so the greeting used to read
/// as an anonymous "GOOD MORNING" forever. It is now seeded from the mailbox's
/// local part the first time the daemon names the account
/// (`Prefs.seedUserName`), and a seed is a claim about someone that they never
/// made — so it comes with a pencil, and the pencil edits IN PLACE rather than
/// routing to Settings, because the thing being corrected is right there.
///
/// The affordance is deliberately SINGLE-USE: it shows only while
/// `prefs.nameChosen` is false, and the first answer — typed here or into the
/// Settings row — retires it for good. A permanent edit button on a greeting is
/// chrome; this is a question the app owes the human exactly once.
///
/// Shared by both clients (the Mac's `DashHero`, the phone's hero), so the
/// nudge cannot ship on one surface and rot on the other.
struct GreetingLine: View {
    @Environment(Prefs.self) private var prefs

    /// The edit buffer. Never bound straight to the pref: a live binding would
    /// mark the name CHOSEN on the first keystroke, which is precisely the
    /// state this view exists to end deliberately.
    @State private var draft = ""
    @State private var editing = false
    @FocusState private var focused: Bool

    var body: some View {
        HStack(spacing: 6) {
            if editing {
                label(Self.greeting() + ",")
                field
            } else {
                label(
                    Self.greeting() + (prefs.userName.isEmpty ? "" : ", \(prefs.userName)"))
                if !prefs.nameChosen { pencil }
            }
        }
        #if os(macOS)
            // Esc closes this, like everything else. It has to be a BINDING and
            // not `.onExitCommand`: the app's key monitor sees the event first
            // and Escape is the one key it never suppresses for a focused text
            // field, so a surface that wants its own Escape must claim it.
            // Registered in `.sitrep` (which outranks `.global` in dispatch
            // order) and only while the editor is up.
            .keyBindings(
                .sitrep,
                editing
                    ? [KeyBinding("Escape", "cancel", allowInInput: true) { editing = false }] : []
            )
        #endif
    }

    private func label(_ text: String) -> some View {
        Text(text)
            .font(Typo.micro)
            .foregroundStyle(Palette.accent)
            .textCase(.uppercase)
            .tracking(0.6)
    }

    private var field: some View {
        TextField("your name", text: $draft)
            .textFieldStyle(.plain)
            .autocorrectionDisabled()
            .focused($focused)
            .font(Typo.micro)
            .foregroundStyle(Palette.accent)
            .tracking(0.6)
            .frame(maxWidth: 160)
            // AN OVERLAY, not a border: it has to say "this is a field" without
            // costing a single point of height, or the serif headline under it
            // would hop down and back every time the editor opens and closes.
            .overlay(alignment: .bottom) {
                Rectangle()
                    .fill(Palette.accent.opacity(0.4))
                    .frame(height: 1)
                    .offset(y: 3)
            }
            .onSubmit(commit)
            // BLUR CANCELS, which is the opposite of the Settings rows' "click
            // away saves" — and for the opposite reason. Saving on blur would
            // commit a half-typed name AND retire the pencil that is the only
            // way to see the mistake, so clicking away costs a retyped word
            // rather than a wrong name you have to hunt through Settings to fix.
            .onChange(of: focused) { _, nowFocused in
                if !nowFocused { editing = false }
            }
            .onAppear { focused = true }
    }

    private var pencil: some View {
        Button {
            draft = prefs.userName
            editing = true
        } label: {
            Image(systemName: "pencil")
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(Palette.accent.opacity(0.8))
                // Clickable past the glyph, but NOT past the line: a target
                // taller than the label it sits beside would push the serif
                // headline down by the height of a pencil nobody can see.
                .padding(3)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("edit your name")
        #if os(macOS)
            // Honest either way. With nothing seeded — a daemon too old to name
            // its mailbox — there is no guess to correct, only something to say.
            .help(
                prefs.userName.isEmpty
                    ? "say what this greeting should call you"
                    : "this is a guess from your email address; click to say what to call you"
            )
        #endif
    }

    /// Enter saves. An EMPTY box is a cancel, not an erasure: a name cleared
    /// from here would take the pencil with it and leave Settings as the only
    /// way back, which is a lot to charge for a stray Enter.
    private func commit() {
        let typed = draft.trimmed
        editing = false
        guard !typed.isEmpty else { return }
        // The setter is what records the name as CHOSEN — see Prefs.userName.
        prefs.userName = typed
    }

    static func greeting(now: Date = Date()) -> String {
        let h = Calendar.current.component(.hour, from: now)
        if h < 12 { return "Good morning" }
        if h < 18 { return "Good afternoon" }
        return "Good evening"
    }
}

// MARK: - DASH HERO

/// A greeting label plus one serif headline stating what needs the reader today.
/// The ONE place the display serif appears — held to a single line per screen so
/// it reads as voice rather than decoration.
private struct DashHero: View {
    let standing: [AttentionUpdate]

    private static let smallWords = [
        "Zero", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine",
    ]

    /// Spell small counts — a numeral in a display serif headline reads like a
    /// dashboard metric, which is the opposite of the intent.
    private static func spell(_ n: Int) -> String {
        (0..<smallWords.count).contains(n) ? smallWords[n] : String(n)
    }

    private var title: String {
        let today = SitrepView.needTodayCount(standing)
        let total = standing.count
        if today > 0 {
            return "\(Self.spell(today)) item\(today == 1 ? "" : "s") "
                + "need\(today == 1 ? "s" : "") you today."
        }
        if total > 0 {
            return "\(Self.spell(total)) item\(total == 1 ? "" : "s") on your plate."
        }
        return "You're all clear."
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            GreetingLine()
            Text(title)
                .font(Typo.hero(38))
                .foregroundStyle(Palette.ink)
                .lineSpacing(-2)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.top, 4)
        .padding(.bottom, 2)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

// MARK: - obligation row

private struct ObligationRow: View {
    @Environment(AppStore.self) private var store
    /// READ AT CLICK TIME ONLY, never in `body` — an Observable property read
    /// during a render pass would subscribe all 10 rows to the rank slider.
    @Environment(Prefs.self) private var prefs
    let update: AttentionUpdate
    let index: Int
    let cursor: SitrepCursor

    /// Hover, in a REFERENCE this body never reads — see `ObligationWash`. The
    /// row deliberately reads neither hover nor focus, so neither can relayout it.
    @State private var hover = RowHover()

    /// Best-effort money amount from an update's one_line ("$142.00"). Hand-
    /// scanned and memoized rather than regex: this runs for every visible row
    /// on every render, and Swift `Regex` is far too slow for that path.
    private var amount: String? { MoneyScan.amount(in: update.one_line) }

    /// Whether this row's TIER asserts a date. Reading the tier rather than the
    /// band is the point: the band admits dateless mail on its own terms, so
    /// only a tier that promised a date can be missing one.
    private var claimsDate: Bool { update.tier == .pastDue || update.tier == .deadline }

    var body: some View {
        let chip = Fmt.deadlineChip(update.deadline)
        let overdue = chip?.overdue ?? false

        // Click anywhere on the row opens the email; done is keyboard-only (e/d).
        // The walk rides along with the click too — how you opened an email must
        // not decide whether `E` inside it has anywhere to go.
        Button {
            cursor.index = index
            store.openThread(update.thread_id, queue: eyesWalk(store, weight: prefs.rankWeight))
        } label: {
            HStack(spacing: 9) {
                Avatar(sender: update.senderString, size: 22)
                Text(SenderCache.resolved(update.senderString).displayName)
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                    .layoutPriority(2)
                if update.hasAttachments {
                    Image(systemName: "paperclip")
                        .font(.system(size: 10))
                        .foregroundStyle(Palette.inkFaintest)
                }
                // The abstracted one-liner carries the meaning; it truncates first.
                Text(update.one_line)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkDim)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .frame(maxWidth: .infinity, alignment: .leading)
                if let amount {
                    Label(amount, systemImage: "receipt")
                        .font(Typo.num(11, weight: .medium))
                        .foregroundStyle(Palette.accentInk)
                        .labelStyle(.titleAndIcon)
                        .layoutPriority(2)
                }
                if let chip {
                    Chip(
                        text: chip.text, tone: overdue ? Palette.danger : Palette.warn,
                        filled: overdue
                    )
                    .layoutPriority(3)
                } else if claimsDate {
                    // Said ONLY where the tier asserts a date the row does not
                    // carry. Most of this zone is dateless by design — live
                    // correspondence — and stamping every one of those rows
                    // "no due date" annotates an omission they never had.
                    Text("no due date")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                        .help(update.field_reasons?.deadline ?? "")
                        .layoutPriority(3)
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        // A BACKGROUND, not a modifier on this body: a background is sized by its
        // primary content, so the wash repainting can never resize the row.
        .background {
            ObligationWash(
                index: index, cursor: cursor, hover: hover,
                tint: overdue ? Palette.danger : Palette.accent)
        }
        .overlay(alignment: .leading) {
            if overdue {
                RoundedRectangle(cornerRadius: 1)
                    .fill(Palette.danger)
                    .frame(width: 2)
                    .padding(.vertical, 5)
            }
        }
        .onHover { over in
            hover.on = over
            if over { cursor.hover(index) }
        }
    }
}

/// One row's hover, as a reference so the row's own body never reads it.
@MainActor
@Observable
private final class RowHover {
    var on = false
}

/// The row's selection + hover paint, as a LEAF placed in `.background`.
///
/// WHY IT IS NOT PART OF THE ROW'S BODY. A `sample` taken during a stuttering
/// scroll put the main thread overwhelmingly in stack SIZING —
/// `LayoutEngineBox.explicitAlignment`, `StackLayout.prioritize`,
/// `ViewDimensions.subscript` — and almost nowhere in view updates. So the cost
/// of a hover was never the re-render; it was the RELAYOUT the re-render
/// triggers. An obligation row is a stack of stacks carrying three
/// layout-priority bands and a `Label`, and re-running its body re-measures all
/// of that, then propagates up through the zone card and the column. A
/// newsletter card, which has neither priorities nor a Label, went smooth as
/// soon as its parent stopped redrawing; these rows did not, and this is why.
///
/// Down here nothing above can be resized: a background is sized by its primary
/// content, so repainting this is paint and nothing else.
private struct ObligationWash: View {
    let index: Int
    let cursor: SitrepCursor
    let hover: RowHover
    let tint: Color

    private static let radius: CGFloat = 9

    /// Short-circuits exactly as it did on the row: with the keyboard idle
    /// `cursor.index` is never read, so the index write every hover performs
    /// still reaches no observer.
    private var focused: Bool { cursor.kbActive && cursor.index == index }

    var body: some View {
        RoundedRectangle(cornerRadius: Self.radius, style: .continuous)
            .fill(
                focused
                    ? tint.opacity(SelectionTone.selected)
                    : (hover.on ? Palette.hairline.opacity(SelectionTone.hover) : .clear)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Self.radius, style: .continuous)
                    .strokeBorder(
                        focused ? tint.opacity(SelectionTone.border) : .clear, lineWidth: 1)
            )
    }
}

// MARK: - status strip

private struct StatusStrip: View {
    @Environment(AppStore.self) private var store
    @State private var refreshing = false

    var body: some View {
        HStack(spacing: 9) {
            ChromeChip(tone: Palette.inkDim, help: "check for new mail now") {
                guard !refreshing else { return }
                refreshing = true
                Task {
                    await SitrepPoller.shared.triggerMailRefresh()
                    refreshing = false
                }
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 10, weight: .semibold))
                        .symbolEffect(.rotate, isActive: refreshing)
                    Text(syncedLabel)
                }
                .font(Typo.micro)
            }
            .disabled(refreshing)

            if let cost = store.sitrep.stats?.stage2?.est_cost_usd_today {
                Text("triage: \(String(format: "$%.2f", cost)) today")
                    .font(Typo.num(11))
                    .foregroundStyle(Palette.inkFaintest)
                    .help("today's stage-2 triage cost estimate")
            }

            Spacer(minLength: 0)
        }
        .padding(.top, 2)
    }

    /// "synced 4m ago" / "synced just now" — never the nonsense "synced now ago".
    private var syncedLabel: String {
        let age =
            store.lastRefresh.map { Fmt.relAge($0) }
            ?? Fmt.relAge(store.sitrep.stats?.last_surfaced_at)
        return (age.isEmpty || age == "now") ? "synced just now" : "synced \(age) ago"
    }
}

// MARK: - dev re-triage button

/// DEV-MODE re-triage: renders nothing unless the developerMode pref is on.
/// Fires POST /client/retriage for the trailing 7 days and then hands the window
/// to `RetriageModal`, which blocks the app until the queues drain — the run
/// rewrites every tier on the board, so there is nothing here worth reading
/// while it happens. `busy` is the STORE's run, not a local flag: the modal
/// outlives this button (a re-triage kicked from the sitrep survives navigating
/// away), so the only honest source for "already going" is the run itself.
struct RetriageButton: View {
    @Environment(AppStore.self) private var store
    @Environment(Prefs.self) private var prefs

    private static let days = 7

    private var busy: Bool { store.retriage != nil }

    var body: some View {
        if prefs.developerMode {
            Button {
                guard !busy else { return }
                Task { await store.startRetriage(days: Self.days) }
            } label: {
                Label("re-triage 7d", systemImage: "arrow.trianglehead.2.clockwise")
                    .font(Typo.micro)
                    .symbolEffect(.rotate, isActive: busy)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 3)
            }
            // Text-only: an outlined pill would put the loudest shape on the
            // page around a dev control most readers never use.
            .buttonStyle(.textAction)
            .disabled(busy)
            .help(
                "dev: reset LLM verdicts for the last \(Self.days) days and re-run triage "
                    + "(rule-decided and sealed mail untouched)")
        }
    }
}

// MARK: - small helpers

extension Array {
    /// Bounds-checked read: a clamped-but-stale cursor index must never trap.
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}

/// A wrapping HStack, for chip rows. SwiftUI has no built-in flow layout.
struct FlowLayout: Layout {
    var spacing: CGFloat = 6

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let maxWidth = proposal.width ?? .infinity
        var x: CGFloat = 0
        var y: CGFloat = 0
        var rowHeight: CGFloat = 0
        for subview in subviews {
            let size = subview.sizeThatFits(.unspecified)
            if x + size.width > maxWidth, x > 0 {
                x = 0
                y += rowHeight + spacing
                rowHeight = 0
            }
            x += size.width + spacing
            rowHeight = max(rowHeight, size.height)
        }
        return CGSize(width: maxWidth == .infinity ? x : maxWidth, height: y + rowHeight)
    }

    func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()
    ) {
        var x = bounds.minX
        var y = bounds.minY
        var rowHeight: CGFloat = 0
        for subview in subviews {
            let size = subview.sizeThatFits(.unspecified)
            if x + size.width > bounds.maxX, x > bounds.minX {
                x = bounds.minX
                y += rowHeight + spacing
                rowHeight = 0
            }
            subview.place(at: CGPoint(x: x, y: y), proposal: ProposedViewSize(size))
            x += size.width + spacing
            rowHeight = max(rowHeight, size.height)
        }
    }
}
