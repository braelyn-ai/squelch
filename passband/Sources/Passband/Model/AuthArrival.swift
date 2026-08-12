// ARRIVAL DETECTION for the 2FA "present, don't read" flow: watches the polled
// sealed metadata and fires once per genuinely-new auth message — a countdown
// ring, plus (for otp/verification only) an audited auto-reveal and a code modal.
//
// Fresh means NOT in the persisted seen-set AND received within ~2 minutes, and
// the first run seeds every current id silently so a backlog stays quiet. The
// seen-set plus ring expiry ARE the read state — read-marking is impossible
// under gmail.readonly.
//
// The seen-set is PER ACCOUNT: the ids in it are one daemon's SQLite ints and
// mean something else entirely in another mailbox.

import Foundation

@MainActor
final class AuthArrival {
    static let shared = AuthArrival()

    /// Base name of the persisted seen-set; the live key is this scoped to the
    /// account (see `seenKey`).
    private static let seenKeyBase = "passband.auth-seen"
    private static let seenCap = 200
    /// A message older than this on first sight is treated as history.
    private static let freshWindow: TimeInterval = 2 * 60

    private var seen: Set<Int>?
    private var seeded = false
    /// WHICH account the in-memory set was read for. Answered explicitly
    /// rather than assumed, because a switch tears the store down before it
    /// commits the new id, and an observation landing in that window would
    /// otherwise seed the new account's watch from the old account's set.
    private var boundTo: UUID?

    private var store: AppStore { AppStore.shared }

    private init() {}

    private static func seenKey(_ account: UUID) -> String {
        AccountIndex.scopedKey(seenKeyBase, account)
    }

    private func loadSeen(_ account: UUID) -> Set<Int> {
        let raw = UserDefaults.standard.array(forKey: Self.seenKey(account)) as? [Int] ?? []
        return Set(raw)
    }

    private func saveSeen(_ set: Set<Int>, _ account: UUID) {
        // Cap to the most-recent ids (numeric order == arrival order).
        let capped = Array(set.sorted().suffix(Self.seenCap))
        UserDefaults.standard.set(capped, forKey: Self.seenKey(account))
    }

    /// An account switch. The set in memory belongs to the account that just
    /// went away — dropped rather than saved, being already persisted under
    /// that account's own key, and re-read for whoever is live at the next
    /// observation.
    func resetForSwitch() {
        seen = nil
        seeded = false
        boundTo = nil
    }

    /// Seconds since an ISO stamp; huge when missing/invalid (=> treat as old).
    private func ageSeconds(_ iso: String?) -> TimeInterval {
        guard let d = Fmt.date(iso) else { return .greatestFiniteMagnitude }
        return Date().timeIntervalSince(d)
    }

    /// Call whenever the sealed list changes (the poller drives this).
    func observe(sealed: [SealedMeta]) {
        // No live account means nothing to record this against — and no key to
        // record it under.
        guard let account = AccountManager.shared.activeId else { return }
        // A different account (or the first observation) re-reads under its
        // key and re-seeds: the new mailbox's current sealed mail is ITS
        // backlog, however loudly the old one's would have rung.
        if boundTo != account || seen == nil {
            boundTo = account
            seen = loadSeen(account)
            seeded = false
        }
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
            if changed { saveSeen(seen, account) }
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
        saveSeen(seen, account)

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
        // The reveal is one account's; the modal it feeds is the whole app's.
        // A code that arrives after a switch belongs to a mailbox the human is
        // no longer looking at, and `m.id` names something else in this one.
        let e = store.epoch
        do {
            let revealed = try await APIClient.shared.revealSealed(m.id)
            guard store.isCurrent(e) else { return }
            store.pushAuthCode(AuthCodeEntry(meta: m, code: AuthCode.extract(revealed.body)))
        } catch {
            guard store.isCurrent(e) else { return }
            // Reveal failed (network / write-guard / already-consumed): show the
            // modal anyway so the human can jump to Auth. No code retained.
            store.pushAuthCode(AuthCodeEntry(meta: m, code: nil))
        }
    }
}
