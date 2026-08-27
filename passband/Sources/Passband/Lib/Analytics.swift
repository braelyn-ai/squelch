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
//
// There is exactly ONE string that leaves without being in that vocabulary: the
// hosted `analytics_id`, adopted as the install's distinct_id by `adopt` below.
// It is not an exception to the rule, it is the rule enforced a second way — a
// UUID shape check admits hex and dashes and nothing else, which carries as
// much mail as a closed set does. Read `adopt`'s comment before adding a second
// one; the honest count of bypasses in this file is one, and it should stay one.

import Foundation

// Only for the device idiom in the event context below, and fenced the way
// Pairing.swift fences its own UIKit import: the Mac target must not so much as
// name a framework it does not have, and the fence sits directly over the one
// expression that needs it so the two can never drift apart.
#if os(iOS)
    import UIKit
#endif

enum Analytics {
    /// PostHog PROJECT API key (phc_…, public by design — it can only ingest).
    /// Empty disables analytics entirely; the env var wins for dev overrides.
    private static let apiKey = "phc_uuYkpwXQYxrz33f4bQsi5ozAQ6LL5kPsnrnU7vVvshni"
    private static let host = "https://us.i.posthog.com"

    private static let client: PostHogClient? = {
        let env = ProcessInfo.processInfo.environment
        let key = env["PASSBAND_POSTHOG_KEY"] ?? apiKey
        let endpoint = env["PASSBAND_POSTHOG_HOST"] ?? host
        guard key.hasPrefix("phc_"), let url = URL(string: "\(endpoint)/batch/")
        else { return nil }
        return PostHogClient(apiKey: key, endpoint: url)
    }()

    /// The Settings > Privacy level, read straight from UserDefaults on every
    /// event — thread-safe where the main-actor Prefs is not, and a change in
    /// the pane applies to the very next capture without any plumbing.
    private static var level: TelemetryLevel {
        TelemetryLevel(
            rawValue: UserDefaults.standard.string(forKey: TelemetryLevel.prefKey) ?? "")
            ?? .full
    }

    /// Install lifecycle observers and record the first screen. Call once from
    /// the app delegate; every other entry point is safe to call regardless.
    static func start() {
        guard let client else { return }
        let nc = NotificationCenter.default
        nc.addObserver(
            forName: Platform.didBecomeActiveNotification, object: nil, queue: nil
        ) { _ in lifecycle("Application Opened") }
        nc.addObserver(
            forName: Platform.didResignActiveNotification, object: nil, queue: nil
        ) { _ in
            lifecycle("Application Backgrounded")
            client.flush()
        }
        nc.addObserver(
            forName: Platform.willTerminateNotification, object: nil, queue: nil
        ) { _ in client.flushBlocking() }
    }

    /// Lifecycle events ride at the `minimal` level — they are what makes a
    /// session visible at all.
    private static func lifecycle(_ event: String) {
        guard level != .none else { return }
        client?.capture(event)
    }

    /// The closed set of event names this app emits. A new event is added HERE
    /// first — an unknown name at a call site is a bug, not a data point.
    private static let allowedEvents: Set<String> = [
        "$screen", "Application Opened", "Application Backgrounded",
        "email_archived", "email_done", "email_reopened", "email_labeled",
        "email_remind",
        "block_rule_created", "compose_opened", "compose_send",
        "thread_opened", "thread_live_arrival", "undo_fired", "assistant_asked",
        "triage_corrected", "triage_digest", "rule_created", "rule_deleted",
        "process_completed", "notification_opened", "sealed_revealed",
        "shipment_cleared", "shipments_poll_kicked",
        "connect_succeeded", "connection_lost", "connection_restored",
        "account_added",
        "tour_completed", "tour_skipped", "whats_new_shown",
        "invite_sent", "invite_nudge_accepted", "invite_nudge_dismissed",
        "group_created", "group_updated", "group_deleted",
    ]

