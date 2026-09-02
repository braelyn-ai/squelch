// The `from:` operator while it is being typed: exactly when the sender menu is
// up, what it is searching for, and what accepting a sender does to the text.
// The rule is "the trailing token is the caret", and every case here is one a
// reader can produce by typing, so a failure reads as a keystroke, not a state.

import Foundation

@main
@MainActor
struct FromOperatorTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        theMenuOpensOnTheTrailingToken()
        aSpaceClosesIt()
        anEarlierOperatorIsFinishedBusiness()
        acceptingCompletesTheOperator()
        acceptingOutsideTheOperatorChangesNothing()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    static func theMenuOpensOnTheTrailingToken() {
        fragment(of: "from:", is: "")
        fragment(of: "from:d", is: "d")
        fragment(of: "from:dan@ex", is: "dan@ex")
        fragment(of: "wifi password from:abst", is: "abst")
        // The daemon's parser is case-insensitive about the operator; so is the
        // menu, or `From:` would search for the literal word and offer nobody.
        fragment(of: "From:Dan", is: "Dan")
        fragment(of: "FROM:", is: "")
        // Other tokens, other operators, and bare words open nothing.
        fragment(of: "", is: nil)
        fragment(of: "wifi", is: nil)
        fragment(of: "after:2026-01-01", is: nil)
        fragment(of: "fromage", is: nil)
        fragment(of: "from", is: nil)
        // A token that merely CONTAINS the operator is not one.
        fragment(of: "xfrom:dan", is: nil)
    }

    static func aSpaceClosesIt() {
        fragment(of: "from:dan ", is: nil)
        fragment(of: "from:dan@example.com  ", is: nil)
        fragment(of: "from:\n", is: nil)
        fragment(of: " ", is: nil)
    }

    static func anEarlierOperatorIsFinishedBusiness() {
        fragment(of: "from:dan wifi", is: nil)
        fragment(of: "from:dan from:", is: "")
        fragment(of: "from:dan after:2026 from:j", is: "j")
    }

    static func acceptingCompletesTheOperator() {
        accepting("dan@example.com", in: "from:d", gives: "from:dan@example.com ")
        accepting("dan@example.com", in: "from:", gives: "from:dan@example.com ")
        accepting(
            "abstract@convex.dev", in: "wifi password from:abst",
            gives: "wifi password from:abstract@convex.dev ")
        // The operator's spelling is normalised on accept: the daemon reads
        // either, and one spelling is easier to look at.
        accepting("dan@example.com", in: "From:D", gives: "from:dan@example.com ")
        // Only the trailing operator is rewritten.
        accepting("j@example.com", in: "from:dan from:j", gives: "from:dan from:j@example.com ")
    }

    static func acceptingOutsideTheOperatorChangesNothing() {
        accepting("dan@example.com", in: "wifi", gives: "wifi")
        accepting("dan@example.com", in: "from:dan ", gives: "from:dan ")
        accepting("dan@example.com", in: "", gives: "")
    }

    // MARK: - helpers

    static func fragment(of query: String, is expected: String?) {
        checks += 1
        let got = FromOperator.fragment(in: query)
        if got != expected {
            failures += 1
            print("FAIL fragment(\(query.debugDescription)) = \(String(describing: got)), expected \(String(describing: expected))")
        }
    }

    static func accepting(_ address: String, in query: String, gives expected: String) {
        checks += 1
        let got = FromOperator.accepting(address, in: query)
        if got != expected {
            failures += 1
            print("FAIL accepting(\(address), in: \(query.debugDescription)) = \(got.debugDescription), expected \(expected.debugDescription)")
        }
    }
}
