// OS keychain storage for the human-door connection settings and the BYOK
// assistant key. Ported from squelch-desktop/src-tauri/src/lib.rs — SAME
// service name and SAME account slots, so an existing squelch-desktop install's
// credentials are picked up by this client with no re-entry.
//
// SECURITY: the API token is written ONLY to the OS keychain. It is never
// written to disk by us, never placed in a log line, and never returned in an
// error message.
//
// The assistant key is the strictest slot: `AssistantKeyStore` can report
// whether a key exists and which provider it routes to, and it can make a
// completion call — `read()` stays fileprivate so the request path (`LLMProxy`)
// is the only thing that handles the secret, which is why that type lives in
// this file. That mirrors the Rust shell, where the key lived only inside
// `llm_complete`.
//
// The ONE exception is `revealAsync()`: Settings shows the stored key behind a
// Show / Edit affordance, and both need the plaintext. It is an explicit,
// human-initiated disclosure to the person who typed the key in the first
// place — not a general read. Nothing else may call it.

import Foundation
import Security

/// Keyring service name shared by every stored field (matches the Tauri shell).
private let keychainService = "squelch-desktop"
/// Keyring "account" (username) slots within the service.
private let accountURL = "server_url"
private let accountToken = "api_token"
/// BYOK assistant key slot — entirely separate from the human-door token above.
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
    /// Read one generic-password slot. A missing entry is `nil` (first run);
    /// any other failure throws. Never logs the value.
    static func read(account: String, service: String = keychainService) throws -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
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
        let base: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
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
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainError.write(status)
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
    /// Load stored settings. Returns nil until BOTH fields have been saved
    /// (the first-run Connect gate relies on this).
    ///
    /// ALWAYS call this off the main actor (see `loadAsync`). A keychain read
    /// can put up the system's "allow access?" panel and block until the human
    /// answers — on the main thread that is a frozen, unpaintable app, which is
    /// exactly the impression you do not want to make on first launch.
    static func load() throws -> ConnectionSettings? {
        let url = try Keychain.read(account: accountURL)
        let token = try Keychain.read(account: accountToken)
        guard let url, let token, !url.isEmpty, !token.isEmpty else { return nil }
        return ConnectionSettings(serverURL: url, apiToken: token)
    }

    /// The safe entry point: performs the (possibly prompting) read on a
    /// background executor so the UI keeps painting and stays responsive.
    static func loadAsync() async -> Result<ConnectionSettings?, Error> {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(returning: Result { try load() })
            }
        }
    }

    /// Persist settings into the OS keychain. The token never touches disk or logs.
    static func save(_ settings: ConnectionSettings) throws {
        try Keychain.write(account: accountURL, value: settings.serverURL)
        try Keychain.write(account: accountToken, value: settings.apiToken)
    }

    /// Off-main-actor write, for the same reason as `loadAsync`.
    static func saveAsync(_ settings: ConnectionSettings) async -> Result<Void, Error> {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(returning: Result { try save(settings) })
            }
        }
    }

    /// Clear stored settings (Disconnect) so the next boot lands on the
    /// Connect gate. Best-effort: failures are swallowed by the caller.
    static func clear() throws {
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

    /// The one function allowed to see the secret. `fileprivate` on purpose:
    /// nothing outside this file — and specifically nothing in the view layer —
    /// can obtain the key. `LLMProxy` lives in this file for that reason.
    fileprivate static func read() -> String? {
        guard let k = try? Keychain.read(account: accountAssistantKey), !k.isEmpty else {
            return nil
        }
        return k
    }

    static func status() -> AssistantKeyStatus {
        guard let k = read() else { return .absent }
        return AssistantKeyStatus(present: true, provider: provider(forKey: k))
    }

    /// Off-main-actor status read — same prompt-blocking reasoning as
    /// `SettingsStore.loadAsync`. Still never yields the key itself.
    static func statusAsync() async -> AssistantKeyStatus {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(returning: status())
            }
        }
    }

    /// Hand the stored key to the Settings UI so it can be shown or edited.
    /// The deliberate hole in the rule above — see this file's header. Off the
    /// main actor for the same prompt-blocking reason as `statusAsync`.
    static func revealAsync() async -> String? {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(returning: read())
            }
        }
    }

    /// Store the user's assistant key. Never logged, never echoed.
    static func set(_ key: String) throws {
        try Keychain.write(account: accountAssistantKey, value: key)
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
    case transport
    case nonJSON

    var errorDescription: String? {
        switch self {
        case .noKey: "No assistant key set — add one in Settings."
        // Deliberately generic: an upstream/transport failure must never
        // surface anything that could include the key.
        case .transport: "assistant request failed (network/tls)"
        case .nonJSON: "assistant returned a non-JSON body"
        }
    }
}

/// Makes assistant completion calls. Reads the key from the keychain, routes by
/// its real prefix (never trusting a caller-supplied provider), injects the auth
/// header, POSTs, and hands back the raw response. The key never leaves this
/// type — it is not a parameter, not a return value, and not in any error.
enum LLMProxy {
    private static let session: URLSession = {
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = 120
        return URLSession(configuration: cfg)
    }()

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
            (data, response) = try await session.data(for: req)
        } catch {
            throw LLMError.transport
        }
        guard let http = response as? HTTPURLResponse else { throw LLMError.transport }
        // Validate it parses as JSON so callers can assume a decodable body.
        guard (try? JSONSerialization.jsonObject(with: data)) != nil else { throw LLMError.nonJSON }
        return LLMResponse(status: http.statusCode, json: data)
    }
}
