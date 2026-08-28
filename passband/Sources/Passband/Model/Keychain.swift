// OS keychain storage for the human-door connection settings. The service name
// and the slot names are fixed — changing one orphans an existing install's
// credentials. Connection settings are stored PER ACCOUNT, one
// `server_url.<uuid>` / `api_token.<uuid>` pair per AccountRecord id (see
// Accounts.swift); the unsuffixed slots are the pre-multi-account layout and
// survive only as that migration's source. The API token is written only to the
// keychain: never to disk, a log line, or an error message.
//
// THIS FILE COMPILES INTO THE NOTIFICATION EXTENSION TOO. That is what the
// per-account slot naming below buys: the extension is handed an account id by
// a push and reads exactly that account's credentials, with no index to consult
// and no second spelling of a slot name to drift from this one. The BYOK
// assistant key deliberately did NOT come along — see AssistantKey.swift.

import Foundation
import Security

/// Keyring service name shared by every stored field.
private let keychainService = "passband"
/// Keyring "account" (username) slot PREFIXES within the service. A live slot
/// is `<prefix>.<account uuid>`; bare, these two are the legacy single-account
/// slots — read once by the migration in Accounts.swift, then deleted.
private let accountURL = "server_url"
private let accountToken = "api_token"
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