    /// The counter events that ride at `minimal`: anonymous counts and
    /// closed-vocabulary enums that measure whether the product works (sends,
    /// triage volume, corrections, connection health) with no behavioral
    /// detail. `full` adds the remaining action verbs (`compose_opened`,
    /// `email_labeled`) — the "which actions are used" layer.
    private static let minimalEvents: Set<String> = [
        "email_archived", "email_done", "email_reopened", "email_remind",
        "block_rule_created", "compose_send", "thread_opened",
        // One bool: mail landed in the thread on screen, and whether the reader
        // was at the end of it (carried to the new message) or up in the
        // history (held in place, pointed at it). Anonymous, and it is the only
        // measure of whether live-loading is worth the round trips.
        "thread_live_arrival",
        "undo_fired", "assistant_asked",
        "triage_corrected", "triage_digest", "rule_created", "rule_deleted",
        "process_completed", "notification_opened", "sealed_revealed",
        "connect_succeeded", "connection_lost", "connection_restored",
        "account_added",
        // Onboarding carries one number, the step it ended on. Whether the
        // first run explains itself is exactly the "does the product work"
        // question this level exists for.
        "tour_completed", "tour_skipped",
        // Sharing carries counts and nothing else: how many invites went, how
        // many did not, and whether the one-time ask was taken up. No address
        // is anywhere near this, at any level.
        "invite_sent", "invite_nudge_accepted", "invite_nudge_dismissed",
        // Send groups carry a mode and a member COUNT. Whether people organize
        // their correspondents into audiences at all, and which shape they
        // reach for, is the "does the product work" question for this feature;
        // who is in one is not measured anywhere, at any level.
        "group_created", "group_updated", "group_deleted",
    ]

    /// The closed set of STRING property values allowed off the machine.
    /// Bools and numbers pass freely — cardinality that small cannot carry
    /// mail. Strings can, so only these exact ones exist.
    private static let allowedStrings: Set<String> =
        Set(MainView.allCases.map(\.rawValue)).union([
            // compose_send / compose_opened
            "new", "reply", "forward", "sent", "guard_blocked", "forbidden", "failure",
            // undo_fired kinds
            "archive", "done", "label", "ruleDelete", "groupDelete", "remind",
            // group_* modes — how an audience is addressed. "individual" is the
            // only one that is not also a header name.
            "to", "bcc", "individual",
            // triage_corrected axes and wire values — the daemon's closed
            // TriageAxis::allowed vocabulary, mirrored in TriageTargets.
            "tier", "category", "sensitivity",
            "past_due", "deadline", "signal", "noise",
            "invoice", "autopay_bill", "banking_statement", "transaction_alert",
            "marketing", "general", "sealed", "normal", "unset",
            // rule_created dispositions
            "surface", "squelch", "filtered",
            // assistant_asked models
            "haiku", "opus",
            // assistant_asked transports
            "relay", "byok",
            // invite_sent sources — where the share sheet was raised from
            // (`ShareOrigin`). "settings" is also a MainView above; a Set union
            // makes the duplicate free, and naming it here is what keeps this
            // list readable as the vocabulary of THIS event.
            "rail", "settings", "nudge",
        ])

    /// Screen views ride at `minimal` alongside lifecycle events.
    static func screen(_ name: String) {
        guard level != .none else { return }
        guard allowedStrings.contains(name) else {
            assertionFailure("analytics: screen name outside vocabulary")
            return
        }
        client?.capture("$screen", properties: ["$screen_name": name])
    }

