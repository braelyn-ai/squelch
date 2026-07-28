// THE app store. One observable object, several logical slices:
//   settings   — connection state + Connect flow
//   sitrep     — the three bands, stats, sealed metadata (the read model)
//   selection  — cursor position, stable by message id across refresh
//   undo       — pending-undo queue for undo-first actions
//   routing    — the routed main view + a browser-style history stack
//   surfaces   — side panel / thread viewer / overlays
//
// The store holds DATA and coordination only. Network calls live in APIClient
// and are invoked by the poller / views, which write results back here.
//
// Ported from squelch-desktop/src/state/store.ts.

import Foundation
import Observation
import SwiftUI

// MARK: - routed views

/// Which primary surface the rail is showing. `sitrep` is the abstracted
/// dashboard (the default on launch); `emails` is the classic band list.
enum MainView: String, Sendable, Hashable, CaseIterable {
    case sitrep, emails, auth, rules, audit, usage, settings

    /// The TOP rail group — also the 1..5 number-key mapping. Usage/Settings
    /// are DELIBERATELY excluded: they live in a bottom group reached by click,
    /// so adding them never renumbers 1..5.
    static let mainViews: [MainView] = [.sitrep, .emails, .auth, .rules, .audit]
    /// The BOTTOM rail group, pinned below a divider.
    static let bottomViews: [MainView] = [.usage, .settings]

    var label: String {
        switch self {
        case .sitrep: "Sitrep"
        case .emails: "Emails"
        case .auth: "Auth"
        case .rules: "Rules"
        case .audit: "Audit"
        case .usage: "Usage"
        case .settings: "Settings"
        }
    }

    var symbol: String {
        switch self {
        case .sitrep: "gauge.with.dots.needle.33percent"
        case .emails: "envelope"
        case .auth: "key"
        case .rules: "slider.horizontal.3"
        case .audit: "scroll"
        case .usage: "waveform.path.ecg"
        case .settings: "gearshape"
        }
    }
}

/// One entry in the view-history stack. Captures the routed view plus, for an
/// Emails drill-target hand-off, the selected id to re-focus.
struct HistoryEntry: Equatable, Sendable {
    var view: MainView
    var selectedId: Int?
}

/// Cap the history so a long session can't grow it without bound.
let historyCap = 50

// MARK: - side views

/// Side panels remaining after Auth/Rules/Audit were promoted to routed views.
/// The thread drill-in is NOT a side view — it's the fullscreen viewer, layered
/// ABOVE these panels so opening a thread from search keeps the panel mounted
/// underneath and Esc returns to it.
enum SideView: Equatable, Sendable {
    case none
    case browse
    case search(query: String)

    var isOpen: Bool { self != .none }

    var title: String {
        switch self {
        case .browse: "browse — all mail"
        case .search: "search"
        case .none: ""
        }
    }
}

// MARK: - connection

enum ConnStatus: Sendable, Equatable {
    case loading      // reading keychain on boot
    case disconnected // no settings -> Connect screen
    case connecting   // testing a candidate URL+token
    case connected
    case error
}

/// Why a refresh failed, kept alongside the message so the UI can distinguish
/// "daemon unreachable" (fix: start squelchd) from "token rejected" (fix: open
/// Settings) rather than lumping every failure into one vague state.
struct RefreshError: Equatable, Sendable {
    var message: String
    var kind: APIErrorKind

    /// True when the failure is the token, not the transport.
    var isAuthFailure: Bool { kind == .unauthorized || kind == .forbidden }
}

// MARK: - undo / toasts

enum UndoKind: Sendable { case archive, done, label, ruleDelete }

/// A queued undo. `revert` is the exact inverse call to fire on `u`/toast-click.
/// Undo-first design: the forward action already fired; this lets the human
/// take it back.
struct PendingUndo: Identifiable, Sendable {
    let id = UUID()
    var kind: UndoKind
    /// The message id for mail actions; the (now-deleted) rule id for ruleDelete.
    var messageId: Int
    var label: String
    var createdAt: Date = Date()
    var revert: @Sendable () async throws -> Void
}

struct Toast: Identifiable, Sendable {
    enum Tone: Sendable { case info, error, success }
    let id = UUID()
    var text: String
    var tone: Tone
}

// MARK: - 2FA present-don't-read

/// A live countdown ring on the auth rail icon. One per freshly-arrived auth
/// message; the ring sweeps over RING_SECONDS then removes itself. The ring +
/// the seen-set ARE the read state (Gmail read-marking is impossible and
/// unwanted under gmail.readonly).
struct AuthRing: Identifiable, Sendable, Equatable {
    var id: Int
    var startedAt: Date
}

/// A queued code-modal entry. `code` is nil when extraction failed (the modal
/// shows an "Open Auth" affordance instead). Held in memory only, never
/// persisted.
struct AuthCodeEntry: Identifiable, Sendable, Equatable {
    var meta: SealedMeta
    var code: String?
    var id: Int { meta.id }
}

