// What a submitted search does to the remembered ones. A ring is the shape of
// code a green build says nothing about — it compiles just as happily while
// keeping the wrong ten things in the wrong order — so every rule in
// RecentSearchRing is held up by a fixture here: newest first, one entry per
// question the daemon would answer identically, ten of them, and an absurd
// query refused rather than cut down to a different search.

import Foundation

@main
@MainActor
struct RecentSearchRingTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        newestFirst()
        reAskingMovesItUp()
        theSpellingKeptIsTheNewestOne()
        tenAndTheEleventhPushesTheFirstOut()
        nothingWorthRememberingIsRefused()
        aLongQueryIsRefusedNotTruncated()
        anOlderListHealsOnTheWayPast()
        identityIsWhatTheDaemonWouldDo()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    static func newestFirst() {
        adding("wifi", to: [], gives: ["wifi"])
        adding("venmo", to: ["wifi"], gives: ["venmo", "wifi"])
        adding("rent", to: ["venmo", "wifi"], gives: ["rent", "venmo", "wifi"])
        // Ends are trimmed: the field hands over whatever was typed, and a
        // trailing space is not a different search.
        adding("  wifi  ", to: [], gives: ["wifi"])
    }

    static func reAskingMovesItUp() {
        // The same question asked again is not a second entry — it is the top
        // of the list, which is the whole reason the ring is ordered.
        adding("wifi", to: ["venmo", "wifi", "rent"], gives: ["wifi", "venmo", "rent"])
        adding("rent", to: ["venmo", "wifi", "rent"], gives: ["rent", "venmo", "wifi"])
        // Already newest: still one entry, still on top.
        adding("venmo", to: ["venmo", "wifi"], gives: ["venmo", "wifi"])
    }

    static func theSpellingKeptIsTheNewestOne() {
        // Search folds case and splits on whitespace, so these are one search
        // asked twice. What survives is how the reader typed it THIS time.
        adding("Venmo Receipt", to: ["venmo receipt"], gives: ["Venmo Receipt"])
        adding("venmo  receipt", to: ["Venmo Receipt"], gives: ["venmo  receipt"])
        adding("FROM:dan@example.com", to: ["from:dan@example.com"],
            gives: ["FROM:dan@example.com"])
    }

    static func tenAndTheEleventhPushesTheFirstOut() {
        var ring: [String] = []
        for i in 1...12 { ring = RecentSearchRing.adding("q\(i)", to: ring) }
        checks += 1
        if ring.count != RecentSearchRing.capacity {
            failures += 1
            print("FAIL cap: kept \(ring.count), expected \(RecentSearchRing.capacity)")
        }
        // Newest at the head, and the two oldest gone rather than the two
        // newest — an eviction from the wrong end keeps the count right and the
        // list useless.
        adding("q13", to: ring, gives: (1...13).reversed().prefix(10).map { "q\($0)" })
    }

    static func nothingWorthRememberingIsRefused() {
        let ring = ["wifi"]
        adding("", to: ring, gives: ring)
        adding("   ", to: ring, gives: ring)
        adding("\n\t ", to: ring, gives: ring)
        // Foundation's whitespace set is wider than Swift's `isWhitespace`, and
        // takes a zero-width space with it — so this trims to "" rather than
        // reaching the fold. Asserted because it is the reader's clipboard that
        // decides, not the rule anybody had in mind.
        adding("\u{200B}", to: ring, gives: ring)
        adding(" \u{00A0}\u{2009} ", to: ring, gives: ring)
    }

    static func aLongQueryIsRefusedNotTruncated() {
        let ring = ["wifi"]
        let atCap = String(repeating: "a", count: RecentSearchRing.maxLength)
        adding(atCap, to: ring, gives: [atCap, "wifi"])
        // One over, and the ring is left ALONE. A truncated query is a
        // different search, and re-running one would answer a question nobody
        // asked.
        adding(atCap + "a", to: ring, gives: ring)
        // Trimming happens first, so a long query padded with spaces is judged
        // on the words.
        adding("  " + atCap + "  ", to: ring, gives: [atCap, "wifi"])
    }

    static func anOlderListHealsOnTheWayPast() {
        // Two spellings of one search, as a build with a looser rule could have
        // left them: the next addition dedupes the whole ring, not just its
        // head, so the list gets shorter instead of carrying the pair forever.
        adding("rent", to: ["Wifi", "wifi", "wifi "], gives: ["rent", "Wifi"])
    }

    static func identityIsWhatTheDaemonWouldDo() {
        same("wifi password", "WiFi   Password")
        same(" wifi\tpassword ", "wifi password")
        same("from:Dan@Example.com", "FROM:dan@example.com")
        // And no more than that: different words, and the same words in a
        // different order, are different searches.
        different("wifi password", "wifi")
        different("wifi password", "password wifi")
        different("venmo", "venmos")
    }

    // MARK: - helpers

    static func adding(_ query: String, to ring: [String], gives expected: [String]) {
        checks += 1
        let got = RecentSearchRing.adding(query, to: ring)
        if got != expected {
            failures += 1
            print(
                "FAIL adding(\(query.debugDescription), to: \(ring)) = \(got), expected \(expected)"
            )
        }
    }

    static func same(_ a: String, _ b: String) {
        checks += 1
        if RecentSearchRing.identity(of: a) != RecentSearchRing.identity(of: b) {
            failures += 1
            print("FAIL \(a.debugDescription) and \(b.debugDescription) should be one search")
        }
    }

    static func different(_ a: String, _ b: String) {
        checks += 1
        if RecentSearchRing.identity(of: a) == RecentSearchRing.identity(of: b) {
            failures += 1
            print("FAIL \(a.debugDescription) and \(b.debugDescription) should be two searches")
        }
    }
}
