// The ledger between two reporters and one reader. These pin the asymmetry
// that is the whole point of the type: a refetch may be asked for over and
// over, an announcement happens once.
//
// Getting it wrong is invisible in the direction that matters. A refetch that
// stops repeating leaves the reply the human is waiting for permanently absent
// from a thread they are staring at, and nothing in the app ever says so.

import Foundation

@main
@MainActor
struct ThreadArrivalsTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        theArrival()
        toldOnceEvenWhenSeenTwice()
        refetchKeepsAsking()
        nothingForAnotherThread()
        nothingBeforeTheReaderHasACopy()
        nothingAlreadyOnScreen()
        aSwitchForgets()
        aReopenRemembers()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    static func ledger(on thread: String = "t1") -> ThreadArrivals {
        var l = ThreadArrivals()
        l.reset(to: thread)
        return l
    }

    /// A reply lands in the thread on screen: fetch it, and say so.
    static func theArrival() {
        var l = ledger()
        expect(
            l.admit(thread: "t1", message: 41, held: 40) == Admission(
                refetch: true, announce: true),
            "a newer message in the open thread is both fetched and announced")
    }

    /// The live feed and the poll both carry the same email. The reader is
    /// told about it once.
    static func toldOnceEvenWhenSeenTwice() {
        var l = ledger()
        _ = l.admit(thread: "t1", message: 41, held: 40)
        let again = l.admit(thread: "t1", message: 41, held: 40)
        expect(!again.announce, "the second reporter of one arrival announces nothing")
        expect(again.refetch, "and still asks for the copy the reader has not got")
    }

    /// A refetch that FAILED leaves the reader holding the old copy, so the
    /// next poll has to be able to ask again — for as long as it takes.
    static func refetchKeepsAsking() {
        var l = ledger()
        for _ in 0..<5 { _ = l.admit(thread: "t1", message: 41, held: 40) }
        expect(
            l.admit(thread: "t1", message: 41, held: 40).refetch,
            "the ask repeats until the reader has adopted it")
        // ...and stops the moment they have.
        expect(
            l.admit(thread: "t1", message: 41, held: 41) == .ignore,
            "and stops on its own once the reader holds it")
    }

    /// Mail for a thread nobody has open. The bands and the banner have it
    /// covered; this type must not touch the reader for it.
    static func nothingForAnotherThread() {
        var l = ledger()
        expect(
            l.admit(thread: "t2", message: 99, held: 40) == .ignore,
            "another thread's mail is not this reader's business")
    }

    /// The viewer has not adopted anything yet, so the load in flight is
    /// already going to bring this message. Racing it would be a second fetch
    /// for one arrival.
    static func nothingBeforeTheReaderHasACopy() {
        var l = ledger()
        expect(
            l.admit(thread: "t1", message: 41, held: nil) == .ignore,
            "a thread still loading needs no help arriving")
    }

    /// What the poll reports on EVERY tick: the same rows, unresolved, sitting
    /// in the bands. None of it is news.
    static func nothingAlreadyOnScreen() {
        var l = ledger()
        expect(
            l.admit(thread: "t1", message: 40, held: 40) == .ignore,
            "the newest message the reader holds is not an arrival")
        expect(
            l.admit(thread: "t1", message: 12, held: 40) == .ignore,
            "and neither is anything under it")
    }

    /// Moving to another email starts a fresh ledger: its own mail has never
    /// been announced here.
    static func aSwitchForgets() {
        var l = ledger()
        _ = l.admit(thread: "t1", message: 41, held: 40)
        l.reset(to: "t2")
        expect(
            l.admit(thread: "t2", message: 41, held: 40).announce,
            "another thread's message 41 is a different email")
        // And back again: t1's ledger was dropped on the way out, which is
        // correct — the viewer reloads t1 from scratch when it is reopened.
        l.reset(to: "t1")
        expect(
            l.admit(thread: "t1", message: 41, held: 40).announce,
            "coming back re-reads the thread, so the arrival is news again")
    }

    /// Opening the SAME thread again (done+next lands back on it, a banner tap
    /// on the email already up) leaves the viewer mounted and holding what it
    /// already fetched. Forgetting here would announce that stack twice.
    static func aReopenRemembers() {
        var l = ledger()
        _ = l.admit(thread: "t1", message: 41, held: 40)
        l.reset(to: "t1")
        expect(
            !l.admit(thread: "t1", message: 41, held: 40).announce,
            "a reopen of the same thread announces nothing a second time")
    }

    static func expect(_ cond: Bool, _ what: String) {
        checks += 1
        if !cond {
            failures += 1
            print("  FAIL: \(what)")
        }
    }
}
