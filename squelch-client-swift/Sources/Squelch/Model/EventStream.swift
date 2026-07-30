// THE RESIDENT NOTIFICATION FEED: one long-lived SSE connection to
// `GET /client/events`, every frame handed to the notifier.
//
// The daemon's events table is the source of truth and this client carries its
// OWN cursor (`?after=<id>`, persisted in UserDefaults). Nothing about the
// connection is server-side state, which is why a second client (an iPhone,
// later) is a bolt-on rather than a refactor — see squelch-api/src/events.rs.
//
// FIRST RUN CONNECTS CURSORLESS, on purpose: with no `after` the server starts
// at the head of the log and sends live events only. A fresh install must not
// be handed a week of backlog as a notification storm.
//
// SECURITY: the bearer token goes in the Authorization header and NOWHERE else
// — never in the URL (a query string lands in logs, referrers and crash
// reports), never in an error message.

import Foundation

// MARK: - SSE framing

/// One dispatched SSE frame: the `id:` in force when it was dispatched, plus
/// the joined `data:` payload.
struct SSEFrame: Equatable, Sendable {
    var id: String?
    var data: String
}

/// Incremental SSE frame assembler, fed one line at a time.
///
/// A pure value type on purpose: every decision here comes from one line plus
/// the accumulated buffer, so the whole protocol surface can be read and
/// reasoned about without a network in the room.
///
/// It handles all of what is actually on this wire:
/// - `:` comment lines — axum's `KeepAlive` writes one every 15s. Ignored, and
///   NOT a frame boundary (treating one as a blank line would dispatch a
///   half-read frame every 15 seconds).
/// - CRLF — the splitter in `connect()` cuts on the LF and leaves the CR
///   attached, which would otherwise ride along inside the JSON and the id.
/// - Several `data:` lines per frame, joined with "\n" as the spec requires.
/// - `id:` persisting across frames as the connection's last-event-id.
/// - Unknown fields (`event:`, `retry:`, whatever a future daemon adds) —
///   skipped, never fatal.
struct SSEParser {
    /// Hard cap on one frame's accumulated data. A frame is display copy for a
    /// notification; past this the server is broken or hostile, and the only
    /// alternative to a cap is an unbounded buffer on a connection we hold open
    /// for days.
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
        // A blank line with an empty buffer dispatches nothing — that is what
        // the double newline after every frame produces, and what a keep-alive
        // ping's own trailing blank line produces.
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

    /// Inactivity watchdog, NOT a request deadline: URLSession resets this
    /// timer on every byte received. The server pings every 15s, so ~4 missed
    /// pings means the flow is dead (a NAT or a sleeping Wi-Fi router drops it
    /// silently, with no FIN for us to notice). Set to hours instead, a dropped
    /// flow would cost hours of missed notifications — which is the exact
    /// failure the server's keep-alive exists to expose.
    private static let inactivityTimeout: TimeInterval = 60
    /// Whole-connection lifetime. A day is effectively "never" for a stream we
    /// reconnect with a cursor: the daily reconnect replays anything the seam
    /// missed and costs one round trip.
    private static let resourceTimeout: TimeInterval = 24 * 60 * 60

    private var task: Task<Void, Never>?
    private var cursor: Int?
    private var cursorLoaded = false
    private let session: URLSession

    /// Refuses every redirect, in the ImageStore `FetchPolicy` mold. The feed
    /// URL is operator-configured and carries the bearer header; a 3xx from it
    /// is a misconfiguration, not a hop to follow — refusal surfaces the 3xx
    /// itself, which fails the 200 check and lands in the normal backoff.
    private static let pinned = NoRedirects()
    private final class NoRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
        func urlSession(
            _ session: URLSession, task: URLSessionTask,
            willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
            completionHandler: @escaping (URLRequest?) -> Void
        ) {
            completionHandler(nil)
        }
    }

    private init() {
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = Self.inactivityTimeout
        cfg.timeoutIntervalForResource = Self.resourceTimeout
        cfg.requestCachePolicy = .reloadIgnoringLocalCacheData
        cfg.httpAdditionalHeaders = [:]
        cfg.httpShouldSetCookies = false
        // NOT waitsForConnectivity. It would park a connect to a dead daemon
        // for the whole resource timeout instead of failing fast, and this
        // class's own backoff is the retry policy we actually want.
        cfg.waitsForConnectivity = false
        session = URLSession(configuration: cfg)
    }

    /// Start following the feed. Idempotent, like SitrepPoller.start().
    func start() {
        guard task == nil else { return }
        task = Task { [weak self] in await self?.run() }
    }

    func stop() {
        task?.cancel()
        task = nil
    }

    // MARK: - reconnect loop

    private func run() async {
        // Ask for the notification grant on the FIRST connect rather than at
        // launch: the prompt should arrive when the app has a reason to notify,
        // not while the user is still reading the Connect screen.
        await Notifier.shared.requestAuthorizationIfNeeded()

        var backoff = Self.backoffBase
        while !Task.isCancelled {
            let opened = Date()
            await connect()
            if Task.isCancelled { return }
            if Date().timeIntervalSince(opened) >= Self.healthyAfter {
                backoff = Self.backoffBase
            }
            try? await Task.sleep(for: .seconds(Self.jittered(backoff)))
            backoff = min(backoff * 2, Self.backoffCap)
        }
    }

    /// Full jitter over the lower half of the window. Jitter is not decoration:
    /// a daemon restart drops every client at once, and identical backoffs mean
    /// they all come back in the same millisecond, forever.
    private static func jittered(_ seconds: TimeInterval) -> TimeInterval {
        .random(in: (seconds / 2)...seconds)
    }

    /// Hold ONE connection until it ends. Never throws: every failure mode here
    /// is "try again in a moment", which is the caller's job.
    private func connect() async {
        guard let request = buildRequest() else { return }
        do {
            let (bytes, response) = try await session.bytes(for: request, delegate: Self.pinned)
            guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
                // 401 (token rotated), 404 (daemon predates /client/events),
                // 5xx — all identical from here: back off and retry. The
                // Connect gate and the sitrep poller are what tell the human
                // their token is wrong; a notification feed that started
                // shouting about it would be the third voice saying so.
                return
            }
            var parser = SSEParser()
            // Split the byte stream by hand rather than with `bytes.lines`.
            // Foundation's AsyncLineSequence silently DROPS empty lines, and
            // the blank line between frames is the only frame terminator SSE
            // has — every event would sit in the parser's buffer unread and
            // the cursor would never advance. Splitting on the LF is safe on
            // UTF-8: 0x0A cannot appear inside a multi-byte sequence.
            var line: [UInt8] = []
            for try await byte in bytes {
                guard byte == UInt8(ascii: "\n") else {
                    // A stream that never sends a newline would grow this
                    // buffer for as long as we hold the connection — days.
                    // Past the frame cap the server is broken or hostile, so
                    // drop the connection and let the backoff loop retry.
                    guard line.count < SSEParser.maxFrameBytes else { return }
                    line.append(byte)
                    continue
                }
                if Task.isCancelled { return }
                consume(parser.feed(String(decoding: line, as: UTF8.self)))
                line.removeAll(keepingCapacity: true)
            }
        } catch {
            // Refused / DNS / inactivity timeout / cancellation. Deliberately
            // silent AND deliberately not stored anywhere: the error text can
            // embed the server URL, and this is a background reconnect, not
            // something the human asked for and is waiting on.
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
            // reconnect forever and wedges the feed behind it.
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
