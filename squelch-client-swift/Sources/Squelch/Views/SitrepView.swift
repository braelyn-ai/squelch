// SITREP VIEW — the abstracted dashboard and default surface on launch: ranked
// standing items, an attention aggregate, newsletters, a status strip and the
// records rail. Owns the "sitrep" KeyContext. No persistent selection — the
// focus glass renders only while the keyboard drives, and hover must NOT drag it:
// `selectionGlass` is conditional, so flipping branches re-creates the subtree.

import SwiftUI

/// How many ranked standing items show before the quiet "{n} more" expander.
private let eyesVisible = 10

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
    /// True only while the keyboard is driving; the focus glass renders on this.
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
    @Namespace private var zoneGlass

    /// Hover + keyboard cursor. A reference, deliberately — see `SitrepCursor`.
    @State private var cursor = SitrepCursor()

    /// Zone data lives in the STORE, not in `@State`: `@State` is discarded on
    /// navigate-away, so a revisit would show empty cards until refetch.
    private var rulesCount: Int? { store.zones.rulesCount }

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
            // GlassEffectContainers live INSIDE this scroll view, one per
            // column: a container sizes itself to what it is offered, so
            // wrapping the whole scrollable body in one leaves the ScrollView
            // with a content height it cannot scroll.
            ScrollView(.vertical) {
                VStack(alignment: .leading, spacing: 18) {
                    DashHero(standing: store.sitrep.standing)
                        .padding(.horizontal, 4)

                    HStack(alignment: .top, spacing: 18) {
                        VStack(spacing: 16) {
                            if !store.sitrep.standing.isEmpty {
                                forYourEyes(visible: visible, overflow: overflow)
                            }
                            if !store.sitrep.new.isEmpty { attentionZone }
                            NewslettersZone(
                                newsletters: store.zones.newsletters, cursor: cursor)
                            StatusStrip(rulesCount: rulesCount)
                        }
                        .frame(maxWidth: .infinity, alignment: .top)

                        VStack(spacing: 14) {
                            CalendarZone()
                            ShipmentsZone()
                            BankingZone()
                            ReceiptsZone()
                        }
                        .frame(width: 306)
                    }
                }
                .padding(.horizontal, 24)
                .padding(.bottom, 28)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
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

    // MARK: - masthead

    private var masthead: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 7) {
                Text("squelch")
                    .font(Typo.serif(19, weight: .medium))
                    .foregroundStyle(Palette.ink)
                Text("sitrep")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .textCase(.uppercase)
            }
            Spacer(minLength: 12)
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
        .padding(.top, 16)
        .padding(.bottom, 12)
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

    private func forYourEyes(visible: [AttentionUpdate], overflow: Int) -> some View {
        ZoneCard(
            symbol: "eye", title: "For your eyes", count: store.sitrep.standing.count
        ) {
            VStack(spacing: 1) {
                eyesRows(visible)
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

    /// THE GLASS CONTAINER IS MOUNTED ONLY WHILE THE KEYBOARD DRIVES.
    ///
    /// Its whole job is morphing the focus glass from row to row, and that glass
    /// exists only when `kbActive` — with the mouse driving, every row takes
    /// `selectionGlass`'s plain-background branch and the container holds no
    /// glass at all. Mounted unconditionally it still re-coordinates its
    /// descendants whenever one of them changes, and a hovered row changes. THAT
    /// is the entire difference between a mouse sweep over this zone, which
    /// stuttered, and the identical sweep over the newsletter grid, which did
    /// not: same per-row re-render, only one of them inside a glass container.
    ///
    /// Yes, the branch is two view identities and flipping it rebuilds the rows.
    /// That is exactly the trade `selectionGlass` already documents — fine once,
    /// when the keyboard takes over; ruinous once per mouse-move.
    @ViewBuilder
    private func eyesRows(_ visible: [AttentionUpdate]) -> some View {
        if cursor.kbActive {
            GlassEffectContainer(spacing: 2) { rowStack(visible) }
        } else {
            rowStack(visible)
        }
    }

    private func rowStack(_ visible: [AttentionUpdate]) -> some View {
        VStack(spacing: 1) {
            ForEach(Array(visible.enumerated()), id: \.element.id) { i, u in
                // No closures passed down: a stored closure is never equal to
                // last render's, so handing rows their actions that way meant
                // SwiftUI could not skip a single one when the parent redrew.
                ObligationRow(update: u, index: i, cursor: cursor, glassNamespace: zoneGlass)
            }
        }
    }

    /// Outside the container on purpose: it carries its own glass and no
    /// `glassEffectID`, so it was never morphing with anything, and leaving it
    /// in would let it merge with the top row in one branch and not the other.
    private func expander(_ overflow: Int) -> some View {
        Button {
            withAnimation(.smooth(duration: 0.28)) { cursor.expanded.toggle() }
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

    // MARK: - (b) attention

    private var attentionZone: some View {
        Button {
            store.setView(.emails)
        } label: {
            ZoneCard(
                symbol: "bell", title: "Attention",
                trailing: AnyView(
                    Image(systemName: "arrow.up.right")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Palette.inkFaintest))
            ) {
                VStack(alignment: .leading, spacing: 9) {
                    HStack(spacing: 4) {
                        Text("\(store.sitrep.new.count)")
                            .font(.system(size: 13, weight: .bold))
                            .foregroundStyle(Palette.ink)
                        Text(
                            "new since \(Fmt.lastChecked(store.sitrep.stats?.last_surfaced_at))"
                        )
                        .font(Typo.rowSub)
                        .foregroundStyle(Palette.inkDim)
                    }
                    SenderChips(items: store.sitrep.new)
                }
            }
        }
        .buttonStyle(.plain)
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
                store.openThread(u.thread_id)
            },
            KeyBinding("v", "fix triage") {
                guard eyesActionable, let u = reachable[safe: cursor.index] else { return }
                store.openTriageFix(
                    TriageFixTarget(
                        messageId: u.id, sender: u.sender, subject: u.one_line,
                        tier: .some(u.tier.rawValue)))
            },
        ]
    }

    /// j/k hand the cursor back to the keyboard, which is what makes the focus
    /// glass reappear.
    private func moveEyes(_ delta: Int) {
        cursor.kbActive = true
        cursor.index = max(0, min(reachable.count - 1, cursor.index + delta))
    }

    private func markNewsletterDone(_ nl: Newsletter) async {
        // Bulk-resolve every aggregated update; one toast, optimistic drop.
        store.zones.newsletters.removeAll { $0.address == nl.address }
        do {
            for item in nl.items {
                try await APIClient.shared.setStatus(item.id, .done)
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

// MARK: - DASH HERO

/// A greeting label plus one serif headline stating what needs the reader today.
/// The ONE place the display serif appears — held to a single line per screen so
/// it reads as voice rather than decoration.
private struct DashHero: View {
    @Environment(Prefs.self) private var prefs
    let standing: [AttentionUpdate]

    private static let smallWords = [
        "Zero", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine",
    ]

    /// Spell small counts — a numeral in a display serif headline reads like a
    /// dashboard metric, which is the opposite of the intent.
    private static func spell(_ n: Int) -> String {
        (0..<smallWords.count).contains(n) ? smallWords[n] : String(n)
    }

    private static func greeting(now: Date = Date()) -> String {
        let h = Calendar.current.component(.hour, from: now)
        if h < 12 { return "Good morning" }
        if h < 18 { return "Good afternoon" }
        return "Good evening"
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
            Text(Self.greeting() + (prefs.userName.isEmpty ? "" : ", \(prefs.userName)"))
                .font(Typo.micro)
                .foregroundStyle(Palette.accent)
                .textCase(.uppercase)
                .tracking(0.6)
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
    let update: AttentionUpdate
    let index: Int
    let cursor: SitrepCursor
    let glassNamespace: Namespace.ID

    /// Drives the hover wash. LOCAL on purpose: the row's own feedback never
    /// depends on the dashboard re-rendering.
    @State private var hovering = false

    /// THE SHORT-CIRCUIT IS LOAD-BEARING. Observation records only the properties
    /// a body actually TOUCHED, so with the keyboard idle this never reads
    /// `cursor.index` — and the index write every hover event performs then has
    /// no observer to invalidate. Collapsing this to a stored `focused` flag
    /// passed from the parent is what used to make a scroll redraw the page.
    private var focused: Bool { cursor.kbActive && cursor.index == index }

    /// Best-effort money amount from an update's one_line ("$142.00"). Hand-
    /// scanned and memoized rather than regex: this runs for every visible row
    /// on every render, and Swift `Regex` is far too slow for that path.
    private var amount: String? { MoneyScan.amount(in: update.one_line) }

    var body: some View {
        let chip = Fmt.deadlineChip(update.deadline)
        let overdue = chip?.overdue ?? false

        // Click anywhere on the row opens the email; done is keyboard-only (e/d).
        Button {
            cursor.index = index
            store.openThread(update.thread_id)
        } label: {
            HStack(spacing: 9) {
                Avatar(sender: update.sender, size: 22)
                Text(SenderCache.resolved(update.sender).displayName)
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
                } else {
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
        // The KEYBOARD-focused row is glass tinted by urgency; a hovered row gets
        // a wash on the SAME branch as a plain row, so a mouse sweep only
        // recolors a fill instead of rebuilding the row.
        .selectionGlass(
            focused, hovering: hovering, tint: overdue ? Palette.danger : Palette.accent,
            id: "eyes-selection", in: glassNamespace)
        .overlay(alignment: .leading) {
            if overdue {
                RoundedRectangle(cornerRadius: 1)
                    .fill(Palette.danger)
                    .frame(width: 2)
                    .padding(.vertical, 5)
            }
        }
        .onHover { over in
            hovering = over
            if over { cursor.hover(index) }
        }
    }
}

// MARK: - sender chips

private struct SenderChips: View {
    let items: [AttentionUpdate]

    /// Dedupe by sender, keeping the first occurrence.
    private var chips: [AttentionUpdate] {
        var seen = Set<String>()
        var out: [AttentionUpdate] = []
        for u in items {
            let key = u.sender.lowercased()
            if seen.contains(key) { continue }
            seen.insert(key)
            out.append(u)
        }
        return out
    }

    var body: some View {
        let shown = Array(chips.prefix(12))
        let extra = chips.count - shown.count
        FlowLayout(spacing: 6) {
            ForEach(shown) { u in
                HStack(spacing: 5) {
                    Avatar(sender: u.sender, size: 16)
                    Text(SenderCache.resolved(u.sender).displayName)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkDim)
                        .lineLimit(1)
                }
                .padding(.horizontal, 7)
                .padding(.vertical, 3)
                // A plain capsule, NOT glass: a dozen live glass passes costs
                // real frames and reads no better than a tinted pill this size.
                .background(Capsule().fill(Palette.hairline.opacity(0.7)))
                .help(u.sender)
            }
            if extra > 0 {
                Text("+\(extra) more")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .padding(.horizontal, 7)
                    .padding(.vertical, 4)
            }
        }
    }
}

// MARK: - status strip

private struct StatusStrip: View {
    @Environment(AppStore.self) private var store
    let rulesCount: Int?
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

            if let rulesCount {
                ChromeChip(
                    text: "\(rulesCount) \(rulesCount == 1 ? "rule" : "rules")",
                    icon: "slider.horizontal.3",
                    tone: Palette.inkDim, help: "sender rules"
                ) { store.setView(.rules) }
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
/// Fires POST /client/retriage for the trailing 7 days and toasts the count.
struct RetriageButton: View {
    @Environment(AppStore.self) private var store
    @Environment(Prefs.self) private var prefs
    @State private var busy = false

    private static let days = 7

    var body: some View {
        if prefs.developerMode {
            Button {
                guard !busy else { return }
                busy = true
                Task {
                    do {
                        let result = try await APIClient.shared.retriage(.days(Self.days))
                        store.pushToast(
                            result.reset > 0
                                ? "re-triaging \(result.reset) email\(result.reset == 1 ? "" : "s") (last \(Self.days)d)…"
                                : "nothing to re-triage in the window", .info)
                    } catch {
                        store.pushToast(errText(error, "re-triage failed"), .error)
                    }
                    busy = false
                }
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
