// SITREP VIEW — the fully-abstracted dashboard. THE DEFAULT SURFACE ON LAUNCH.
//
// ZERO individual email rows: this is the situation report, not the mailbox.
//   a. FOR YOUR EYES — the top-10 standing items, ranked by a configurable
//      blend of urgency (time) + severity (importance). Rows: avatar + sender,
//      one-line + amount + due date. A quiet "{n} more" expands the full ranked
//      list in place. Actions: done (d/e), open (Enter), fix triage (v).
//   b. ATTENTION — aggregate only: "N new since <relative last check>" +
//      deduped sender chips. Click → Emails.
//   c. NEWSLETTERS — the rule-onboarding surface.
//   d. STATUS STRIP — last sync, today's triage cost, rules count.
//   right rail: CALENDAR · SHIPMENTS · BANKING · RECEIPTS.
//
// Minimal keymap in its own "sitrep" KeyContext: j/k move between the VISIBLE
// ranked rows, d/e mark done, Enter opens fullscreen, v corrects a wrong
// verdict. The global 1..5 nav works here too.
//
// SELECTION MODEL — the same one the inbox uses (owner call, 2026-07-24). There
// is NO persistent selection: the tinted focus glass renders only while the
// KEYBOARD is driving, and any mouse hover hides it, paints a cheap wash on the
// hovered row, and re-anchors the cursor so j/k continue from there. Action keys
// need kbActive OR a live hover, or nothing is highlighted and `e` quietly
// resolves row 0.
//
// This is also why hover does NOT drag the glass: `selectionGlass` is a
// conditional modifier, so flipping a row between its two branches re-creates
// that row's whole subtree (state lost, `.task` re-run). Doing that twice per
// row you cross made a mouse sweep visibly lag the cursor.
//
// GLASS: the whole dashboard is one GlassEffectContainer. That is what makes
// adjacent zones read as a single sheet of material that parts around them
// rather than four separate panes — and it is precisely the behavior CSS
// cannot produce, because backdrop-filter has no notion of neighbors.
//
// Ported from squelch-desktop/src/views/SitrepView.tsx.

import SwiftUI

/// How many ranked standing items show before the quiet "{n} more" expander.
private let eyesVisible = 10

struct SitrepView: View {
    @Environment(AppStore.self) private var store
    @Environment(Prefs.self) private var prefs
    @Namespace private var zoneGlass

    // For-your-eyes cursor (j/k), over the VISIBLE rows only — the keyboard
    // never reaches collapsed-away rows.
    @State private var eyesIndex = 0
    /// True only while the KEYBOARD is driving the cursor — see the selection
    /// model note in the header.
    @State private var eyesKbActive = false
    /// True only while the cursor is actually over a row.
    @State private var eyesHovering = false
    @State private var eyesExpanded = false
    @State private var rulesCount: Int?
    /// Hovered newsletter address — `e` while hovering marks that sender's
    /// window done, deferring to the For-your-eyes handler when nothing hovers.
    @State private var hoveredNewsletter: String?
    /// Fed from HERE rather than by the zone itself — see the note on
    /// NewslettersZone. This view is always mounted, so its `.task` always runs.
    @State private var newsletters: [Newsletter] = []

    private var ranked: [AttentionUpdate] {
        Ranking.rank(store.sitrep.standing, weight: prefs.rankWeight)
    }
    private var visibleEyes: [AttentionUpdate] {
        eyesExpanded ? ranked : Array(ranked.prefix(eyesVisible))
    }
    private var eyesOverflow: Int { ranked.count - eyesVisible }

