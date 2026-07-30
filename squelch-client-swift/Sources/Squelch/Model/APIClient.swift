// Typed client for the squelch human door (/client/*). One method per route.
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

    init() {
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = Self.requestTimeout
        cfg.timeoutIntervalForResource = 60
        cfg.httpAdditionalHeaders = [:]
        cfg.requestCachePolicy = .reloadIgnoringLocalCacheData
        session = URLSession(configuration: cfg)
    }

    /// Set the base URL + bearer token used by all subsequent requests.
    func configure(baseURL: String, token: String) {
        // Normalize: strip trailing slashes so we can join with `/client/...`.
        var base = baseURL
        while base.hasSuffix("/") { base.removeLast() }
        config = Config(baseURL: base, token: token)
    }

    var isConfigured: Bool { config != nil }

    private func requireConfig() throws -> Config {
        guard let config else { throw APIError(.network, 0, "client not configured") }
        return config
    }

    // MARK: - request plumbing

    private enum Method: String { case GET, POST, DELETE }

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
        as type: T.Type = T.self
    ) async throws -> T {
        var payload: Data? = nil
        if let body { payload = try Self.encoder.encode(AnyEncodable(body)) }
        // Routes that take no body still need `{}` on some handlers; callers
        // pass an explicit empty struct where that matters.
        let req = try buildRequest(
            path: path, method: .POST, query: query, body: payload, timeout: Self.requestTimeout)
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

    func getUpdates(_ params: UpdatesParams = UpdatesParams()) async throws -> Page<AttentionUpdate> {
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
            ])
    }

    func getThread(_ threadId: String) async throws -> ClientThreadView {
        let escaped =
            threadId.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? threadId
        return try await get("/client/thread/\(escaped)")
    }

    func search(_ q: String, limit: Int? = nil, cursor: String? = nil, mode: SearchMode? = nil)
        async throws -> Page<SearchHit>
    {
        try await get(
            "/client/search",
            query: [
                "q": q, "limit": limit.map(String.init), "cursor": cursor, "mode": mode?.rawValue,
            ])
    }

    func getStats() async throws -> StoreStats { try await get("/client/stats") }

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

    func listRules() async throws -> [SenderRule] { try await get("/client/rules") }

    @discardableResult
    func createRule(_ body: CreateRuleBody) async throws -> CreatedRule {
        try await post("/client/rules", body: body)
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

    @discardableResult
    func actionArchive(_ messageId: Int) async throws -> StatusResult {
        try await post(
            "/client/actions/archive", body: ArchiveBody(message_id: messageId, confirm: true))
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
    /// explicit consent.
    @discardableResult
    func actionSend(
        body: String, replyToMessageId: Int? = nil, to: String? = nil, subject: String? = nil,
        overrideGuard: Bool = false
    ) async throws -> SendResult {
        try await post(
            "/client/actions/send",
            body: SendBody(
                reply_to_message_id: replyToMessageId, to: to, subject: subject, body: body,
                confirm: true, override_guard: overrideGuard))
    }

    // MARK: - refresh / retriage

    /// Poke the daemon to poll Gmail immediately. Fire-and-forget server-side.
    @discardableResult
    func refreshMail() async throws -> RefreshResult { try await post("/client/refresh") }

    struct RetriageMessageBody: Codable, Sendable { var message_id: Int }
    struct RetriageDaysBody: Codable, Sendable { var days: Int }

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
