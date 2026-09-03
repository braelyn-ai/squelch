// Audit log: what the agent (/mcp door) and this app (/client door) have done.
// GET /client/audit, newest first, read-only; a row expands on click.
// Mail-derived sender/subject render as text only, never markup. Sealed
// sender/subject are deliberately shown here (as on the Auth page), sealed
// content never is — see docs/SECURITY.md. An undo lands as a new audit row.
//
// IT IS A SETTINGS CARD, NOT A ROUTED PAGE. It used to be the fifth rail
// destination, which was a whole window spent on a ledger most people read
// after something surprised them — and reading it is the same act as reading
// what the app is allowed to do, which is what Settings is. So it composes like
// every other section here (`AuditSection`, filed under its own Settings
// category) and is reached with `A` from the mail list, ⌘, , or by searching
// settings for "who did this".
//
// THE SHAPE: a time gutter down the left, the ledger in the middle, the filters
// down the right. A ledger is read two ways — "what happened around then" and
// "show me every X" — and those are the two edges. Times lead each line because
// scanning for when is the more common of the two and a ragged right-hand
// timestamp is not a column you can scan at all.
//
// THE LEDGER PAGES ITSELF. It keeps no ScrollView of its own — the settings
// pane is already scrolling, and a scroller inside a scroller traps the wheel —
// so it is a LazyVStack in that pane's scroll, which is what makes the rows
// build as they come into view instead of all at once. A sentinel under the
// last row asks for the next page when it appears, so reaching the bottom IS
// the request; there is no button, and nothing is fetched for rows nobody has
// scrolled to.
//
// The paging itself is a GROWING LIMIT rather than a cursor, because
// `/client/audit` takes only `limit` and answers with the newest N. Asking for
// a bigger N and keeping the answer is therefore correct against the daemon as
// it stands, and honest at the end: a page that comes back shorter than it was
// asked for is the end of the log. The daemon clamps at 500 (MAX_LIMIT), so
// that is where the ledger stops.

import SwiftUI

struct AuditSection: View {
    @Environment(AppStore.self) private var store

    @State private var auditState: Loadable<[AuditEntry]> = .loading
    /// The one open row, by entry id. ONE, not a set: opening a row is reading
    /// it, and two rows open at once turns a ledger into a ragged stack where
    /// the eye has to work out which detail line belongs to which action.
    @State private var openId: Int?
    /// How many rows the last request asked the daemon for. Grows a page at a
    /// time as the bottom is reached; never shrinks, so a refresh re-pulls
    /// everything already read rather than yanking rows out from under it.
    @State private var wanted = AuditSection.pageSize
    /// The log has no more to give: the last page came back short, or the
    /// daemon's own clamp has been reached.
    @State private var exhausted = false

    // The filters. All of them start neutral, and neutral means "everything" —
    // an empty `verbs` is no verb filter rather than no rows, which is the one
    // place a facet list can lie about the ledger.
    @State private var query = ""
    @State private var who: WhoFilter = .all
    @State private var when: WhenFilter = .all
    @State private var verbs: Set<String> = []
    @State private var reversibleOnly = false

    private static let inboxLabel = "INBOX"
    /// Rows per page. Sized so the first pull fills a settings pane and a bit
    /// over, which is what makes the second page arrive before the reader has
    /// finished the first.
    private static let pageSize = 40
    /// squelch-api clamps `limit` to MAX_LIMIT; asking past it returns the same
    /// page for ever, so paging stops here whether or not the log is finished.
    private static let ceiling = 500

    /// Newest first by ts, falling back to id — never trust server ordering.
    private var rows: [AuditEntry] {
        (auditState.value ?? []).sorted { a, b in
            let ta = Fmt.date(a.ts)
            let tb = Fmt.date(b.ts)
            if let ta, let tb, ta != tb { return ta > tb }
            return a.id > b.id
        }
    }

