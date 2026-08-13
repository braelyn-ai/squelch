// OS keychain storage for the human-door connection settings and the BYOK
// assistant key. The service name and the slot names are fixed — changing one
// orphans an existing install's credentials. Connection settings are stored
// PER ACCOUNT, one `server_url.<uuid>` / `api_token.<uuid>` pair per
// AccountRecord id (see Accounts.swift); the unsuffixed slots are the
// pre-multi-account layout and survive only as that migration's source. The
// API token is written only to the keychain: never to disk, a log line, or an
// error message. The assistant key is stricter — `read()` is fileprivate so
// `LLMProxy` (which lives in this file for that reason) is its only consumer,
// and `revealAsync()` is the one deliberate hole, for Settings'
// human-initiated Show / Edit.
//
// Both LLMProxy entry points hold that line: `complete()` and `stream()` each
// read the key inside themselves and hand back only provider output — the key
// is never a parameter, a returned/yielded value, or anything an error carries.

import Foundation
import Security

/// Keyring service name shared by every stored field.
private let keychainService = "passband"
/// Keyring "account" (username) slot PREFIXES within the service. A live slot
/// is `<prefix>.<account uuid>`; bare, these two are the legacy single-account
/// slots — read once by the migration in Accounts.swift, then deleted.
private let accountURL = "server_url"
private let accountToken = "api_token"
/// BYOK assistant key slot — entirely separate from the human-door tokens
/// above, and GLOBAL rather than per-account: the key is the human's, not the
/// mailbox's, and one assistant serves every account.
private let accountAssistantKey = "assistant_api_key"

enum KeychainError: Error, LocalizedError {
    case write(OSStatus)
    case read(OSStatus)

    var errorDescription: String? {
        switch self {
        // Never include the value; the status code alone is diagnosable.
        case .write(let s): "keychain write failed (\(s))"
        case .read(let s): "keychain read failed (\(s))"
        }
    }
}

enum Keychain {
    /// The three attributes that IDENTIFY one slot. Every query below starts
    /// here: a read, a write and a delete that disagreed on any of them would
    /// address different items and quietly stop being each other's inverse.
    private static func baseQuery(account: String, service: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    /// Read one generic-password slot. A missing entry is `nil` (first run);
    /// any other failure throws. Never logs the value.
    static func read(account: String, service: String = keychainService) throws -> String? {
        var query = baseQuery(account: account, service: service)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess else { throw KeychainError.read(status) }
        guard let data = item as? Data, let value = String(data: data, encoding: .utf8) else {
            return nil
        }
        return value
    }

    /// Upsert one generic-password slot.
    static func write(account: String, value: String, service: String = keychainService) throws {
        let base = baseQuery(account: account, service: service)
        let data = Data(value.utf8)
        // Try update first (the common path after first run).
        let update = SecItemUpdate(
            base as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        if update == errSecSuccess { return }
        if update == errSecItemNotFound {
            var add = base
            add[kSecValueData as String] = data
            add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlocked
            let status = SecItemAdd(add as CFDictionary, nil)
            guard status == errSecSuccess else { throw KeychainError.write(status) }
            return
        }
        throw KeychainError.write(update)
    }

    /// Remove one slot entirely. A missing entry is a no-op.
    static func delete(account: String, service: String = keychainService) throws {
        let query = baseQuery(account: account, service: service)
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainError.write(status)
        }
    }
}

/// Run one keychain call OFF the main actor. Every read here can raise the
/// system's "allow access?" panel and block until the human answers — on the
/// main actor that is a frozen UI, so the `…Async` wrappers below are the only
/// entry points the app is allowed to use. Module-visible for the one caller
/// outside this file that also touches the keychain:
/// `AccountIndex.loadOrMigrate`.
func offMain<T>(_ work: @escaping @Sendable () -> T) async -> T {
    await withCheckedContinuation { continuation in
        DispatchQueue.global(qos: .userInitiated).async {
            continuation.resume(returning: work())
        }
    }
}

// MARK: - human-door settings

/// The connection settings the app needs to talk to the human door.
/// `apiToken` is sensitive and lives only in the keychain at rest.
struct ConnectionSettings: Sendable, Equatable {
    var serverURL: String
    var apiToken: String
}

enum SettingsStore {
    /// The two slots one account's credentials occupy. Derived in ONE place
    /// for the same reason `Keychain.baseQuery` is: a reader and a writer that
    /// spelled the suffix differently would address different items and
    /// quietly stop being each other's inverse.
    private static func urlSlot(_ accountId: UUID) -> String {
        "\(accountURL).\(accountId.uuidString)"
    }
    private static func tokenSlot(_ accountId: UUID) -> String {
        "\(accountToken).\(accountId.uuidString)"
    }

