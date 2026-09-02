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
//
// `AccountManager` at the bottom is the live, observable view of all that — the
// list the UI renders, the id everything account-scoped derives its keys from,
// the entry point to a switch, and the owner of the permanently per-account
// objects: an `EventStream` per record, live account or not, and a
// `BackgroundAuthWatch` for every record that is NOT live.

import Foundation
import Observation

/// One account the client can talk to. NO CREDENTIALS — the server URL and
/// token live in the keychain under `id`. `label` is the human's name for it
/// and may be empty, in which case the UI falls back to the server's host:port.
struct AccountRecord: Codable, Sendable, Equatable, Identifiable {
    var id: UUID
    var label: String
    var createdAt: Date
    /// The daemon's host, with its port when the URL names one. Kept in the
    /// index so the UI can NAME an unlabelled account without going to the
    /// keychain for it: a menu is rebuilt on every render and a keychain read
    /// can raise the system's access panel, which blocks the main actor until
    /// the human answers. A hostname is not a credential — it is the thing the
    /// human typed into the connect form, and the token behind it stays where
    /// it was.
    ///
    /// Empty means "not known yet": a record written by a build from before
    /// this field existed. `AccountManager.noteDisplayHost` fills those in from
    /// the credentials the notification feed reads at boot anyway, so nothing
    /// pays an extra keychain round trip for it.
    var displayHost: String

    init(
        id: UUID = UUID(), label: String = "", createdAt: Date = Date(), displayHost: String = ""
    ) {
        self.id = id
        self.label = label
        self.createdAt = createdAt
        self.displayHost = displayHost
    }

    /// Decoded by hand for ONE reason: `displayHost` post-dates the stored
    /// format, and the synthesized decoder would reject every record written
    /// without it — which is every record on an install that has been upgraded.
    /// A missing host is simply unknown, not a corrupt index.
    enum CodingKeys: String, CodingKey { case id, label, createdAt, displayHost }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UUID.self, forKey: .id)
        label = try c.decode(String.self, forKey: .label)
        createdAt = try c.decode(Date.self, forKey: .createdAt)
        displayHost = try c.decodeIfPresent(String.self, forKey: .displayHost) ?? ""
    }

    /// The host:port to show for a server URL. The port is included because
    /// two daemons on one machine differ by nothing else, which is exactly the
    /// shape of a local two-account setup. Anything unparseable falls back to
    /// the raw string: it is what the human typed, so it is the best name we
    /// have for it.
    static func host(from serverURL: String) -> String {
        guard let comps = URLComponents(string: serverURL), let host = comps.host, !host.isEmpty
        else { return serverURL }
        guard let port = comps.port else { return host }
        return "\(host):\(port)"
    }

    /// What the UI calls this account: the human's label, the daemon's host
    /// when they have not given one, and a last-resort placeholder when the
    /// host is not known either (a pre-`displayHost` record whose feed has not
    /// come up yet). Never empty — a nameless row in a switcher is unclickable
    /// in practice.
    var displayName: String {
        let named = label.trimmingCharacters(in: .whitespacesAndNewlines)
        if !named.isEmpty { return named }
        if !displayHost.isEmpty { return displayHost }
        return "unnamed account"
    }

    /// The one character the rail badge wears.
    var initial: String {
        guard let first = displayName.first(where: { !$0.isWhitespace }) else { return "?" }
        return String(first).uppercased()
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
}

enum AccountIndex {
    /// The two UserDefaults keys the index itself occupies.
    private static let accountsKey = "passband.accounts"
    private static let activeKey = "passband.accounts.active"

