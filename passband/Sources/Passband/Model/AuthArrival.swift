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
// mean something else entirely in another mailbox. It is also SHARED — the
// class below runs this flow for the account on screen, and
// `BackgroundAuthWatch` runs a much thinner one for every account that is not,
// both against the same stored set. `AuthSeenSet` is that shared rule, kept in
// one place so the two can never disagree about what "already seen" means.

import Foundation

// MARK: - the shared seen-set

/// One account's record of which auth messages have already been reacted to,
/// plus the freshness rule that decides whether a newly-seen one is worth
/// making a noise about at all.
///
/// TWO WATCHERS, ONE SET. `AuthArrival` folds observations in for the live
/// account; `BackgroundAuthWatch` does it for the accounts that are not. They
/// are exclusive per account — `AccountManager` starts one at the exact moment
/// it stops the other — and both persist under the same scoped key, so a
/// message one of them has already answered for is never answered for a second
/// time by the other after a switch.
///
/// A value type carrying its own account id: the key it reads and writes is
/// derived from that id and nothing else, so a set can never be saved back
/// under an account it was not loaded for.
struct AuthSeenSet {
    /// Base name of the persisted set; the live key is this scoped to the
    /// account (see `key`). Mirrored in `AccountIndex.scopedDefaultsKeys`,
    /// which is what carries it across a migration and drops it on a removal.
    private static let keyBase = "passband.auth-seen"
    private static let cap = 200
    /// A message older than this on first sight is treated as history.
    static let freshWindow: TimeInterval = 2 * 60

    /// Whose set this is. Read by the owners to notice that the live account
    /// has moved out from under them.
    let accountId: UUID

    private var ids: Set<Int>
    /// Whether this instance has taken its first look yet. Per INSTANCE and
    /// deliberately not persisted: the silent seeding below is about what was
    /// already sitting in the mailbox when this watcher started watching.
    private var seeded = false

    init(accountId: UUID) {
        self.accountId = accountId
        let raw = UserDefaults.standard.array(forKey: Self.key(accountId)) as? [Int] ?? []
        ids = Set(raw)
    }

    static func key(_ accountId: UUID) -> String {
        AccountIndex.scopedKey(keyBase, accountId)
    }

    /// Seconds since an ISO stamp; huge when missing/invalid (=> treat as old).
    /// Shared with the callers, which order what comes back by it.
    static func age(_ iso: String?, now: Date = Date()) -> TimeInterval {
        guard let d = Fmt.date(iso) else { return .greatestFiniteMagnitude }
        return now.timeIntervalSince(d)
    }

    /// Fold one observation of the sealed list in and answer with the messages
    /// worth reacting to: not already seen, AND received recently enough to
    /// still be live.
    ///
    /// The FIRST observation answers with nothing. Whatever is sealed when a
    /// watcher starts is that mailbox's backlog, and seeding it silently is
    /// what stops a launch — or an account switch — from firing a fortnight of
    /// history at the human.
    mutating func arrivals(in sealed: [SealedMeta], now: Date = Date()) -> [SealedMeta] {
        if !seeded {
            seeded = true
            var changed = false
            for m in sealed where ids.insert(m.id).inserted { changed = true }
            if changed { save() }
            return []
        }

        var fresh: [SealedMeta] = []
        for m in sealed {
            // Marked the moment it is seen, so a re-poll can never fire twice
            // for one message. Late-arriving history is recorded here and then
            // stays quiet, which is the whole difference between the two.
            guard ids.insert(m.id).inserted else { continue }
            if Self.age(m.received_at, now: now) <= Self.freshWindow { fresh.append(m) }
        }
        // Written only when something actually fires. An id seen but too old to
        // fire costs nothing to re-examine after a restart — it will be just as
        // old then — and not writing keeps a quiet mailbox from rewriting this
        // key on every poll.
        guard !fresh.isEmpty else { return [] }
        save()
        return fresh
    }

    private func save() {
        // Cap to the most-recent ids (numeric order == arrival order).
        let capped = Array(ids.sorted().suffix(Self.cap))
        UserDefaults.standard.set(capped, forKey: Self.key(accountId))
    }
}

// MARK: - the live account's arrival flow

@MainActor
final class AuthArrival {
    static let shared = AuthArrival()

    /// The live account's seen-set, or nil before the first observation. Which
    /// account it belongs to is answered explicitly by the set itself rather
    /// than assumed, because a switch tears the store down before it commits
    /// the new id, and an observation landing in that window would otherwise
    /// seed the new account's watch from the old account's set.
    private var seen: AuthSeenSet?

    private var store: AppStore { AppStore.shared }

    private init() {}

    /// An account switch. The set in memory belongs to the account that just
    /// went away — dropped rather than saved, being already persisted under
    /// that account's own key, and re-read for whoever is live at the next
    /// observation. The background watcher that picks that account up reads
    /// the same key, from disk, for the same reason.
    func resetForSwitch() {
        seen = nil
    }

    /// Call whenever the sealed list changes (the poller drives this).
    func observe(sealed: [SealedMeta]) {
        // No live account means nothing to record this against — and no key to
        // record it under.
        guard let account = AccountManager.shared.activeId else { return }
        // A different account (or the first observation) re-reads under its
        // key and re-seeds: the new mailbox's current sealed mail is ITS
        // backlog, however loudly the old one's would have rung.
        if seen?.accountId != account { seen = AuthSeenSet(accountId: account) }
        let arrivals = seen?.arrivals(in: sealed) ?? []
        guard !arrivals.isEmpty else { return }

        // Ring for every arrival; oldest-first so the queue ends up newest-first.
        let ordered = arrivals.sorted {
            AuthSeenSet.age($0.received_at) > AuthSeenSet.age($1.received_at)
        }
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
