// The wire contract behind the spam page, pinned against what the daemon
// ACTUALLY sends.
//
// This suite exists because of a bug it would have caught in one line. The
// client's `refreshSpam()` was declared to return `StatusResult`, which
// requires a `status` key; `POST /client/spam/refresh` answers
// `{"triggered": true}`, the same shape `POST /client/refresh` has always used.
// So the POST left, the daemon fetched the folder perfectly, 197 messages
// landed — and the DECODE of the acknowledgement threw. The client took the
// throw as a failed request and told the reader it could not reach their
// provider's spam folder, while the folder sat there fully synced.
//
// Nothing else could have caught it. The Swift suites are network-free by
// design and never build `APIClient`; the Rust tests assert the JSON the
// handler returns and know nothing of Swift's types. The seam between the two
// is checked by nobody, so the shapes are copied in here VERBATIM from a live
// daemon's responses and decoded into the very types the app uses.
//
// The rule this encodes: a fire-and-forget call fails at the only place it can
// still be seen. Its response is the one part nobody looks at, and typing it
// wrong turns a working feature into a broken-looking one.

import Foundation

@main
@MainActor
struct SpamWireTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        theRefreshAcknowledgementDecodes()
        theNotSpamAcknowledgementDecodes()
        statsCarryTheSpamCountAndStamp()
        aDaemonTooOldSaysNothingRatherThanZero()
        theStampParsesAsADate()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    static func expect(_ ok: Bool, _ what: String) {
        checks += 1
        if !ok {
            failures += 1
            print("  FAIL: \(what)")
        }
    }

    /// THE REGRESSION. Verbatim from `POST /client/spam/refresh`.
    static func theRefreshAcknowledgementDecodes() {
        let body = #"{"triggered":true}"#
        let decoded = try? JSONDecoder().decode(RefreshResult.self, from: Data(body.utf8))
        expect(decoded?.triggered == true, "the spam refresh acknowledgement decodes")

        // And the type it was WRONGLY given must not decode it, so this test
        // fails if anyone re-types the call that way.
        let wrong = try? JSONDecoder().decode(StatusResult.self, from: Data(body.utf8))
        expect(wrong == nil, "StatusResult cannot decode it — that was the bug")
    }

    /// The other new call, which was right, pinned so it stays right. Verbatim
    /// from `POST /client/actions/not_spam`.
    static func theNotSpamAcknowledgementDecodes() {
        let body = #"{"status":"not_spam","message_id":4213}"#
        let decoded = try? JSONDecoder().decode(StatusResult.self, from: Data(body.utf8))
        expect(decoded?.status == "not_spam", "the not-spam acknowledgement decodes")
        expect(decoded?.message_id == 4213, "and carries the message it acted on")
    }

    /// The two fields the page's three empty states are built from. Verbatim
    /// from a live `GET /client/stats`, fractional seconds and all.
    static func statsCarryTheSpamCountAndStamp() {
        let json = """
            {"tier_counts":{},"total":0,"sealed":0,"spam":195,\
            "spam_synced_at":"2026-09-01T21:05:09.897666Z",\
            "bands":{"standing":0,"new":0,"open":0}}
            """
        guard let s = try? JSONDecoder().decode(StoreStats.self, from: Data(json.utf8)) else {
            return expect(false, "stats with spam fields decode")
        }
        expect(s.spam == 195, "the spam count arrives")
        expect(s.spam_synced_at == "2026-09-01T21:05:09.897666Z", "and the completion stamp")
    }

    /// A daemon too old for any of it. Both fields must come back nil rather
    /// than zero: nil means "cannot answer", and the page hides the door on it.
    /// A zero would mean "looked, found nothing", which is a different claim
    /// and one nobody made.
    static func aDaemonTooOldSaysNothingRatherThanZero() {
        let json = """
            {"tier_counts":{},"total":0,"sealed":0,\
            "bands":{"standing":0,"new":0,"open":0}}
            """
        guard let s = try? JSONDecoder().decode(StoreStats.self, from: Data(json.utf8)) else {
            return expect(false, "an older daemon's stats still decode")
        }
        expect(s.spam == nil, "no spam count rather than 0")
        expect(s.spam_synced_at == nil, "and no stamp rather than a date")
    }

    /// The stamp is only useful if it PARSES: the page compares it against its
    /// own request time to tell "still fetching" from "fetched and empty". The
    /// daemon writes `to_rfc3339`, which carries six fractional digits, and a
    /// bare ISO8601 parser returns nil for that — the trap `Fmt.date` already
    /// carries two formatters for.
    static func theStampParsesAsADate() {
        expect(Fmt.date("2026-09-01T21:05:09.897666Z") != nil, "the fractional stamp parses")
        expect(Fmt.date("2026-09-01T21:05:09Z") != nil, "and a plain one still does")
        expect(Fmt.date(nil) == nil, "and absence stays absence")
    }
}
