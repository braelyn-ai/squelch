// WHERE THE RING LIVES: device-local, per ACCOUNT, and nowhere near the daemon.
// The same shape as ThreadStyleLedger and AuthDecisions, for the same two
// reasons — there is no server field for this, and the thing being remembered
// belongs to one mailbox. A query typed at the work account has no business
// surfacing under the personal one's search field, which is exactly what a
// single global list would do.
//
// The FOLD is `RecentSearchRing`, which is pure and asserted in test.sh; this
// file is only its storage and its account scoping. Nothing here decides what a
// ring should contain.

import Foundation
import SwiftUI

@MainActor
@Observable
final class RecentSearchStore {
    static let shared = RecentSearchStore()

    /// Base name of the stored list; the live key is this scoped to the active
    /// account. nil when there is no live account — nothing to read, nowhere to
    /// write, which is the Connect gate's state.
    private static let keyBase = "passband.search.recents"
    private static var key: String? {
        guard let id = AccountManager.shared.activeId else { return nil }
        return AccountIndex.scopedKey(keyBase, id)
    }

    /// Newest first. Read straight from views: @Observable means the empty
    /// state re-renders when a search is remembered under it.
    private(set) var queries: [String] = []

    private init() { reload() }

    /// Re-read for whatever account is live NOW. Called by the account switch
    /// AFTER the new id is committed, beside the other ledgers: this key is
    /// derived from that id, so reloading any earlier re-reads the account that
    /// just went away.
    func reload() {
        queries = Self.key.flatMap { UserDefaults.standard.stringArray(forKey: $0) } ?? []
    }

    /// Remember a query the reader has ACTED on. Callers pass the term the hits
    /// on screen were fetched for, never the live field text — see the panel's
    /// `remember()` and docs/SEARCH.md §4.2.
    func record(_ query: String) {
        guard let key = Self.key else { return }
        let next = RecentSearchRing.adding(query, to: queries)
        // Nothing moved: the ring refused it, or it is already the newest.
        // Skipping the write keeps re-opening the same search from touching
        // disk on every hit.
        guard next != queries else { return }
        queries = next
        UserDefaults.standard.set(next, forKey: key)
    }

    /// Forget the lot. Search terms are the reader's own words about their own
    /// mail, so the list that keeps them has to have a way out that is not
    /// "delete the app" — the button is in the empty state's own header, beside
    /// what it clears.
    func clear() {
        queries = []
        guard let key = Self.key else { return }
        UserDefaults.standard.removeObject(forKey: key)
    }
}
