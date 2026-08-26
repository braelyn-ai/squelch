// The recipient-list grammar, and the one operation in it with real
// consequences: moving somebody to Bcc.
//
// This suite exists because every failure mode here is silent. A list split on
// the wrong comma reports a person who does not exist and loses the one who
// did. A move that adds without removing leaves the address in a header every
// recipient can read while the sender believes it was blind. Nothing on screen
// contradicts either of those — the composer would show exactly what it was
// told — so the only place they can be caught is here.

import Foundation

@main
@MainActor
struct RecipientsTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        splittingRespectsQuotedNames()
        theKeyIsTheBareMailbox()
        aHalfTypedFragmentIsNobody()
        movingTakesTheAddressOutOfEveryOtherField()
        movingCarriesTheDisplayName()
        movingMatchesAcrossSpellings()
        movingSomebodyAlreadyThereIsANoOp()
        addingNeverPromotesSomebodyOutOfBcc()
        removingIsIdempotent()
        countsAndSlots()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - list grammar

    static func splittingRespectsQuotedNames() {
        equal(Recipients.split(""), [], "an empty list names nobody")
        equal(Recipients.split("   ,  , "), [], "separators alone name nobody")
        equal(Recipients.split("a@x.test"), ["a@x.test"], "one address")
        equal(
            Recipients.split("a@x.test, b@y.test"), ["a@x.test", "b@y.test"],
            "two addresses")
        // THE COMMA THAT IS NOT A SEPARATOR. Splitting here would report two
        // recipients, neither of whom is real, and drop John entirely.
        equal(
            Recipients.split("\"Doe, John\" <j@x.test>, b@y.test"),
            ["\"Doe, John\" <j@x.test>", "b@y.test"],
            "a comma inside a quoted display name is part of the name")
    }

    static func theKeyIsTheBareMailbox() {
        equal(Recipients.key("bob@x.test"), "bob@x.test", "a bare address is itself")
        equal(Recipients.key("Bob Smith <bob@x.test>"), "bob@x.test", "the angle pair wins")
        equal(Recipients.key("  BOB@X.TEST "), "bob@x.test", "case-folded and trimmed")
        equal(
            Recipients.key("\"Doe, John\" <j@x.test>"), "j@x.test",
            "a quoted name does not confuse the brackets")
        // A display name may legitimately contain angle brackets; the address is
        // always the LAST pair.
        equal(
            Recipients.key("\"<the boss>\" <b@x.test>"), "b@x.test",
            "the last angle pair is the address")
    }

    static func aHalfTypedFragmentIsNobody() {
        // A live field holds a fragment mid-typing. Two fragments must never
        // compare equal, or the second one typed would silently replace the
        // first somewhere else.
        equal(Recipients.key("bo"), "", "no @ is not an address yet")
        equal(Recipients.key(""), "", "empty is nobody")
        var r = Recipients(to: "alice@x.test")
        r.move("bo", to: .bcc)
        equal(r.to, "alice@x.test", "a fragment moves nothing")
        equal(r.bcc, "", "and lands nowhere")
    }

    // MARK: - the move

    static func movingTakesTheAddressOutOfEveryOtherField() {
        var r = Recipients(to: "alice@x.test, bob@x.test", cc: "carol@x.test")
        r.move("bob@x.test", to: .bcc)
        equal(r.to, "alice@x.test", "gone from To")
        equal(r.cc, "carol@x.test", "Cc untouched")
        equal(r.bcc, "bob@x.test", "landed in Bcc")

        // THE FAILURE THIS EXISTS FOR: an add that did not remove would leave
        // bob in To as well, so his address rides in a header every recipient
        // reads while the sender believes he was blind-copied.
        equal(r.slot(of: "bob@x.test"), RecipientSlot.bcc, "in exactly one field")
        equal(r.all.count, 3, "and counted exactly once")
    }

    static func movingCarriesTheDisplayName() {
        var r = Recipients(to: "Bob Smith <bob@x.test>")
        r.move("Bob Smith <bob@x.test>", to: .cc)
        equal(r.cc, "Bob Smith <bob@x.test>", "the token moves whole")
        equal(r.to, "", "and only once")
    }

    static func movingMatchesAcrossSpellings() {
        // The two lists spell the same person differently — which is ordinary,
        // since one was derived from a header and the other typed. Matching on
        // the bare mailbox is what keeps them one person.
        var r = Recipients(to: "Bob Smith <bob@x.test>", cc: "BOB@X.TEST")
        r.move("bob@x.test", to: .bcc)
        equal(r.to, "", "the named copy went")
        equal(r.cc, "", "so did the shouted one")
        equal(r.bcc, "bob@x.test", "one person, one field")
    }

    static func movingSomebodyAlreadyThereIsANoOp() {
        var r = Recipients(to: "alice@x.test", bcc: "bob@x.test")
        r.move("bob@x.test", to: .bcc)
        equal(r.bcc, "bob@x.test", "still there, exactly once")
        equal(r.to, "alice@x.test", "and nobody else moved")
    }

    static func addingNeverPromotesSomebodyOutOfBcc() {
        // Autocomplete offering somebody who is already blind-copied must not
        // quietly move them into the visible header. An explicit `move` is the
        // only thing allowed to do that.
        var r = Recipients(to: "alice@x.test", bcc: "bob@x.test")
        r.add("bob@x.test", to: .to)
        equal(r.to, "alice@x.test", "not added to To")
        equal(r.bcc, "bob@x.test", "left where the sender put them")

        r.add("carol@x.test", to: .cc)
        equal(r.cc, "carol@x.test", "somebody new does land")
    }

    static func removingIsIdempotent() {
        var r = Recipients(to: "alice@x.test, bob@x.test")
        r.remove("bob@x.test")
        equal(r.to, "alice@x.test", "removed")
        r.remove("bob@x.test")
        equal(r.to, "alice@x.test", "removing them again is the state you asked for")
        r.remove("nobody@x.test")
        equal(r.to, "alice@x.test", "and so is removing a stranger")
    }

    static func countsAndSlots() {
        let r = Recipients(
            to: "\"Doe, John\" <j@x.test>", cc: "b@y.test, c@z.test", bcc: "d@w.test")
        equal(r.count(.to), 1, "the quoted comma is not a second person")
        equal(r.count(.cc), 2, "two copies")
        equal(r.count(.bcc), 1, "one blind copy")
        equal(r.all.count, 4, "four people in all")
        equal(r.slot(of: "C@Z.TEST"), RecipientSlot.cc, "found case-insensitively")
        equal(r.slot(of: "stranger@x.test"), nil, "a stranger is in no field")
    }

    // MARK: - assert

    static func equal<T: Equatable>(_ got: T?, _ want: T?, _ what: String) {
        checks += 1
        if got != want {
            failures += 1
            print("  FAIL \(what): got \(String(describing: got)), want \(String(describing: want))")
        }
    }
}