    /// Load one account's stored settings; nil until BOTH fields are saved
    /// (the first-run Connect gate relies on that). ALWAYS call off the main
    /// actor (see `loadAsync`): a keychain read can raise the system's "allow
    /// access?" panel and block until the human answers, freezing the UI.
    static func load(accountId: UUID) throws -> ConnectionSettings? {
        let url = try Keychain.read(account: urlSlot(accountId))
        let token = try Keychain.read(account: tokenSlot(accountId))
        guard let url, let token, !url.isEmpty, !token.isEmpty else { return nil }
        return ConnectionSettings(serverURL: url, apiToken: token)
    }

    /// The safe entry point: runs the (possibly prompting) read on a background
    /// executor so the UI keeps painting.
    static func loadAsync(accountId: UUID) async -> Result<ConnectionSettings?, Error> {
        await offMain { Result { try load(accountId: accountId) } }
    }

    /// Persist one account's settings into the OS keychain. The token never
    /// touches disk or logs.
    static func save(_ settings: ConnectionSettings, accountId: UUID) throws {
        try Keychain.write(account: urlSlot(accountId), value: settings.serverURL)
        try Keychain.write(account: tokenSlot(accountId), value: settings.apiToken)
    }

    /// Off-main-actor write, for the same reason as `loadAsync`.
    static func saveAsync(_ settings: ConnectionSettings, accountId: UUID) async -> Result<
        Void, Error
    > {
        await offMain { Result { try save(settings, accountId: accountId) } }
    }

    /// Clear one account's stored settings (Disconnect, or removing an
    /// account) so nothing is left to reconnect with. Best-effort: failures
    /// are swallowed by the caller.
    static func clear(accountId: UUID) throws {
        try Keychain.delete(account: urlSlot(accountId))
        try Keychain.delete(account: tokenSlot(accountId))
    }

    // MARK: legacy single-account slots

    /// The unsuffixed slots a pre-multi-account install wrote. ONLY
    /// `AccountIndex`'s one-time migration may call these two — everything
    /// else addresses an account by id.
    static func loadLegacy() throws -> ConnectionSettings? {
        let url = try Keychain.read(account: accountURL)
        let token = try Keychain.read(account: accountToken)
        guard let url, let token, !url.isEmpty, !token.isEmpty else { return nil }
        return ConnectionSettings(serverURL: url, apiToken: token)
    }

    /// Delete the legacy slots. Called only once the migration has the same
    /// credentials safely under an account id.
    static func clearLegacy() throws {
        try Keychain.delete(account: accountURL)
        try Keychain.delete(account: accountToken)
    }
}

// MARK: - BYOK assistant key

enum AssistantProvider: String, Sendable {
    case anthropic, openai

    var label: String {
        switch self {
        case .anthropic: "Anthropic"
        case .openai: "OpenAI"
        }
    }
}

/// Whether an assistant key is stored, and (if so) which provider it routes to.
/// Lets Settings show "key set — Anthropic" without ever handling the secret.
struct AssistantKeyStatus: Sendable, Equatable {
    var present: Bool
    var provider: AssistantProvider?

    static let absent = AssistantKeyStatus(present: false, provider: nil)
}

enum AssistantKeyStore {
    /// Provider inferred from the key prefix, matching the server-side Stage-2
    /// routing. Never exposes the key value.
    fileprivate static func provider(forKey key: String) -> AssistantProvider {
        key.hasPrefix("sk-ant-") ? .anthropic : .openai
    }

    /// No provider's key ever contains whitespace, but a pasted one can — a
    /// terminal that soft-wrapped the key embeds a newline mid-string, edge
    /// trimming misses it, and URLSession then silently DROPS the whole
    /// `x-api-key` header rather than send an invalid value. The provider's
    /// answer ("x-api-key header is required") says nothing about why. So
    /// whitespace is stripped wholesale, on the way in AND out — out too, so a
    /// key stored before this rule heals at first use instead of demanding a
    /// re-paste.
    private static func sanitized(_ key: String) -> String {
        key.filter { !$0.isWhitespace }
    }

    /// The one function allowed to see the secret. `fileprivate` on purpose:
    /// nothing outside this file — the view layer especially — can obtain the
    /// key.
    fileprivate static func read() -> String? {
        guard let k = try? Keychain.read(account: accountAssistantKey) else { return nil }
        let key = sanitized(k)
        return key.isEmpty ? nil : key
    }

