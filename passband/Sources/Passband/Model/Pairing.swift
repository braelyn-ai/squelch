// Device pairing: trading a short code the daemon printed for THIS install's
// own device token. `squelchd pair` mints a one-shot code; claiming it here
// mints a per-device token that `squelchd token list` can see and
// `squelchd token revoke` can kill, which is the whole reason pairing exists —
// a shared static bearer can be neither named nor revoked per machine.
//
// /client/pair is the one human-door route that is NOT behind the bearer, since
// the caller has no token yet. Two consequences shape this file: the request
// carries the code as its only credential (so redirects are refused outright —
// a 302 would hand the code to whatever host it named), and the daemon answers
// EVERY failure with a bare 401 so that expired, wrong and burned codes are
// indistinguishable. This file invents no detail the server withheld.
//
// The code and the issued token live in locals here and nowhere else: not in an
// error, not in a stored property, never in a log line. The token's one
// destination is `AppStore.connect`, which puts it in the keychain.

import Foundation

// Only for the device name below. Kept to a fenced import rather than routed
// through Platform.swift on purpose: the pairing test suite compiles this file
// with Sessions.swift and nothing else (see test.sh), so a reference to the
// shim would take the suite's whole point — a pure, app-free decoder test —
// away with it.
#if !os(macOS)
    import UIKit
#endif

/// A `passband://pair?url=…&code=…` deep link. `squelchd pair` prints one so a
/// click carries both halves of the pairing across from the terminal.
struct PairLink: Equatable, Sendable {
    var serverURL: String
    /// Already normalized (uppercase, no dashes), ready to send.
    var code: String

    /// Parse a link, or return nil for anything malformed. STRICT on purpose:
    /// this is untrusted input handed over by whatever app opened the URL, and
    /// the app acts on it by talking to the host it names. Anything that is not
    /// a complete, well-shaped pair link is dropped rather than half-applied.
    init?(_ url: URL) {
        guard url.scheme?.lowercased() == "passband" else { return nil }
        // `passband://pair?…` puts "pair" in the host; `passband:pair?…` puts it
        // in the path. Accept both spellings of the same intent.
        let action =
            url.host?.lowercased()
            ?? url.path.trimmingCharacters(in: CharacterSet(charactersIn: "/")).lowercased()
        guard action == "pair" else { return nil }

        guard let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems
        else { return nil }
        let value = { (name: String) -> String? in
            items.first { $0.name == name }?.value
        }
        guard let rawURL = value("url")?.trimmingCharacters(in: .whitespacesAndNewlines),
            Pairing.isUsableServerURL(rawURL)
        else { return nil }
        let normalized = Pairing.normalize(value("code") ?? "")
        guard normalized.count == Pairing.codeLength else { return nil }

        serverURL = rawURL
        code = normalized
    }

    /// Whether the named server is this machine's own daemon. NO link claims
    /// itself, loopback or not: a `passband://` URL is openable by any web page
    /// and every claim spends one of the live code's five attempts, so a link
    /// only ever fills the form and pairing is always one deliberate press.
    ///
    /// What this gates is the NOTICE. A loopback URL is the default `squelchd
    /// pair` prints for its own machine, so it fills the field quietly; any
    /// other host came from outside and gets named on screen (see ConnectView),
    /// because a prefilled remote URL that blends into the form is how a link
    /// gets a code typed into a server the user never chose.
    var isLoopback: Bool {
        guard let host = URLComponents(string: serverURL)?.host?.lowercased() else { return false }
        return host == "127.0.0.1" || host == "localhost" || host == "::1" || host == "[::1]"
    }

    /// The host to name in that notice. The host alone, because the host is
    /// what decides where the code goes; the whole string only as a fallback
    /// this parser makes unreachable (a link with no host never becomes one).
    var displayHost: String {
        guard let host = URLComponents(string: serverURL)?.host, !host.isEmpty else {
            return serverURL
        }
        return host
    }
}