    /// Everything except the verb facet, which is what the verb counts are
    /// counted over — a facet that counted itself would show every action at
    /// zero the moment one of them was picked.
    private var preVerb: [AuditEntry] {
        rows.filter {
            who.matches($0) && when.matches($0) && matchesQuery($0)
                && (!reversibleOnly || Self.undoFor($0) != nil)
        }
    }

    private var filtered: [AuditEntry] {
        preVerb.filter { verbs.isEmpty || verbs.contains(Self.actionVerb($0)) }
    }

    private var filtering: Bool {
        who != .all || when != .all || !verbs.isEmpty || reversibleOnly
            || !query.trimmed.isEmpty
    }

    var body: some View {
        // THE CARD IS THE LEDGER, AND ONLY THE LEDGER. Everything you can press
        // — the filters, the search, the reload — stands beside it in its own
        // column, because a control inside the pane it controls reads as one
        // more entry in the log. Out here the card is a document and the column
        // is the desk it sits on.
        //
        // A phone has one column, so they stack, and the controls go ABOVE the
        // ledger there: a control below the thing it controls is a control you
        // scroll past before you know it exists.
        #if os(macOS)
            HStack(alignment: .top, spacing: 18) {
                // The card takes everything the rail leaves it — SectionCard is
                // already `maxWidth: .infinity`, and the pane above hands this
                // section the window's full height, so the ledger runs from the
                // sub-nav to the filters and from the header to the floor.
                card
                // STATIONARY. It is outside the ledger's scroller by
                // construction, so picking a filter and reading the answer
                // never costs a scroll back up to change your mind.
                sidebar.frame(width: 200)
            }
            .frame(maxHeight: .infinity, alignment: .top)
            .task { await load(Self.pageSize) }
        #else
            VStack(alignment: .leading, spacing: 14) {
                sidebar
                card
            }
            .task { await load(Self.pageSize) }
        #endif
    }

    @ViewBuilder private var card: some View {
        SectionCard(label: "Audit log", note: countNote) {
            if auditState.value == nil, auditState.isLoading {
                BandNote("loading audit…")
            } else if let error = auditState.error, auditState.value == nil {
                BandNote(error)
            } else if rows.isEmpty {
                BandNote("No agent or app actions recorded yet.")
            } else {
                // THE ONLY THING ON THIS SCREEN THAT SCROLLS, on the Mac: the
                // rows move under a header and a filter rail that do not. The
                // phone keeps the pane's own scroller instead — one column, one
                // scroll, and a scroll view inside a scroll view on a touch
                // screen is a fight over every drag.
                #if os(macOS)
                    ScrollView { ledger }
                        .frame(maxHeight: .infinity)
                #else
                    ledger
                #endif
            }
        }
        #if os(macOS)
            .frame(maxHeight: .infinity, alignment: .top)
        #endif
    }