    /// The migration's own bookkeeping, none of it state the app reads:
    ///
    /// - `migratedKey` — the repair has RUN to a conclusion. An empty index
    ///   after that means the human removed their last account, not that they
    ///   never had one, so the legacy slots must not be looked at again.
    /// - `migratingKey` — the account id an in-progress repair committed to,
    ///   parked before the first keychain write so a retry reuses it.
    /// - `orphansKey` — ids whose keychain slots outlived their index entry
    ///   because the delete failed. Retried at every boot.
    /// - `legacyOrphanKey` — the same thing for the LEGACY slots, minus the
    ///   id: there is only ever one pair of them, so a flag says it.
    private static let migratedKey = "passband.accounts.migrated"
    private static let migratingKey = "passband.accounts.migrating"
    private static let orphansKey = "passband.accounts.orphans"
    private static let legacyOrphanKey = "passband.accounts.orphan-legacy"

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
        "passband.thread-style",
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

    /// Make one account live, touching NOTHING else. This is a switch's commit
    /// point, and a switch has no business reordering the list or rewriting a
    /// record on its way through.
    static func setActive(_ id: UUID) {
        defaults.set(id.uuidString, forKey: activeKey)
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

    // MARK: - orphaned keychain slots

    /// Remember an account id whose keychain slots we FAILED to delete. The
    /// index entry that named them is going away regardless — the human asked
    /// to disconnect and is entitled to have it happen — and a slot no entry
    /// names is a live API token nothing will ever address again. Parked here,
    /// `sweepOrphans` retries the delete at every boot until it takes.
    static func parkOrphan(_ id: UUID) {
        var ids = defaults.stringArray(forKey: orphansKey) ?? []
        guard !ids.contains(id.uuidString) else { return }
        ids.append(id.uuidString)
        defaults.set(ids, forKey: orphansKey)
    }

    /// Delete the legacy single-account slots, parking a refusal for the boot
    /// sweep. Every caller is a point past which those slots are never READ
    /// again, so a failed delete leaves a live API token nothing addresses —
    /// exactly the problem `parkOrphan` exists for.
    private static func clearLegacySlots() {
        do {
            try SettingsStore.clearLegacy()
            defaults.removeObject(forKey: legacyOrphanKey)
        } catch {
            defaults.set(true, forKey: legacyOrphanKey)
        }
    }

    /// Close the pre-multi-account question for good, when the index goes
    /// EMPTY — a Disconnect, or removing the last account.
    ///
    /// An empty index is precisely the shape `migrate` reads as "possibly a
    /// single-account install", and `migratedKey` is the only thing between
    /// that reading and resurrecting the account the human just removed. Every
    /// path that concludes the repair sets that flag — but a repair whose
    /// legacy READ was denied never reached one: it leaves the flag unset and
    /// the slots full, and the removal above hands the next boot an empty
    /// index to find them from. So both are done here, at the moment the last
    /// account goes: the slots deleted, the question marked answered.
    static func sealLegacyIfEmpty() {
        guard load().accounts.isEmpty else { return }
        clearLegacySlots()
        // A parked migration id would be stranded for good once the flag below
        // is set — the repair branches that discard it are unreachable after.
        discardAbandonedMigration()
        defaults.set(true, forKey: migratedKey)
    }

    /// Retry every parked delete, keeping the ones that fail again. Called
    /// only from `loadOrMigrate`, which is already off the main actor — these
    /// are keychain calls and can raise the access panel.
    private static func sweepOrphans() {
        if defaults.bool(forKey: legacyOrphanKey) { clearLegacySlots() }
        let ids = defaults.stringArray(forKey: orphansKey) ?? []
        guard !ids.isEmpty else { return }
        var left: [String] = []
        for raw in ids {
            // An unparseable entry names no slots; dropping it is the only
            // thing that can be done about it.
            guard let id = UUID(uuidString: raw) else { continue }
            do {
                try SettingsStore.clear(accountId: id)
            } catch {
                left.append(raw)
            }
        }
        if left.isEmpty {
            defaults.removeObject(forKey: orphansKey)
        } else {
            defaults.set(left, forKey: orphansKey)
        }
    }

    // MARK: - migration

    /// Read the index, first sweeping any failed deletes and repairing a
    /// pre-multi-account install. Off the main actor because both touch the
    /// keychain, which can raise the system's "allow access?" panel and block
    /// until the human answers (see `offMain`).
    static func loadOrMigrate() async -> AccountIndexState {
        await offMain {
            sweepOrphans()
            return migrate()
        }
    }

    /// The one-time repair, in commit order. Each early return leaves the
    /// legacy install exactly as it was — no half-migrated state is ever
    /// visible, and the next boot just tries again.
    private static func migrate() -> AccountIndexState {
        let state = load()
        // Already indexed — the ordinary path on every boot after the first,
        // including installs that were never single-account.
        guard state.accounts.isEmpty else {
            // Conclude the repair if some earlier boot could not. An install
            // holding accounts IS multi-account, and the legacy slots are
            // already unreachable from here — a migration into a non-empty
            // index would mint a duplicate account, so this code never does
            // one. Leaving the question open instead lets a later "remove the
            // last account" reopen it and resurrect one from those slots, so
            // they are cleared rather than left holding a live token.
            if !defaults.bool(forKey: migratedKey) {
                clearLegacySlots()
                discardAbandonedMigration()
                defaults.set(true, forKey: migratedKey)
            }
            return state
        }
        // ALREADY CONCLUDED ONCE. An empty index past this point means the
        // human removed their last account, not that they never had one:
        // without this flag a Disconnect whose legacy cleanup had failed (step
        // 4 is best-effort) would be undone by the very next boot, which would
        // find those slots and resurrect the account from them.
        guard !defaults.bool(forKey: migratedKey) else { return state }

        let legacy: ConnectionSettings?
        do {
            legacy = try SettingsStore.loadLegacy()
        } catch {
            // The read failed or was denied. Leave everything alone, conclude
            // nothing, and try again next boot — deleting nothing loses
            // nothing, and marking the repair done here would lose an install.
            return state
        }
        guard let legacy else {
            // Nothing to carry across: a fresh install (or one whose legacy
            // slots a previous run already took). Conclude the repair so no
            // later boot reads those slots — and so a Disconnect cannot be
            // undone by one.
            discardAbandonedMigration()
            defaults.set(true, forKey: migratedKey)
            return state
        }

        // The id this repair commits under, MINTED ONCE and parked before the
        // keychain is touched. A crash between the slot write below and the
        // index commit would otherwise leave a second copy of the token under
        // an id nothing names; reusing the parked id means the retry overwrites
        // those very slots instead of stranding them.
        //
        // The host comes along for free: the legacy credentials are already in
        // hand here, so the migrated record can name itself without anyone
        // going back to the keychain to ask.
        let record = AccountRecord(
            id: pendingMigrationId(), displayHost: AccountRecord.host(from: legacy.serverURL))

        // 1. Credentials into the suffixed slots. Both or neither: a partial
        //    write is rolled back so the retry does not strand a lone slot.
        //    The parked id SURVIVES the rollback — that is what makes the
        //    retry address the same two slots.
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
        //    multi-account and everything left below is litter, not state. The
        //    parked id has done its job the moment the index names it.
        let migrated = AccountIndexState(accounts: [record], activeId: record.id)
        save(migrated)
        // Concluded BEFORE the parked id is released: a crash between these two
        // writes then reads as "done, with litter" (the benign early return
        // above) rather than "abandoned" — the repair branch must never see an
        // index that names the parked id.
        defaults.set(true, forKey: migratedKey)
        defaults.removeObject(forKey: migratingKey)

        // 4. Legacy cleanup. A failure here (or a crash before it) leaves
        //    slots and keys that are never read again — the `migratedKey`
        //    above is what guarantees the "never" — so this is about the token
        //    still sitting in them, which is why a refused delete is parked
        //    for the boot sweep rather than shrugged off.
        clearLegacySlots()
        for base in scopedDefaultsKeys {
            defaults.removeObject(forKey: base)
        }

        return migrated
    }

    /// The id a (possibly retried) migration commits under. Parked BEFORE the
    /// first keychain write and reused until the index commit clears it.
    private static func pendingMigrationId() -> UUID {
        if let raw = defaults.string(forKey: migratingKey), let id = UUID(uuidString: raw) {
            return id
        }
        let id = UUID()
        defaults.set(id.uuidString, forKey: migratingKey)
        return id
    }

    /// A parked migration id with no legacy credentials left to migrate: an
    /// earlier attempt wrote the suffixed slots and then lost its source before
    /// the index commit. Nothing will ever name that id, so its slots are
    /// deleted here rather than left holding a live token — and a refused
    /// delete goes to the orphan sweep, which is exactly this problem.
    private static func discardAbandonedMigration() {
        guard let raw = defaults.string(forKey: migratingKey),
            let id = UUID(uuidString: raw)
        else { return }
        // The index naming the parked id means the migration COMMITTED and only
        // the parked key's removal was lost. Those slots are the live account's
        // only credentials — the legacy pair is already gone on every path that
        // reaches here — so deleting them would take both copies of the token.
        if load().accounts.contains(where: { $0.id == id }) {
            defaults.removeObject(forKey: migratingKey)
            return
        }
        do {
            try SettingsStore.clear(accountId: id)
        } catch {
            parkOrphan(id)
        }
        defaults.removeObject(forKey: migratingKey)
    }
}

// MARK: - the live account list

/// The observable face of the index: the accounts the UI lists, which one is
/// live, and the operations that change either. ONE instance — the index it
/// mirrors is a single UserDefaults record, so a second copy would be a second
/// opinion about which mailbox the app is showing.
///
/// Every mutation writes the index FIRST and re-reads it into the mirror
/// second, so the published list is never the optimistic version of a write
/// that did not happen.
@MainActor
@Observable
final class AccountManager {
    static let shared = AccountManager()