    var body: some View {
        VStack(spacing: 0) {
            masthead
            // ONE scroll surface for the dashboard. The GlassEffectContainers
            // live INSIDE, one per column: a container sizes itself to what it
            // is offered, so wrapping the entire scrollable body in one gave
            // the ScrollView a content height it could not scroll. Per-column
            // containers are also where merging actually reads — glass merging
            // across a whole scrolling page is not a thing anyone can perceive.
            ScrollView(.vertical) {
                VStack(alignment: .leading, spacing: 18) {
                    DashHero(standing: store.sitrep.standing)
                        .padding(.horizontal, 4)

                    HStack(alignment: .top, spacing: 18) {
                        VStack(spacing: 16) {
                            if !store.sitrep.standing.isEmpty { forYourEyes }
                            if !store.sitrep.new.isEmpty { attentionZone }
                            NewslettersZone(
                                newsletters: $newsletters,
                                hovered: $hoveredNewsletter,
                                reload: { newsletters = await NewsletterFeed.load() })
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
        .task { await loadRulesCount() }
        .task {
            newsletters = await NewsletterFeed.load()
            // Warm every hero BEFORE the zone is scrolled to. Resolving them
            // lazily meant the decode landed while the scroll was in flight,
            // which is precisely when the main actor has no time to spare.
            HeroCache.shared.preload(newsletters.map(\.latestThreadId))
        }
        // PRELOAD every For-your-eyes email so opening one is instant. The
        // visible top-10 warm immediately; the collapsed remainder trickles so a
        // long list never stampedes the daemon. Prefetch dedupes in-flight and
        // fresh entries, so re-runs on re-rank or the 10s poll are near-free.
        .onChange(of: ranked.map(\.thread_id)) { _, ids in
            ThreadPrefetch.shared.warm(ids, immediate: eyesVisible, spacing: .milliseconds(150))
        }
        .onAppear {
            ThreadPrefetch.shared.warm(
                ranked.map(\.thread_id), immediate: eyesVisible, spacing: .milliseconds(150))
        }
        .onChange(of: visibleEyes.count) { _, count in
            eyesIndex = max(0, min(eyesIndex, max(0, count - 1)))
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

    private var forYourEyes: some View {
        ZoneCard(
            symbol: "eye", title: "For your eyes", count: store.sitrep.standing.count
        ) {
            GlassEffectContainer(spacing: 2) {
            VStack(spacing: 1) {
                ForEach(Array(visibleEyes.enumerated()), id: \.element.id) { i, u in
                    ObligationRow(
                        update: u,
                        focused: eyesKbActive && i == eyesIndex,
                        glassNamespace: zoneGlass,
                        onHover: {
                            eyesHovering = true
                            eyesKbActive = false
                            eyesIndex = i
                        },
                        onOpen: {
                            eyesIndex = i
                            store.openThread(u.thread_id)
                        })
                }
                if eyesOverflow > 0 {
                    Button {
                        withAnimation(.smooth(duration: 0.28)) { eyesExpanded.toggle() }
                    } label: {
                        Text(eyesExpanded ? "show less" : "\(eyesOverflow) more")
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaint)
                            .padding(.horizontal, 11)
                            .padding(.vertical, 4)
                    }
                    .buttonStyle(.glass)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 6)
                }
            }
            // Leaving the zone entirely ends the hover; moving BETWEEN rows keeps
            // it (the phase stays .active), so a sweep never flickers actionable
            // off and on.
            .onContinuousHover { phase in
                if case .ended = phase { eyesHovering = false }
            }
            }
        }
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
    /// the keyboard cursor or a live hover.
    private var eyesActionable: Bool { eyesKbActive || eyesHovering }

    private var bindings: [KeyBinding] {
        [
            KeyBinding("j", "next item") { moveEyes(+1) },
            KeyBinding("k", "prev item") { moveEyes(-1) },
            KeyBinding("ArrowDown", "next item") { moveEyes(+1) },
            KeyBinding("ArrowUp", "prev item") { moveEyes(-1) },
            KeyBinding("d", "mark done") {
                guard eyesActionable, let u = visibleEyes[safe: eyesIndex] else { return }
                Task { await Actions.done(u) }
            },
            // `e` first tries the hovered newsletter card (bulk-resolve that
            // sender's window); with nothing hovered it DECLINES and the
            // for-your-eyes done handler below runs instead.
            KeyBinding(declining: "e", "mark done") {
                if let addr = hoveredNewsletter,
                    let nl = newsletters.first(where: { $0.address == addr })
                {
                    Task { await markNewsletterDone(nl) }
                    return true
                }
                guard eyesActionable, let u = visibleEyes[safe: eyesIndex] else { return false }
                Task { await Actions.done(u) }
                return true
            },
            KeyBinding("Enter", "open email") {
                guard eyesActionable, let u = visibleEyes[safe: eyesIndex] else { return }
                store.openThread(u.thread_id)
            },
            // `v` used to be a second way to open the email, which Enter already
            // does. Repurposed for the triage-fix palette: this is the surface
            // where a wrong verdict is most visible, so it is where correcting
            // it should be cheapest.
            KeyBinding("v", "fix triage") {
                guard eyesActionable, let u = visibleEyes[safe: eyesIndex] else { return }
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
        eyesKbActive = true
        eyesIndex = max(0, min(visibleEyes.count - 1, eyesIndex + delta))
    }

    private func markNewsletterDone(_ nl: Newsletter) async {
        // Bulk-resolve every aggregated update; one toast, optimistic drop.
        newsletters.removeAll { $0.address == nl.address }
        do {
            for item in nl.items { try await APIClient.shared.setStatus(item.id, .done) }
            store.pushToast(
                "done: \(SenderCache.resolved(nl.sender).displayName) (\(nl.items.count))", .info)
        } catch {
            store.pushToast("some marks failed; refresh to re-sync", .error)
        }
    }

    private func loadRulesCount() async {
        // Non-fatal: just omit the chip. Never surface the token/url.
        rulesCount = try? await APIClient.shared.listRules().count
    }
}

/// The masthead's freshness stamp, isolated in its own view ON PURPOSE.
///
/// `lastRefresh` changes on every 10s poll. Read inline in the dashboard's body
/// it would invalidate the ENTIRE sitrep — hero, zones, rows, rails — six times
/// a minute. Scoped here, a poll re-renders one line of text.
private struct SyncLabel: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        Text(label)
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaintest)
    }

    private var label: String {
        guard let last = store.lastRefresh else { return "syncing…" }
        let rel = Fmt.relAge(ISO8601DateFormatter().string(from: last))
        return (rel == "now" || rel.isEmpty) ? "synced just now" : "synced \(rel) ago"
    }
}

// MARK: - DASH HERO

/// The editorial centerpiece: a tiny greeting label with the human's name, and
/// a big Newsreader-serif headline stating what needs them today. This is the
/// ONE place the display serif appears — keeping it to a single line per screen
/// is what makes it read as voice rather than decoration.
private struct DashHero: View {
    @Environment(Prefs.self) private var prefs
    let standing: [AttentionUpdate]

    private static let smallWords = [
        "Zero", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine",
    ]

    /// Spell small counts ("Two obligations…") — a numeral in a display serif
    /// headline reads like a dashboard metric, which is the opposite of the
    /// intent.
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
    let update: AttentionUpdate
    let focused: Bool
    let glassNamespace: Namespace.ID
    let onHover: () -> Void
    let onOpen: () -> Void

    /// Drives the cheap hover wash. Kept LOCAL on purpose: it is the row's own
    /// feedback, so it never depends on the dashboard re-rendering.
    @State private var hovering = false

    /// Best-effort money amount pulled from an update's one_line ("$142.00").
    /// Scanned by hand rather than by regex, and memoized: this is evaluated for
    /// every visible row on every render, and Swift `Regex` is far too slow to
    /// sit in that path.
    private var amount: String? { MoneyScan.amount(in: update.one_line) }

    var body: some View {
        let chip = Fmt.deadlineChip(update.deadline)
        let overdue = chip?.overdue ?? false

        // Click anywhere on the row opens the email; done is keyboard-only
        // (e/d), same as the inbox — no per-row checkmark button.
        Button(action: onOpen) {
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
        // The KEYBOARD-focused row is glass, tinted by urgency — overdue items
        // carry the danger tint IN the material rather than as a flat wash on top
        // of it. A hovered row gets the wash instead: same branch as a plain row,
        // so sweeping the mouse only recolors a fill.
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
            if over { onHover() }
        }
    }
}

// MARK: - sender chips

private struct SenderChips: View {
    let items: [AttentionUpdate]

    /// Dedupe by sender, keep first occurrence; cap so the zone stays glanceable.
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
                // A plain capsule, NOT glass: this row can hold a dozen chips,
                // and a dozen live glass passes costs real frames while reading
                // no better than a tinted pill at this size.
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
            Button {
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
                .padding(.horizontal, 9)
                .padding(.vertical, 4)
            }
            .buttonStyle(.glass)
            .foregroundStyle(Palette.inkDim)
            .disabled(refreshing)
            .help("check for new mail now")

            if let cost = store.sitrep.stats?.stage2?.est_cost_usd_today {
                Text("triage: \(String(format: "$%.2f", cost)) today")
                    .font(Typo.num(11))
                    .foregroundStyle(Palette.inkFaintest)
                    .help("today's stage-2 triage cost estimate")
            }

            if let rulesCount {
                Button {
                    store.setView(.rules)
                } label: {
                    Label(
                        "\(rulesCount) \(rulesCount == 1 ? "rule" : "rules")",
                        systemImage: "slider.horizontal.3"
                    )
                    .font(Typo.micro)
                    .padding(.horizontal, 9)
                    .padding(.vertical, 4)
                }
                .buttonStyle(.glass)
                .foregroundStyle(Palette.inkDim)
                .help("sender rules")
            }
            Spacer(minLength: 0)
        }
        .padding(.top, 2)
    }

    /// "synced 4m ago" / "synced just now". The desktop build concatenated
    /// unconditionally and produced the nonsense "synced now ago" whenever the
    /// age token was "now"; the masthead already phrased it correctly, so this
    /// matches the masthead rather than reproducing the typo.
    private var syncedLabel: String {
        let iso = store.lastRefresh.map { ISO8601DateFormatter().string(from: $0) }
        let age = Fmt.relAge(iso ?? store.sitrep.stats?.last_surfaced_at)
        return (age.isEmpty || age == "now") ? "synced just now" : "synced \(age) ago"
    }
}

// MARK: - dev re-triage button

/// DEV-MODE re-triage. Renders nothing unless the developerMode pref is on.
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
            .buttonStyle(.glass)
            .foregroundStyle(Palette.inkFaint)
            .disabled(busy)
            .help(
                "dev: reset LLM verdicts for the last \(Self.days) days and re-run triage "
                    + "(rule-decided and sealed mail untouched)")
        }
    }
}

// MARK: - small helpers

extension Array {
    /// Bounds-checked read — the ported code indexes cursors constantly and a
    /// clamped-but-stale index must never trap.
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
