// THE SITREP RIGHT RAIL + the newsletters zone. Calendar / Shipments / Banking
// / Receipts are RECORDS, not actions: they always render, empty state and all,
// because a column that comes and goes as mail arrives is one you stop trusting.
// These records are auto-resolved out of the attention bands at ingest, so this
// rail is their ONLY surface — clicking a row opens the underlying email.

import SwiftUI

// MARK: - calendar

/// Calendar mail from the last 24h (server window). Records ordered by arrival,
/// not an agenda; cancellations strike through.
struct CalendarZone: View {
    @Environment(AppStore.self) private var store
    private var rows: [CalendarUpdate] { store.zones.calendar }

    var body: some View {
        ZoneCard(
            symbol: "calendar", title: "Calendar", count: rows.count, tint: Palette.accent
        ) {
            if rows.isEmpty {
                EmptyNote("No calendar updates.")
            } else {
                VStack(spacing: 1) {
                    ForEach(rows) { c in
                        Button {
                            store.openRecord(thread: c.thread_id, message: c.message_id)
                        } label: {
                            HStack(spacing: 7) {
                                Text(c.event_title ?? c.organizer ?? "calendar event")
                                    .font(Typo.rowSub)
                                    .foregroundStyle(Palette.inkDim)
                                    .strikethrough(c.kind == .cancellation)
                                    .lineLimit(1)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                if let tag = c.kind.tag {
                                    Chip(text: tag, tone: tagTone(c.kind))
                                }
                                Text(when(c))
                                    .font(Typo.num(10))
                                    .foregroundStyle(Palette.inkFaintest)
                            }
                            .padding(.horizontal, 7)
                            .padding(.vertical, 5)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(RecordRowStyle())
                    }
                }
            }
        }
        .task { await store.refreshZones() }
    }

    private func tagTone(_ kind: CalendarKind) -> Color {
        kind == .cancellation ? Palette.danger : Palette.inkFaint
    }

    private func when(_ c: CalendarUpdate) -> String {
        guard c.starts_at != nil else { return "" }
        return Fmt.isToday(c.starts_at) ? Fmt.timeOfDay(c.starts_at) : Fmt.shortDate(c.starts_at)
    }
}

// MARK: - shipments

/// Still-active shipments plus anything delivered TODAY; older deliveries drop
/// out. No j/k, but each card opens its email and the Track chip is real.
struct ShipmentsZone: View {
    @Environment(AppStore.self) private var store
    private var shipments: [Shipment] { store.zones.shipments }

    private var rows: [Shipment] {
        shipments.filter { $0.status != .delivered || Fmt.isToday($0.last_update) }
    }

    var body: some View {
        ZoneCard(
            symbol: "shippingbox", title: "Shipments", count: rows.count, tint: Palette.warn
        ) {
            if rows.isEmpty {
                EmptyNote("Nothing en route.")
            } else {
                VStack(spacing: 8) {
                    ForEach(rows) { ShipmentCard(shipment: $0) }
                }
            }
        }
        .task { await store.refreshZones() }
    }
}

private struct ShipmentCard: View {
    @Environment(AppStore.self) private var store
    let shipment: Shipment

    @State private var hovering = false

    private var title: String {
        let trimmed = shipment.item_name.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? "Package via \(shipment.carrier.label)" : trimmed
    }

    /// Status → tone: out_for_delivery is the loud one, delivered fades back.
    private var tone: Color {
        switch shipment.status {
        case .outForDelivery: Palette.warn
        case .shipped: Palette.accent
        case .exception: Palette.danger
        case .delivered: Palette.positive
        case .ordered: Palette.inkFaintest
        }
    }

    /// Where the card jumps: the feeding email's thread. `nil` on a row from an
    /// older daemon that sent no thread_id — then the card is inert and never
    /// paints the hover invitation it can't honor.
    private var target: String? {
        guard let tid = shipment.thread_id, !tid.isEmpty else { return nil }
        return tid
    }