    /// The accounts, in index order — which is switching order, so the UI's
    /// numbering comes straight from the array.
    private(set) var accounts: [AccountRecord] = []
    /// The live one. nil is the Connect gate: first run, or the last account
    /// removed.
    private(set) var activeId: UUID?

    var active: AccountRecord? { accounts.first { $0.id == activeId } }

    private init() { adopt(AccountIndex.load()) }

    /// Mirror an ALREADY-PERSISTED index. `AppStore.loadSettings` hands the
    /// migration's result here, because this singleton may well have been
    /// built (and read an empty index) before the repair ran.
    func adopt(_ state: AccountIndexState) {
        accounts = state.accounts
        activeId = state.activeId
    }

    /// Re-read the index from disk. For the paths that write it through
    /// `AccountIndex` directly — `AppStore.connect`, `revalidate`,
    /// `removeAccount`.
    func reload() { adopt(AccountIndex.load()) }

    /// What a credential read for one account actually found. `missing` means
    /// the slots are gone (the account is genuinely broken); `unreadable` means
    /// the keychain refused the read — denied access panel, locked keychain —
    /// and the slots may be perfectly intact behind the refusal.
    enum CredentialLoad {
        case ok(ConnectionSettings)
        case missing
        case unreadable
    }

    /// One account's credentials, with the failure shape preserved. Off the
    /// main actor (a keychain read can raise the access panel). Callers that
    /// would tear something down on `missing` must NOT do so on `unreadable`.
    func credentialLoad(for id: UUID) async -> CredentialLoad {
        switch await SettingsStore.loadAsync(accountId: id) {
        case .success(let stored?): return .ok(stored)
        case .success(nil): return .missing
        case .failure: return .unreadable
        }
    }