    /// HOW TO NARROW THE LOG — the whole of the pressable half, and nothing
    /// else. It carries no explanation of what an audit log is: the rows say
    /// that better than a paragraph does, and a wall of prose at the top of a
    /// filter rail is read once and then scrolled past for ever.
    @ViewBuilder private var sidebar: some View {
        VStack(alignment: .leading, spacing: 14) {
            if !rows.isEmpty { filters }
            reloadRow
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// The count in the card's corner: how much of the ledger is on screen, and
    /// how much there is. It says both only while a filter is up — "23 of 200"
    /// with nothing filtered would be arithmetic about the page size rather
    /// than a fact about the log.
    private var countNote: String? {
        guard !rows.isEmpty else { return nil }
        return filtering ? "\(filtered.count) of \(rows.count)" : "\(rows.count)"
    }

    // MARK: - the ledger

    @ViewBuilder private var ledger: some View {
        // LAZY, and that is the whole trick: inside the settings pane's own
        // ScrollView, a LazyVStack builds a row when it is about to be seen and
        // not before, which is also what makes the sentinel below a scroll
        // signal rather than something that fires on mount.
        LazyVStack(alignment: .leading, spacing: 1) {
            if filtered.isEmpty, filtering {
                // NOTHING TO SCROLL TO, so the list cannot ask for more the way
                // it usually does. A filter that matches only old entries has
                // to be able to reach them, so an empty result keeps pulling
                // pages on its own until one matches or the log runs out.
                BandNote("Nothing loaded so far matches those filters.")
                    .onAppear { loadMore() }
                    .onChange(of: rows.count) { _, _ in loadMore() }
            }
            ForEach(filtered) { entry in
                AuditRow(
                    entry: entry,
                    open: openId == entry.id,
                    undo: Self.undoFor(entry),
                    // Toggling to nil on a second press, so the row that
                    // opened is also the row that closes.
                    onToggle: { openId = openId == entry.id ? nil : entry.id },
                    onUndo: { Task { await performUndo(entry) } })
                    // REACHING THE LAST ROW IS THE REQUEST. The trigger rides
                    // the row rather than a sentinel under it because a
                    // LazyVStack builds this row only when it is about to be
                    // seen, and because the row's identity moves down the list
                    // with each page — a sentinel would need a changing `.id`
                    // to fire twice, and destroying a view at the bottom of a
                    // live scroll nudges the offset.
                    .onAppear { if entry.id == filtered.last?.id { loadMore() } }
            }
            spinner
            endNote
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// That there is more, while it is on its way. Purely a report: the ask
    /// happens on the last row above it.
    @ViewBuilder private var spinner: some View {
        if !exhausted {
            HStack(spacing: 8) {
                WaitDots()
                Text("older entries")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
            }
            .padding(.horizontal, 11)
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// WHERE THE LEDGER ENDS, said once, at the bottom of the rows it is about.
    /// It is not a control and it is not chrome — it is the last line of the
    /// document, which is why it stayed in the card when the buttons left.
    @ViewBuilder private var endNote: some View {
        if exhausted, !rows.isEmpty {
            Text(
                wanted >= Self.ceiling
                    ? "the most recent \(rows.count) entries"
                    : "that is the whole log"
            )
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaintest)
            .padding(.horizontal, 11)
            .padding(.top, 8)
        }
    }

    /// The reload, and anything the last pull has to apologise for. Text rather
    /// than a button with chrome: it is about the list, and a column of filters
    /// should not grow something that looks like one more setting.
    @ViewBuilder private var reloadRow: some View {
        HStack(spacing: 10) {
            Button("refresh") { Task { await reload() } }
                .buttonStyle(.plain)
                .font(Typo.micro)
                .foregroundStyle(auditState.isLoading ? Palette.inkFaintest : Palette.inkDim)
                .disabled(auditState.isLoading)
            if let error = auditState.error, auditState.value != nil {
                Text(error)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.warn)
                    .lineLimit(2)
            }
            Spacer(minLength: 0)
        }
    }

    // MARK: - the filters

    /// FACETS, NOT A QUERY LANGUAGE. Every control here is a fact the log
    /// already knows about itself — who acted, when, which action, whether it
    /// can be undone — so the list of actions is built from the rows in hand
    /// rather than from a fixed vocabulary. A daemon that starts auditing
    /// something new grows a filter for it the day it ships, and an action
    /// nobody has taken never appears as a dead option.
    @ViewBuilder private var filters: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 6) {
                Text("filter")
                    .font(Typo.sectionLabel)
                    .foregroundStyle(Palette.inkFaint)
                    .textCase(.uppercase)
                Spacer(minLength: 0)
                if filtering {
                    Button("clear") { clearFilters() }
                        .buttonStyle(.plain)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.accent)
                }
            }

            searchField

            filterGroup("who") {
                ForEach(WhoFilter.allCases, id: \.self) { option in
                    FilterPill(label: option.label, active: who == option) {
                        who = option
                    }
                }
            }

            filterGroup("when") {
                ForEach(WhenFilter.allCases, id: \.self) { option in
                    FilterPill(label: option.label, active: when == option) {
                        when = option
                    }
                }
            }

            if !verbCounts.isEmpty {
                filterGroup("action") {
                    ForEach(verbCounts, id: \.verb) { entry in
                        FilterPill(
                            label: entry.verb, count: entry.count,
                            active: verbs.contains(entry.verb)
                        ) {
                            if verbs.contains(entry.verb) {
                                verbs.remove(entry.verb)
                            } else {
                                verbs.insert(entry.verb)
                            }
                        }
                    }
                }
            }

            FilterPill(label: "reversible only", active: reversibleOnly) {
                reversibleOnly.toggle()
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var searchField: some View {
        HStack(spacing: 5) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(Palette.inkFaintest)
            TextField("sender or subject", text: $query)
                .textFieldStyle(.plain)
                .font(.system(size: 11))
                .foregroundStyle(Palette.ink)
                .autocorrectionDisabled()
                #if os(iOS)
                    .textInputAutocapitalization(.never)
                #endif
            if !query.isEmpty {
                Button {
                    query = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.system(size: 10))
                        .foregroundStyle(Palette.inkFaintest)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.horizontal, 7)
        .padding(.vertical, 5)
        .background(
            RoundedRectangle(cornerRadius: 7, style: .continuous)
                .fill(Palette.canvas.opacity(0.65))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 7, style: .continuous)
                .strokeBorder(Palette.hairlineStrong, lineWidth: 0.75)
        )
    }

    /// One labelled band of pills. The pills WRAP (see `FlowRow`) because their
    /// widths are the log's own words: "filed the sent copy into the thread" is
    /// as long as the daemon needs it to be, and a fixed grid would either clip
    /// it or leave a column of air beside "sent".
    @ViewBuilder private func filterGroup<Content: View>(
        _ label: String, @ViewBuilder pills: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(label)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
            FlowRow(spacing: 5, lineSpacing: 5) { pills() }
        }
    }

    /// Distinct action verbs in the rows the OTHER filters left, commonest
    /// first, ties broken alphabetically so the list does not reshuffle under
    /// the pointer when two counts are equal.
    private var verbCounts: [(verb: String, count: Int)] {
        var counts: [String: Int] = [:]
        for entry in preVerb { counts[Self.actionVerb(entry), default: 0] += 1 }
        // A verb that is currently PICKED stays on the list even when this
        // pass counts none of it, or turning a filter off would mean hunting
        // for the pill that turned it on.
        for verb in verbs where counts[verb] == nil { counts[verb] = 0 }
        return counts.map { (verb: $0.key, count: $0.value) }
            .sorted { $0.count == $1.count ? $0.verb < $1.verb : $0.count > $1.count }
    }

    private func clearFilters() {
        query = ""
        who = .all
        when = .all
        verbs = []
        reversibleOnly = false
    }

    /// Free text against everything the row SHOWS, plus the raw action slug —
    /// somebody who read "archive" in a daemon log should find it here even
    /// though the row says "archived".
    private func matchesQuery(_ e: AuditEntry) -> Bool {
        let needle = query.trimmed.lowercased()
        guard !needle.isEmpty else { return true }
        let haystack = [
            e.target_sender, e.target_subject, e.target, e.detail, e.action, e.actor,
            Self.actionVerb(e),
        ]
        return haystack.contains { ($0 ?? "").lowercased().contains(needle) }
    }

    // MARK: - loading

    /// One pull, for `limit` newest rows. The daemon answers with the newest N,
    /// so a larger N is a superset of the last answer and simply replaces it —
    /// no merging, no cursor, and no chance of the two disagreeing about the
    /// order they were in.
    private func load(_ limit: Int) async {
        await $auditState.load("audit failed") { try await APIClient.shared.getAudit(limit: limit) }
        wanted = limit
        // A page that came back SHORT is the end of the log. Only ever
        // concluded from a request that actually succeeded: on a failure the
        // last good rows are still in hand and counting them would read as an
        // ending that is really a dropped connection.
        guard auditState.error == nil else { return }
        exhausted = (auditState.value?.count ?? 0) < limit || limit >= Self.ceiling
    }

    /// Re-pull everything already on screen. Deliberately not a reset to one
    /// page: `refresh` means "is this still what happened", not "forget what I
    /// have read".
    private func reload() async {
        await load(wanted)
    }

    private func loadMore() {
        guard !exhausted, !auditState.isLoading else { return }
        Task { await load(min(wanted + Self.pageSize, Self.ceiling)) }
    }

    private func performUndo(_ entry: AuditEntry) async {
        guard let spec = Self.undoFor(entry) else { return }
        do {
            try await spec.run()
            store.pushToast("undone: \(Self.actionVerb(entry))", .info)
            // The undo lands as its own audit row; re-pull so it shows.
            await reload()
        } catch {
            store.pushToast(errText(error, "undo failed"), .error)
        }
    }

    // MARK: - readable entries

    /// Actors rendered as "the agent"; several spellings tolerated because the
    /// agent door's actor string isn't pinned.
    static func actorChip(_ actor: String) -> Chip {
        switch actorKind(actor) {
        case .agent: Chip(text: "Agent", tone: Palette.lock, filled: true)
        case .you: Chip(text: "You", tone: Palette.accent, filled: true)
        // Unknown actor: show it verbatim rather than mislabeling.
        case .other: Chip(text: actor.isEmpty ? "?" : actor, tone: Palette.inkFaint, filled: true)
        }
    }

    enum ActorKind { case agent, you, other }

    static func actorKind(_ actor: String) -> ActorKind {
        let lower = actor.lowercased()
        if ["agent", "mcp", "assistant", "ai"].contains(where: lower.hasPrefix) { return .agent }
        if ["client-api", "client", "app", "user"].contains(lower) { return .you }
        return .other
    }

    /// Both dotted and underscore slug spellings, so a rename on either side
    /// degrades gracefully. set_status is detail-driven.
    private static let actionVerbs: [String: String] = [
        "archive": "archived",
        "label": "relabeled a message",
        "send.echo": "filed the sent copy into the thread",
        "reveal_sealed": "revealed auth message",
        "reveal": "revealed auth message",
        "unsubscribe": "opened unsubscribe",
        "unsub_resolution": "resolved unsubscribe prompt",
        "rule.create": "created a sender rule",
        "create_rule": "created a sender rule",
        "rule.update": "updated a sender rule",
        "update_rule": "updated a sender rule",
        "rule.delete": "deleted a sender rule",
        "delete_rule": "deleted a sender rule",
    ]

    static func actionVerb(_ e: AuditEntry) -> String {
        if e.action == "send" {
            // A reply audits with the parent message id as its target; a fresh
            // message has no target, and calling it a reply misreports the ledger.
            return (e.target ?? "").isEmpty ? "sent a message" : "sent a reply"
        }
        if e.action == "set_status" {
            switch (e.detail ?? "").lowercased() {
            case "done": return "marked done"
            case "open": return "reopened"
            case "new": return "reset to new"
            default: return "changed status"
            }
        }
        if let verb = actionVerbs[e.action] { return verb }
        // Tolerate namespaced variants like "rule.set.v2" → match on the prefix.
        if let dot = e.action.firstIndex(of: "."), String(e.action[..<dot]) == "rule" {
            return "changed a sender rule"
        }
        return e.action.isEmpty ? "did something" : e.action
    }

    struct UndoSpec {
        var label: String
        var run: () async throws -> Void
    }

    /// Strict decimal parse, mirroring the server's SQLite CAST: a permissive
    /// parse accepts hex/exponent forms CAST maps to 0, so an undo could fire
    /// against a different id than the row displayed.
    static func parseAuditId(_ raw: String?) -> Int? {
        guard let raw, !raw.isEmpty,
            raw.allSatisfy({ $0.isASCII && $0.isNumber }),
            let id = Int(raw), id > 0
        else {
            return nil
        }
        return id
    }

    /// The safe inverse for a row, or nil — only successful, reversible actions.
    static func undoFor(_ e: AuditEntry) -> UndoSpec? {
        if e.action == "archive", e.detail == "ok", let id = parseAuditId(e.target) {
            return UndoSpec(label: "restore") {
                try await APIClient.shared.actionLabel(id, add: [inboxLabel])
            }
        }
        if e.action == "set_status", e.detail == "done", let id = parseAuditId(e.target) {
            return UndoSpec(label: "reopen") {
                try await APIClient.shared.setStatus(id, .open)
            }
        }
        if e.action == "rule.create" || e.action == "create_rule" {
            // The new rule id arrives in `detail`; `target` is the pattern.
            if let id = parseAuditId(e.detail) {
                return UndoSpec(label: "delete rule") {
                    try await APIClient.shared.deleteRule(id)
                }
            }
        }
        return nil
    }
}

// MARK: - the filter vocabularies

enum WhoFilter: CaseIterable {
    case all, you, agent

