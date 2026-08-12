// LOGIN CODES, ON THE DASHBOARD. The Mac puts auth mail behind a rail icon with
// a countdown ring on it and a whole page (`g`) behind that; a phone has neither,
// and a login code is the single most time-critical thing in an inbox — you are
// standing at a login form holding the phone that has the code on it.
//
// PRESENT, DON'T READ, exactly as the Mac does it. This zone renders sealed
// METADATA only — who it is from, what kind, how long ago. No body is fetched to
// draw it. Revealing is always a deliberate human tap, and it goes through the
// same audited `revealSealed` call and the same `store.pushAuthCode` queue the
// arrival flow uses, so the modal that appears is AuthCodeModal with its own 30s
// self-destruct and its own copy button. Nothing here touches the pasteboard and
// nothing here holds a code: the extracted string goes straight into the store
// entry the modal reads and is dropped when the modal is dismissed.
//
// Anything that is not a code kind (a sign-in alert, a magic link) has no digits
// to present, so it opens the one-time RevealPanel instead — the same view the
// Mac's auth page opens for the same kinds.

import SwiftUI

struct MobileAuthZone: View {
    @Environment(AppStore.self) private var store

    /// Newer than this heads the zone and keeps its live ring. Same window the
    /// Mac's auth page calls "last hour".
    private static let liveWindow: TimeInterval = 60 * 60
    /// How many rows before the zone stops growing. A dashboard zone is a
    /// glance; the backlog is the auth page's job, and the phone has no auth
    /// page yet, so this is deliberately a little longer than the Mac's rail.
    private static let maxRows = 5

    @State private var busy: Int?
    @State private var revealing: SealedMeta?

    /// Newest first — a code you are waiting for is always the newest thing here.
    private var rows: [SealedMeta] {
        store.sitrep.sealed
            .sorted { age($0.received_at) < age($1.received_at) }
            .prefix(Self.maxRows)
            .map { $0 }
    }

    private func age(_ iso: String?) -> TimeInterval {
        guard let d = Fmt.date(iso) else { return .greatestFiniteMagnitude }
        return Date().timeIntervalSince(d)
    }

    var body: some View {
        ZoneCard(
            symbol: "key.fill", title: "Login codes", count: store.sitrep.sealed.count,
            subtitle: "sealed · revealing is audited", tint: Palette.lock
        ) {
            if rows.isEmpty {
                // Always on the board, empty or not — the same reason the
                // newsletters zone states: a card that vanishes reads as a
                // missing feature rather than as "nothing right now".
                Text("No codes or sign-in mail waiting.")
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.vertical, 4)
            } else {
                VStack(spacing: 2) {
                    ForEach(rows) { meta in
                        AuthRow(
                            meta: meta,
                            live: age(meta.received_at) <= Self.liveWindow,
                            busy: busy == meta.id,
                            onReveal: { Task { await reveal(meta) } })
                    }
                }
            }
        }
        // The one-time panel for kinds with no digits to present. A full-screen
        // cover rather than a sheet: it renders a sandboxed web body, and a
        // half-height card is not a reading surface.
        .fullScreenCover(item: $revealing) { meta in
            RevealPanel(meta: meta) { revealing = nil }
                .background(Palette.canvas.ignoresSafeArea())
        }
    }

    /// The desktop `reveal(_:)`, verbatim in shape: code kinds resolve to digits,
    /// everything else opens the panel. The digits land in the store's auth queue
    /// — the same place the arrival flow puts them — so the modal, its timer and
    /// its dismissal are all the shared implementation.
    private func reveal(_ meta: SealedMeta) async {
        guard AuthCode.isCodeKind(meta.kind) else {
            revealing = meta
            return
        }
        guard busy == nil else { return }
        busy = meta.id
        defer { busy = nil }
        do {
            let revealed = try await APIClient.shared.revealSealed(meta.id)
            store.pushAuthCode(AuthCodeEntry(meta: meta, code: AuthCode.extract(revealed.body)))
        } catch {
            store.pushToast(errText(error, "reveal failed"), .error)
        }
    }
}

/// One sealed message: who, what kind, how old — and a reveal affordance that
/// says what it will do. Never the subject line of a code mail's body, never a
/// preview: the point of the seal is that nothing read it.
private struct AuthRow: View {
    @Environment(AppStore.self) private var store
    let meta: SealedMeta
    let live: Bool
    let busy: Bool
    let onReveal: () -> Void

    /// The store's own countdown ring for this id, if the arrival flow armed one.
    private var ring: AuthRing? { store.authRings.first { $0.id == meta.id } }

    var body: some View {
        ListRow(
            selected: false, cornerRadius: 10, tint: Palette.lock, hPadding: 10, vPadding: 8,
            hoverFill: false, action: onReveal
        ) { _, _ in
            HStack(spacing: 10) {
                ZStack {
                    Avatar(sender: meta.sender, size: 26)
                    if let ring { AuthLiveRing(ring: ring, size: 32) }
                }
                .frame(width: 32, height: 32)

                VStack(alignment: .leading, spacing: 2) {
                    Text(SenderCache.resolved(meta.sender).displayName)
                        .font(.system(size: 14, weight: .medium))
                        .foregroundStyle(Palette.ink)
                        .lineLimit(1)
                    HStack(spacing: 6) {
                        Label(AuthCopy.label(meta.kind), systemImage: AuthCopy.symbol(meta.kind))
                            .font(Typo.micro)
                            .foregroundStyle(Palette.lock)
                        Text(Fmt.relAge(meta.received_at))
                            .font(Typo.num(11))
                            .foregroundStyle(Palette.inkFaintest)
                        if live {
                            Text("live")
                                .font(Typo.micro)
                                .foregroundStyle(Palette.positive)
                        }
                    }
                }

                Spacer(minLength: 6)

                if busy {
                    ProgressView().controlSize(.small)
                } else {
                    Text(AuthCode.isCodeKind(meta.kind) ? "reveal" : "open")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.lock)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 5)
                        .glassCapsule(tint: Palette.lockSoft)
                }
            }
        }
        .accessibilityLabel(
            "\(AuthCopy.label(meta.kind)) from \(SenderCache.resolved(meta.sender).displayName)")
    }
}

/// The Mac's rail ring, at avatar size. Same `AuthRing` state, same expiry call
/// — the sweep is one 60s countdown per arrival wherever it is drawn, so a code
/// that has aged out stops looking live on both shells at the same moment.
private struct AuthLiveRing: View {
    @Environment(AppStore.self) private var store
    let ring: AuthRing
    let size: CGFloat

    @State private var progress: CGFloat = 1

    var body: some View {
        Circle()
            .trim(from: 0, to: progress)
            .stroke(Palette.lock, style: StrokeStyle(lineWidth: 2, lineCap: .round))
            .rotationEffect(.degrees(-90))
            .frame(width: size, height: size)
            .allowsHitTesting(false)
            .task {
                // A late mount starts partway through, so the ring still finishes
                // ~60s after it was armed rather than over-running.
                let elapsed = Date().timeIntervalSince(ring.startedAt)
                let remaining = max(0, ringSeconds - elapsed)
                guard remaining > 0 else {
                    store.expireAuthRing(ring.id)
                    return
                }
                progress = CGFloat(remaining / ringSeconds)
                withAnimation(.linear(duration: remaining)) { progress = 0 }
                try? await Task.sleep(for: .seconds(remaining))
                store.expireAuthRing(ring.id)
            }
    }
}
