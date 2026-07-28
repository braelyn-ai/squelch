// THE SITREP RIGHT RAIL + the newsletters zone.
//
// Calendar / Shipments / Banking / Receipts are RECORDS, not actions. Unlike
// the left column's zones (which hide when empty) the rail always shows its
// cards with an empty state, because a column that appears and disappears as
// mail arrives is a column you stop trusting to be there.
//
// These records are auto-resolved out of the attention bands at ingest, so this
// rail is their ONLY surface — clicking a row opens the underlying email.
//
// Ported from squelch-desktop/src/views/SitrepView.tsx (zones) and
// src/lib/newsletters.ts.

import SwiftUI

// MARK: - calendar

/// Calendar mail from the last 24h (server window). Records ordered by arrival,
/// not an agenda; cancellations strike through.
struct CalendarZone: View {
    @Environment(AppStore.self) private var store
    @State private var rows: [CalendarUpdate] = []

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
                            store.viewInEmails(c.message_id)
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
        .task { rows = (try? await APIClient.shared.getCalendar()) ?? [] }
    }

    private func tagTone(_ kind: CalendarKind) -> Color {
        kind == .cancellation ? Palette.danger : Palette.inkFaint
    }

    /// Compact when-column: time if the event is today, short date otherwise.
    private func when(_ c: CalendarUpdate) -> String {
        guard c.starts_at != nil else { return "" }
        return Fmt.isToday(c.starts_at) ? Fmt.timeOfDay(c.starts_at) : Fmt.shortDate(c.starts_at)
    }
}

// MARK: - shipments

/// Fetches with includeDelivered=true then keeps a shipment when it's still
/// active OR it was delivered TODAY. Yesterday's-and-older deliveries drop out.
/// View-only by design: no j/k, but each card's Track button is real.
struct ShipmentsZone: View {
    @State private var shipments: [Shipment] = []

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
        .task { shipments = (try? await APIClient.shared.getShipments(includeDelivered: true)) ?? [] }
    }
}

private struct ShipmentCard: View {
    let shipment: Shipment

    private var title: String {
        let trimmed = shipment.item_name.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? "Package via \(shipment.carrier.label)" : trimmed
    }

    /// Status → tone. out_for_delivery is the loud one (it's arriving today),
    /// exception is an error, delivered fades back into the record.
    private var tone: Color {
        switch shipment.status {
        case .outForDelivery: Palette.warn
        case .shipped: Palette.accent
        case .exception: Palette.danger
        case .delivered: Palette.positive
        case .ordered: Palette.inkFaintest
        }
    }

    var body: some View {
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
                    Button {
                        Opener.open(url)
                    } label: {
                        Label("Track", systemImage: "arrow.up.right")
                            .font(Typo.micro)
                            .padding(.horizontal, 7)
                            .padding(.vertical, 2.5)
                    }
                    .buttonStyle(.glass)
                    .foregroundStyle(Palette.accent)
                    .help("track \(shipment.tracking_number) · \(shipment.carrier.label)")
                }
            }
        }
        .padding(9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .fill(Palette.hairline.opacity(0.5))
        )
        .opacity(shipment.status == .delivered ? 0.65 : 1)
    }
}

/// Carrier favicon, falling back to a neutral package glyph for amazon/unknown
/// (no clean single domain) or a failed fetch.
private struct CarrierBadge: View {
    let carrier: Carrier
    @State private var image: NSImage?
    @State private var failed = false