    /// One account's credentials for callers with nothing to tear down: the
    /// feed starters, which drop the id and retry on the next `.connected`
    /// transition either way.
    func settings(for id: UUID) async -> ConnectionSettings? {
        guard case .ok(let stored) = await credentialLoad(for: id) else { return nil }
        return stored
    }

    /// Record a NEW account: credentials into the keychain first, then the
    /// index entry that names them — an entry whose slots are empty would read
    /// as an account with nothing behind it. nil when the keychain refused.
    ///
    /// NOT activated. Making an account live is `switchTo`, a whole-world
    /// change, and not something that should fall out of saving a token.
    @discardableResult
    func add(label: String = "", settings: ConnectionSettings) async -> AccountRecord? {
        let record = AccountRecord(
            label: label, displayHost: AccountRecord.host(from: settings.serverURL))
        guard case .success = await SettingsStore.saveAsync(settings, accountId: record.id) else {
            // Roll a partial write back for the same reason the migration
            // does: a lone slot under an id nothing names is unreachable.
            _ = await offMain { Result { try SettingsStore.clear(accountId: record.id) } }
            return nil
        }
        AccountIndex.upsert(record, activate: false)
        reload()
        // An account added while the app is already up gets its ears here:
        // nothing else will start them, because the `.connected` transition
        // that brings the feeds up happened long before this account existed.
        // The watch too, and unconditionally — `add` never activates, so a new
        // account is by definition one of the inactive ones. (`addAccount`
        // switches to it a moment later, and that switch stops this watch.)
        startStream(record.id)
        startWatch(record.id)
        return record
    }

