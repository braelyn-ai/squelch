// THE LAST FEW QUERIES, as a value: what a submitted search does to the list of
// remembered ones. Pure Foundation, no storage and no SwiftUI, so test.sh can
// hold the rules up alone — a ring is the kind of code a green build says
// nothing about, because it compiles just as happily while remembering the
// wrong ten things in the wrong order.
//
// THREE RULES, and each is a decision rather than a detail:
//
//   * NEWEST FIRST, DEDUPED BY WHAT THE DAEMON WOULD DO WITH IT. Search folds
//     case and splits on whitespace, so `Venmo` and `venmo  receipt` vs
//     `venmo receipt` are not two searches to offer twice — they are one search
//     asked twice, and the second asking moves it to the top rather than
//     growing the list. The SPELLING kept is the newest one: the reader's most
//     recent way of typing it is the one they will recognise.
//   * TEN, and the eleventh pushes the first out. The list lives under the
//     search field in a 460pt strip; a longer one is a screen of history where
//     a handful of shortcuts was wanted.
//   * AN ABSURDLY LONG QUERY IS REFUSED, NOT TRUNCATED. A truncated query is a
//     DIFFERENT SEARCH, and re-running one from this list would quietly answer
//     a question nobody asked. Refusing costs the reader nothing: the query
//     they just ran is still in the field.

import Foundation

enum RecentSearchRing {
    /// How many queries are kept.
    static let capacity = 10

    /// The longest query worth remembering, in characters. Generous enough for
    /// a question-shaped search (the deeper lane takes whole sentences) and
    /// short enough that a pasted document never lands in UserDefaults.
    static let maxLength = 200

    /// What makes two queries the SAME question: case folded, whitespace runs
    /// collapsed, ends trimmed — exactly the differences the daemon's tokenizer
    /// would erase before ranking anything.
    static func identity(of query: String) -> String {
        query.split(whereSeparator: { $0.isWhitespace || $0.isNewline })
            .map { $0.lowercased() }
            .joined(separator: " ")
    }

    /// Fold a submitted query into the ring. Returns the ring UNCHANGED when
    /// the query is not worth remembering, so a caller can compare and skip the
    /// write.
    static func adding(_ query: String, to ring: [String]) -> [String] {
        let kept = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !kept.isEmpty, kept.count <= maxLength else { return ring }
        // Blank-but-not-empty (a query of pure whitespace arrives trimmed to ""
        // above, but a lone zero-width run does not) has no identity to match
        // and nothing to search for either.
        guard !identity(of: kept).isEmpty else { return ring }
        // ONE PASS OVER THE NEW QUERY AND THE OLD RING, first occurrence of
        // each identity wins. That is what makes the newest spelling the kept
        // one — and it re-folds the ENTIRE ring rather than only matching the
        // head against it, so a list left holding two spellings of one search
        // (by an older build, or a looser rule) gets shorter on the way past
        // instead of carrying the pair forever.
        var seen = Set<String>()
        var next: [String] = []
        for candidate in [kept] + ring {
            let id = identity(of: candidate)
            guard !id.isEmpty, seen.insert(id).inserted else { continue }
            next.append(candidate)
            if next.count == capacity { break }
        }
        return next
    }
}
