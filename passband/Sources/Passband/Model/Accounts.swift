// The ACCOUNT INDEX: which accounts this client knows about, in what order,
// and which one is live. One daemon = one email = one account record.
//
// NOTHING SECRET LIVES HERE. The index is UserDefaults — id, label and
// creation date only. Every id names a pair of keychain slots
// (`server_url.<uuid>` / `api_token.<uuid>`, see SettingsStore), and that
// indirection is the whole point: the index is cheap and prompt-free to read
// on the main actor, the credentials behind it are not.
//
// The array is ORDERED and the order is meaningful — it is the switching order
// the UI numbers its accounts by, so appending must never reshuffle what came
// before.
//
// A pre-multi-account install has its credentials in the two unsuffixed legacy
// slots and no index at all. `loadOrMigrate()` is the one-time repair. It is
// written to be re-runnable: every step that can fail leaves the legacy state
// untouched, so a denied keychain prompt or a crash simply means the next boot
// tries again.

import Foundation

/// One account the client can talk to. IDENTITY ONLY — the server URL and
/// token live in the keychain under `id`. `label` is the human's name for it
/// and may be empty, in which case the UI falls back to the server's host:port.
struct AccountRecord: Codable, Sendable, Equatable, Identifiable {
    var id: UUID
    var label: String
    var createdAt: Date

    init(id: UUID = UUID(), label: String = "", createdAt: Date = Date()) {
        self.id = id
        self.label = label
        self.createdAt = createdAt
    }
}

/// The whole persisted index as one value: the ordered records plus which of
/// them is live.
struct AccountIndexState: Sendable, Equatable {
    var accounts: [AccountRecord] = []
    var activeId: UUID?

    /// The live record. nil when there is none (first run, or after the last
    /// account was removed) and also when the stored active id names a record
    /// that is gone — callers treat both as "no account", which is exactly the
    /// Connect gate.
    var active: AccountRecord? {
        guard let activeId else { return nil }
        return accounts.first { $0.id == activeId }
    }

    static let empty = AccountIndexState()
}

enum AccountIndex {
    /// The two UserDefaults keys the index itself occupies.
    private static let accountsKey = "passband.accounts"
    private static let activeKey = "passband.accounts.active"

    /// UserDefaults state that belongs to ONE ACCOUNT rather than to the
    /// install: the SSE cursor, the 2FA seen-set and the code-reveal
    /// decisions. Each is stored under `scopedKey`, and the migration below
    /// carries the legacy install's values across.
    ///
    /// Deliberately NOT in this list: `passband.pref.signature`,
    /// `passband.name` and `passband.favicons` — those belong to the human or
    /// to the render cache, not to a mailbox.
    static let scopedDefaultsKeys = [
        "passband.events.lastSeen",
        "passband.auth-seen",
        "passband.auth-decisions",
    ]

    /// The per-account name for one of those keys. Derived in ONE place so a
    /// reader, a writer and the migration can never disagree on the spelling —
    /// the same trap `Keychain.baseQuery` guards against.
    static func scopedKey(_ base: String, _ accountId: UUID) -> String {
        "\(base).\(accountId.uuidString)"
    }

    private static var defaults: UserDefaults { .standard }