    /// Rename one account. Label only — an id is identity and the position in
    /// the list is the human's switching number, so neither may move.
    func rename(_ id: UUID, to label: String) {
        guard var record = accounts.first(where: { $0.id == id }), record.label != label else {
            return
        }
        record.label = label
        AccountIndex.upsert(record, activate: false)
        reload()
    }

    /// Record the host one account's credentials point at, so the UI can name
    /// it without a keychain read (see `AccountRecord.displayHost`).
    ///
    /// Called from the feed's start, which has JUST read those credentials for
    /// its own reasons: the index learns the host for free rather than paying
    /// a second access panel for it. A no-op unless something actually changed,
    /// which is the usual case — this writes on the first boot after an upgrade
    /// (a record from before the field existed) and after a daemon moves.
    func noteDisplayHost(_ id: UUID, serverURL: String) {
        let host = AccountRecord.host(from: serverURL)
        guard var record = accounts.first(where: { $0.id == id }), record.displayHost != host
        else { return }
        record.displayHost = host
        AccountIndex.upsert(record, activate: false)
        reload()
    }

    /// Forget one account, credentials first. A refused keychain delete parks
    /// the id for the boot sweep rather than being swallowed: the index entry
    /// is about to go, and it is the only thing that still names those slots.
    func remove(_ id: UUID) async {
        // Both ears FIRST, before the credentials they are holding are deleted
        // and before `AccountIndex.remove` drops this account's scoped keys: a
        // stream or a watch left running through that would keep dialling a
        // daemon this install has just forgotten, and would write the cursor
        // (or the seen-set) straight back under the id that was removed.
        stopStream(id)
        stopWatch(id)
        // The daemon is told to stop pushing while the credentials that
        // authorize saying so are still here: after the clear below there is
        // nothing left to present, and the registration would outlive the
        // account that asked for it.
        #if os(iOS)
            if let settings = await settings(for: id) {
                await PushRegistration.shared.unregister(settings)
            }
        #endif
        let cleared = await offMain { Result { try SettingsStore.clear(accountId: id) } }
        if case .failure = cleared { AccountIndex.parkOrphan(id) }
        AccountIndex.remove(id)
        // The LAST one leaves an empty index, which is the one shape a boot
        // can still read as a pre-multi-account install — see
        // `sealLegacyIfEmpty`. Off the main actor with the delete above it,
        // being the same kind of call.
        await offMain { AccountIndex.sealLegacyIfEmpty() }
        reload()
    }