enum PairError: Error, Equatable {
    /// The server URL is not something we can POST to.
    case badServerURL
    /// Transport failure, DNS, TLS, timeout.
    case unreachable
    /// The daemon said no. Deliberately undifferentiated: so did the daemon.
    case refused
    /// The route is not there, so this daemon predates pairing.
    case unsupported
    /// A 2xx whose body was not the token payload.
    case badResponse
    /// 5xx.
    case serverFailure
}

extension PairError: LocalizedError {
    /// Never quotes the code, the token, the URL or the response body.
    var errorDescription: String? {
        switch self {
        case .badServerURL: "that server url is not an http(s) address"
        case .unreachable: "cannot reach that server url"
        // ONE sentence for every rejection, because the daemon gives one
        // answer for all of them: naming a cause here would invent an oracle
        // the wire deliberately does not have.
        case .refused: "that code did not work or has expired. run squelchd pair for a fresh one."
        case .unsupported:
            "this daemon is too old to pair. update it, or connect with an api token."
        case .badResponse: "the server answered with something unexpected"
        case .serverFailure: "the daemon could not finish pairing"
        }
    }
}

enum Pairing {
    /// Codes are 8 Crockford base32 characters, displayed XXXX-XXXX.
    static let codeLength = 8

    /// 100 chars is the daemon's own ceiling on a device name; trimming here
    /// means the field can't be typed into a request that 400s.
    static let maxDeviceNameLength = 100

    /// Fold typed input to exactly what the daemon normalizes before hashing:
    /// uppercase, dashes and whitespace removed. Nothing else — Crockford's
    /// I/L→1 and O→0 aliasing is NOT applied, because the daemon does not apply
    /// it either, and a client that "helpfully" rewrote a character would hash
    /// to something the server never minted.
    static func normalize(_ raw: String) -> String {
        raw.uppercased().filter { !$0.isWhitespace && $0 != "-" }
    }

    /// Group a normalized code the way `squelchd pair` prints it.
    static func formatted(_ code: String) -> String {
        let normalized = normalize(code)
        guard normalized.count == codeLength else { return normalized }
        let split = normalized.index(normalized.startIndex, offsetBy: codeLength / 2)
        return "\(normalized[..<split])-\(normalized[split...])"
    }

    /// Whether typed input is the right SHAPE to be worth a round-trip. Not
    /// validation of the code itself — only the daemon can say that — just the
    /// gate that keeps the button dark until there is a whole code to send.
    static func looksComplete(_ raw: String) -> Bool { normalize(raw).count == codeLength }