    var body: some View {
        // The whole card opens the email; the Track chip is a real Button inside
        // it. The card is a tap gesture, not an outer Button, because a Button
        // beats a gesture in hit-testing — the chip keeps its clicks by
        // construction (AttachmentStrip's card + download button, same shape).
        VStack(alignment: .leading, spacing: 7) {
            HStack(spacing: 7) {
                CarrierBadge(carrier: shipment.carrier)
                Text(title)
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .help(title)
            }
            HStack(spacing: 7) {
                Chip(
                    text: shipment.status.label, tone: tone,
                    symbol: shipment.status == .delivered ? "checkmark.circle.fill" : nil,
                    filled: shipment.status == .outForDelivery)
                Spacer(minLength: 0)
                if let url = shipment.tracking_url {
                    ChromeChip(
                        text: "Track", icon: "arrow.up.right", tone: Palette.accent,
                        help: "track \(shipment.tracking_number) · \(shipment.carrier.label)"
                    ) { Opener.open(url) }
                }
            }
        }
        .padding(9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .fill(Palette.hairline.opacity(hovering && target != nil ? 0.85 : 0.5))
        )
        .contentShape(Rectangle())
        .onTapGesture { if let target { store.openThread(target) } }
        .onHover { hovering = $0 }
        .opacity(shipment.status == .delivered ? 0.65 : 1)
    }
}

/// Carrier favicon, falling back to a package glyph for amazon/unknown (no clean
/// single domain) or a failed fetch.
private struct CarrierBadge: View {
    let carrier: Carrier
    @State private var image: PlatformImage?
    @State private var failed = false

    var body: some View {
        Group {
            if let image, !failed {
                Image(platformImage: image)
                    .resizable().interpolation(.high).aspectRatio(contentMode: .fit)
                    .frame(width: 20, height: 20)
            } else {
                Image(systemName: "shippingbox.fill")
                    .font(.system(size: 13))
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(width: 20, height: 20)
            }
        }
        .help(carrier.label)
        .task {
            guard let domain = carrier.faviconDomain, let url = SenderID.faviconURL(domain) else {
                failed = true
                return
            }
            image = await FaviconLoader.shared.load(url: url, domain: domain)
            failed = image == nil
        }
    }
}

// MARK: - banking

/// Statements & transaction alerts, latest first: institution + kind tag +
/// masked account hint, amount right-aligned (a statement's amount is the TOTAL
/// balance the extractor pulled).
struct BankingZone: View {
    @Environment(AppStore.self) private var store
    private var records: [BankingRecord] { store.zones.banking }

    /// How many the rail shows. Statements are ~monthly, so a short recency
    /// list beats a today-only filter that would sit empty.
    private static let shown = 8

    var body: some View {
        let rows = Array(records.prefix(Self.shown))
        ZoneCard(
            symbol: "building.columns", title: "Banking", count: rows.count, tint: Palette.accentInk
        ) {
            if rows.isEmpty {
                EmptyNote("No statements or alerts.")
            } else {
                VStack(spacing: 1) {
                    ForEach(rows) { r in
                        Button {
                            store.openRecord(thread: r.thread_id, message: r.message_id)
                        } label: {
                            HStack(spacing: 7) {
                                Text(institutionLabel(r))
                                    .font(Typo.rowSub)
                                    .foregroundStyle(Palette.inkDim)
                                    .lineLimit(1)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                Chip(text: r.kind.tag, tone: Palette.inkFaintest)
                                if r.amount != nil {
                                    Text(Fmt.usd(r.amount, currency: r.currency))
                                        .font(Typo.num(11, weight: .medium))
                                        .foregroundStyle(Palette.accentInk)
                                }
                            }
                            .padding(.horizontal, 7)
                            .padding(.vertical, 5)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(RecordRowStyle())
                    }
                }
            }
        }
        .task { await store.refreshZones() }
    }

    private func institutionLabel(_ r: BankingRecord) -> String {
        let base = r.institution ?? r.from_addr.map(SenderID.displayName) ?? "bank"
        return r.account_hint.map { "\(base) \($0)" } ?? base
    }

}

// MARK: - receipts

/// Money already paid: merchant (left), total (right). Only TODAY's receipts
/// show — a fresh daily digest rather than a growing ledger.
struct ReceiptsZone: View {
    @Environment(AppStore.self) private var store
    private var receipts: [Receipt] { store.zones.receipts }