/// How long an auth ring sweeps before it disappears.
let ringSeconds: TimeInterval = 60

// MARK: - compose

/// Draft + review state for the send ceremony.
struct ComposeState: Sendable, Equatable {
    enum Phase: Sendable { case edit, review }
    var replyToMessageId: Int?
    var to: String = ""
    var subject: String = ""
    var body: String = ""
    /// "edit" = composing; "review" = guard verdict shown, second Enter fires.
    var phase: Phase = .edit
    /// Redacted guard kinds from a 422; empty means the guard passed (or hasn't
    /// been asked yet).
    var guardKinds: [String] = []
    var sending = false
    var error: String?
}

/// The email currently being reclassified by the `v` palette.
struct TriageFixTarget: Sendable, Equatable {
    var messageId: Int
    /// Shown so you can see what you are reclassifying.
    var sender: String
    var subject: String
    /// Current values, for the "was" labels. nil = unknown, and a dimension the
    /// caller does not know is OMITTED rather than shown as "unset" — claiming a
    /// value is unset when we simply never fetched it would be a small lie in
    /// the one place accuracy matters.
    var tier: String??
    var category: String??
}

/// The rule editor's open request. Three shapes: tune (`sender`), create from
/// scratch (`rule == .some(nil)`), or edit an existing rule.
struct RuleEditorRequest: Identifiable, Sendable {
    let id = UUID()
    var sender: String?
    var rule: SenderRule?
    /// Preselect a disposition (the newsletters CTA preselects "filtered").
    var disposition: Disposition?
    var want: String?
    /// Explicit match_pattern override; wins over deriving from `sender`.
    var pattern: String?
    /// Called after a successful save so the opener re-fetches its list.
    var onSaved: (@MainActor @Sendable () -> Void)?
}

// MARK: - the store

/// The read model the SitrepView renders: updates bucketed by band.
struct SitrepData: Sendable, Equatable {
    var standing: [AttentionUpdate] = []
    var new: [AttentionUpdate] = []
    var open: [AttentionUpdate] = []
    var stats: StoreStats?
    var sealed: [SealedMeta] = []
}

@MainActor
@Observable
final class AppStore {
    static let shared = AppStore()

    // MARK: settings slice
    var connStatus: ConnStatus = .loading
    var settings: ConnectionSettings?
    var connError: String?

    // MARK: sitrep slice
    var sitrep = SitrepData()
    var lastRefresh: Date?
    var refreshError: RefreshError?

    /// Daemon health. "Down" = refreshes failing AND nothing has ever loaded
    /// this session — there is no data worth rendering, so the routed view is
    /// replaced by the down pane (Settings excepted: it must stay reachable to
    /// fix the token/URL). Once a sync HAS landed, failures degrade to a
    /// stale-data banner and the (stale) view stays up.
    var daemonDown: Bool { refreshError != nil && lastRefresh == nil }

    // MARK: routing slice
    var activeView: MainView = .sitrep
    var history: [HistoryEntry] = [HistoryEntry(view: .sitrep, selectedId: nil)]
    var historyIndex = 0

    // MARK: selection slice — stable by message id
    var selectedId: Int?

    // MARK: surfaces
    var sideView: SideView = .none
    /// The fullscreen email viewer. Orthogonal to sideView (it layers above),
    /// so closing the viewer restores whatever was underneath.
    var threadId: String?
    /// The ordered list the viewer was opened FROM, so "done + next" (e/d) can
    /// advance in place. Empty when opened from a surface without a queue.
    var threadQueue: [AttentionUpdate] = []
    var compose: ComposeState?
    var triageFix: TriageFixTarget?
    var ruleEditor: RuleEditorRequest?
    var processModeOpen = false
    var askBarOpen = false
    var shortcutsOpen = false

    // MARK: undo / toasts
    var undos: [PendingUndo] = []
    var toasts: [Toast] = []

    // MARK: 2FA
    var authRings: [AuthRing] = []
    /// Newest-first queue of code-modal entries (only otp/login_code/verification).
    var authQueue: [AuthCodeEntry] = []

    private init() {}

    // MARK: - settings

    func loadSettings() async {
        // Off the main actor: a keychain read can put up the system's "allow
        // access?" panel and block until answered.
        switch await SettingsStore.loadAsync() {
        case .success(let stored):
            if let stored {
                await APIClient.shared.configure(
                    baseURL: stored.serverURL, token: stored.apiToken)
                settings = stored
                connStatus = .connected
                connError = nil
            } else {
                connStatus = .disconnected
            }
        case .failure:
            connStatus = .disconnected
            connError = "settings load failed"
        }
    }