    /// The name this device carries in `squelchd token list`, so revoking the
    /// right one later is a matter of reading the row. The Mac's sharing name
    /// ("Braelyn's MacBook Pro") is what a person recognises; the hostname is
    /// the fallback, minus the `.local` that Bonjour hangs off it.
    static func defaultDeviceName() -> String {
        #if os(macOS)
            let sharing = Host.current().localizedName ?? ""
            if !sharing.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                return clampDeviceName(sharing)
            }
            var host = ProcessInfo.processInfo.hostName
            if host.hasSuffix(".local") { host.removeLast(6) }
            let trimmed = host.trimmingCharacters(in: .whitespacesAndNewlines)
            return clampDeviceName(trimmed.isEmpty ? "Mac" : trimmed)
        #else
            // `Host` is AppKit-era Foundation and does not exist here. UIDevice's
            // name is the equivalent answer — the user-assigned device name when
            // the app is entitled to it, the model name ("iPhone") otherwise,
            // which is still a row a human can recognise in `squelchd token list`.
            let trimmed = UIDevice.current.name.trimmingCharacters(in: .whitespacesAndNewlines)
            return clampDeviceName(trimmed.isEmpty ? "iPhone" : trimmed)
        #endif
    }

    /// Trim and cap a device name to what the daemon will accept.
    static func clampDeviceName(_ raw: String) -> String {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        return String(trimmed.prefix(maxDeviceNameLength))
    }

    /// Whether a string is a URL this app is willing to POST a pairing code to.
    /// http(s) with a host, and nothing else: the server URL field accepts free
    /// text, and a `file:` or custom-scheme value must not become a request.
    static func isUsableServerURL(_ raw: String) -> Bool {
        guard let comps = URLComponents(string: raw), let scheme = comps.scheme?.lowercased(),
            scheme == "http" || scheme == "https",
            let host = comps.host, !host.isEmpty
        else { return false }
        return true
    }

    // MARK: - the claim

    private struct ClaimBody: Encodable {
        var code: String
        var device_name: String
    }

    /// What a successful claim hands back. `token` is the ONLY copy that will
    /// ever exist of this device's credential: the daemon stores a hash and
    /// cannot reissue it.
    struct IssuedToken: Sendable {
        var token: String
        var deviceId: Int
        var name: String
    }

    private struct IssuedWire: Decodable {
        var token: String
        var device_id: Int
        var name: String?
    }

    /// Its own session rather than APIClient's: that client exists to attach a
    /// bearer token, and this is the call made because there isn't one yet.
    private static let session = Sessions.ephemeral(
        timeout: 15, resource: 30, cookies: .off, urlCache: false,
        cachePolicy: .reloadIgnoringLocalCacheData, emptyHeaders: true)

    /// REFUSES EVERY REDIRECT. The POST body carries the pairing code, and
    /// URLSession replays a body across a 307/308 hop verbatim — so one 3xx from
    /// a hijacked or misconfigured host would post the user's live code
    /// somewhere else, where claiming it would mint that host a real token. No
    /// daemon 3xx's /client/pair, so refusing costs nothing: the redirect
    /// surfaces as a non-200 instead.
    private static let pinned = SchemePinned(allow: [])

    /// Trade one pairing code for this device's token. Never retried
    /// automatically: a code is single-use and a failed claim counts against the
    /// live code's attempt budget, so a retry loop would burn the user's code
    /// for them.
    static func claim(baseURL: String, code: String, deviceName: String) async throws -> IssuedToken
    {
        var base = baseURL.trimmingCharacters(in: .whitespacesAndNewlines)
        while base.hasSuffix("/") { base.removeLast() }
        guard isUsableServerURL(base), let url = URL(string: base + "/client/pair") else {
            throw PairError.badServerURL
        }

        let normalizedCode = normalize(code)
        let name = clampDeviceName(deviceName)
        guard normalizedCode.count == codeLength, !name.isEmpty else { throw PairError.refused }

        var req = URLRequest(url: url, timeoutInterval: 15)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue("application/json", forHTTPHeaderField: "Accept")
        req.httpBody = try JSONEncoder().encode(
            ClaimBody(code: normalizedCode, device_name: name))

        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: req, delegate: pinned)
        } catch {
            // Never the underlying error text: it quotes the request we built.
            throw PairError.unreachable
        }
        guard let http = response as? HTTPURLResponse else { throw PairError.unreachable }
        switch http.statusCode {
        case 200..<300: break
        case 404: throw PairError.unsupported
        case 500...: throw PairError.serverFailure
        // 401 and everything else the door can say about a code: one answer.
        default: throw PairError.refused
        }

        guard let wire = try? JSONDecoder().decode(IssuedWire.self, from: data),
            !wire.token.isEmpty
        else { throw PairError.badResponse }
        return IssuedToken(token: wire.token, deviceId: wire.device_id, name: wire.name ?? name)
    }

    /// Human text for a claim failure. Anything that is not a `PairError` is
    /// reported as a refusal rather than surfaced: an unexpected error's
    /// description is exactly the kind of string that could quote the request.
    static func message(for error: Error) -> String {
        (error as? PairError)?.errorDescription ?? PairError.refused.errorDescription!
    }
}