    private var rows: [Receipt] { receipts.filter { Fmt.isToday($0.received_at) } }

    var body: some View {
        ZoneCard(
            symbol: "receipt", title: "Receipts", count: rows.count, tint: Palette.positive
        ) {
            if rows.isEmpty {
                EmptyNote("No receipts today.")
            } else {
                VStack(spacing: 1) {
                    ForEach(rows) { r in
                        Button {
                            store.openRecord(thread: r.thread_id, message: r.message_id)
                        } label: {
                            HStack(spacing: 7) {
                                Text(SenderCache.resolved(r.senderString).displayName)
                                    .font(Typo.rowSub)
                                    .foregroundStyle(Palette.inkDim)
                                    .lineLimit(1)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .help(r.from_addr)
                                if r.amount != nil {
                                    Text(Fmt.usd(r.amount, currency: r.currency))
                                        .font(Typo.num(11, weight: .medium))
                                        .foregroundStyle(Palette.positive)
                                }
                            }
                            .padding(.horizontal, 7)
                            .padding(.vertical, 5)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(RecordRowStyle())
                    }
                }
            }
        }
        .task { await store.refreshZones() }
    }

}

// MARK: - newsletters

/// THE RULE-ONBOARDING SURFACE: recurring marketing senders; a ruled sender is
/// marked only by the card's accent border (rules are edited in the Rules view).
/// Clicking opens that sender's latest email with their whole window as the
/// viewer's h/l queue; `e` while hovering marks it done.
///
/// THE FETCH LIVES IN SitrepView, NOT HERE: SwiftUI gives `EmptyView` no
/// lifetime, so `.task`/`.onAppear` on a view that resolves to nothing never
/// fire. Any zone that can hide itself must be fed by a parent that cannot.
/// NOTHING HERE IS A CLOSURE OR A BINDING, deliberately: both compare unequal on
/// every render, so a zone that took them could never be skipped when the
/// dashboard redrew — and skipping it is what keeps a scroll from re-measuring
/// every card. `[Newsletter]` is Equatable and the cursor is one stable
/// reference, so SwiftUI can prove this subtree unchanged and leave it alone.
struct NewslettersZone: View {
    @Environment(AppStore.self) private var store
    let newsletters: [Newsletter]
    let cursor: SitrepCursor

    /// Narrowest a card may be drawn; the grid fits as many equal columns of at
    /// least this width as the zone allows.
    private static let cardMinimum: CGFloat = 190
    /// Gutter, both axes.
    private static let gap: CGFloat = 10

    var body: some View {
        // Always on the board, empty or not: a zone that vanishes reads as a
        // missing feature, not as "nothing this week".
        ZoneCard(
            symbol: "envelope.open", title: "Newsletters", count: newsletters.count,
            subtitle: "recurring noise · choose what you want",
            // This zone is a WEEK of RECURRING senders; the rest of the noise has
            // no other door on the dashboard.
            trailing: AnyView(
                ChromeChip(text: "all noise", help: "the emails tab's noise page") {
                    store.openMail(.noise)
                })
        ) {
            if newsletters.isEmpty {
                EmptyNote("No recurring senders this week.")
            } else {
                grid
            }
        }
    }

