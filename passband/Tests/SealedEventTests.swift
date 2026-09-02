// The fork the notify lane put in the event feed: a sealed event is the fast
// word that a login code arrived, and it is NOT a piece of mail with a thread
// to open.
//
// Both halves of that are only ever wrong in a way nobody sees. Route a sealed
// event as an ordinary one and the banner names the sender of a login code and
// its tap opens a thread the daemon refuses to serve — a dead notification
// stacked on top of the poller's real one. Route an ordinary event as auth and
// the mail that was worth interrupting for silently stops arriving.
//
// The third assertion is the one that matters most and is the least visible:
// what the auth banner SAYS. A subject line so often IS the code ("725104 is
// your Acme code" is a real subject), so the fixture below puts a six-digit
// number in `one_line` and every check demands its absence. The daemon writes a
// fixed per-kind phrase into that field for a sealed row, but a client that
// leaned on that would leak the day the field's source moved.
//
// And the decode: `sealed_kind` is one optional key added to a required-field
// wire type, on a feed whose consumer ADVANCES THE CURSOR past a frame it
// cannot decode. A decoder made stricter here does not fail loudly, it loses
// notifications permanently, so both shapes of the frame are pinned.

import Foundation

@main
@MainActor
struct SealedEventTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        anOrdinaryEventDecodesWithNoSealedKey()
        aSealedEventCarriesItsKind()
        anUnheardOfKindKeepsItsRawString()
        sealedRoutesToTheAuthSignal()
        everythingElseRoutesToTheThreadBanner()
        theAuthBannerNamesTheKindAndTheSender()
        theAuthBannerNeverCarriesTheSubject()
        theAuthBannerStandsAloneWithNoAccountName()
        anUnnamedSenderStillSaysSomething()
        authBannersOfOneMailboxShareAGroup()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - fixtures

    /// A frame as the daemon writes it. `sealedKind` nil omits the key
    /// ENTIRELY rather than sending null, which is the shape an older daemon
    /// serves and the one the optional exists for.
    static func frame(
        sealedKind: String? = nil,
        sender: String = "Acme Security <no-reply@acme.com>",
        oneLine: String = "Login code arrived"
    ) -> Data {
        var fields = [
            "\"id\": 41",
            "\"kind\": \"urgent\"",
            "\"message_id\": 907",
            "\"thread_id\": \"t-abc\"",
            "\"tier\": \"signal\"",
            "\"importance\": 90",
            "\"sender\": \(quoted(sender))",
            "\"one_line\": \(quoted(oneLine))",
            "\"created_at\": \"2026-09-01T10:00:00Z\"",
        ]
        if let sealedKind { fields.append("\"sealed_kind\": \(quoted(sealedKind))") }
        return Data("{\(fields.joined(separator: ","))}".utf8)
    }

    static func quoted(_ s: String) -> String {
        "\"\(s.replacingOccurrences(of: "\"", with: "\\\""))\""
    }

    static func decode(_ data: Data) -> Event? {
        try? JSONDecoder().decode(Event.self, from: data)
    }

    // MARK: - the decode

    /// The overwhelming majority of frames, and every frame a pre-notify-lane
    /// daemon serves. An absent key is nil, not a decode failure.
    static func anOrdinaryEventDecodesWithNoSealedKey() {
        guard let e = decode(frame()) else {
            return expect(false, "an event with no sealed_kind key still decodes")
        }
        expect(e.sealed_kind == nil, "an absent sealed_kind decodes to nil")
        expect(e.id == 41 && e.message_id == 907, "and every other field survives it")
    }

    static func aSealedEventCarriesItsKind() {
        guard let e = decode(frame(sealedKind: "otp")) else {
            return expect(false, "an event carrying sealed_kind decodes")
        }
        expect(e.sealed_kind == .otp, "sealed_kind reads as the kind it names")
    }

    /// SealedKind is total by construction. A kind this build has never heard
    /// of must keep its raw string rather than collapse onto a known case —
    /// otp and verification auto-reveal, and a stranger that became one of them
    /// would reveal a body nobody asked for.
    static func anUnheardOfKindKeepsItsRawString() {
        guard let e = decode(frame(sealedKind: "passkey_challenge")) else {
            return expect(false, "an unknown sealed_kind still decodes the event")
        }
        expect(
            e.sealed_kind == .unknown("passkey_challenge"),
            "an unknown kind is carried verbatim, not folded onto a known case")
        expect(e.sealed_kind != nil, "and is still routed as auth mail")
    }

    // MARK: - the routing

    static func sealedRoutesToTheAuthSignal() {
        for kind in ["otp", "password_reset", "magic_link", "login_alert", "verification"] {
            guard let e = decode(frame(sealedKind: kind)) else {
                expect(false, "\(kind) decodes")
                continue
            }
            guard case .authSignal = EventBanner.routing(for: e) else {
                expect(false, "a \(kind) event is an auth signal, not a thread banner")
                continue
            }
            expect(true, "a \(kind) event routes to the auth signal")
        }
    }

    /// Every OTHER event, including the urgent ones a sealed row is otherwise
    /// indistinguishable from: the daemon stamps a sealed row `urgent`/`signal`
    /// like any other urgent mail, so nothing but the key may decide this.
    static func everythingElseRoutesToTheThreadBanner() {
        for kind in ["urgent", "deadline", "surfaced", "opened"] {
            let raw = String(
                decoding: frame(), as: UTF8.self
            ).replacingOccurrences(of: "\"kind\": \"urgent\"", with: "\"kind\": \"\(kind)\"")
            guard let e = decode(Data(raw.utf8)) else {
                expect(false, "\(kind) decodes")
                continue
            }
            guard case .threadBanner = EventBanner.routing(for: e) else {
                expect(false, "an ordinary \(kind) event is a thread banner")
                continue
            }
            expect(true, "an ordinary \(kind) event routes to the thread banner")
        }
    }

    // MARK: - what the auth banner says

    static func authCopy(
        _ e: Event, account: String? = "Work"
    ) -> EventBanner.Copy {
        EventBanner.authCopy(kind: e.sealed_kind, sender: e.sender, accountName: account)
    }

    static func theAuthBannerNamesTheKindAndTheSender() {
        guard let e = decode(frame(sealedKind: "otp")) else { return expect(false, "decodes") }
        let copy = authCopy(e)
        expect(copy.title.contains("Login code"), "the banner leads with the kind's label")
        expect(copy.title.contains("Work"), "and names the mailbox the code landed in")
        expect(copy.body == "from Acme Security", "the body names who wants the code")
        expect(copy.sound, "and it always chimes: a code expires while you are not looking")
    }

    /// THE ONE THAT MATTERS. A subject so often IS the code, so the fixture's
    /// `one_line` carries a six-digit number and no field of the banner may
    /// contain any part of it.
    static func theAuthBannerNeverCarriesTheSubject() {
        guard
            let e = decode(
                frame(sealedKind: "otp", oneLine: "725104 is your Acme verification code"))
        else { return expect(false, "decodes") }
        let copy = authCopy(e)
        let everything = [copy.title, copy.subtitle, copy.body, copy.threadIdentifier].joined(
            separator: " ")
        expect(!everything.contains("725104"), "no field of an auth banner carries the code")
        expect(
            !everything.contains("verification code"),
            "nor a word of the subject the code sat in")
        expect(
            !everything.contains(e.one_line),
            "the event's one_line reaches no part of the banner")
        expect(
            everything.contains("Login code") && everything.contains("Acme Security"),
            "what it does say is the kind and the sender, and only those")
    }

    /// The notification service extension has no account labels — it reads
    /// credentials out of the shared keychain and nothing else — so the label
    /// has to stand alone rather than trail an empty separator.
    static func theAuthBannerStandsAloneWithNoAccountName() {
        guard let e = decode(frame(sealedKind: "login_alert")) else {
            return expect(false, "decodes")
        }
        let copy = authCopy(e, account: nil)
        expect(copy.title == "Sign-in alert", "with no account name the label stands alone")
        expect(!copy.title.contains("·"), "and no separator is left dangling")
    }

    static func anUnnamedSenderStillSaysSomething() {
        guard let e = decode(frame(sealedKind: "magic_link", sender: "")) else {
            return expect(false, "decodes")
        }
        let copy = authCopy(e)
        expect(copy.body == "New auth mail.", "a banner with no sender is not a blank banner")
        expect(copy.title.contains("Sign-in link"), "and the kind is still said")
    }

    /// A login code and the sign-in alert behind it are one conversation. The
    /// group is unprefixed here; both posters fold the account in, because two
    /// daemons' auth mail is not one conversation.
    static func authBannersOfOneMailboxShareAGroup() {
        guard let otp = decode(frame(sealedKind: "otp")),
            let alert = decode(frame(sealedKind: "login_alert", sender: "ops@acme.com"))
        else { return expect(false, "decodes") }
        expect(
            authCopy(otp).threadIdentifier == authCopy(alert).threadIdentifier,
            "one mailbox's auth banners stack into one group")
        expect(
            authCopy(otp).threadIdentifier == EventBanner.authGroup,
            "and the group is the shared constant, not a per-event string")
    }

    static func expect(_ cond: Bool, _ what: String) {
        checks += 1
        if !cond {
            failures += 1
            print("  FAIL: \(what)")
        }
    }
}
