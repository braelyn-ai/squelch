// ARRIVAL DETECTION for the 2FA "present, don't read" flow: watches the polled
// sealed metadata and fires once per genuinely-new auth message — a countdown
// ring, plus (for otp/verification only) an audited auto-reveal and a code modal.
//
// Fresh means NOT in the persisted seen-set AND received within ~2 minutes, and
// the first run seeds every current id silently so a backlog stays quiet. The
// seen-set plus ring expiry ARE the read state — read-marking is impossible
// under gmail.readonly.

import Foundation

@MainActor
final class AuthArrival {
    static let shared = AuthArrival()

    private static let seenKey = "squelch.auth-seen"
    private static let seenCap = 200
    /// A message older than this on first sight is treated as history.
    private static let freshWindow: TimeInterval = 2 * 60

    private var seen: Set<Int>?
    private var seeded = false

    private var store: AppStore { AppStore.shared }

    private init() {}

    private func loadSeen() -> Set<Int> {
        let raw = UserDefaults.standard.array(forKey: Self.seenKey) as? [Int] ?? []
        return Set(raw)
    }

    private func saveSeen(_ set: Set<Int>) {
        // Cap to the most-recent ids (numeric order == arrival order).
        let capped = Array(set.sorted().suffix(Self.seenCap))
        UserDefaults.standard.set(capped, forKey: Self.seenKey)
    }

    /// Seconds since an ISO stamp; huge when missing/invalid (=> treat as old).
    private func ageSeconds(_ iso: String?) -> TimeInterval {
        guard let d = Fmt.date(iso) else { return .greatestFiniteMagnitude }
        return Date().timeIntervalSince(d)
    }

    /// Call whenever the sealed list changes (the poller drives this).
    func observe(sealed: [SealedMeta]) {
        if seen == nil { seen = loadSeen() }
        guard var seen else { return }

        // First run of this session: seed the backlog silently, so we only ever
        // fire for messages that arrive AFTER we are watching.
        if !seeded {
            seeded = true
            var changed = false
            for m in sealed where !seen.contains(m.id) {
                seen.insert(m.id)
                changed = true
            }
            self.seen = seen
            if changed { saveSeen(seen) }
            return
        }

        var arrivals: [SealedMeta] = []
        for m in sealed {
            if seen.contains(m.id) { continue }
            seen.insert(m.id)  // mark immediately so a re-poll never double-fires
            // Only fresh messages fire the flow; late-arriving history stays quiet
            // but is still recorded as seen above.
            if ageSeconds(m.received_at) <= Self.freshWindow { arrivals.append(m) }
        }
        self.seen = seen
        guard !arrivals.isEmpty else { return }
        saveSeen(seen)

        // Ring for every arrival; oldest-first so the queue ends up newest-first.
        let ordered = arrivals.sorted { ageSeconds($0.received_at) > ageSeconds($1.received_at) }
        for m in ordered {
            store.pushAuthRing(m.id)
            if AuthCode.isCodeKind(m.kind) {
                Task { await revealAndQueue(m) }
            }
        }
    }

    /// Auto-reveal a code message (audited) and enqueue the extracted code.
    private func revealAndQueue(_ m: SealedMeta) async {
        do {
            let revealed = try await APIClient.shared.revealSealed(m.id)
            store.pushAuthCode(AuthCodeEntry(meta: m, code: AuthCode.extract(revealed.body)))
        } catch {
            // Reveal failed (network / write-guard / already-consumed): show the
            // modal anyway so the human can jump to Auth. No code retained.
            store.pushAuthCode(AuthCodeEntry(meta: m, code: nil))
        }
    }
}