    /// NON-LAZY on purpose: a LazyVGrid rebuilds cards as they cross the
    /// viewport, and each rebuild costs a hero lookup and a fresh `.task`. A
    /// week of recurring senders is bounded, so holding all of it is cheaper.
    private var grid: some View {
        AdaptiveGrid(minimum: Self.cardMinimum, spacing: Self.gap) { cards }
    }

    @ViewBuilder private var cards: some View {
        ForEach(newsletters) { nl in
            NewsletterCard(newsletter: nl, cursor: cursor)
        }
    }
}

/// Fetch + derive for the newsletters zone. Free-standing so the always-mounted
/// SitrepView can own it.
enum NewsletterFeed {
    /// Pull a generous window of noise-tier updates and filter to the last 7
    /// days client-side (the wire model carries no received_at).
    private static let fetchLimit = 200

    static func load() async -> [Newsletter] {
        do {
            async let updates = APIClient.shared.getUpdates(
                UpdatesParams(tier: .noise, limit: fetchLimit))
            async let rules = APIClient.shared.listRules()
            // Best-effort: an older daemon has no /client/marketing, and the
            // zone falls back to the legacy heuristic rather than rendering empty.
            let marketing = (try? await APIClient.shared.getMarketing()) ?? []
            let (page, rl) = try await (updates, rules)
            return Newsletters.derive(
                updates: page.items, rules: rl,
                marketingIds: Set(marketing.map(\.message_id)))
        } catch {
            // Non-fatal: leave the zone empty rather than surfacing token/url.
            return []
        }
    }
}

private struct NewsletterCard: View {
    @Environment(AppStore.self) private var store
    let newsletter: Newsletter
    let cursor: SitrepCursor

    @State private var hovering = false

    private var summaryText: String {
        Newsletters.truncate(Newsletters.cleanSummary(newsletter.summary), 90)
    }

    var body: some View {
        Button(action: open) {
            // Hero left as a FIXED square, text right: every card in the grid
            // keeps the same height whether or not its sender ships art.
            HStack(alignment: .top, spacing: 9) {
                NewsletterHero(threadId: newsletter.latestThreadId)
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 6) {
                        Text(SenderCache.resolved(newsletter.sender).displayName)
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundStyle(Palette.ink)
                            .lineLimit(1)
                        Spacer(minLength: 4)
                        Text("\(newsletter.count) this week")
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaintest)
                            .fixedSize()
                    }
                    if !newsletter.summary.isEmpty {
                        Text(summaryText)
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaint)
                            .lineLimit(2)
                            .multilineTextAlignment(.leading)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .padding(9)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .fill(Palette.hairline.opacity(hovering ? 0.85 : 0.5))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(
                        newsletter.rule != nil ? Palette.accent.opacity(0.35) : .clear,
                        lineWidth: 1)
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { over in
            hovering = over
            // Nothing READS this during a render — only the `e` handler, at
            // key-press time — so this write is free even at scroll frequency.
            cursor.newsletter = over ? newsletter.address : nil
        }
    }

    private func open() {
        guard !newsletter.latestThreadId.isEmpty else { return }
        store.openThread(newsletter.latestThreadId, queue: newsletter.items)
    }

}

/// Hero thumbnail mined from the latest email's sanitized html via the shared
/// thread cache. Gated on the remote-images pref: with images "on demand" NO
/// network fetch happens for unopened mail (see docs/SECURITY.md §3).
private struct NewsletterHero: View {
    let threadId: String
    @State private var resolved: HeroCache.Hero?

    /// Side of the square thumb.
    private static let side: CGFloat = 54

    /// TUNABLE width:height cap on how wide a hero may be DRAWN. Wider art is
    /// cropped to exactly this ratio rather than letterboxed whole: a 728x90
    /// masthead fitted into the square becomes an unreadable few-point strip.
    private static let maxAspect: CGFloat = 7