    /// ISO-8601 dates rather than Codable's default (a bare seconds double) so
    /// `defaults read app.passband.client passband.accounts` is legible to a
    /// human debugging an install. Encoder and decoder must agree; they are
    /// here together for that reason. The format quantises `createdAt` to the
    /// second, which is why nothing keys off it — identity is `id`, and order
    /// is the array's.
    private static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.dateEncodingStrategy = .iso8601
        return e
    }()
    private static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.dateDecodingStrategy = .iso8601
        return d
    }()

    // MARK: - read / write

    /// Read the index. UserDefaults only — no keychain, so no "allow access?"
    /// panel and no reason to leave the main actor. Unreadable JSON reads as
    /// no accounts, which lands on the Connect gate rather than crashing.
    static func load() -> AccountIndexState {
        let raw = defaults.string(forKey: accountsKey) ?? ""
        let accounts = (try? decoder.decode([AccountRecord].self, from: Data(raw.utf8))) ?? []
        let activeId = defaults.string(forKey: activeKey).flatMap(UUID.init(uuidString:))
        return AccountIndexState(accounts: accounts, activeId: activeId)
    }

    /// Persist the index. Stored as a JSON *string* for the same legibility
    /// reason as the date strategy above.
    static func save(_ state: AccountIndexState) {
        if let data = try? encoder.encode(state.accounts) {
            defaults.set(String(decoding: data, as: UTF8.self), forKey: accountsKey)
        }
        if let id = state.activeId {
            defaults.set(id.uuidString, forKey: activeKey)
        } else {
            defaults.removeObject(forKey: activeKey)
        }
    }

    /// The record a freshly-probed connection's credentials belong under: the
    /// live one when this install already has an account (a re-connect, or
    /// Settings re-validating a token), otherwise a brand new one.
    ///
    /// NOT persisted here. `upsert` does that, and only after the keychain
    /// write has succeeded — an index entry whose slots are empty would read
    /// as a connected account with nothing behind it.
    static func activeOrNew() -> AccountRecord {
        load().active ?? AccountRecord()
    }

    /// Record one account and (by default) make it live. An existing id is
    /// replaced IN PLACE so re-connecting an account keeps its position.
    static func upsert(_ record: AccountRecord, activate: Bool = true) {
        var state = load()
        if let i = state.accounts.firstIndex(where: { $0.id == record.id }) {
            state.accounts[i] = record
        } else {
            state.accounts.append(record)
        }
        if activate { state.activeId = record.id }
        save(state)
    }

    /// Forget one account: drop the record, hand `active` to the first
    /// survivor (nil when that was the last one — the Connect gate), and clear
    /// its scoped UserDefaults so a later re-pair of the same daemon does not
    /// inherit a stale cursor or seen-set under a new id.
    ///
    /// The KEYCHAIN slots are the caller's to clear
    /// (`SettingsStore.clear(accountId:)`): a delete that can raise the access
    /// panel does not belong hidden behind a defaults write.
    static func remove(_ id: UUID) {
        var state = load()
        state.accounts.removeAll { $0.id == id }
        if state.activeId == id { state.activeId = state.accounts.first?.id }
        save(state)
        forgetScopedDefaults(for: id)
    }

    /// Drop every per-account UserDefaults key belonging to one account.
    static func forgetScopedDefaults(for id: UUID) {
        for base in scopedDefaultsKeys {
            defaults.removeObject(forKey: scopedKey(base, id))
        }
    }

    // MARK: - migration

    /// Read the index, first repairing a pre-multi-account install. Off the
    /// main actor because the repair reads and writes the keychain, which can
    /// raise the system's "allow access?" panel and block until the human
    /// answers (see `offMain`).
    static func loadOrMigrate() async -> AccountIndexState {
        await offMain { migrate() }
    }

    /// The one-time repair, in commit order. Each early return leaves the
    /// legacy install exactly as it was — no half-migrated state is ever
    /// visible, and the next boot just tries again.
    private static func migrate() -> AccountIndexState {
        let state = load()
        // Already indexed — the ordinary path on every boot after the first,
        // including installs that were never single-account.
        guard state.accounts.isEmpty else { return state }

        // Nothing to carry across: no legacy credentials (a fresh install), or
        // the read failed / was denied. Both mean "leave everything alone";
        // deleting nothing loses nothing.
        guard let legacy = try? SettingsStore.loadLegacy() else { return state }

        let record = AccountRecord()

        // 1. Credentials into the suffixed slots. Both or neither: a partial
        //    write is rolled back so the retry does not strand a lone slot
        //    under an id nothing will ever name again.
        do {
            try SettingsStore.save(legacy, accountId: record.id)
        } catch {
            try? SettingsStore.clear(accountId: record.id)
            return state
        }

        // 2. Account-scoped UserDefaults, COPIED. The originals go in step 4,
        //    after the index below is durable.
        for base in scopedDefaultsKeys {
            guard let value = defaults.object(forKey: base) else { continue }
            defaults.set(value, forKey: scopedKey(base, record.id))
        }

        // 3. The index. THIS is the commit point: from here the install is
        //    multi-account and everything left below is litter, not state.
        let migrated = AccountIndexState(accounts: [record], activeId: record.id)
        save(migrated)

        // 4. Legacy cleanup, best-effort. A failure here (or a crash before
        //    it) leaves slots and keys that are simply never read again.
        try? SettingsStore.clearLegacy()
        for base in scopedDefaultsKeys {
            defaults.removeObject(forKey: base)
        }

        return migrated
    }
}