    var label: String {
        switch self {
        case .all: "anyone"
        case .you: "you"
        case .agent: "agent"
        }
    }

    func matches(_ e: AuditEntry) -> Bool {
        switch self {
        case .all: true
        case .you: AuditSection.actorKind(e.actor) == .you
        case .agent: AuditSection.actorKind(e.actor) == .agent
        }
    }
}

/// AGE BANDS, and one deliberate leniency: a row whose `ts` will not parse
/// passes EVERY band. The alternative is an audit log that hides evidence
/// because a timestamp was malformed, which is the one thing a ledger may
/// never do — better a row filed under the wrong day than a row nobody sees.
enum WhenFilter: CaseIterable {
    case all, today, week, month

    var label: String {
        switch self {
        case .all: "any time"
        case .today: "today"
        case .week: "7 days"
        case .month: "30 days"
        }
    }

    func matches(_ e: AuditEntry) -> Bool {
        guard self != .all else { return true }
        guard let date = Fmt.date(e.ts) else { return true }
        switch self {
        case .all: return true
        case .today: return Calendar.current.isDateInToday(date)
        case .week: return date >= Date().addingTimeInterval(-7 * 86_400)
        case .month: return date >= Date().addingTimeInterval(-30 * 86_400)
        }
    }
}

// MARK: - the row

/// ONE LEDGER LINE: time down the left gutter, the act in the middle, and what
/// it was done to under it.
///
/// The routed page could afford fixed-width columns because it had a window; a
/// card has whatever the pane leaves it, and the same struct now draws on a
/// phone. So only the time keeps a fixed column — it is what the eye runs down
/// — and everything else takes the width it is given, with the mail-derived
/// strings on their own line where a long subject truncates without pushing the
/// verb off the edge.
private struct AuditRow: View {
    let entry: AuditEntry
    let open: Bool
    let undo: AuditSection.UndoSpec?
    let onToggle: () -> Void
    let onUndo: () -> Void