    /// Falling back to the cache INSIDE the read keeps a recycled card from
    /// flashing empty: `@State` is gone the moment the card is dropped, the
    /// cached hero is not, so it repaints on the very first frame.
    private var hero: HeroCache.Hero? { resolved ?? HeroCache.shared.cached(threadId) }

    var body: some View {
        content
            .task(id: threadId) {
                // Already answered — `hero` is drawing that answer, and resolving
                // again costs a second body pass. The guard is INSIDE the task,
                // not around it: a conditional modifier would change this view's
                // identity as the verdict lands.
                guard !HeroCache.shared.isResolved(threadId) else { return }
                resolved = await HeroCache.shared.resolve(threadId)
            }
    }

    /// The no-hero branch is a REAL zero-size leaf, not an implicit EmptyView:
    /// `.task` on a view that resolves to nothing never fires, and this view
    /// starts with no image, so the fetch that produces one would never run.
    @ViewBuilder
    private var content: some View {
        if let hero {
            art(hero)
                .frame(width: Self.side, height: Self.side)
                // The rest of the square, in the art's own dominant colour, or
                // the neutral well when sampling failed — which must stay
                // distinguishable from a white sample.
                .background(hero.fill ?? Palette.canvas.opacity(0.7))
                .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        } else {
            // Zero in BOTH axes: a zero-height-only placeholder would still
            // collect the HStack's spacing and indent the text.
            Color.clear.frame(width: 0, height: 0)
        }
    }

    /// The art itself, unclipped and unbacked — the square frame and the fill
    /// behind it belong to the caller.
    @ViewBuilder
    private func art(_ hero: HeroCache.Hero) -> some View {
        if ultraWide(hero.image) {
            // Past `maxAspect`, draw at the capped ratio and let the ENDS crop:
            // `.fill` overflows on the long axis and `.clipped` keeps the middle,
            // which is where the wordmark lives.
            Image(platformImage: hero.image)
                .resizable()
                .aspectRatio(contentMode: .fill)
                .frame(width: Self.side, height: Self.side / Self.maxAspect)
                .clipped()
        } else {
            Image(platformImage: hero.image)
                .resizable()
                .aspectRatio(contentMode: fit(hero.image))
        }
    }

    /// Wider than the cap, and guarded against a zero-height decode.
    private func ultraWide(_ image: PlatformImage) -> Bool {
        image.size.height > 0 && image.size.width > image.size.height * Self.maxAspect
    }

    /// Fit by shape: tall/square heroes crop to fill (photos survive a crop),
    /// WIDE art letterboxes — filling a square with a wordmark would leave a
    /// meaningless slice of two letters. Only reached for art within `maxAspect`.
    private func fit(_ image: PlatformImage) -> ContentMode {
        image.size.width > image.size.height * 1.2 ? .fit : .fill
    }
}

// MARK: - adaptive grid

/// `LazyVGrid(columns: [.adaptive(minimum:spacing:)])` geometry WITHOUT the
/// laziness: as many equal-width columns as fit at `minimum`, each row as tall as
/// its tallest card with shorter cards centred (matching `GridItem`'s default
/// `.center`). A `Layout` sees every subview on every pass, so nothing is created
/// or destroyed by scrolling — that is the whole point.
struct AdaptiveGrid: Layout {
    /// Narrowest a column may be. Columns then stretch to divide the width
    /// evenly, matching `.adaptive`'s default unbounded maximum.
    var minimum: CGFloat
    var spacing: CGFloat

    /// Column geometry, kept BETWEEN the sizing pass and the placement pass.
    ///
    /// Without this every layout measured every card TWICE — `sizeThatFits` and
    /// `placeSubviews` each called `metrics`, which maps `sizeThatFits` over all
    /// of them — and a card's height is a two-line text layout, not a constant.
    /// Keyed on the width it was measured at, so a window resize invalidates it
    /// and nothing else needs to.
    struct Cache {
        var measuredAt: CGFloat?
        var measuredCount = 0
        var metrics: Metrics?
    }

