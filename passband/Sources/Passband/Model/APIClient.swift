// Typed client for the squelchd human door (/client/*). One method per route.
// Bearer token + base URL come from the keychain; call `configure()` once before
// any request.
//
// Invariant: the token rides the Authorization header and nowhere else — never
// logged, never in a thrown message, never persisted here.

import Foundation

/// The raw bytes of one attachment plus the mime/filename the server served.
struct FetchedAttachment: Sendable {
    var bytes: Data
    var mime: String
    var filename: String
}

/// Search recall mode. Server default is hybrid when vectors exist.
enum SearchMode: String, Sendable { case keyword, semantic, hybrid }

/// Scope for the dev re-triage call: one message, or a trailing-days window.
enum RetriageScope: Sendable {
    case message(Int)
    case days(Int)
}

actor APIClient {
    static let shared = APIClient()

    private struct Config: Sendable {
        var baseURL: String
        var token: String
    }

    private var config: Config?
    private let session: URLSession

    /// 15s is generous for a local daemon. Without a hard timeout a daemon dying
    /// mid-request hangs forever and wedges a poller's in-flight guard with it.
    private static let requestTimeout: TimeInterval = 15
    private static let attachmentTimeout: TimeInterval = 30
    /// A forward is the one send that moves the whole original — the daemon
    /// fetches it back from Gmail and uploads it again, attachments and all —
    /// so 15s would report slow-but-successful forwards as failures and invite
    /// a second, duplicate send, which is the one mistake this app cannot undo.
    ///
    /// This is an IDLE timer, not a deadline: `timeoutIntervalForRequest` is
    /// reset by every byte that arrives, so it fires when the daemon has gone
    /// quiet for a minute, not when the work has taken one. The wall clock is
    /// the session's resource ceiling (`Self.resourceCeiling`), and that is
    /// where the daemon's real worst case had to be paid for — a ~60s-class raw
    /// fetch, plus the send, plus the 5s echo wait, is past two minutes.
    private static let forwardTimeout: TimeInterval = 60

    /// WALL CLOCK for any one request, forward included. It has to sit above the
    /// slowest thing the daemon does, because it is a hard deadline that no
    /// amount of steady progress resets: at 60s it would cut a forward that was
    /// transferring the whole time and hand the caller a "failed" for mail that
    /// went out — an invitation to send it twice, and precisely the bug
    /// `forwardTimeout` was raised to close. 180 clears the ~125s worst case
    /// with room, and no other route comes near it (they idle out at 15s or 30s
    /// long before this matters).
    private static let resourceCeiling: TimeInterval = 180

    init() {
        session = Sessions.ephemeral(
            timeout: Self.requestTimeout, resource: Self.resourceCeiling,
            cachePolicy: .reloadIgnoringLocalCacheData, emptyHeaders: true)
    }

    /// Set the base URL + bearer token used by all subsequent requests.
    func configure(baseURL: String, token: String) {
        // Normalize: strip trailing slashes so we can join with `/client/...`.
        var base = baseURL
        while base.hasSuffix("/") { base.removeLast() }
        config = Config(baseURL: base, token: token)
    }

    /// Forget the configured daemon. After this, requests fail with "client
    /// not configured" instead of presenting a token the user asked to forget.
    func deconfigure() {
        config = nil
    }

    var isConfigured: Bool { config != nil }

    private func requireConfig() throws -> Config {
        guard let config else { throw APIError(.network, 0, "client not configured") }
        return config
    }

    // MARK: - request plumbing

    private enum Method: String { case GET, POST, PUT, DELETE }

    private func buildRequest(
        path: String,
        method: Method,
        query: [String: String?],
        body: Data?,
        timeout: TimeInterval
    ) throws -> URLRequest {
        let cfg = try requireConfig()
        guard var comps = URLComponents(string: cfg.baseURL + path) else {
            throw APIError(.network, 0, "bad server url")
        }
        let pairs = query.compactMap { key, value -> URLQueryItem? in
            guard let value, !value.isEmpty else { return nil }
            return URLQueryItem(name: key, value: value)
        }
        if !pairs.isEmpty { comps.queryItems = pairs.sorted { $0.name < $1.name } }
        guard let url = comps.url else { throw APIError(.network, 0, "bad server url") }

        var req = URLRequest(url: url, timeoutInterval: timeout)
        req.httpMethod = method.rawValue
        req.setValue("Bearer \(cfg.token)", forHTTPHeaderField: "Authorization")
        req.setValue("application/json", forHTTPHeaderField: "Accept")
        if let body {
            req.httpBody = body
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        return req
    }

    /// Perform a request and return the raw body. Non-2xx throws an APIError
    /// whose message comes from the server's `{"error": …}` body when present.
    private func perform(_ req: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: req)
        } catch {
            // Transport failure OR timeout — never echo the token/url detail.
            throw APIError(.network, 0, "cannot reach squelch server")
        }
        guard let http = response as? HTTPURLResponse else {
            throw APIError(.network, 0, "cannot reach squelch server")
        }
        guard (200..<300).contains(http.statusCode) else {
            let kind = APIErrorKind.forStatus(http.statusCode)
            var message = "request failed (\(http.statusCode))"
            if let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
               let err = obj["error"] as? String {
                message = err
            }
            let guards = kind == .guardBlocked ? parseGuardKinds(message) : nil
            throw APIError(kind, http.statusCode, message, guardKinds: guards)
        }
        return (data, http)
    }

    private static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        return d
    }()

    private static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        // Nil optionals are dropped rather than emitted as nulls, which is what
        // the server's serde expects.
        return e
    }()

    private func decode<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
        do {
            return try Self.decoder.decode(T.self, from: data)
        } catch {
            throw APIError(.unknown, 0, "unexpected response shape")
        }
    }

    private func get<T: Decodable>(
        _ path: String, query: [String: String?] = [:], as type: T.Type = T.self
    ) async throws -> T {
        let req = try buildRequest(
            path: path, method: .GET, query: query, body: nil, timeout: Self.requestTimeout)
        let (data, _) = try await perform(req)
        return try decode(T.self, from: data)
    }

    private func post<T: Decodable>(
        _ path: String, body: Encodable? = nil, query: [String: String?] = [:],
        as type: T.Type = T.self, timeout: TimeInterval = APIClient.requestTimeout
    ) async throws -> T {
        var payload: Data? = nil
        if let body { payload = try Self.encoder.encode(AnyEncodable(body)) }
        // Routes that take no body still need `{}` on some handlers; callers
        // pass an explicit empty struct where that matters.
        let req = try buildRequest(
            path: path, method: .POST, query: query, body: payload, timeout: timeout)
        let (data, _) = try await perform(req)
        return try decode(T.self, from: data)
    }

    private func put<T: Decodable>(
        _ path: String, body: Encodable? = nil, as type: T.Type = T.self
    ) async throws -> T {
        var payload: Data? = nil
        if let body { payload = try Self.encoder.encode(AnyEncodable(body)) }
        let req = try buildRequest(
            path: path, method: .PUT, query: [:], body: payload, timeout: Self.requestTimeout)
        let (data, _) = try await perform(req)
        return try decode(T.self, from: data)
    }

    private func postNoContent(_ path: String, body: Encodable? = nil) async throws {
        var payload: Data? = nil
        if let body { payload = try Self.encoder.encode(AnyEncodable(body)) }
        let req = try buildRequest(
            path: path, method: .POST, query: [:], body: payload, timeout: Self.requestTimeout)
        _ = try await perform(req)
    }

    private func deleteNoContent(_ path: String) async throws {
        let req = try buildRequest(
            path: path, method: .DELETE, query: [:], body: nil, timeout: Self.requestTimeout)
        _ = try await perform(req)
    }

    // MARK: - reads

    /// `peek: true` reads the same rows WITHOUT stamping the seen-ledger — for a
    /// reader acting on the user's behalf (the embedded agent) that will surface
    /// only some of what it fetched. Every UI fetch leaves it false, because the
    /// UI showing a row IS the surfacing event.
    func getUpdates(_ params: UpdatesParams = UpdatesParams(), peek: Bool = false) async throws
        -> Page<AttentionUpdate>
    {
        try await get(
            "/client/updates",
            query: [
                "since": params.since,
                "min_importance": params.min_importance.map(String.init),
                "tier": params.tier?.rawValue,
                "status": params.status?.rawValue,
                "band": params.band?.rawValue,
                "limit": params.limit.map(String.init),
                "cursor": params.cursor,
                "peek": peek ? "true" : nil,
                "reminders": params.remindersPending ? "pending" : nil,
                "spam": params.spamOnly ? "only" : nil,
            ])
    }

    func getThread(_ threadId: String) async throws -> ClientThreadView {
        let escaped =
            threadId.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? threadId
        return try await get("/client/thread/\(escaped)")
    }

    /// `sort` is the reader's standing preference (`Prefs.searchSort`), passed
    /// on every search rather than stored server-side: the daemon serves every
    /// paired client, and one device's ordering is not another's.
    ///
    /// A cursor is only meaningful beside the sort it was issued under — the
    /// offset indexes one particular ranking — so a caller paging must pass the
    /// same sort it opened with.
    func search(
        _ q: String,
        limit: Int? = nil,
        cursor: String? = nil,
        mode: SearchMode? = nil,
        sort: SearchSortChoice? = nil
    ) async throws -> Page<SearchHit> {
        try await get(
            "/client/search",
            query: [
                "q": q, "limit": limit.map(String.init), "cursor": cursor, "mode": mode?.rawValue,
                "sort": sort?.rawValue,
            ])
    }

    /// Recipient autocomplete over Sent-derived contacts (people the user has
    /// actually written to). Ranked server-side; empty fragment returns [].
    func contacts(_ q: String, limit: Int = 8) async throws -> [ContactHit] {
        try await get("/client/contacts", query: ["q": q, "limit": String(limit)])
    }

    /// The sent page: mail the user WROTE, newest first, already ordered by the
    /// daemon. HUMAN DOOR ONLY, like everything else on this client — the agent
    /// door has no route that enumerates what its principal has sent.
    func listSent(limit: Int? = nil, cursor: String? = nil) async throws -> Page<SentItem> {
        try await get(
            "/client/sent", query: ["limit": limit.map(String.init), "cursor": cursor])
    }

    func getStats() async throws -> StoreStats { try await get("/client/stats") }

    /// Tell the daemon a thread was OPENED. Fire and forget: it is a ledger
    /// write with nothing to show for it, and a failed one costs a rounding
    /// error in a statistic, never anything the user sees.
    ///
    /// Its own call rather than a side effect of `getThread` because a warmed
    /// thread opens from the prefetch cache without a request, so the reader is
    /// the only thing that knows a person looked at it.
    func markThreadOpened(_ threadId: String) async throws {
        let escaped =
            threadId.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? threadId
        try await postNoContent("/client/thread/\(escaped)/opened")
    }

    /// Whether this daemon can share invites, and what the mail could say.
    /// Asked before anything is shown, so a daemon that cannot share never
    /// renders a button whose only outcome is a refusal.
    func inviteAvailability() async throws -> InviteAvailability {
        try await get("/client/invites")
    }

    /// Mint a code per friend and mail each of them from the user's own
    /// mailbox. One request, N sends, and each recipient's outcome comes back
    /// on its own row.
    ///
    /// A LONGER TIMEOUT than the default, because the daemon does the work
    /// serially: a mint plus a Gmail send per recipient, up to five of them. The
    /// request finishing is what tells the user which ones went; giving up on it
    /// early would leave them staring at mail that may or may not have been
    /// sent under their name.
    func sendInvites(to recipients: [String], subject: String, body: String) async throws
        -> InviteSendResponse
    {
        struct Payload: Encodable {
            let recipients: [String]
            let subject: String
            let body: String
        }
        return try await post(
            "/client/invites",
            body: Payload(recipients: recipients, subject: subject, body: body),
            timeout: 90)
    }

    /// Prove a CANDIDATE credential pair against /client/stats without adopting
    /// it. Same test `connect` makes, minus the part that makes it this app's
    /// identity: adding a second account has to check its daemon while the live
    /// account keeps working, and `configure` is a whole-app change — every
    /// request in flight would finish against the candidate.
    ///
    /// So the request is built here from the arguments instead of from
    /// `config`, and the answer is decoded rather than merely 200-checked: a
    /// host that answers but is not a daemon must fail the same way it fails at
    /// the Connect gate.
    func probe(baseURL: String, token: String) async throws {
        var base = baseURL
        while base.hasSuffix("/") { base.removeLast() }
        guard let url = URL(string: base + "/client/stats") else {
            throw APIError(.network, 0, "bad server url")
        }
        var req = URLRequest(url: url, timeoutInterval: Self.requestTimeout)
        req.httpMethod = Method.GET.rawValue
        req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        req.setValue("application/json", forHTTPHeaderField: "Accept")
        let (data, _) = try await perform(req)
        _ = try decode(StoreStats.self, from: data)
    }

    func getUsage(days: Int? = nil) async throws -> UsageResponse {
        try await get("/client/usage", query: ["days": days.map(String.init)])
    }

    func getTriageConfig() async throws -> TriageConfig { try await get("/client/triage-config") }

    func setTriageConfig(_ patch: TriageConfigPatch) async throws -> TriageConfig {
        try await post("/client/triage-config", body: patch)
    }

    func getAudit(limit: Int? = nil) async throws -> [AuditEntry] {
        try await get("/client/audit", query: ["limit": limit.map(String.init)])
    }

    func getShipments(includeDelivered: Bool = false) async throws -> [Shipment] {
        try await get(
            "/client/shipments", query: ["include_delivered": includeDelivered ? "true" : "false"])
    }

    func getReceipts(days: Int? = nil) async throws -> [Receipt] {
        try await get("/client/receipts", query: ["days": days.map(String.init)])
    }

    func getCalendar(hours: Int? = nil) async throws -> [CalendarUpdate] {
        try await get("/client/calendar", query: ["hours": hours.map(String.init)])
    }

    func getBanking() async throws -> [BankingRecord] { try await get("/client/banking") }

    func getMarketing(days: Int? = nil) async throws -> [MarketingOffer] {
        try await get("/client/marketing", query: ["days": days.map(String.init)])
    }

    func getTriageDebug(_ messageId: Int) async throws -> TriageDebug {
        try await get("/client/triage-debug/\(messageId)")
    }

    // MARK: - attachments

    /// Fetch one attachment's raw bytes over the authenticated human door.
    /// Bearer auth means an <img src> pointed at this route can't work, so ALL
    /// attachment bytes flow through here.
    func fetchAttachment(_ id: Int, fallbackName: String = "attachment") async throws
        -> FetchedAttachment
    {
        let req = try buildRequest(
            path: "/client/attachments/\(id)", method: .GET, query: [:], body: nil,
            timeout: Self.attachmentTimeout)
        let (data, http) = try await perform(req)
        let mime = http.value(forHTTPHeaderField: "Content-Type") ?? "application/octet-stream"
        let name =
            Self.filenameFromDisposition(http.value(forHTTPHeaderField: "Content-Disposition"))
            ?? fallbackName
        return FetchedAttachment(bytes: data, mime: mime, filename: name)
    }

    /// Parse a filename out of a Content-Disposition header. Handles the RFC
    /// 5987 `filename*=UTF-8''…` form and the plain quoted `filename="…"` form.
    nonisolated static func filenameFromDisposition(_ header: String?) -> String? {
        guard let header else { return nil }
        if let m = header.firstMatch(
            of: /filename\*\s*=\s*(?:[Uu][Tt][Ff]-8|[\w-]*)''([^;]+)/)
        {
            let raw = String(m.1).trimmingCharacters(in: .whitespaces)
            if let decoded = raw.removingPercentEncoding { return decoded }
            return raw
        }
        if let m = header.firstMatch(of: /filename\s*=\s*"?([^";]+)"?/) {
            return String(m.1).trimmingCharacters(in: .whitespaces)
        }
        return nil
    }

    // MARK: - rules

    // MARK: - send groups

    /// Every group, for the Groups page. Counts only; membership rides on
    /// [`group(_:)`].
    func listGroups() async throws -> [SendGroup] { try await get("/client/groups") }

    /// Composer autocomplete. An empty fragment is answered with nothing rather
    /// than everything, so callers may pass what the user has typed as-is.
    func searchGroups(_ q: String, limit: Int = 8) async throws -> [SendGroup] {
        try await get("/client/groups", query: ["q": q, "limit": String(limit)])
    }

    /// One group WITH its membership.
    func group(_ id: Int) async throws -> SendGroup { try await get("/client/groups/\(id)") }

    @discardableResult
    func createGroup(_ body: GroupBody) async throws -> SendGroup {
        try await post("/client/groups", body: body)
    }

    /// `members` REPLACES the membership: the editor sends the list it is
    /// showing, so a member dropped on screen is dropped on the daemon.
    @discardableResult
    func updateGroup(_ id: Int, _ body: GroupBody) async throws -> SendGroup {
        try await put("/client/groups/\(id)", body: body)
    }

    /// The route answers `{"deleted": true}`; the body carries nothing a caller
    /// needs, because a failed delete is a thrown status rather than a `false`.
    func deleteGroup(_ id: Int) async throws { try await deleteNoContent("/client/groups/\(id)") }

    /// What has already gone to this group: recorded sends and mail derived from
    /// matching stored recipients against the membership, newest first.
    func groupHistory(_ id: Int, limit: Int? = nil, offset: Int? = nil) async throws
        -> GroupHistoryPage
    {
        try await get(
            "/client/groups/\(id)/history",
            query: ["limit": limit.map(String.init), "offset": offset.map(String.init)])
    }

    /// The membership cap, served rather than compiled in: the app and the
    /// daemon ship on separate trains and a baked-in limit is one that disagrees.
    func groupLimits() async throws -> GroupLimits { try await get("/client/groups/limits") }

    func listRules() async throws -> [SenderRule] { try await get("/client/rules") }

    @discardableResult
    func createRule(_ body: CreateRuleBody) async throws -> CreatedRule {
        try await post("/client/rules", body: body)
    }

    /// Edit one rule IN PLACE. The body mirrors create's exactly (the route
    /// decodes the same struct), so a pattern change is an update rather than a
    /// second row. Another account's id and an unknown id are the same 404.
    @discardableResult
    func updateRule(_ id: Int, _ body: CreateRuleBody) async throws -> CreatedRule {
        try await put("/client/rules/\(id)", body: body)
    }

    func deleteRule(_ id: Int) async throws { try await deleteNoContent("/client/rules/\(id)") }

    // MARK: - unsubscribe

    struct UnsubBody: Codable, Sendable { var message_id: Int }
    struct UnsubResolutionBody: Codable, Sendable {
        var sender: String
        var resolution: String
    }

    func unsubscribe(messageId: Int) async throws -> UnsubscribeResult {
        try await post("/client/unsubscribe", body: UnsubBody(message_id: messageId))
    }

    func getUnsubscribes() async throws -> [UnsubscribeRecord] {
        try await get("/client/unsubscribes")
    }

    @discardableResult
    func setUnsubResolution(sender: String, resolution: UnsubResolution) async throws
        -> UnsubResolutionResult
    {
        try await post(
            "/client/unsubscribes/resolution",
            body: UnsubResolutionBody(sender: sender, resolution: resolution.rawValue))
    }

    // MARK: - sealed

    func listSealed() async throws -> [SealedMeta] { try await get("/client/sealed") }

    /// Reveal exactly one sealed body: audited server-side, `no-store`, and the
    /// returned body must live in view state only — see docs/SECURITY.md §4.
    func revealSealed(_ id: Int) async throws -> RevealedSealed {
        try await post("/client/sealed/\(id)/reveal")
    }

    // MARK: - lifecycle / actions

    struct StatusBody: Codable, Sendable { var status: String }

    @discardableResult
    func setStatus(_ messageId: Int, _ status: AttentionStatus) async throws -> StatusResult {
        try await post(
            "/client/updates/\(messageId)/status", body: StatusBody(status: status.rawValue))
    }

    // MARK: - reminders

    struct ReminderBody: Codable, Sendable { var remind_at: String }
    struct ReminderResult: Codable, Sendable {
        var message_id: Int
        var remind_at: String?
    }

    /// RFC3339 in UTC, which is the only shape the daemon parses. Reminders are
    /// CHOSEN in local time ("tomorrow morning" is a wall clock, not an
    /// offset), so this conversion is the one place the two meet.
    nonisolated(unsafe) private static let wireStamp: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime]
        f.timeZone = TimeZone(secondsFromGMT: 0)
        return f
    }()
    private static let wireStampLock = NSLock()

    /// The same rendering, for a caller that has to name the instant it just
    /// asked for (an optimistic row) rather than send it.
    nonisolated static func rfc3339(_ date: Date) -> String {
        wireStampLock.lock()
        defer { wireStampLock.unlock() }
        return wireStamp.string(from: date)
    }

    /// Park this thread until `date`. The daemon marks the thread done now and
    /// re-opens the message when the stamp comes due — so this is a resolve and
    /// a schedule in one call, and the caller's optimistic removal is correct.
    @discardableResult
    func setReminder(_ messageId: Int, at date: Date) async throws -> ReminderResult {
        try await post(
            "/client/updates/\(messageId)/reminder",
            body: ReminderBody(remind_at: Self.rfc3339(date)))
    }

    /// Drop a pending reminder. Idempotent server-side: a row with none already
    /// answers 200, which is what lets an undo fire without checking first.
    func clearReminder(_ messageId: Int) async throws {
        try await deleteNoContent("/client/updates/\(messageId)/reminder")
    }

    @discardableResult
    func actionArchive(_ messageId: Int) async throws -> StatusResult {
        try await post(
            "/client/actions/archive", body: ArchiveBody(message_id: messageId, confirm: true))
    }

    /// Rescue one message from the provider's spam folder: Gmail drops its SPAM
    /// label and regains INBOX, and the daemon requeues the row for a real
    /// triage verdict. The ONLY write the spam page makes — there is no
    /// "mark as spam" going the other way, because Passband cannot show the
    /// effects of training a filter it does not read.
    /// Ask the daemon to go and fetch the provider's spam folder. FIRE AND
    /// FORGET: the daemon does not track that folder, so this is a real Gmail
    /// round trip measured in tens of seconds, and the answer arrives through
    /// the ordinary updates poll rather than this response.
    ///
    /// `RefreshResult`, the same `{"triggered": bool}` `refreshMail` above
    /// decodes, because it is the same kind of poke at the same sync loop. It
    /// was briefly typed as `StatusResult`, which requires a `status` key this
    /// route does not send: the POST reached the daemon and the fetch ran
    /// perfectly, the DECODE of the acknowledgement threw, and the page reported
    /// that it could not reach the provider. A wrong response type on a
    /// fire-and-forget call fails at the only place it can still be seen.
    @discardableResult
    func refreshSpam() async throws -> RefreshResult {
        try await post("/client/spam/refresh", body: EmptyBody())
    }

    @discardableResult
    func actionNotSpam(_ messageId: Int) async throws -> StatusResult {
        try await post(
            "/client/actions/not_spam", body: NotSpamBody(message_id: messageId, confirm: true))
    }

    @discardableResult
    func actionLabel(_ messageId: Int, add: [String] = [], remove: [String] = []) async throws
        -> StatusResult
    {
        try await post(
            "/client/actions/label",
            body: LabelBody(message_id: messageId, add: add, remove: remove, confirm: true))
    }

    /// Send. Two-phase by design: call without `overrideGuard` for the verdict
    /// (a guarded body yields a 422 with `.guardKinds`), then with it after
    /// explicit consent. A nil `subject` on a reply is omitted from the JSON, so
    /// the daemon derives `Re: <parent subject>`; pass "" only to mean it.
    @discardableResult
    func actionSend(
        body: String, replyToMessageId: Int? = nil, to: String? = nil, cc: String? = nil,
        bcc: String? = nil, groupId: Int? = nil, subject: String? = nil,
        overrideGuard: Bool = false, draftId: Int? = nil, includeTracker: Bool = false,
        replyAll: Bool = false, forwardOfMessageId: Int? = nil
    ) async throws -> SendResult {
        try await post(
            "/client/actions/send",
            body: SendBody(
                reply_to_message_id: replyToMessageId,
                // Omitted when absent, like every other optional here — and
                // never set beside `reply_to_message_id`: the daemon refuses a
                // send that claims to be both an answer and a forward.
                forward_of_message_id: forwardOfMessageId,
                to: to,
                // NOT collapsed to nil when empty, unlike everything else here:
                // `""` is this client saying "nobody, and I mean it", while
                // absent hands the copy list back to the daemon's derivation.
                // The caller decides which it means; see `ComposeSubmit`.
                cc: cc,
                // Nothing derives a blind copy, so empty and absent are the
                // same thing and the caller may pass either.
                bcc: bcc?.isEmpty == true ? nil : bcc,
                // Attribution for an expanded group; the whole audience for a
                // fan-out, whose membership the daemon reads itself.
                group_id: groupId,
                subject: subject, body: body,
                // Both composers write markdown; the daemon renders the HTML
                // half from this same source after the guard has scanned it.
                body_format: "markdown",
                confirm: true, override_guard: overrideGuard, draft_id: draftId,
                // Omitted rather than `false`, like `subject`: an untracked send
                // says nothing about tracking at all.
                include_tracker: includeTracker ? true : nil,
                // Same omission rule, and the daemon expands the set itself —
                // this is a flag, never a recipient list.
                reply_all: replyAll ? true : nil),
            timeout: forwardOfMessageId == nil ? Self.requestTimeout : Self.forwardTimeout)
    }

    /// The recipients a reply to `messageId` would carry, derived server-side.
    /// DISPLAY ONLY: the send derives the same set again from the parent, so a
    /// failure here costs a preview and never the send. 404 on a sealed or
    /// unknown message; a daemon with no write credential answers with an error.
    func replyRecipients(_ messageId: Int, all: Bool = true) async throws -> ReplyRecipients {
        try await get(
            "/client/messages/\(messageId)/reply_recipients",
            query: ["all": all ? "true" : nil])
    }

    // MARK: - shipments

    /// Dismiss one shipment from the records rail. IDEMPOTENT server-side, and
    /// not a delete: the row is hidden, not destroyed, and it comes back on its
    /// own the moment the carrier or a new email reports something new — which
    /// is why nothing here offers an undo.
    ///
    /// The ack is deliberately NOT decoded, exactly as `correctTriage`'s is not:
    /// the route answers with a small object the client renders nothing from, and
    /// a decode failure would toast an error for a write that succeeded. A
    /// non-2xx still throws out of `perform`, which is the only failure that
    /// matters here.
    func clearShipment(_ id: Int) async throws {
        try await postNoContent("/client/shipments/\(id)/clear")
    }

    // MARK: - read tracking

    /// Every recorded open of one SENT message, oldest first. `messageId` is the
    /// local id — the `echo_message_id` the send returned. Never a 404: an
    /// untracked or unknown message is an empty list.
    func messageOpens(_ messageId: Int) async throws -> [MessageOpen] {
        let result: MessageOpensResult = try await get("/client/messages/\(messageId)/opens")
        return result.opens
    }

    func getTrackingConfig() async throws -> TrackingConfig {
        try await get("/client/tracking-config")
    }

    /// Persist the client's default. The daemon never reads it when sending —
    /// it is remembered preference, and every send still states its own
    /// `include_tracker`.
    @discardableResult
    func setTrackingDefault(_ enabled: Bool) async throws -> TrackingConfig {
        try await post("/client/tracking-config", body: TrackingConfigBody(default_enabled: enabled))
    }

    // MARK: - markdown preview

    struct MarkdownPreviewBody: Codable, Sendable { var source: String }
    struct MarkdownPreviewResult: Codable, Sendable { var html: String }

    /// The daemon's own send-path markdown render, for previews. What comes
    /// back is byte-for-byte the HTML half a send of `source` would carry.
    func markdownPreview(_ source: String) async throws -> String {
        let result: MarkdownPreviewResult = try await post(
            "/client/markdown/preview", body: MarkdownPreviewBody(source: source))
        return result.html
    }

    // MARK: - drafts

    /// Every draft this account holds. At most one per reply target plus one
    /// new-message draft, enforced server-side, and a draft whose parent has since
    /// been sealed is already filtered out — so what comes back is exactly what
    /// can be restored.
    func listDrafts() async throws -> [DraftView] { try await get("/client/drafts") }

    /// Upsert by KEY: `replyToMessageId` (nil = the new-message slot) names the
    /// draft, so a second PUT edits the same row rather than making another. A
    /// sealed or unknown parent is a 404 and stores nothing.
    @discardableResult
    func putDraft(
        replyToMessageId: Int?, to: String, cc: String = "", bcc: String = "", subject: String,
        body: String
    ) async throws -> DraftView {
        try await put(
            "/client/drafts",
            body: DraftBody(
                reply_to_message_id: replyToMessageId, to: to, cc: cc, bcc: bcc, subject: subject,
                body: body))
    }

    /// Discard one draft. Another account's id and an unknown id are the same 404.
    func deleteDraft(_ id: Int) async throws { try await deleteNoContent("/client/drafts/\(id)") }

    // MARK: - refresh / retriage

    /// Poke the daemon to poll Gmail immediately. Fire-and-forget server-side.
    @discardableResult
    func refreshMail() async throws -> RefreshResult { try await post("/client/refresh") }

    struct RetriageMessageBody: Codable, Sendable { var message_id: Int }
    struct RetriageDaysBody: Codable, Sendable { var days: Int }

    /// How far the live re-triage has got. 404 on a daemon too old to report it.
    func retriageProgress() async throws -> RetriageProgress {
        try await get("/client/retriage")
    }

    @discardableResult
    func retriage(_ scope: RetriageScope) async throws -> RetriageResult {
        switch scope {
        case .message(let id):
            return try await post("/client/retriage", body: RetriageMessageBody(message_id: id))
        case .days(let d):
            return try await post("/client/retriage", body: RetriageDaysBody(days: d))
        }
    }

    // MARK: - shredder

    func getShredder() async throws -> ShredStats { try await get("/client/shredder") }

    func setShredder(_ patch: ShredderPatch) async throws -> ShredStats {
        try await post("/client/shredder", body: patch)
    }

    struct EmptyBody: Codable, Sendable {}

    func runShredder() async throws -> ShredderRunResult {
        try await post("/client/shredder/run", body: EmptyBody())
    }

    // MARK: - triage feedback

    struct CorrectTriageBody: Codable, Sendable {
        var message_id: Int
        var dimension: String
        var to_value: String
        var note: String?
    }

    /// Record that triage got one wrong and apply the fix (one server-side
    /// transaction: the triage row moves AND a training row is stored). The
    /// response is deliberately NOT decoded — a decode failure would toast an
    /// error for a write that succeeded.
    func correctTriage(messageId: Int, dimension: TriageAxis, toValue: String, note: String? = nil)
        async throws
    {
        try await postNoContent(
            "/client/triage-feedback",
            body: CorrectTriageBody(
                message_id: messageId, dimension: dimension.rawValue, to_value: toValue, note: note)
        )
    }

    func getTriageFeedback(limit: Int? = nil) async throws -> [TriageFeedback] {
        try await get("/client/triage-feedback", query: ["limit": limit.map(String.init)])
    }
}

/// Type-erasing box so `post(body:)` can take any Encodable.
private struct AnyEncodable: Encodable {
    private let encodeIt: (Encoder) throws -> Void
    init(_ wrapped: Encodable) { encodeIt = wrapped.encode(to:) }
    func encode(to encoder: Encoder) throws { try encodeIt(encoder) }
}
