// THE THREADS THE READER HAS ANSWERED THE STYLE QUESTION FOR, and nothing else:
// a thread with no entry here is drawn the way `Prefs.threadStyle` says, so the
// global default keeps governing every thread nobody has had an opinion about.
//
// Device-local and per ACCOUNT, the same shape as AuthDecisions and for the same
// reason: there is no server field for this, and the map is keyed by thread id,
// which is one daemon's.

import Foundation
import SwiftUI

@MainActor
@Observable
final class ThreadStyleLedger {
    static let shared = ThreadStyleLedger()

    /// Base name of the stored map; the live key is this scoped to the account.
    /// nil when there is no live account — nothing to read, nowhere to write.
    private static let keyBase = "passband.thread-style"
    private static var key: String? {
        guard let id = AccountManager.shared.activeId else { return nil }
        return AccountIndex.scopedKey(keyBase, id)
    }
    /// Cap the stored map so it cannot grow without bound.
    private static let cap = 500

    private var store: [String: String] = [:]

    private init() { reload() }

    /// Re-read the ledger for whatever account is live NOW. Called by the
    /// account switch, AFTER the new id is committed — this key is derived
    /// from it, so reloading any earlier would re-read the account that just
    /// went away.
    func reload() {
        store =
            Self.key.flatMap { UserDefaults.standard.dictionary(forKey: $0) as? [String: String] }
            ?? [:]
    }

    /// This thread's own style, or nil when it has never been given one — which
    /// is the common case and means "whatever the default is today".
    func style(_ threadId: String) -> ThreadStyle? {
        store[threadId].flatMap(ThreadStyle.init(rawValue:))
    }

    func set(_ threadId: String, _ style: ThreadStyle) {
        guard let key = Self.key else { return }
        var next = store
        next[threadId] = style.rawValue
        // A thread id carries no order, so neither does the eviction: the cap
        // is a bound and nothing more, and a dropped entry costs one keypress
        // to state again. The thread just written is never a candidate.
        if next.count > Self.cap {
            let stale = next.keys.filter { $0 != threadId }.sorted()
            for k in stale.prefix(next.count - Self.cap) { next.removeValue(forKey: k) }
        }
        store = next
        UserDefaults.standard.set(next, forKey: key)
    }

    /// Forget this thread's opinion, putting it back under the global default.
    /// What a toggle BACK to the default does: an exception that agrees with the
    /// rule is not an exception, and keeping it would freeze the thread against
    /// a later change in Settings.
    func clear(_ threadId: String) {
        guard let key = Self.key, store[threadId] != nil else { return }
        var next = store
        next.removeValue(forKey: threadId)
        store = next
        UserDefaults.standard.set(next, forKey: key)
    }
}