    /// Counter events (`minimalEvents`) ride at `minimal`; the remaining
    /// action verbs are `full`-only.
    static func capture(_ event: String, _ properties: [String: Any] = [:]) {
        switch level {
        case .none: return
        case .minimal: guard minimalEvents.contains(event) else { return }
        case .full: break
        }
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

    /// At-most-daily events, for periodic snapshot counters like
    /// `triage_digest`. UserDefaults-backed so a relaunch does not re-emit;
    /// the level gate stays in `capture`.
    static func daily(_ event: String, _ properties: [String: Any] = [:]) {
        guard level != .none else { return }  // don't stamp a day we sent nothing
        let key = "app.passband.analytics.daily.\(event)"
        let last = UserDefaults.standard.double(forKey: key)
        let now = Date().timeIntervalSince1970
        guard now - last >= 24 * 3600 else { return }
        UserDefaults.standard.set(now, forKey: key)
        capture(event, properties)
    }

    /// Marks that a hosted `aid` has already been adopted. First aid wins,
    /// forever.
    private static let adoptedKey = "app.passband.analytics.aidAdopted"

    /// Adopt the analytics id a hosted pairing link carried, as this install's
    /// PostHog `distinct_id`.
    ///
    /// WHY THIS EXISTS: PostHog otherwise sees a random per-install UUID, and
    /// the control plane sees an email address, and nothing joins them — so
    /// "did anyone we invited ever open the app" is unanswerable. The `aid` is
    /// the join key, minted control-plane side and opaque on both ends. PostHog
    /// still never sees an email; it sees an id that only the control plane can
    /// resolve to a person, which is the whole design.
    ///
    /// THIS BYPASSES THE EVENT VOCABULARY ON PURPOSE. Every other string that
    /// leaves this file must appear in `allowedStrings`, because mail content
    /// could only ever escape as a free-form string. A UUID is not free-form:
    /// the shape check below reduces it to hex and dashes at fixed offsets,
    /// which carries exactly as much mail as a closed set does — none. That
    /// check IS the guarantee here, so it is not optional, and the id must NOT
    /// be added to `allowedStrings`/`allowedEvents` instead: those sets are
    /// closed and enumerable and a per-person id would end both properties.
    ///
    /// THREAT MODEL: any web page can open a `passband://` URL, so a hostile
    /// page can name an `aid` of its choosing. It buys nothing. Adoption
    /// happens only after a claim AND a successful connect against the daemon
    /// the SAME link named, so the attacker needs a live, unspent pairing code
    /// for a daemon the victim then completes pairing with — at which point
    /// they had the mailbox, not merely the analytics. There is no read-back
    /// channel either: the id is written outward and never surfaced. Residual
    /// risk is analytics pollution (one install's events filed under an id of
    /// the attacker's choosing), and nothing about mail, credentials, or the
    /// account.
    static func adopt(analyticsId: String) {
        guard UUID(uuidString: analyticsId) != nil else {
            assertionFailure("analytics: adopted id is not a uuid")
            return
        }
        // FIRST AID WINS. A `distinct_id` is per-install and an install is one
        // human; the multi-account case is that one human running two mailboxes,
        // not two people sharing a Mac. The funnel this exists for is "the
        // person we invited started using the app", so the FIRST hosted account
        // is the one that answers it — a second account's aid is simply not
        // adopted, and the person keeps the identity their history is under.
        let defaults = UserDefaults.standard
        guard !defaults.bool(forKey: adoptedKey) else { return }
        defaults.set(true, forKey: adoptedKey)

        // With telemetry off the id still flips, but no alias event is sent.
        // The flip costs nothing and is not a transmission: it means that if
        // the user later turns telemetry back on, what they send arrives as
        // THEM rather than opening a third identity. The alias is a real event,
        // so it waits — while the user has said nothing leaves, nothing leaves.
        client?.adoptDistinctId(analyticsId.lowercased(), mergeHistory: level != .none)
    }
}

/// Buffered batch uploader. All state is confined to `queue`; the public
/// surface is safe to call from any thread or actor.
final class PostHogClient: @unchecked Sendable {
    private let apiKey: String
    private let endpoint: URL
    private let queue = DispatchQueue(label: "app.passband.analytics")

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
    /// not identity, so a random UUID minted once is the whole story — until a
    /// hosted pairing hands over an `aid`, at which point `adoptDistinctId`
    /// swaps this for the control plane's opaque per-person id so the two
    /// halves of the funnel are the same row. Still a UUID, still carrying no
    /// address: the control plane holds the email, PostHog holds the id, and
    /// nothing on this machine can join them.
    ///
    /// A `var`, but a queue-confined one: every read and write below happens
    /// inside `queue`. Initialized directly in `init` rather than on the queue,
    /// which is safe because nothing can enqueue against a client that has not
    /// finished being constructed, and the lazy `static let` that builds it is
    /// the memory barrier that publishes the initial value to every later
    /// queue block.
    private var distinctId: String

    private static let bufferCap = 500
    private static let flushAt = 20
    private static let flushEvery: TimeInterval = 15

    /// Where the install id lives across launches. An adopted `aid` is written
    /// straight back to this same key rather than to one beside it: after
    /// adoption the aid simply IS this install's id, so a relaunch reads it
    /// like any other and needs no re-adoption logic at all.
    private static let distinctIdKey = "app.passband.analytics.distinctId"