    /// Test a candidate URL+token via /client/stats; on success persist + connect.
    @discardableResult
    func connect(serverURL: String, apiToken: String) async -> Bool {
        connStatus = .connecting
        connError = nil
        // Probe with a throwaway config so a bad token never gets persisted.
        await APIClient.shared.configure(baseURL: serverURL, token: apiToken)
        do {
            _ = try await APIClient.shared.getStats()  // 401 => bad token; network => bad url
            try await SettingsStore.saveAsync(
                ConnectionSettings(serverURL: serverURL, apiToken: apiToken)).get()
            settings = ConnectionSettings(serverURL: serverURL, apiToken: apiToken)
            connStatus = .connected
            connError = nil
            // Fresh connection = fresh sync history; a stale lastRefresh from a
            // prior session must not make a failing daemon look recently synced.
            lastRefresh = nil
            refreshError = nil
            return true
        } catch {
            connStatus = .error
            connError = Self.connectErrorText(error)
            return false
        }
    }

    /// Settings-screen re-validate: test a candidate and, on success, persist +
    /// swap the live client — WITHOUT dropping connStatus out of "connected" on
    /// failure (so the Settings view stays mounted rather than bouncing to the
    /// Connect gate).
    func revalidate(serverURL: String, apiToken: String) async -> (ok: Bool, error: String?) {
        let prev = settings
        await APIClient.shared.configure(baseURL: serverURL, token: apiToken)
        do {
            _ = try await APIClient.shared.getStats()
            try await SettingsStore.saveAsync(
                ConnectionSettings(serverURL: serverURL, apiToken: apiToken)).get()
            settings = ConnectionSettings(serverURL: serverURL, apiToken: apiToken)
            return (true, nil)
        } catch {
            // Restore the prior working client — never leave the app pointed at
            // a bad config because the human fat-fingered the token.
            if let prev {
                await APIClient.shared.configure(baseURL: prev.serverURL, token: prev.apiToken)
            }
            return (false, Self.connectErrorText(error))
        }
    }

    private static func connectErrorText(_ error: Error) -> String {
        guard let api = error as? APIError else { return "connection failed" }
        switch api.kind {
        case .unauthorized: return "token rejected (401)"
        case .network: return "cannot reach that server URL"
        default: return api.message
        }
    }

    func disconnect() {
        // Wipe persisted settings so the next boot lands on the Connect gate.
        try? SettingsStore.clear()
        connStatus = .disconnected
        settings = nil
        sitrep = SitrepData()
        lastRefresh = nil
        refreshError = nil
        selectedId = nil
        activeView = .sitrep
        threadId = nil
        threadQueue = []
        sideView = .none
        history = [HistoryEntry(view: .sitrep, selectedId: nil)]
        historyIndex = 0
    }

    // MARK: - routing + history

    /// Browser-style history push: truncate any forward entries past the
    /// cursor, append, cap the length (dropping oldest), point at the new tail.
    private func pushHistory(_ entry: HistoryEntry) {
        var trimmed = Array(history.prefix(historyIndex + 1))
        trimmed.append(entry)
        if trimmed.count > historyCap { trimmed.removeFirst(trimmed.count - historyCap) }
        history = trimmed
        historyIndex = trimmed.count - 1
    }

    func setView(_ view: MainView) {
        // Navigating ANYWHERE dismisses an open thread viewer — the rail is
        // visible beside it, so a rail click means "leave this email and go
        // there", not "change the page underneath the overlay".
        threadId = nil
        threadQueue = []
        // No-op if we're already on this exact view+selection — a repeat press
        // shouldn't spam identical history entries.
        let cur = history.indices.contains(historyIndex) ? history[historyIndex] : nil
        if let cur, cur.view == view, cur.selectedId == selectedId {
            activeView = view
            return
        }
        activeView = view
        pushHistory(HistoryEntry(view: view, selectedId: selectedId))
    }

    /// Switch to the Emails view with a specific update selected. Used by the
    /// Sitrep dashboard's "view" affordances to hand off to the band list with
    /// the right row focused.
    func viewInEmails(_ id: Int) {
        activeView = .emails
        selectedId = id
        threadId = nil
        threadQueue = []
        pushHistory(HistoryEntry(view: .emails, selectedId: id))
    }

    var canGoBack: Bool { historyIndex > 0 }
    var canGoForward: Bool { historyIndex < history.count - 1 }

    func goBack() {
        guard historyIndex > 0 else { return }
        historyIndex -= 1
        let entry = history[historyIndex]
        activeView = entry.view
        selectedId = entry.selectedId
    }

    func goForward() {
        guard historyIndex < history.count - 1 else { return }
        historyIndex += 1
        let entry = history[historyIndex]
        activeView = entry.view
        selectedId = entry.selectedId
    }

    // MARK: - selection

