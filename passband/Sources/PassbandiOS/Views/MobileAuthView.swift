// LOGIN CODES, BEHIND THE KEY IN QUICK LOOK'S BAR. The Mac puts auth mail behind
// a rail icon with a countdown ring on it and a whole page (`g`) behind that; the
// phone has the same two halves, at phone scale: a key glyph that wears a live
// dot when something just landed, and this page behind it.
//
// IT IS NOT A TAB. Codes are not a place you live; they are a thing you look up
// in the ten seconds a form is waiting on you and then leave. A tab spends
// permanent bar width on a surface that is empty most days.
//
// THE KEY SITS ON QUICK LOOK, which is the tab whose whole thesis is "something
// the app pulled out of your mail and is holding for you to look up" — the same
// sentence this page is. It spent a while on the sitrep's bar instead, and the
// reason it left is that the dashboard's trailing corner is worth more to a door
// you open every day (the composer) than to one that is empty most days.
//
// AND THE URGENT HALF DOES NOT WAIT TO BE FOUND. Moving the key one tab over
// would have made the most time-critical row on the phone the hardest to reach,
// so the case that is actually a race — a code-kind message that just landed —
// raises its own card at the top of the sitrep and reveals FROM THERE, without
// coming through this page at all (MobileSitrepView). `freshCodes` is the gate
// and `revealCode` is the tap, both below, so the two surfaces cannot disagree
// about what is fresh or take different routes to the same digits.
//
// PRESENT, DON'T READ, exactly as the Mac does it. This page renders sealed
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

struct MobileAuthView: View {
    @Environment(AppStore.self) private var store

    /// Newer than this keeps its live ring and reads "live". Same window the
    /// Mac's auth page calls "last hour" — and the same one the key's dot and
    /// the sitrep's fresh-code card ask about, which is why it and `isLive` are
    /// internal rather than private: three surfaces disagreeing about what
    /// "live" means would make the dot, the card and the row contradict each
    /// other.
    static let liveWindow: TimeInterval = 60 * 60

    static func age(_ iso: String?) -> TimeInterval {
        guard let d = Fmt.date(iso) else { return .greatestFiniteMagnitude }
        return Date().timeIntervalSince(d)
    }

    static func isLive(_ meta: SealedMeta) -> Bool {
        age(meta.received_at) <= liveWindow
    }

    /// The sitrep card's gate: sealed mail that is BOTH inside the live window
    /// and actually holding digits, newest first.
    ///
    /// BOTH HALVES, because only one of them is a race. A sign-in alert from
    /// four minutes ago is live and is still a thing you read when you get to
    /// it — nobody is standing at a form waiting on it — so it stays a row on
    /// this page and a dot on the key rather than jumping the dashboard. What
    /// earns the top of the sitrep is the case where the phone is the only
    /// thing between you and being logged in.
    static func freshCodes(_ sealed: [SealedMeta]) -> [SealedMeta] {
        sealed
            .filter { AuthCode.isCodeKind($0.kind) && isLive($0) }
            .sorted { age($0.received_at) < age($1.received_at) }
    }

    /// The reveal itself, shared by this page and the sitrep's card: ONE audited
    /// `revealSealed` and one `pushAuthCode`, so the modal that appears, its 30s
    /// self-destruct and its copy button are the same wherever the tap happened.
    /// The caller owns its own busy flag; nothing is held here, and the digits
    /// go straight into the store entry the modal reads.
    @MainActor
    static func revealCode(_ meta: SealedMeta, into store: AppStore) async {
        do {
            let revealed = try await APIClient.shared.revealSealed(meta.id)
            store.pushAuthCode(AuthCodeEntry(meta: meta, code: AuthCode.extract(revealed.body)))
        } catch {
            store.pushToast(errText(error, "reveal failed"), .error)
        }
    }

    @State private var busy: Int?
    @State private var revealing: SealedMeta?

    /// EVERY sealed message, newest first. The zone this grew out of stopped at
    /// five because a card on a shared column is a glance; a page is the whole
    /// list by definition, and the row you came looking for after a day away is
    /// exactly the one a cap would have cut. The sort still puts the newest on
    /// top, because a code you are waiting for is always the newest thing here.
    private var rows: [SealedMeta] {
        store.sitrep.sealed.sorted { Self.age($0.received_at) < Self.age($1.received_at) }
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                masthead
                codes
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 28)
        }
        .background(Palette.canvas)
        .navigationTitle("Login Codes")
        .navigationBarTitleDisplayMode(.inline)
        // The sealed list rides the sitrep pull, not the zones fetch, and this
        // page's one promise is "the code you are waiting for, now". A
        // pull-to-refresh that could not fetch a just-arrived code would betray
        // the only reason anyone opens it.
        .refreshable { _ = await SitrepPoller.shared.pull() }
        // The one-time panel for kinds with no digits to present. A full-screen
        // cover rather than a sheet: it renders a sandboxed web body, and a
        // half-height card is not a reading surface.
        .fullScreenCover(item: $revealing) { meta in
            RevealPanel(meta: meta) { revealing = nil }
                .background(Palette.canvas.ignoresSafeArea())
        }
    }

    /// The one serif line on this screen, and under it the sentence the zone
    /// carried as a card subtitle. Both halves of that line are load-bearing
    /// promises rather than decoration, so they survive the move verbatim.
    private var masthead: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("login codes")
                .font(Typo.micro)
                .foregroundStyle(Palette.accent)
                .textCase(.uppercase)
                .tracking(0.6)
            Text("Held sealed until you ask.")
                .font(Typo.hero(26))
                .foregroundStyle(Palette.ink)
                .fixedSize(horizontal: false, vertical: true)
            Text("sealed · revealing is audited")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, 4)
    }

    /// One glass pane holding the rows, rather than the `ZoneCard` the zone
    /// used: a card's header earns its space by naming its contents while three
    /// other cards sit beside it, and this screen has no beside. The nav title
    /// and the masthead say it once.
    private var codes: some View {
        VStack(spacing: 2) {
            if rows.isEmpty {
                // Stated, not omitted — the same reason the zone gave: a surface
                // that renders nothing reads as broken rather than as "nothing
                // right now", and this one is reached by a deliberate tap.
                Text("No codes or sign-in mail waiting.")
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 8)
            } else {
                ForEach(rows) { meta in
                    AuthRow(
                        meta: meta,
                        live: Self.isLive(meta),
                        busy: busy == meta.id,
                        onReveal: { Task { await reveal(meta) } })
                }
            }
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .passbandGlass(.pane, cornerRadius: 18, tint: Palette.lock.opacity(0.12))
    }

    /// The desktop `reveal(_:)`, verbatim in shape: code kinds resolve to digits,
    /// everything else opens the panel. This half — the kind branch and the busy
    /// flag — is the page's; the audited call itself is `revealCode` above,
    /// which the sitrep's card runs too.
    private func reveal(_ meta: SealedMeta) async {
        guard AuthCode.isCodeKind(meta.kind) else {
            revealing = meta
            return
        }
        guard busy == nil else { return }
        busy = meta.id
        defer { busy = nil }
        await Self.revealCode(meta, into: store)
    }
}

/// One sealed message: who, what kind, how old — and a reveal affordance that
/// says what it will do. Never the subject line of a code mail's body, never a
/// preview: the point of the seal is that nothing read it.
///
/// INTERNAL, not private, because the sitrep's fresh-code card draws THIS row
/// rather than a second one that looks like it. A card that rendered the same
/// message a little differently from the page behind it would be two answers to
/// "what just arrived".
struct AuthRow: View {
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
