// PostHog capture, written natively. The official SDK is deliberately NOT
// used: build.sh compiles a flat swiftc file list with no package resolution,
// and posthog-ios drags in vendored libwebp for session replay — a feature
// that does not exist on macOS anyway. Everything PostHog offers a Mac app
// (events, screens, session analytics) is one JSON POST to /batch, so the
// client lives here where all of it can be read.
//
// PRIVACY: nothing content-shaped goes through this file, and that is
// ENFORCED, not just observed: every event name and every string value must
// appear in the closed vocabulary below or it is dropped (and trips an assert
// in a debug build). Email data — subjects, senders, bodies, labels, questions
// — could only ever leave as a free-form string, so free-form strings cannot
// leave at all.

import AppKit
import Foundation

enum Analytics {
    /// PostHog PROJECT API key (phc_…, public by design — it can only ingest).
    /// Empty disables analytics entirely; the env var wins for dev overrides.
    private static let apiKey = "phc_uuYkpwXQYxrz33f4bQsi5ozAQ6LL5kPsnrnU7vVvshni"
    private static let host = "https://us.i.posthog.com"

    private static let client: PostHogClient? = {
        let env = ProcessInfo.processInfo.environment
        let key = env["SQUELCH_POSTHOG_KEY"] ?? apiKey
        let endpoint = env["SQUELCH_POSTHOG_HOST"] ?? host
        guard key.hasPrefix("phc_"), let url = URL(string: "\(endpoint)/batch/")
        else { return nil }
        return PostHogClient(apiKey: key, endpoint: url)
    }()

    /// Install lifecycle observers and record the first screen. Call once from
    /// the app delegate; every other entry point is safe to call regardless.
    static func start() {
        guard let client else { return }
        let nc = NotificationCenter.default
        nc.addObserver(
            forName: NSApplication.didBecomeActiveNotification, object: nil, queue: nil
        ) { _ in client.capture("Application Opened") }
        nc.addObserver(
            forName: NSApplication.didResignActiveNotification, object: nil, queue: nil
        ) { _ in
            client.capture("Application Backgrounded")
            client.flush()
        }
        nc.addObserver(
            forName: NSApplication.willTerminateNotification, object: nil, queue: nil
        ) { _ in client.flushBlocking() }
    }

    /// The closed set of event names this app emits. A new event is added HERE
    /// first — an unknown name at a call site is a bug, not a data point.
    private static let allowedEvents: Set<String> = [
        "$screen", "Application Opened", "Application Backgrounded",
        "email_archived", "email_done", "email_reopened", "email_labeled",
        "block_rule_created", "compose_opened", "compose_send",
        "thread_opened", "undo_fired", "assistant_asked",
    ]

    /// The closed set of STRING property values allowed off the machine.
    /// Bools and numbers pass freely — cardinality that small cannot carry
    /// mail. Strings can, so only these exact ones exist.
    private static let allowedStrings: Set<String> =
        Set(MainView.allCases.map(\.rawValue)).union([
            // compose_send / compose_opened
            "new", "reply", "sent", "guard_blocked", "forbidden", "failure",
            // undo_fired kinds
            "archive", "done", "label", "ruleDelete",
        ])

    static func screen(_ name: String) {
        guard allowedStrings.contains(name) else {
            assertionFailure("analytics: screen name outside vocabulary")
            return
        }
        client?.capture("$screen", properties: ["$screen_name": name])
    }

    static func capture(_ event: String, _ properties: [String: Any] = [:]) {
        guard allowedEvents.contains(event) else {
            assertionFailure("analytics: event outside vocabulary: \(event)")
            return
        }
        var safe: [String: Any] = [:]
        for (key, value) in properties {
            switch value {
            case is Bool, is Int, is Double:
                safe[key] = value
            case let s as String where allowedStrings.contains(s):
                safe[key] = s
            default:
                assertionFailure("analytics: value outside vocabulary for \(key)")
            }
        }
        client?.capture(event, properties: safe)
    }
}

/// Buffered batch uploader. All state is confined to `queue`; the public
/// surface is safe to call from any thread or actor.
final class PostHogClient: @unchecked Sendable {
    private let apiKey: String
    private let endpoint: URL
    private let queue = DispatchQueue(label: "dev.squelch.analytics")

    private var buffer: [[String: Any]] = []
    private var inFlight = false

    // Session analytics need only a shared $session_id on the events; PostHog
    // derives duration and entry/exit server-side. The id must be a UUIDv7 —
    // the ingestion pipeline rejects sessions whose ids do not time-order.
    // Rotation mirrors posthog-ios: 30 idle minutes ends a session, and one
    // session never exceeds 24 hours.
    private var sessionId = ""
    private var sessionStart = Date.distantPast
    private var lastActivity = Date.distantPast

    /// Anonymous install id. Analytics for a single-user app needs stability,
    /// not identity, so a random UUID minted once is the whole story.
    private let distinctId: String

    private static let bufferCap = 500
    private static let flushAt = 20
    private static let flushEvery: TimeInterval = 15