    private var hasResolved: Bool {
        !(entry.target_sender ?? "").isEmpty || !(entry.target_subject ?? "").isEmpty
    }

    private var age: String {
        let relative = Fmt.relAge(entry.ts)
        return relative.isEmpty ? "now" : relative
    }

    var body: some View {
        ListRow(
            selected: open, cornerRadius: 8, hPadding: 11, vPadding: 7, action: onToggle
        ) { _, _ in
            HStack(alignment: .top, spacing: 10) {
                // The gutter. Fixed width and leading-aligned so the times make
                // a column you can run a finger down, which is the whole reason
                // they moved off the ragged right edge.
                Text(age)
                    .font(Typo.num(10))
                    .foregroundStyle(open ? Palette.inkDim : Palette.inkFaintest)
                    .frame(width: 34, alignment: .leading)
                    .help(entry.ts)

                VStack(alignment: .leading, spacing: 3) {
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        AuditSection.actorChip(entry.actor)
                        Text(AuditSection.actionVerb(entry))
                            .font(Typo.rowSub)
                            .foregroundStyle(Palette.ink)
                            .lineLimit(1)
                            .help(entry.action)
                        Spacer(minLength: 6)
                        if let undo, open {
                            Button(undo.label, action: onUndo)
                                .buttonStyle(.glass)
                                .font(Typo.micro)
                                .foregroundStyle(Palette.accent)
                                .help("undo — \(undo.label)")
                        }
                    }
                    target
                    if open, let detail = entry.detail, !detail.isEmpty {
                        Text("— \(detail)")
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaintest)
                    }
                }
            }
        }
    }

    @ViewBuilder private var target: some View {
        HStack(spacing: 5) {
            if hasResolved {
                if let sender = entry.target_sender, !sender.isEmpty {
                    Text(sender)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkDim)
                }
                if let subject = entry.target_subject, !subject.isEmpty {
                    Text("·").foregroundStyle(Palette.inkFaintest)
                    Text(subject)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaint)
                }
            } else if let target = entry.target, !target.isEmpty {
                Text(target).font(Typo.mono(10)).foregroundStyle(Palette.inkFaint)
            }
        }
        .lineLimit(open ? nil : 1)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