    /// Commit a switch: persist the live id and mirror it. Called by
    /// `AppStore.switchAccount` at the point the new client is configured, so
    /// that everything deriving a key from `activeId` (the 2FA seen-set, the
    /// decisions ledger) flips in the same breath as the connection does.
    func markActive(_ id: UUID) {
        let previous = activeId
        AccountIndex.setActive(id)
        activeId = id
        // THE AUTH WATCHERS TRADE PLACES, here, at the one moment the live
        // account changes — and here rather than in the caller so that no
        // future switch path can forget to. The account arriving on screen
        // hands its auth mail to SitrepPoller → AuthArrival, which would
        // otherwise be a second writer of the same seen-set; the one leaving
        // picks up the background watch it had no use for while it was live.
        //
        // `startWatch` refuses an id the index no longer holds, which is the
        // case that matters here: removing the LIVE account switches to a
        // survivor, and the account being left behind is the one just deleted.
        stopWatch(id)
        if let previous, previous != id { startWatch(previous) }
    }

    /// Make another account the live one.
    ///
    /// Step (1) of the switch sequence — is this a real, different account —
    /// lives here; `AppStore.switchAccount` is the rest of it, because the
    /// state being torn down is that store's own and half of it is private.
    ///
    /// Note what this does NOT do: touch the feeds. Every account's stream
    /// keeps running across a switch, because which mailbox is on screen has
    /// nothing to do with which mailboxes are worth being told about.
    func switchTo(_ id: UUID) async {
        guard let record = accounts.first(where: { $0.id == id }), id != activeId else { return }
        await AppStore.shared.switchAccount(to: record)
    }

    /// The next account in index order, wrapping. The menu item behind it is
    /// deliberately chordless (see PassbandCommands): every account inside the
    /// first nine already has its own ⌘number, so this is a discoverability
    /// affordance rather than the primary way to move.
    func switchToNext() async {
        guard accounts.count > 1 else { return }
        // An unknown active id (nothing live yet) lands on the first account,
        // which is the only sensible "next" from nowhere.
        let current = accounts.firstIndex { $0.id == activeId } ?? -1
        let next = accounts[(current + 1) % accounts.count]
        await switchTo(next.id)
    }

    // MARK: - the notification feeds

    /// One SSE connection per account, keyed by id — INCLUDING the accounts
    /// that are not live. That is the entire point of holding them: the human
    /// wants to hear about mail in every mailbox they have added, not only the
    /// one currently on screen. Owned here rather than by `AppStore` because
    /// the store is only ever the ACTIVE account's world.
    private var streams: [UUID: EventStream] = [:]

    /// The other ear, and the exact COMPLEMENT of the streams above: one
    /// background auth watcher per account that is NOT live. A sealed event now
    /// DOES ride the feed (docs/NOTIFY.md §11.6) but it carries no subject and
    /// no received-at worth trusting, so it is a doorbell and not the answer —
    /// the only way to learn what landed is still to ask, which is what these
    /// do. The live account has no entry: its auth mail comes through
    /// SitrepPoller → AuthArrival, which runs the whole richer flow (ring,
    /// audited auto-reveal, code modal).
    ///
    /// So these are the fallback AND the fast path's second half. They keep
    /// their 30s cadence for the mail no event ever mentions (a daemon with the
    /// notify lane off, a frame lost to a dropped connection), and
    /// `noteSealedEvent` below rings them the moment one does.
    private var watches: [UUID: BackgroundAuthWatch] = [:]

    /// The accounts whose feeds SHOULD be up. It differs from `streams` only
    /// across the keychain read that starting one has to await — and that gap
    /// is exactly why it exists. A `remove` landing inside it takes the id out
    /// of here, so the read resumes, finds itself unwanted, and drops the
    /// stream instead of dialling a daemon this install has just forgotten.
    private var wantedStreams: Set<UUID> = []

    /// The same wanted-set guard for the watchers, protecting the same gap —
    /// separate because the two lifecycles genuinely differ: a switch leaves
    /// every stream alone and moves exactly two watches.
    private var wantedWatches: Set<UUID> = []

    /// Whether the background ears should be running AT ALL — raised by the
    /// `.connected` transition and dropped by its opposite. Without it, an
    /// account recorded while the app is sitting on the Connect gate would
    /// quietly open a connection from a screen that says the app has no
    /// identity.
    private var feedsUp = false