    var body: some View {
        Group {
            if let image, !failed {
                Image(nsImage: image)
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

/// Statements & transaction alerts — the latest few, dense record rows:
/// institution + kind tag + masked account hint, amount right-aligned (a
/// statement's amount is the TOTAL balance the extractor pulled).
struct BankingZone: View {
    @Environment(AppStore.self) private var store
    @State private var records: [BankingRecord] = []

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
                            open(r)
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
        .task { await load() }
    }

    private func institutionLabel(_ r: BankingRecord) -> String {
        let base = r.institution ?? r.from_addr.map(SenderID.displayName) ?? "bank"
        return r.account_hint.map { "\(base) \($0)" } ?? base
    }

    /// thread_id shipped after the banking wire type — a daemon that predates it
    /// sends rows without one. Fall back to the emails-page jump so the click
    /// always does SOMETHING.
    private func open(_ r: BankingRecord) {
        if let tid = r.thread_id, !tid.isEmpty {
            store.openThread(tid)
        } else {
            store.viewInEmails(r.message_id)
        }
    }

    private func load() async {
        records = (try? await APIClient.shared.getBanking()) ?? []
        // Preload the shown records' emails; they live in the column until
        // newer records rotate them out, so cache for a day.
        ThreadPrefetch.shared.warm(
            records.prefix(Self.shown).compactMap(\.thread_id), immediate: 2)
    }
}

// MARK: - receipts

/// Records of money already paid — the clean merchant name (left) and the total
/// (right). Only TODAY's receipts show: a fresh daily digest, like a paper
/// receipt you'd glance at and file.
struct ReceiptsZone: View {
    @Environment(AppStore.self) private var store
    @State private var receipts: [Receipt] = []

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
                            open(r)
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
        .task { await load() }
    }

    private func open(_ r: Receipt) {
        if let tid = r.thread_id, !tid.isEmpty {
            store.openThread(tid)
        } else {
            store.viewInEmails(r.message_id)
        }
    }

    private func load() async {
        receipts = (try? await APIClient.shared.getReceipts()) ?? []
        // The column drops these at local midnight, so match the cache TTL.
        let midnight =
            Calendar.current.nextDate(
                after: Date(), matching: DateComponents(hour: 0, minute: 0),
                matchingPolicy: .nextTime) ?? Date().addingTimeInterval(3600)
        let ttl = max(60, midnight.timeIntervalSinceNow)
        for id in rows.compactMap(\.thread_id) {
            ThreadPrefetch.shared.prefetch(id, fresh: ttl)
        }
    }
}

// MARK: - newsletters

/// THE RULE-ONBOARDING SURFACE. Surfaces recurring marketing senders and, when
/// a rule already governs one, shows that rule as an editable chip. Clicking a
/// card OPENS the latest email of that sender (with the sender's whole window
/// as the viewer's h/l queue); `e` while hovering marks that window done.
///
/// THE FETCH LIVES IN SitrepView, NOT HERE, and that is load-bearing.
/// SwiftUI gives `EmptyView` no lifetime: `.task`/`.onAppear` on a view that
/// resolves to nothing NEVER FIRE. This zone hides itself while empty, so when
/// it owned its own `.task` the loop closed on itself — empty because it never
/// fetched, never fetched because it was empty. It was invisible for its entire
/// existence. The React original had no such trap: hooks run before the
/// `return null`. Any zone that can hide itself must be fed from a parent that
/// cannot.
struct NewslettersZone: View {
    @Environment(AppStore.self) private var store
    @Binding var newsletters: [Newsletter]
    @Binding var hovered: String?
    /// Re-derive after a rule save, so the chip/CTA reflects it immediately.
    let reload: () async -> Void

    var body: some View {
        // ALWAYS on the board, empty or not (owner call, 2026-07-28). A zone
        // that vanishes reads as a missing feature, not as "nothing this week".
        ZoneCard(
            symbol: "envelope.open", title: "Newsletters", count: newsletters.count,
            subtitle: "recurring noise · choose what you want"
        ) {
            if newsletters.isEmpty {
                EmptyNote("No recurring senders this week.")
            } else {
                LazyVGrid(
                    columns: [GridItem(.adaptive(minimum: 190), spacing: 10)], spacing: 10
                ) {
                    ForEach(newsletters) { nl in
                        NewsletterCard(
                            newsletter: nl,
                            onOpen: {
                                guard !nl.latestThreadId.isEmpty else { return }
                                store.openThread(nl.latestThreadId, queue: nl.items)
                            },
                            onEdit: { edit(nl) },
                            onHover: { over in hovered = over ? nl.address : nil })
                    }
                }
            }
        }
    }

    private func edit(_ nl: Newsletter) {
        guard let rule = nl.rule else { return }
        store.openRuleEditor(
            RuleEditorRequest(rule: rule, onSaved: { Task { await reload() } }))
    }
}

/// Fetch + derive for the newsletters zone. Free-standing so the ALWAYS-mounted
/// SitrepView can own it; see the note on NewslettersZone.
enum NewsletterFeed {
    /// Pull a generous window of noise-tier updates and filter to the last 7
    /// days client-side (the wire model carries no received_at).
    private static let fetchLimit = 200

    static func load() async -> [Newsletter] {
        do {
            async let updates = APIClient.shared.getUpdates(
                UpdatesParams(tier: .noise, limit: fetchLimit))
            async let rules = APIClient.shared.listRules()
            // Best-effort: an older daemon has no /client/marketing, in which
            // case the zone falls back to the legacy heuristic rather than
            // rendering empty.
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
    @Environment(Prefs.self) private var prefs
    let newsletter: Newsletter
    let onOpen: () -> Void
    let onEdit: () -> Void
    let onHover: (Bool) -> Void

    @State private var hovering = false

    var body: some View {
        Button(action: onOpen) {
            VStack(alignment: .leading, spacing: 0) {
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
                        Text(Newsletters.truncate(Newsletters.cleanSummary(newsletter.summary), 90))
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaint)
                            .lineLimit(2)
                            .multilineTextAlignment(.leading)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    if let rule = newsletter.rule {
                        // A ruled card keeps its chip as the EDIT affordance.
                        Button(action: onEdit) {
                            HStack(spacing: 5) {
                                Text(rule.disposition.label)
                                    .font(Typo.micro)
                                    .foregroundStyle(Palette.accent)
                                if !rule.want_text.isEmpty {
                                    Text(Newsletters.truncate(rule.want_text, 40))
                                        .font(Typo.micro)
                                        .foregroundStyle(Palette.inkFaintest)
                                        .lineLimit(1)
                                }
                                Image(systemName: "pencil")
                                    .font(.system(size: 9))
                                    .foregroundStyle(Palette.inkFaintest)
                            }
                            .padding(.horizontal, 7)
                            .padding(.vertical, 3)
                        }
                        .buttonStyle(.glass)
                        .help("edit this rule")
                    }
                }
                .padding(9)
            }
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
            onHover(over)
        }
    }
}

/// Hero thumbnail for a newsletter card: mined from the LATEST email's
/// sanitized html via the shared thread cache. Renders nothing until (and
/// unless) a hero resolves, and is gated on the remote-images pref — with
/// images "on demand" NO network fetch happens for unopened mail.
private struct NewsletterHero: View {
    @Environment(Prefs.self) private var prefs
    let threadId: String
    @State private var image: NSImage?

    var body: some View {
        Group {
            if let image {
                Image(nsImage: image)
                    .resizable()
                    .aspectRatio(contentMode: fit(image))
                    .frame(height: 84)
                    .frame(maxWidth: .infinity)
                    .background(Palette.canvas.opacity(0.7))
                    .clipShape(
                        UnevenRoundedRectangle(
                            topLeadingRadius: 12, bottomLeadingRadius: 0,
                            bottomTrailingRadius: 0, topTrailingRadius: 12, style: .continuous))
            }
        }
        .task(id: threadId) { await load() }
    }

    /// Fit by shape: tall/square heroes crop to fill (photos survive a crop);
    /// WIDE art letterboxes (cropping a wordmark chops its lettering).
    private func fit(_ image: NSImage) -> ContentMode {
        image.size.width > image.size.height * 1.2 ? .fit : .fill
    }

    private func load() async {
        guard prefs.loadRemoteImages, !threadId.isEmpty else { return }
        guard let view = try? await ThreadPrefetch.shared.fetch(threadId, fresh: 600) else {
            return
        }
        guard let newest = view.messages.last, let html = newest.html,
            let src = Trackers.extractHeroSrc(html), let url = URL(string: src)
        else { return }
        image = await RemoteImageLoader.shared.load(url)
    }
}

/// Fetches newsletter hero art. Referrer-suppressed and cookie-free, matching
/// the posture of every other remote-image fetch in the app.
@MainActor
final class RemoteImageLoader {
    static let shared = RemoteImageLoader()
    private var cache: [URL: NSImage] = [:]
    private let session: URLSession = {
        let cfg = URLSessionConfiguration.default
        cfg.timeoutIntervalForRequest = 10
        cfg.httpShouldSetCookies = false
        return URLSession(configuration: cfg)
    }()
    /// 8MB per image, matching the Rust shell's cap.
    private static let maxBytes = 8 * 1024 * 1024

    private init() {}

    func load(_ url: URL) async -> NSImage? {
        if let hit = cache[url] { return hit }
        guard url.scheme == "http" || url.scheme == "https" else { return nil }
        var req = URLRequest(url: url)
        req.setValue("", forHTTPHeaderField: "Referer")
        guard let (data, response) = try? await session.data(for: req),
            let http = response as? HTTPURLResponse, http.statusCode == 200,
            data.count <= Self.maxBytes,
            let mime = http.value(forHTTPHeaderField: "Content-Type"),
            mime.lowercased().hasPrefix("image/"),
            let image = NSImage(data: data)
        else { return nil }
        cache[url] = image
        return image
    }
}

// MARK: - shared bits

/// The rail's empty state. Present rather than hidden: a column that vanishes
/// when it has nothing is a column you stop trusting to be there.
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

/// A record row: no chrome at rest, a soft wash on hover. Records are for
/// reading; the affordance appears only when you reach for it.
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