// MARK: - filter chrome

/// A filter as a pill: the option, optionally how many rows it would leave, and
/// whether it is on. Deliberately NOT `Chip` — a Chip is a LABEL, something the
/// data says about itself, and giving one a tap target would teach that every
/// chip in the app is a control.
private struct FilterPill: View {
    let label: String
    var count: Int?
    let active: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 5) {
                Text(label)
                    .font(Typo.chip)
                    .foregroundStyle(active ? .white : Palette.inkDim)
                    .lineLimit(1)
                if let count {
                    Text("\(count)")
                        .font(Typo.num(9))
                        .foregroundStyle(active ? Color.white.opacity(0.7) : Palette.inkFaintest)
                }
            }
            .padding(.horizontal, 9)
            .padding(.vertical, 4)
            .background(
                Capsule().fill(active ? Palette.accent.opacity(0.85) : Palette.hairline.opacity(0.6))
            )
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(active ? [.isButton, .isSelected] : [.isButton])
    }
}

/// A row of pills that wraps, because the pills are words out of the log and
/// their widths are not ours to choose. SwiftUI has no flow container, and the
/// alternatives are worse in both directions: an HStack clips the tail of a
/// long verb, and a VStack of one pill per line turns a facet list into a
/// column of mostly air.
private struct FlowRow: Layout {
    var spacing: CGFloat = 5
    var lineSpacing: CGFloat = 5

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout Void) -> CGSize {
        // An unspecified width means "how big would you like to be" — answer
        // with one line, which is what a horizontal stack would say too.
        let limit = proposal.width ?? .infinity
        var size = CGSize(width: 0, height: 0)
        var line = CGSize(width: 0, height: 0)
        for view in subviews {
            let item = view.sizeThatFits(.unspecified)
            if line.width > 0, line.width + spacing + item.width > limit {
                size.width = max(size.width, line.width)
                size.height += line.height + lineSpacing
                line = .zero
            }
            line.width += (line.width > 0 ? spacing : 0) + item.width
            line.height = max(line.height, item.height)
        }
        size.width = max(size.width, line.width)
        size.height += line.height
        return size
    }

    func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout Void
    ) {
        let limit = bounds.width
        var x = bounds.minX
        var y = bounds.minY
        var lineHeight: CGFloat = 0
        for view in subviews {
            let item = view.sizeThatFits(.unspecified)
            if x > bounds.minX, x - bounds.minX + item.width > limit {
                x = bounds.minX
                y += lineHeight + lineSpacing
                lineHeight = 0
            }
            view.place(
                at: CGPoint(x: x, y: y), anchor: .topLeading,
                proposal: ProposedViewSize(item))
            x += item.width + spacing
            lineHeight = max(lineHeight, item.height)
        }
    }
}
