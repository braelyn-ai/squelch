// THE RESIDENT NOTIFICATION FEED: one long-lived SSE connection to
// `GET /client/events`, every frame handed to the notifier.
//
// The daemon's events table is the truth; this client carries its OWN cursor
// (`?after=<id>`, persisted in UserDefaults) and no connection state is
// server-side. FIRST RUN CONNECTS CURSORLESS on purpose — with no `after` the
// server sends live events only, so a fresh install is not handed a week of
// backlog as a notification storm. The bearer token goes in the Authorization
// header and NOWHERE else: never in the URL, never in an error message.

import Foundation

// MARK: - SSE framing

/// One dispatched SSE frame: the `id:` in force when it was dispatched, plus
/// the joined `data:` payload.
struct SSEFrame: Equatable, Sendable {
    var id: String?
    var data: String
}

/// Incremental SSE frame assembler, fed one line at a time. A pure value type, so
/// the whole protocol surface is testable without a network in the room.
///
/// What is on this wire: `:` keep-alive comments (ignored, and NOT a frame
/// boundary — treating one as blank dispatches a half-read frame every 15s), a
/// trailing CR the LF splitter leaves behind, several `data:` lines joined with
/// "\n", `id:` persisting across frames, and unknown fields skipped.
struct SSEParser {
    /// Hard cap on one frame's accumulated data. A frame is display copy for a
    /// notification; past this the server is broken or hostile, and the only
    /// alternative is an unbounded buffer on a connection we hold open for days.
    static let maxFrameBytes = 256 * 1024

    private var data: [String] = []
    private var bytes = 0
    private var overflowed = false
    /// Survives dispatch, per the spec: `id:` sets the connection's
    /// last-event-id, which later frames inherit if they carry none.
    private var lastEventId: String?

    /// Feed one line. Returns a frame when that line terminated one.
    mutating func feed(_ rawLine: String) -> SSEFrame? {
        // The splitter cuts on the LF; a CRLF stream leaves the CR behind.
        var line = Substring(rawLine)
        if line.hasSuffix("\r") { line = line.dropLast() }

        if line.isEmpty { return dispatch() }
        if line.hasPrefix(":") { return nil }  // comment / keep-alive ping

        let field: Substring
        var value: Substring
        if let colon = line.firstIndex(of: ":") {
            field = line[line.startIndex..<colon]
            value = line[line.index(after: colon)...]
            // Exactly ONE leading space is framing, not value.
            if value.hasPrefix(" ") { value = value.dropFirst() }
        } else {
            // A bare field name with no colon is a field with an empty value.
            field = line
            value = ""
        }

        switch field {
        case "data":
            bytes += value.utf8.count + 1
            if bytes > Self.maxFrameBytes {
                overflowed = true
                data.removeAll(keepingCapacity: false)
            } else if !overflowed {
                data.append(String(value))
            }
        case "id":
            // The spec says an id containing NUL is ignored. Ours are integers.
            if !value.contains("\0") { lastEventId = String(value) }
        default:
            break
        }
        return nil
    }

    private mutating func dispatch() -> SSEFrame? {
        defer {
            data.removeAll(keepingCapacity: true)
            bytes = 0
            overflowed = false
        }
        // A blank line with an empty buffer dispatches nothing — which is what both
        // the double newline after a frame and a keep-alive ping produce.
        guard !overflowed, !data.isEmpty else { return nil }
        return SSEFrame(id: lastEventId, data: data.joined(separator: "\n"))
    }
}

// MARK: - the connection

@MainActor
final class EventStream {
    static let shared = EventStream()

    /// The client's cursor. Read with `object(forKey:)` and NOT
    /// `integer(forKey:)`: the absent case must stay nil, because `after=0` is
    /// the legitimate "replay the entire log" cursor on the server. Reading a
    /// missing key as 0 would turn every fresh install into a backlog storm.
    private static let cursorKey = "squelch.events.lastSeen"

    private static let backoffBase: TimeInterval = 1
    private static let backoffCap: TimeInterval = 60
    /// A connection that lived this long was real, so its failure is a fresh
    /// incident and starts the backoff over. Without this a daemon that
    /// restarts nightly would creep up to the 60s cap and stay there.
    private static let healthyAfter: TimeInterval = 30

    /// Inactivity watchdog, NOT a request deadline: URLSession resets it on every
    /// byte. The server pings every 15s, so this is ~4 missed pings — enough to
    /// call a silently-dropped flow (a NAT, a sleeping router, no FIN) dead.
    private static let inactivityTimeout: TimeInterval = 60
    /// Whole-connection lifetime. A day is effectively "never" for a cursored
    /// stream: the daily reconnect replays the seam for one round trip.
    private static let resourceTimeout: TimeInterval = 24 * 60 * 60

    private let resident = ResidentTask()
    private var cursor: Int?
    private var cursorLoaded = false

    /// NOT waitsForConnectivity: it would park a connect to a dead daemon for the
    /// whole resource timeout instead of failing fast, and this class's own
    /// backoff is the retry policy we actually want.
    private let session = Sessions.ephemeral(
        timeout: EventStream.inactivityTimeout, resource: EventStream.resourceTimeout,
        cookies: .neverSent, cachePolicy: .reloadIgnoringLocalCacheData,
        emptyHeaders: true, waitsForConnectivity: false)