    /// Bring up everything that listens on an account's behalf: one event feed
    /// per account, and one auth watch per account that is not the live one.
    /// Called on the `.connected` transition, which is the moment this install
    /// is known to have an identity. Idempotent: an account already listening
    /// keeps what it has rather than being cut and redialled.
    func startAllFeeds() {
        feedsUp = true
        for account in accounts {
            startStream(account.id)
            startWatch(account.id)
        }
    }

    /// Drop every feed and every watch. The `.disconnected` transition — an app
    /// with no identity on screen holds no connections either.
    func stopAllFeeds() {
        feedsUp = false
        wantedStreams.removeAll()
        wantedWatches.removeAll()
        // Everything is stopped BEFORE its dictionary is emptied: a
        // `ResidentTask` is retained by the runtime while its loop runs, so a
        // stream merely dropped on the floor would keep reconnecting forever
        // with nothing left in the process holding a reference to stop it.
        for stream in streams.values { stream.stop() }
        streams.removeAll()
        for watch in watches.values { watch.stop() }
        watches.removeAll()
    }

    /// Start ONE account's feed. Asynchronous underneath because the
    /// credentials come from the keychain, but fire-and-forget from the
    /// caller's side: nothing in the app waits on a notification feed.
    func startStream(_ id: UUID) {
        // `insert` reporting no insertion means the id is already wanted —
        // streaming, or mid-keychain-read. Either way a second connection to
        // the same daemon is nobody's intent.
        guard feedsUp, wantedStreams.insert(id).inserted else { return }
        Task {
            let stored = await settings(for: id)
            // The world can have moved during that read: a stop, a removal, a
            // whole disconnect. `wantedStreams` answers all three at once. A
            // stream already under this id means a `restartFeeds` overtook us
            // with credentials it did not need the keychain for; that one is
            // the fresher of the two, so leave it alone.
            guard wantedStreams.contains(id), streams[id] == nil else { return }
            guard let stored else {
                // No credentials behind the record: slots gone, or the
                // keychain refused. A feed with nothing to connect to is a
                // backoff loop that never ends, so none is started and the id
                // goes back to unwanted — the next `.connected` transition (or
                // the next launch) is the retry.
                wantedStreams.remove(id)
                return
            }
            // The FIRST keychain read every account pays at boot, so the index
            // takes its host from here rather than raising a panel of its own.
            // (An inactive account's auth watch pays a second one; see
            // `startWatch`.)
            noteDisplayHost(id, serverURL: stored.serverURL)
            let stream = EventStream(accountId: id, settings: stored)
            streams[id] = stream
            stream.start()
        }
    }

    /// Stop and forget ONE account's feed, including a start still waiting on
    /// the keychain.
    func stopStream(_ id: UUID) {
        wantedStreams.remove(id)
        streams.removeValue(forKey: id)?.stop()
    }

    /// Rebuild one account's ears against credentials that have just changed —
    /// a re-validated token, a moved server URL.
    ///
    /// A stream's settings are fixed for its lifetime BY DESIGN (see
    /// EventStream), and a watch's for the same reason, so new credentials mean
    /// a new connection: mutating one in place is how a feed ends up holding
    /// one account's URL with another's token. The settings are handed in
    /// rather than re-read, because the caller has just written them and a
    /// second keychain round trip is a second chance to raise the access panel.
    func restartFeeds(_ id: UUID, with settings: ConnectionSettings) {
        // Unconditional, even with the feeds down: whatever is connected right
        // now is presenting credentials that are no longer the truth.
        stopStream(id)
        stopWatch(id)
        // Before the `feedsUp` bail: a moved daemon has to rename its row in
        // the switcher whether or not the feeds happen to be running.
        noteDisplayHost(id, serverURL: settings.serverURL)
        guard feedsUp else { return }
        wantedStreams.insert(id)
        let stream = EventStream(accountId: id, settings: settings)
        streams[id] = stream
        stream.start()
        // Both callers today are the LIVE account re-validating itself, where
        // this is a no-op — `startWatch` refuses the account on screen. It is
        // here so that the day something re-validates an inactive account, its
        // watch comes back too instead of staying silently dead. That path pays
        // one keychain read for credentials already in hand, which is the
        // cheaper mistake than the alternative shape: a watch started from
        // settings handed in by a caller who might not be talking about it.
        startWatch(id)
    }