    /// Flat, band-ordered id list the keymap uses for j/k movement.
    var orderedIds: [Int] {
        (sitrep.standing + sitrep.new + sitrep.open).map(\.id)
    }

    var selectedUpdate: AttentionUpdate? {
        guard let selectedId else { return nil }
        return (sitrep.standing + sitrep.new + sitrep.open).first { $0.id == selectedId }
    }

    func moveSelection(_ delta: Int) {
        let ids = orderedIds
        guard !ids.isEmpty else { return }
        let idx = selectedId.flatMap { ids.firstIndex(of: $0) } ?? -1
        selectedId = ids[min(max(idx + delta, 0), ids.count - 1)]
    }

    // MARK: - surfaces

    func openThread(_ threadId: String, queue: [AttentionUpdate] = []) {
        self.threadId = threadId
        self.threadQueue = queue
    }

    func closeThread() {
        threadId = nil
        threadQueue = []
    }

    func openSide(_ view: SideView) { sideView = view }
    func closeSide() { sideView = .none }

    func openCompose(_ state: ComposeState) { compose = state }
    func closeCompose() { compose = nil }

    func openTriageFix(_ target: TriageFixTarget) { triageFix = target }
    func closeTriageFix() { triageFix = nil }

    func openRuleEditor(_ request: RuleEditorRequest) { ruleEditor = request }
    func closeRuleEditor() { ruleEditor = nil }

    // MARK: - undo

    private static let undoTTL: TimeInterval = 5

    func pushUndo(
        kind: UndoKind, messageId: Int, label: String,
        revert: @escaping @Sendable () async throws -> Void
    ) {
        let entry = PendingUndo(kind: kind, messageId: messageId, label: label, revert: revert)
        undos.append(entry)
        // Auto-expire from the queue after the window.
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.undoTTL))
            self?.undos.removeAll { $0.id == entry.id }
        }
    }

    /// Undo the given (or most recent) queued action.
    func fireUndo(_ id: UUID? = nil) async {
        let entry: PendingUndo?
        if let id {
            entry = undos.first { $0.id == id }
        } else {
            entry = undos.last
        }
        guard let entry else { return }
        undos.removeAll { $0.id == entry.id }
        do {
            try await entry.revert()
            pushToast("undone: \(entry.label)", .info)
        } catch {
            pushToast("undo failed: \(entry.label)", .error)
        }
    }

    // MARK: - toasts

    @discardableResult
    func pushToast(_ text: String, _ tone: Toast.Tone = .info) -> UUID {
        let toast = Toast(text: text, tone: tone)
        toasts.append(toast)
        // Notices are ephemeral (undos own their own 5s window and a click
        // target); auto-dismiss so the stack cannot accumulate forever.
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(6))
            self?.toasts.removeAll { $0.id == toast.id }
        }
        return toast.id
    }

    func dismissToast(_ id: UUID) { toasts.removeAll { $0.id == id } }

    // MARK: - 2FA

    /// Start a 60s countdown ring for a freshly-arrived auth message.
    /// Deduped: one ring per id; re-arming restarts the sweep.
    func pushAuthRing(_ id: Int) {
        authRings.removeAll { $0.id == id }
        authRings.append(AuthRing(id: id, startedAt: Date()))
    }

    func expireAuthRing(_ id: Int) { authRings.removeAll { $0.id == id } }

    /// Enqueue a code-modal entry, newest-first, deduped by id.
    func pushAuthCode(_ entry: AuthCodeEntry) {
        guard !authQueue.contains(where: { $0.meta.id == entry.meta.id }) else { return }
        authQueue.insert(entry, at: 0)
    }

    /// Pop the front (currently-shown) code-modal entry on dismiss.
    func dismissAuthCode() {
        if !authQueue.isEmpty { authQueue.removeFirst() }
    }

    // MARK: - band mutation (optimistic)

    /// Optimistically pull a message id out of whatever band holds it and keep
    /// the selection valid (advance to the next row, else the previous).
    /// Returns a `restore` thunk that re-inserts the removed rows on failure.
    func removeFromBands(_ messageId: Int) -> () -> Void {
        let prev = sitrep
        // Compute the next selection BEFORE mutating, using the flat order.
        let orderBefore = orderedIds
        let posBefore = orderBefore.firstIndex(of: messageId) ?? 0

        sitrep.standing.removeAll { $0.id == messageId }
        sitrep.new.removeAll { $0.id == messageId }
        sitrep.open.removeAll { $0.id == messageId }

        if selectedId == messageId {
            let after = orderedIds
            if after.isEmpty {
                selectedId = nil
            } else {
                selectedId = after[min(max(posBefore, 0), after.count - 1)]
            }
        }

        return { [weak self] in
            guard let self else { return }
            self.sitrep.standing = prev.standing
            self.sitrep.new = prev.new
            self.sitrep.open = prev.open
            self.selectedId = messageId
        }
    }
}