    static func status() -> AssistantKeyStatus {
        guard let k = read() else { return .absent }
        return AssistantKeyStatus(present: true, provider: provider(forKey: k))
    }

    /// Off-main-actor status read — same prompt-blocking reason as
    /// `SettingsStore.loadAsync`. Still never yields the key itself.
    static func statusAsync() async -> AssistantKeyStatus {
        await offMain { status() }
    }

    /// Hand the stored key to Settings' Show / Edit affordance — the one
    /// deliberate hole in the rule above, and nothing else may call it. Off the
    /// main actor for the same prompt-blocking reason as `statusAsync`.
    static func revealAsync() async -> String? {
        await offMain { read() }
    }

    /// Store the user's assistant key. Never logged, never echoed.
    static func set(_ key: String) throws {
        try Keychain.write(account: accountAssistantKey, value: sanitized(key))
    }

    /// Forget the stored assistant key.
    static func clear() throws { try Keychain.delete(account: accountAssistantKey) }
}

// MARK: - BYOK LLM proxy

/// Result of one LLM round-trip: the upstream HTTP status plus the raw JSON
/// body (an Anthropic message, an OpenAI completion, or a provider error — the
/// caller inspects `status` and shapes accordingly).
struct LLMResponse: Sendable {
    var status: Int
    var json: Data
}

enum LLMError: Error, LocalizedError {
    case noKey
    case wrongProvider
    case transport
    case nonJSON
    /// Upstream said no: the HTTP status plus whatever `error.message` the
    /// provider's JSON body carried (nil when the body wasn't parseable).
    case provider(status: Int, message: String?)

    var errorDescription: String? {
        switch self {
        case .noKey: "No assistant key set — add one in Settings."
        case .wrongProvider:
            "Streaming needs an Anthropic key (sk-ant-…). Paste one in Settings."
        // Deliberately generic: an upstream/transport failure must never
        // surface anything that could include the key.
        case .transport: "assistant request failed (network/tls)"
        case .nonJSON: "assistant returned a non-JSON body"
        // The provider's own words when we have them, matching what
        // `complete()`'s callers already show from `parsed.error?.message`.
        case .provider(let status, let message):
            message ?? "assistant request failed (\(status))"
        }
    }
}

/// Makes assistant completion calls. Routes by the key's real prefix, never a
/// caller-supplied provider. The key never leaves this type — not a parameter,
/// not a return value, not in any error.
enum LLMProxy {
    private static let session = Sessions.ephemeral(timeout: 120)

    /// Its own session for streamed calls. On a `bytes(for:)` transfer the
    /// request timeout is an INACTIVITY timeout — URLSession resets it on every
    /// byte — which is exactly what a long answer needs: generous between
    /// chunks, never a deadline on the whole generation.
    private static let streamSession = Sessions.ephemeral(timeout: streamTimeout)
    private static let streamTimeout: TimeInterval = 120

    /// REFUSES EVERY REDIRECT, on both sessions. URLSession strips
    /// `Authorization` across origins but carries custom headers — `x-api-key`
    /// above all — verbatim through a hop, so a 302 from a compromised or
    /// hijacked provider host would post the user's own BYOK key to whatever
    /// host it named, and the user would see only "assistant request failed".
    /// Neither provider legitimately 3xx's /v1/messages, so refusing costs
    /// nothing: the 3xx surfaces to the caller as a non-200. Same empty
    /// allow-list EventStream keeps on the token-bearing feed.
    private static let pinned = SchemePinned(allow: [])

    /// Cap on a non-200 body we read to find the provider's error text. Past
    /// this the upstream is broken or hostile and there is nothing to quote.
    private static let maxErrorBytes = 64 * 1024