    // MARK: - the background auth watches

    /// Start ONE account's auth watch. Refuses the LIVE account outright: that
    /// mailbox's auth mail belongs to `AuthArrival`, and a watcher on top of it
    /// would be a second writer of one seen-set and a second banner for one
    /// code. Asynchronous underneath for its own keychain read, and
    /// fire-and-forget for the same reason `startStream` is.
    ///
    /// That read is a SECOND access-panel risk per inactive account, on top of
    /// the one the feed already pays. The alternative — caching every account's
    /// credentials in this class so both ears could share one read — puts N
    /// live API tokens in a long-lived main-actor dictionary to save a prompt,
    /// which is not a trade worth making.
    func startWatch(_ id: UUID) {
        guard feedsUp, id != activeId, accounts.contains(where: { $0.id == id }) else { return }
        // `insert` reporting no insertion means the id is already wanted —
        // watching, or mid-keychain-read.
        guard wantedWatches.insert(id).inserted else { return }
        Task {
            let stored = await settings(for: id)
            // The world can have moved during that read: a stop, a removal, a
            // whole disconnect — `wantedWatches` answers all three. A watch
            // already under this id means something overtook us; that one is
            // the fresher, so leave it alone.
            guard wantedWatches.contains(id), watches[id] == nil else { return }
            // Or this account went LIVE while we were reading, and `AuthArrival`
            // has it now. Unwanted rather than merely abandoned: an id left
            // wanted with no watch behind it is an id the `insert` guard above
            // silently refuses forever, and switching away again would then
            // never start one.
            guard id != activeId else {
                wantedWatches.remove(id)
                return
            }
            guard let stored else {
                // No credentials behind the record: slots gone, or the keychain
                // refused. A watcher with nothing to dial is a backoff loop
                // that never ends, so none is started and the id goes back to
                // unwanted — the next switch (or the next launch) is the retry.
                wantedWatches.remove(id)
                return
            }
            let watch = BackgroundAuthWatch(accountId: id, settings: stored)
            watches[id] = watch
            watch.start()
        }
    }

    /// Stop and forget ONE account's auth watch, including a start still
    /// waiting on the keychain.
    func stopWatch(_ id: UUID) {
        wantedWatches.remove(id)
        watches.removeValue(forKey: id)?.stop()
    }

    // MARK: - the sealed doorbell

    /// A sealed event just arrived on ONE account's feed: go and ask that
    /// account's daemon what landed, now, instead of at the next tick.
    ///
    /// THE ROUTING LIVES HERE because this is the only object that knows which
    /// of the two ears is listening to a given mailbox — `EventStream` is
    /// deliberately ignorant of which account is on screen, and asking it to
    /// compare would give every stream in the process an opinion about that.
    /// The two branches are the same complement as `streams` vs `watches`.
    ///
    /// FIRE AND FORGET, and idempotent by the layers underneath: the fetch it
    /// pokes joins one already in flight rather than stacking a second, and
    /// `AuthSeenSet` decides exactly once whether what comes back is worth a
    /// noise. So an event that beats the poll, an event that loses to it, and
    /// an event replayed across a reconnect all cost at most one extra request
    /// and produce at most one banner.
    func noteSealedEvent(for id: UUID) {
        // Not an account this install still has: nothing to ask, nowhere to
        // ask it. A stream for a removed account is already being torn down.
        guard accounts.contains(where: { $0.id == id }) else { return }
        if id == activeId {
            // The live account's sealed list rides the sitrep read model, which
            // is what `AuthArrival` watches. Pulling just that one leg is the
            // whole poll's work minus four requests nothing here is waiting on.
            Task { await SitrepPoller.shared.refreshSealed() }
        } else {
            watches[id]?.pollNow()
        }
    }
}