    private let iso: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()

    private let context: [String: Any] = {
        let info = Bundle.main.infoDictionary ?? [:]
        let os = ProcessInfo.processInfo.operatingSystemVersion
        let version = info["CFBundleShortVersionString"] as? String ?? "0"
        return [
            "$app_name": "Squelch",
            "$app_version": version,
            "$app_build": info["CFBundleVersion"] as? String ?? "0",
            "$os_name": "macOS",
            "$os_version": "\(os.majorVersion).\(os.minorVersion).\(os.patchVersion)",
            "$device_type": "Desktop",
            "$lib": "squelch-native",
            "$lib_version": version,
        ]
    }()

    init(apiKey: String, endpoint: URL) {
        self.apiKey = apiKey
        self.endpoint = endpoint

        let key = "dev.squelch.analytics.distinctId"
        if let saved = UserDefaults.standard.string(forKey: key) {
            distinctId = saved
        } else {
            distinctId = UUID().uuidString.lowercased()
            UserDefaults.standard.set(distinctId, forKey: key)
        }

        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + Self.flushEvery, repeating: Self.flushEvery)
        timer.setEventHandler { [weak self] in self?.flushLocked() }
        timer.resume()
        flushTimer = timer
    }
    private var flushTimer: DispatchSourceTimer?

    func capture(_ event: String, properties: [String: Any] = [:]) {
        // JSON-shaped dictionaries are not Sendable by type, but every one of
        // them is confined to `queue` — the box states that instead of warning.
        let boxed = Unchecked(properties)
        queue.async {
            let now = Date()
            var props = self.context
            props["$session_id"] = self.currentSessionId(now)
            for (k, v) in boxed.value { props[k] = v }
            self.buffer.append([
                "event": event,
                "distinct_id": self.distinctId,
                "timestamp": self.iso.string(from: now),
                "properties": props,
            ])
            if self.buffer.count > Self.bufferCap {
                self.buffer.removeFirst(self.buffer.count - Self.bufferCap)
            }
            if self.buffer.count >= Self.flushAt { self.flushLocked() }
        }
    }

    func flush() {
        queue.async { self.flushLocked() }
    }

    /// Terminate-path flush: the process is about to exit, so give the upload
    /// a bounded moment to leave the machine instead of dying in the buffer.
    func flushBlocking(timeout: TimeInterval = 1.5) {
        let sem = DispatchSemaphore(value: 0)
        queue.async { self.flushLocked { sem.signal() } }
        _ = sem.wait(timeout: .now() + timeout)
    }

    // MARK: - queue-confined

    private func currentSessionId(_ now: Date) -> String {
        if sessionId.isEmpty
            || now.timeIntervalSince(lastActivity) > 30 * 60
            || now.timeIntervalSince(sessionStart) > 24 * 3600
        {
            sessionId = Self.uuidV7(now)
            sessionStart = now
        }
        lastActivity = now
        return sessionId
    }

    private func flushLocked(_ done: (@Sendable () -> Void)? = nil) {
        guard !inFlight, !buffer.isEmpty else {
            done?()
            return
        }
        let batch = Unchecked(buffer)
        buffer.removeAll()
        inFlight = true

        var req = URLRequest(url: endpoint)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try? JSONSerialization.data(
            withJSONObject: ["api_key": apiKey, "batch": batch.value])

        URLSession.shared.dataTask(with: req) { _, response, error in
            self.queue.async {
                self.inFlight = false
                let code = (response as? HTTPURLResponse)?.statusCode ?? 0
                // Requeue only what can succeed later: network failures and
                // server errors. A 4xx means the payload itself is bad and a
                // retry would loop forever — those events are dropped.
                if error != nil || code == 0 || code >= 500 {
                    self.buffer.insert(contentsOf: batch.value, at: 0)
                    if self.buffer.count > Self.bufferCap {
                        self.buffer.removeFirst(self.buffer.count - Self.bufferCap)
                    }
                }
                done?()
            }
        }.resume()
    }

    /// UUIDv7: 48-bit unix milliseconds, then version/variant bits over random.
    private static func uuidV7(_ date: Date) -> String {
        var b = [UInt8](repeating: 0, count: 16)
        let ms = UInt64(date.timeIntervalSince1970 * 1000)
        for i in 0..<6 { b[i] = UInt8((ms >> (8 * UInt64(5 - i))) & 0xFF) }
        for i in 6..<16 { b[i] = UInt8.random(in: 0...255) }
        b[6] = (b[6] & 0x0F) | 0x70
        b[8] = (b[8] & 0x3F) | 0x80
        let hex = b.map { String(format: "%02x", $0) }.joined()
        var s = hex
        for offset in [8, 13, 18, 23] {
            s.insert("-", at: s.index(s.startIndex, offsetBy: offset))
        }
        return s
    }
}

/// Sendability asserted by the owner, not the type: used for JSON-shaped
/// values that only ever cross into the client's one serial queue.
private struct Unchecked<T>: @unchecked Sendable {
    let value: T
    init(_ value: T) { self.value = value }
}
