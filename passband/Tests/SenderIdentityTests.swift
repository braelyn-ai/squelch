// Which senders leave the device, and which never do.
//
// `eligibleFaviconDomain` is the whole privacy boundary in SenderIdentity: it
// answers nil for a human correspondent, and that nil is the reason the
// correspondent graph stays local. A heuristic guarding something that quiet
// is asserted rather than reasoned about — a widening that reaches one address
// too far does not crash, does not fail a build, and shows up as an icon
// nobody looks at twice.
//
// The display-name arm is the new one, and it is the one most of these are
// about: a brand that signs its bulk mail with its own name is a brand,
// however its ESP spelled the mailbox.

import Foundation

@main
@MainActor
struct SenderIdentityTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        brandsAreRecognised()
        peopleAreNot()
        theBoundaryHolds()
        namesStayCorrect()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    static func brandsAreRecognised() {
        // The original arm: the local-part IS the domain's label.
        expect(SenderID.isBrand("eBay <eBay@eBay.com>"), "local-part equal to the label")
        expect(SenderID.isBrand("ebay@ebay.com"), "the same with no display name at all")

        // The arm this suite exists for. Bulk mail leaves from whatever
        // mailbox the sender's ESP minted, and none of these is on
        // `robotLocals` — before the display name counted, every one of them
        // was treated as a person.
        expect(SenderID.isBrand("Airbnb <express@airbnb.com>"), "display name over a routing box")
        expect(SenderID.isBrand("Notion <team@mail.notion.so>"), "and through a mail subdomain")
        expect(SenderID.isBrand("STRIPE <e@stripe.com>"), "case is not part of the match")
        expect(SenderID.isBrand("Booking.com <news-2938@booking.com>"), "punctuation is not either")

        // A robot local-part still stands on its own, with or without a name.
        expect(SenderID.isRobot("no-reply@stripe.com"), "the robot arm is untouched")
        expect(SenderID.isRobot("\"Chase\" <no.reply.alerts@chase.com>"), "squashed markers too")
    }

    static func peopleAreNot() {
        expect(!SenderID.isBrand("Sarah Chen <sarah@acme.com>"), "a person at a company")
        expect(!SenderID.isBrand("sarah@acme.com"), "a bare human address")
        expect(!SenderID.isBrand("Sarah <sarah@gmail.com>"), "a first name on a consumer host")

        // EXACT, on letters and digits. A prefix match would read the address
        // below as the brand Sam, which is a person's mail leaving the device.
        expect(!SenderID.isBrand("Samuel Smith <ssmith@sam.com>"), "no prefix matching")
        // And a qualified team name is not the company asserting itself.
        expect(!SenderID.isBrand("Airbnb Support <express@airbnb.com>"), "\"Airbnb Support\" is not")
        expect(!SenderID.isBrand("Airbnb, Inc. <express@airbnb.com>"), "nor \"Airbnb, Inc.\"")

        // A display name that is just the address again is not a name.
        expect(!SenderID.isBrand("sarah@acme.com <sarah@acme.com>"), "the address is not a name")
    }

    static func theBoundaryHolds() {
        // What actually reaches the network, which is the only question that
        // matters here. A domain, never an address, and only for a sender that
        // named itself a service.
        expect(
            SenderID.eligibleFaviconDomain("Airbnb <express@airbnb.com>") == "airbnb.com",
            "a brand resolves to its registrable domain")
        expect(
            SenderID.eligibleFaviconDomain("no-reply@stripe.com") == "stripe.com",
            "so does a robot")
        expect(
            SenderID.eligibleFaviconDomain("Sarah Chen <sarah@acme.com>") == nil,
            "A HUMAN CORRESPONDENT NEVER LEAVES THE DEVICE")
        expect(
            SenderID.eligibleFaviconDomain("Samuel Smith <ssmith@sam.com>") == nil,
            "nor one who nearly matched their own domain")
        expect(
            SenderID.eligibleFaviconDomain("express@airbnb.com") == nil,
            "an unnamed routing box stays local — the brand is not asserted")
        expect(
            SenderID.eligibleFaviconDomain("Bob") == nil,
            "and a sender with no domain has nothing to ask about")
    }

    static func namesStayCorrect() {
        // `displayName` calls `isBrand` on its way to an answer, so a change to
        // one is a change to the other. These pin that the rows still read the
        // way they did.
        expect(
            SenderID.displayName("Airbnb <express@airbnb.com>") == "Airbnb",
            "a display name still wins outright")
        expect(
            SenderID.displayName("ebay@ebay.com") == "ebay",
            "a brand with no name shows its local-part as given")
        expect(
            SenderID.displayName("no-reply@stripe.com") == "Stripe",
            "a robot shows the capitalized domain label")
        expect(
            SenderID.displayName("sarah@acme.com") == "sarah@acme.com",
            "and everyone else shows the address")

        // Initials come off the NAME, never the full address.
        expect(SenderID.initials("Sarah Chen <sarah@acme.com>") == "SC", "two words, two letters")
        expect(SenderID.initials("bboynton97@gmail.com") == "BB", "never the domain's letters")
    }

    // MARK: - harness

    static func expect(_ ok: Bool, _ label: String) {
        checks += 1
        if !ok {
            failures += 1
            print("FAIL: \(label)")
        }
    }
}