    /// Refuses every redirect — an empty allow-list: the feed URL is
    /// operator-configured and carries the bearer header, so a 3xx from it is a
    /// misconfiguration, not a hop to follow. Refusal surfaces the 3xx, which
    /// fails the 200 check and backs off.
    private static let pinned = SchemePinned(allow: [])

    private init() {}

    /// Start following the feed. Idempotent.
    func start() {
        resident.start { [weak self] in await self?.run() }
    }

    func stop() {
        resident.stop()
    }

    // MARK: - reconnect loop

    private func run() async {
        // Ask for the notification grant on the FIRST connect, not at launch: the
        // prompt should arrive when the app has a reason to notify.
        await Notifier.shared.requestAuthorizationIfNeeded()

        var backoff = Backoff(base: Self.backoffBase, cap: Self.backoffCap)
        while !Task.isCancelled {
            let opened = Date()
            await connect()
            if Task.isCancelled { return }
            // A connection is "successful" by how long it LIVED, not by how it
            // ended: every one of them ends in a failure eventually.
            if Date().timeIntervalSince(opened) >= Self.healthyAfter {
                backoff.reset()
            }
            await backoff.sleep()
        }
    }

    /// Hold ONE connection until it ends. Never throws: every failure mode here
    /// is "try again in a moment", which is the caller's job.
    private func connect() async {
        guard let request = buildRequest() else { return }
        do {
            let (bytes, response) = try await session.bytes(for: request, delegate: Self.pinned)
            guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
                // 401 (token rotated), 404 (older daemon) and 5xx are identical
                // from here: back off and retry. The Connect gate and the sitrep
                // poller are what tell the human their token is wrong.
                return
            }
            var parser = SSEParser()
            // Split by hand rather than with `bytes.lines`: AsyncLineSequence
            // silently DROPS empty lines, and the blank line between frames is
            // SSE's only frame terminator, so every event would sit unread in the
            // buffer. Splitting on LF is safe on UTF-8 — 0x0A cannot appear
            // inside a multi-byte sequence.
            var line: [UInt8] = []
            for try await byte in bytes {
                guard byte == UInt8(ascii: "\n") else {
                    // A stream that never sends a newline would grow this buffer
                    // for as long as we hold the connection — days. Past the cap,
                    // drop it and let the backoff loop retry.
                    guard line.count < SSEParser.maxFrameBytes else { return }
                    line.append(byte)
                    continue
                }
                if Task.isCancelled { return }
                consume(parser.feed(String(decoding: line, as: UTF8.self)))
                line.removeAll(keepingCapacity: true)
            }
        } catch {
            // Refused / DNS / inactivity timeout / cancellation. Silent and stored
            // nowhere: the error text can embed the server URL, and this is a
            // background reconnect nobody is waiting on.
        }
    }

    private static let decoder = JSONDecoder()

    private func consume(_ frame: SSEFrame?) {
        guard let frame else { return }
        if let data = frame.data.data(using: .utf8),
            let event = try? Self.decoder.decode(Event.self, from: data)
        {
            note(seen: event.id)
            Notifier.shared.post(event)
        } else if let id = frame.id.flatMap({ Int($0) }) {
            // An undecodable frame still ADVANCES the cursor, mirroring the
            // server's pump: otherwise one malformed row replays on every
            // reconnect and wedges the feed behind it.
            note(seen: id)
        }
    }

    // MARK: - request

    private func buildRequest() -> URLRequest? {
        guard let settings = AppStore.shared.settings else { return nil }
        var base = settings.serverURL
        while base.hasSuffix("/") { base.removeLast() }
        guard var comps = URLComponents(string: base + "/client/events") else { return nil }
        if let after = loadCursor() {
            comps.queryItems = [URLQueryItem(name: "after", value: String(after))]
        }
        guard let url = comps.url else { return nil }

        var req = URLRequest(url: url)
        req.httpMethod = "GET"
        req.setValue("Bearer \(settings.apiToken)", forHTTPHeaderField: "Authorization")
        req.setValue("text/event-stream", forHTTPHeaderField: "Accept")
        req.setValue("no-store", forHTTPHeaderField: "Cache-Control")
        req.cachePolicy = .reloadIgnoringLocalCacheData
        req.timeoutInterval = Self.inactivityTimeout
        return req
    }

    // MARK: - cursor

    private func loadCursor() -> Int? {
        if !cursorLoaded {
            cursorLoaded = true
            cursor = UserDefaults.standard.object(forKey: Self.cursorKey) as? Int
        }
        return cursor
    }

    /// Advance and persist the cursor. MAX, never "last written": a frame that
    /// arrives out of order (or a replay overlapping the live seam) must not
    /// rewind us into re-notifying about mail the human already dismissed.
    private func note(seen id: Int) {
        let current = loadCursor()
        guard id > (current ?? Int.min) else { return }
        cursor = id
        UserDefaults.standard.set(id, forKey: Self.cursorKey)
    }
}