    private let iso: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()

    /// The per-event envelope PostHog reads to place a session on a platform.
    ///
    /// The platform half is COMPILED, not detected: this file ships in both the
    /// Mac and the iOS target, and a hardcoded "macOS" here reported every
    /// iPhone as a desktop — which does not merely mislabel iOS, it poisons
    /// every whole-app number by folding two platforms into one. `$os_version`
    /// was always honest (ProcessInfo answers per-process); only the two
    /// constants beside it were not.
    private let context: [String: Any] = {
        let info = Bundle.main.infoDictionary ?? [:]
        let os = ProcessInfo.processInfo.operatingSystemVersion
        let version = info["CFBundleShortVersionString"] as? String ?? "0"
        var ctx: [String: Any] = [
            "$app_name": "Passband",
            "$app_version": version,
            "$app_build": info["CFBundleVersion"] as? String ?? "0",
            "$os_version": "\(os.majorVersion).\(os.minorVersion).\(os.patchVersion)",
            "$lib": "passband-native",
            "$lib_version": version,
        ]
        #if os(iOS)
            ctx["$os_name"] = "iOS"
            // PostHog's own device buckets: an iPad is a Tablet, everything
            // else the target runs on is a Mobile. Read off the main thread on
            // purpose and safely — `userInterfaceIdiom` is a compile-time-ish
            // constant for the process, not view state.
            ctx["$device_type"] =
                UIDevice.current.userInterfaceIdiom == .pad ? "Tablet" : "Mobile"
        #else
            ctx["$os_name"] = "macOS"
            ctx["$device_type"] = "Desktop"
        #endif
        return ctx
    }()

    init(apiKey: String, endpoint: URL) {
        self.apiKey = apiKey
        self.endpoint = endpoint

        if let saved = UserDefaults.standard.string(forKey: Self.distinctIdKey) {
            distinctId = saved
        } else {
            distinctId = UUID().uuidString.lowercased()
            UserDefaults.standard.set(distinctId, forKey: Self.distinctIdKey)
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

    /// Switch this install's `distinct_id` to an id the control plane minted,
    /// optionally telling PostHog that the two ids are one person.
    ///
    /// `$create_alias` is the merge, and its direction is the part that is easy
    /// to get backwards: the ENVELOPE carries the OLD id and the `alias`
    /// property carries the NEW one, matching what posthog-js and posthog-python
    /// send. PostHog folds the aliased id into the one already on the event, so
    /// naming the new id in the envelope would merge the wrong direction and
    /// strand this install's history.
    ///
    /// Everything is done inside `queue` because `distinctId` and `buffer` live
    /// there, and doing it in one block is also what makes the ordering true:
    /// events already buffered keep the id they were captured under, because
    /// they happened to the old identity. Only what is captured after this
    /// block runs belongs to the new one.
    ///
    /// `mergeHistory: false` still flips the id — see `Analytics.adopt`.
    func adoptDistinctId(_ newId: String, mergeHistory: Bool) {
        queue.async {
            let old = self.distinctId
            // Idempotent: the same aid arrives again on every re-pair and every
            // hosted sign-in, and a second alias to an id we already answer to
            // would be a wasted event at best.
            guard newId != old else { return }

            if mergeHistory {
                let now = Date()
                var props = self.context
                props["$session_id"] = self.currentSessionId(now)
                props["distinct_id"] = old
                props["alias"] = newId
                self.buffer.append([
                    "event": "$create_alias",
                    "distinct_id": old,
                    "timestamp": self.iso.string(from: now),
                    "properties": props,
                ])
                // The cap trims from the FRONT, so the alias — appended last —
                // is the one event a full buffer cannot cost us.
                if self.buffer.count > Self.bufferCap {
                    self.buffer.removeFirst(self.buffer.count - Self.bufferCap)
                }
            }

            self.distinctId = newId
            UserDefaults.standard.set(newId, forKey: Self.distinctIdKey)
            // Sent now rather than on the next 15s tick: the alias is what makes
            // the funnel joinable, and the moment right after pairing is exactly
            // when a first-run install is most likely to be quit.
            self.flushLocked()
        }
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
