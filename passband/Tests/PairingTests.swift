// The pairing code and its deep link, in a room with no daemon. Everything
// asserted here is a decision made BEFORE any request goes out: how typed input
// folds to what the daemon hashes, and which links are allowed to point this
// app at a server at all.
//
// The hostile cases are the point. A `passband://` URL is openable by any web
// page, so the parser is asserted to REJECT — not repair — links that name a
// non-http server, carry a short code, or spell an action other than pair. And
// normalization is asserted to be exactly the daemon's (uppercase, drop dashes
// and whitespace) and nothing more: a client that folded Crockford's I/L/O the
// way a decoder would would hash to a code the server never minted.

import Foundation

@main
@MainActor
struct PairingTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        normalization()
        formatting()
        completeness()
        deviceNames()
        serverURLs()
        linkParsing()
        linkRejection()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - cases

    /// Uppercase, dashes and whitespace gone. Every spelling of one code lands
    /// on the same string the daemon hashes.
    static func normalization() {
        equal(Pairing.normalize("abcd-efgh"), "ABCDEFGH", "lowercase with dash")
        equal(Pairing.normalize("ABCDEFGH"), "ABCDEFGH", "already normal")
        equal(Pairing.normalize(" abcd efgh \n"), "ABCDEFGH", "spaces and a newline")
        equal(Pairing.normalize("a-b-c-d-e-f-g-h"), "ABCDEFGH", "dashes everywhere")
        equal(Pairing.normalize(""), "", "empty stays empty")
        // NOT Crockford-decoded: I stays I. The daemon does not alias it, so
        // aliasing here would send a code that hashes to nothing.
        equal(Pairing.normalize("ILOU1234"), "ILOU1234", "ambiguous letters survive verbatim")
        // Only dashes and whitespace are stripped; other junk must survive so
        // the claim fails loudly instead of silently sending a shorter code.
        equal(Pairing.normalize("ABCD_EFGH"), "ABCD_EFGH", "underscore is not stripped")
    }

    /// Display grouping, and the refusal to group something that is not a code.
    static func formatting() {
        equal(Pairing.formatted("ABCDEFGH"), "ABCD-EFGH", "grouped in halves")
        equal(Pairing.formatted("abcdefgh"), "ABCD-EFGH", "normalizes first")
        equal(Pairing.formatted("ABCD-EFGH"), "ABCD-EFGH", "idempotent")
        equal(Pairing.formatted("ABC"), "ABC", "short input is left alone")
        equal(Pairing.formatted("ABCDEFGHIJ"), "ABCDEFGHIJ", "long input is left alone")
    }

    /// The shape gate in front of the button.
    static func completeness() {
        equal(Pairing.looksComplete("ABCD-EFGH"), true, "full code")
        equal(Pairing.looksComplete("abcdefgh"), true, "full code, unformatted")
        equal(Pairing.looksComplete("ABCD-EFG"), false, "one short")
        equal(Pairing.looksComplete("ABCD-EFGHI"), false, "one long")
        equal(Pairing.looksComplete(""), false, "empty")
        equal(Pairing.looksComplete("        "), false, "whitespace only")
    }

    /// Names are trimmed and capped to what the daemon accepts, so the field
    /// cannot produce a request that 400s.
    static func deviceNames() {
        equal(Pairing.clampDeviceName("  Studio Mac  "), "Studio Mac", "trimmed")
        equal(
            Pairing.clampDeviceName(String(repeating: "x", count: 200)).count,
            Pairing.maxDeviceNameLength, "capped at the daemon's ceiling")
        equal(Pairing.clampDeviceName("   "), "", "whitespace-only collapses to empty")
        // Whatever this machine is called, the default must be sendable as-is.
        let fallback = Pairing.defaultDeviceName()
        equal(fallback.isEmpty, false, "default device name is never empty")
        equal(fallback.count <= Pairing.maxDeviceNameLength, true, "default name is within the cap")
    }

    /// What the app is willing to POST a live pairing code to.
    static func serverURLs() {
        equal(Pairing.isUsableServerURL("http://127.0.0.1:8848"), true, "loopback http")
        equal(Pairing.isUsableServerURL("https://mail.example.com"), true, "https")
        equal(Pairing.isUsableServerURL("ftp://example.com"), false, "wrong scheme")
        equal(Pairing.isUsableServerURL("file:///etc/passwd"), false, "file is not a server")
        equal(Pairing.isUsableServerURL("passband://pair"), false, "our own scheme is not a server")
        equal(Pairing.isUsableServerURL("127.0.0.1:8848"), false, "no scheme at all")
        equal(Pairing.isUsableServerURL("http://"), false, "scheme with no host")
        equal(Pairing.isUsableServerURL(""), false, "empty")
    }

    /// Both spellings of the link the daemon prints.
    static func linkParsing() {
        let link = parse("passband://pair?url=http%3A%2F%2F127.0.0.1%3A8848&code=ABCD-EFGH")
        equal(link?.serverURL, "http://127.0.0.1:8848", "server url decoded")
        equal(link?.code, "ABCDEFGH", "code stored normalized")

        // Host-less spelling, and the query in the other order.
        let opaque = parse("passband:pair?code=abcdefgh&url=https%3A%2F%2Fdaemon.example.com")
        equal(opaque?.serverURL, "https://daemon.example.com", "opaque form parses")
        equal(opaque?.code, "ABCDEFGH", "opaque form normalizes the code")

        // Scheme and action are matched case-insensitively; a link pasted
        // through something that upcased it still works.
        let shouty = parse("PASSBAND://PAIR?url=http%3A%2F%2F127.0.0.1%3A8848&code=ABCDEFGH")
        equal(shouty?.code, "ABCDEFGH", "uppercase scheme and action")
    }

    /// Everything the parser must refuse rather than repair. A rejected link
    /// leaves the Connect gate exactly as the user left it.
    static func linkRejection() {
        nilLink("https://example.com/pair?url=http%3A%2F%2F127.0.0.1&code=ABCDEFGH", "wrong scheme")
        nilLink("passband://open?url=http%3A%2F%2F127.0.0.1&code=ABCDEFGH", "wrong action")
        nilLink("passband://pair", "no query at all")
        nilLink("passband://pair?code=ABCDEFGH", "no server url")
        nilLink("passband://pair?url=http%3A%2F%2F127.0.0.1%3A8848", "no code")
        nilLink("passband://pair?url=http%3A%2F%2F127.0.0.1%3A8848&code=ABC", "short code")
        nilLink("passband://pair?url=http%3A%2F%2F127.0.0.1%3A8848&code=ABCDEFGHIJ", "long code")
        // The reason the URL is validated at parse time: a link must not be
        // able to aim a claim at a scheme this process can reach locally.
        nilLink("passband://pair?url=file%3A%2F%2F%2Ftmp%2Fx&code=ABCDEFGH", "file url")
        nilLink("passband://pair?url=javascript%3Aalert(1)&code=ABCDEFGH", "javascript url")
        nilLink("passband://pair?url=&code=ABCDEFGH", "empty server url")
    }

    // MARK: - helpers

    static func parse(_ raw: String) -> PairLink? {
        guard let url = URL(string: raw) else { return nil }
        return PairLink(url)
    }

    /// Prints the SERVER URL and never the link: a `PairLink` carries the
    /// pairing code, and a failure message is a log line. The Rust half writes
    /// redacting `Debug` impls by hand for the same reason — a credential that
    /// can be interpolated eventually is.
    static func nilLink(_ raw: String, _ label: String, line: Int = #line) {
        checks += 1
        if let url = URL(string: raw), let link = PairLink(url) {
            failures += 1
            print("FAIL (line \(line)): \(label)\n  want: nil\n   got: a link to \(link.serverURL)")
        }
    }

    // MARK: - assertions

    static func equal<T: Equatable>(
        _ got: T, _ want: T, _ label: String, line: Int = #line
    ) {
        checks += 1
        if got != want {
            failures += 1
            print("FAIL (line \(line)): \(label)\n  want: \(want)\n   got: \(got)")
        }
    }
}