    /// Make ONE completion call. `body` is a fully-formed provider request body
    /// MINUS auth (model, messages, tools, max_tokens, …).
    static func complete(body: Data) async throws -> LLMResponse {
        guard let key = AssistantKeyStore.read() else { throw LLMError.noKey }

        var req: URLRequest
        switch AssistantKeyStore.provider(forKey: key) {
        case .anthropic:
            req = URLRequest(url: URL(string: "https://api.anthropic.com/v1/messages")!)
            req.setValue(key, forHTTPHeaderField: "x-api-key")
            req.setValue("2023-06-01", forHTTPHeaderField: "anthropic-version")
        case .openai:
            req = URLRequest(url: URL(string: "https://api.openai.com/v1/chat/completions")!)
            req.setValue("Bearer \(key)", forHTTPHeaderField: "authorization")
        }
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "content-type")
        req.httpBody = body

        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: req, delegate: Self.pinned)
        } catch {
            throw LLMError.transport
        }
        guard let http = response as? HTTPURLResponse else { throw LLMError.transport }
        // Validate it parses as JSON so callers can assume a decodable body.
        guard (try? JSONSerialization.jsonObject(with: data)) != nil else { throw LLMError.nonJSON }
        return LLMResponse(status: http.statusCode, json: data)
    }

    /// Make ONE streaming call and yield each SSE frame's `data:` payload as it
    /// arrives. `body` is a fully-formed Anthropic request body MINUS auth, with
    /// `"stream": true` already set by the caller.
    ///
    /// Same invariant as `complete()`: the key is read HERE, inside the task,
    /// and never leaves — not a parameter, not a yielded element, not inside any
    /// error this can throw. Anthropic only: an OpenAI key finishes the stream
    /// with `.wrongProvider` rather than posting a body no one can read to a
    /// host that never agreed to see it.
    static func stream(body: Data) -> AsyncThrowingStream<String, Error> {
        AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    guard let key = AssistantKeyStore.read() else { throw LLMError.noKey }
                    guard AssistantKeyStore.provider(forKey: key) == .anthropic else {
                        throw LLMError.wrongProvider
                    }

                    var req = URLRequest(url: URL(string: "https://api.anthropic.com/v1/messages")!)
                    req.httpMethod = "POST"
                    req.setValue(key, forHTTPHeaderField: "x-api-key")
                    req.setValue("2023-06-01", forHTTPHeaderField: "anthropic-version")
                    req.setValue("application/json", forHTTPHeaderField: "content-type")
                    req.setValue("text/event-stream", forHTTPHeaderField: "Accept")
                    req.timeoutInterval = streamTimeout
                    req.httpBody = body

                    let bytes: URLSession.AsyncBytes
                    let response: URLResponse
                    do {
                        (bytes, response) = try await streamSession.bytes(
                            for: req, delegate: Self.pinned)
                    } catch {
                        // Refused / DNS / TLS / inactivity. Never the error text:
                        // it can quote the request we just built.
                        throw LLMError.transport
                    }
                    guard let http = response as? HTTPURLResponse else { throw LLMError.transport }
                    guard http.statusCode == 200 else {
                        throw LLMError.provider(
                            status: http.statusCode, message: await errorMessage(from: bytes))
                    }

                    var parser = SSEParser()
                    // Split by hand rather than with `bytes.lines`: AsyncLineSequence
                    // silently DROPS empty lines, and the blank line between frames
                    // is SSE's only frame terminator, so every event would sit
                    // unread in the buffer (see EventStream.connect). Splitting on
                    // LF is safe on UTF-8 — 0x0A cannot appear inside a multi-byte
                    // sequence.
                    var line: [UInt8] = []
                    do {
                        for try await byte in bytes {
                            guard byte == UInt8(ascii: "\n") else {
                                // A stream that never sends a newline would grow this
                                // buffer for as long as the answer runs. Past the cap
                                // the upstream is broken; fail rather than swell.
                                guard line.count < SSEParser.maxFrameBytes else {
                                    throw LLMError.transport
                                }
                                line.append(byte)
                                continue
                            }
                            try Task.checkCancellation()
                            if let frame = parser.feed(String(decoding: line, as: UTF8.self)) {
                                continuation.yield(frame.data)
                            }
                            line.removeAll(keepingCapacity: true)
                        }
                    } catch let error as LLMError {
                        throw error
                    } catch is CancellationError {
                        throw CancellationError()
                    } catch {
                        throw LLMError.transport
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            // A consumer that stops reading (or is cancelled) must take the
            // upstream connection down with it.
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    /// Pull a bounded prefix of a non-200 body and dig out `error.message`.
    /// Returns nil for anything unparseable; never surfaces headers or the key.
    private static func errorMessage(from bytes: URLSession.AsyncBytes) async -> String? {
        var raw: [UInt8] = []
        do {
            for try await byte in bytes {
                raw.append(byte)
                if raw.count >= maxErrorBytes { break }
            }
        } catch {
            // A body that died mid-read is still worth parsing.
        }
        let json = try? JSONSerialization.jsonObject(with: Data(raw))
        guard let object = json as? [String: Any],
            let error = object["error"] as? [String: Any],
            let message = error["message"] as? String, !message.isEmpty
        else { return nil }
        return message
    }
}