    func makeCache(subviews: Subviews) -> Cache { Cache() }

    /// The subviews changed, so the heights we hold describe cards that are gone.
    /// `metrics` ALSO re-checks the count on every read: this is the documented
    /// invalidation hook, but a stale row height is a visibly broken grid, and
    /// the count check costs nothing to carry.
    func updateCache(_ cache: inout Cache, subviews: Subviews) {
        cache.measuredAt = nil
        cache.metrics = nil
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout Cache) -> CGSize
    {
        let metrics = metrics(width: proposal.width, subviews: subviews, cache: &cache)
        return CGSize(width: metrics.width, height: metrics.height)
    }

    func placeSubviews(
        in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout Cache
    ) {
        let metrics = metrics(width: bounds.width, subviews: subviews, cache: &cache)
        var y = bounds.minY
        var index = 0
        for rowHeight in metrics.rowHeights {
            for column in 0..<metrics.columns where index < subviews.count {
                let height = metrics.heights[index]
                subviews[index].place(
                    at: CGPoint(
                        x: bounds.minX + CGFloat(column) * (metrics.columnWidth + spacing),
                        y: y + (rowHeight - height) / 2),
                    proposal: ProposedViewSize(width: metrics.columnWidth, height: height))
                index += 1
            }
            y += rowHeight + spacing
        }
    }

    struct Metrics {
        var columns: Int
        var columnWidth: CGFloat
        var heights: [CGFloat]
        var rowHeights: [CGFloat]
        var width: CGFloat
        var height: CGFloat
    }

    /// Cached geometry for this width, measuring only on a miss.
    private func metrics(width proposed: CGFloat?, subviews: Subviews, cache: inout Cache)
        -> Metrics
    {
        let width = (proposed?.isFinite == true) ? max(0, proposed!) : minimum
        if let hit = cache.metrics, cache.measuredAt == width,
            cache.measuredCount == subviews.count
        {
            return hit
        }
        let measured = measure(width: width, subviews: subviews)
        cache.measuredAt = width
        cache.measuredCount = subviews.count
        cache.metrics = measured
        return measured
    }

    /// Column count is the most that fit at `minimum` with a gutter between each;
    /// leftover is split evenly. A nil or infinite proposal is a SwiftUI sizing
    /// probe, not a width — answer it with one column at `minimum`, because
    /// feeding infinity into the column arithmetic traps on the `Int` conversion.
    private func measure(width: CGFloat, subviews: Subviews) -> Metrics {
        let columns = max(1, Int((width + spacing) / (minimum + spacing)))
        let columnWidth = max(0, (width - spacing * CGFloat(columns - 1)) / CGFloat(columns))
        let heights = subviews.map {
            $0.sizeThatFits(ProposedViewSize(width: columnWidth, height: nil)).height
        }
        var rowHeights: [CGFloat] = []
        var index = 0
        while index < heights.count {
            let row = heights[index..<min(index + columns, heights.count)]
            rowHeights.append(row.max() ?? 0)
            index += columns
        }
        let total = rowHeights.reduce(0, +) + spacing * CGFloat(max(0, rowHeights.count - 1))
        return Metrics(
            columns: columns, columnWidth: columnWidth, heights: heights, rowHeights: rowHeights,
            width: width, height: total)
    }
}

// MARK: - shared bits

/// The rail's empty state — present rather than hidden.
struct EmptyNote: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaintest)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.vertical, 2)
    }
}

/// A record row: no chrome at rest, a soft wash on hover.
struct RecordRowStyle: ButtonStyle {
    @State private var hovering = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(
                RoundedRectangle(cornerRadius: 7, style: .continuous)
                    .fill(hovering ? Palette.accentSoft.opacity(0.7) : .clear)
            )
            .opacity(configuration.isPressed ? 0.7 : 1)
            .onHover { hovering = $0 }
    }
}

