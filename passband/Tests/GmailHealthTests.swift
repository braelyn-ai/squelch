// The wire contract behind the mailbox-disconnected banner, and the timestamp
// trap underneath it.
//
// The banner's whole job is to be the first thing that ever tells a person
// their mail stopped. Everything it renders comes off one small object on
// /client/stats, so a decode that quietly yields nil here is a banner that
// never appears — which is exactly the silence it was built to end, restored
// through the back door.
//
// The stamp is the sharp edge. `Fmt.date` tries a fractional parser and a plain
// one because the daemon emits fractional seconds on some fields and not
// others, and EITHER formatter alone returns nil for the other shape. The
// first cut of this feature reached for a bare `ISO8601DateFormatter`, which
// happened to match what the daemon sends today and would have started
// returning nil the moment that field's source changed — dropping "expired 3
// hours ago" with nothing announcing the loss. Both shapes are pinned below so
// that stays a fixed bug rather than a rediscovered one.

import Foundation

@main
@MainActor
struct GmailHealthTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        aConnectedMailboxSaysSoAndOffersNothing()
        aDeadCredentialCarriesTheRepair()
        selfHostIsToldWithoutALink()
        silenceIsNotGoodNews()
        bothStampShapesParse()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    static func decode(_ json: String) -> StoreStats? {
        try? JSONDecoder().decode(StoreStats.self, from: Data(json.utf8))
    }

    /// The minimum a StoreStats needs to decode, so each case below can say only
    /// the part it is about.
    static func stats(gmail: String?) -> String {
        let g = gmail.map { ",\"gmail\":\($0)" } ?? ""
        return """
            {"tier_counts":{},"total":0,"sealed":0,\
            "bands":{"standing":0,"new":0,"open":0}\(g)}
            """
    }

    static func aConnectedMailboxSaysSoAndOffersNothing() {
        guard let s = decode(stats(gmail: "{\"connected\":true}")) else {
            return expect(false, "a connected mailbox decodes")
        }
        expect(s.gmail?.connected == true, "connected is true")
        // No link while it works: an invitation to re-consent for no reason is
        // worse than no invitation.
        expect(s.gmail?.reconnect_url == nil, "a working mailbox is offered no link")
        expect(s.gmail?.disconnected_since == nil, "and no since-when")
    }

    static func aDeadCredentialCarriesTheRepair() {
        let json = """
            {"connected":false,"disconnected_since":"2026-08-26T00:01:25+00:00",\
            "reconnect_url":"https://signup.passband.app/reconnect"}
            """
        guard let s = decode(stats(gmail: json)), let g = s.gmail else {
            return expect(false, "a disconnected mailbox decodes")
        }
        expect(!g.connected, "connected is false")
        expect(
            g.reconnect_url == "https://signup.passband.app/reconnect",
            "hosted carries the link that repairs it")
        // THE ONE THAT REGRESSED. A stamp that decodes as a String but will not
        // parse as a Date is a banner missing its most useful sentence.
        expect(Fmt.date(g.disconnected_since) != nil, "and a since-when that PARSES")
    }

    static func selfHostIsToldWithoutALink() {
        let json = "{\"connected\":false,\"disconnected_since\":\"2026-08-26T00:01:25+00:00\"}"
        guard let g = decode(stats(gmail: json))?.gmail else {
            return expect(false, "self-host decodes")
        }
        expect(!g.connected, "self-host is told it is disconnected")
        // `squelchd auth` at a shell is not a link anything can offer, and a
        // button that goes nowhere is worse than the sentence that is true.
        expect(g.reconnect_url == nil, "and is offered no link")
    }

    /// A daemon too old to say, or a door with no metrics handle, sends nothing.
    /// Nil must stay nil rather than defaulting into a cheerful answer: the
    /// client reads `== false`, so absence renders no banner and never an alarm.
    static func silenceIsNotGoodNews() {
        guard let s = decode(stats(gmail: nil)) else {
            return expect(false, "stats without the key still decode")
        }
        expect(s.gmail == nil, "absence decodes to nil, not to a default")
        // AppStore.gmailDisconnected is exactly this expression, and it is
        // `== false` rather than `!= true` for this case: a daemon that said
        // nothing must render no banner, never an alarm.
        let disconnected = (s.gmail?.connected == false)
        expect(!disconnected, "silence never reads as disconnected")
    }

    /// BOTH SHAPES, because the daemon is not consistent and either formatter
    /// alone silently answers nil for the other one.
    static func bothStampShapesParse() {
        // What `/client/stats` sends today: whole seconds, numeric offset.
        expect(
            Fmt.date("2026-08-26T00:01:25+00:00") != nil,
            "the stamp the daemon sends today parses")
        // What it would send the day this field is sourced from `Utc::now()`.
        expect(
            Fmt.date("2026-08-26T01:12:02.249947+00:00") != nil,
            "and a fractional-seconds stamp parses too")
        // Zulu, the third spelling anything RFC3339 may hand over.
        expect(Fmt.date("2026-08-26T00:01:25Z") != nil, "and the Z spelling")
        expect(Fmt.date(nil) == nil, "nil in, nil out")
        expect(Fmt.date("not a date") == nil, "and garbage does not become a date")
    }

    static func expect(_ cond: Bool, _ what: String) {
        checks += 1
        if !cond {
            failures += 1
            print("  FAIL: \(what)")
        }
    }
}
