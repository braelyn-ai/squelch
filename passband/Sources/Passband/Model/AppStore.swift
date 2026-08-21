// THE app store: one observable object holding connection state, the sitrep read
// model, selection (stable by message id across refresh), the undo queue, the
// routed view + history stack, and the overlay surfaces.
//
// DATA and coordination only — network calls live in APIClient, and the poller
// and views write their results back here.

import Foundation
import Observation
import SwiftUI

// MARK: - routed views

/// Which primary surface the rail is showing. `sitrep` is the abstracted
/// dashboard (the default on launch); `emails` is the classic band list.
enum MainView: String, Sendable, Hashable, CaseIterable {
    case sitrep, emails, auth, rules, audit, usage, settings

    /// The TOP rail group — also the 1..5 number-key mapping. Usage/Settings are
    /// excluded so that adding them never renumbers 1..5.
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

// MARK: - mail pages

/// Which page the emails tab shows. `inbox` is the flat all-tiers list; `noise`
/// is the spam-folder equivalent — the same rows and the same verbs, narrowed to
/// the noise tier BY THE DAEMON so nothing has to be discarded client-side.
/// `sent` is the odd one out: outbound mail, off its own route, with none of the
/// triage verbs (nothing triaged it, and nothing can resolve it).
enum MailMode: String, Sendable, Hashable, CaseIterable {
    case inbox, noise, sent

    /// The page's name — the header title and the segmented control both.
    var label: String {
        switch self {
        case .inbox: "all mail"
        case .noise: "noise"
        case .sent: "sent"
        }
    }

    /// Server-side tier filter for the page; nil = every tier. Nil for `sent`
    /// too, but vacuously: that page never goes to /client/updates at all, so
    /// there is no tier to narrow.
    var tier: Tier? {
        switch self {
        case .inbox, .sent: nil
        case .noise: .noise
        }
    }

    /// What `n` flips to, so the key is one binding rather than two. It stays
    /// the inbox/noise flip from every page: from `sent`, `n` dips into noise
    /// exactly as it would from the inbox.
    var flipped: MailMode { self == .noise ? .inbox : .noise }
}

// MARK: - side views

/// The side panels. The thread drill-in is NOT one — it is the fullscreen viewer,
/// layered ABOVE these, so opening a thread from search keeps the panel mounted
/// beside it. Esc from the reader sheds the panel first (the email stays up);
/// the next Esc closes the reader.
enum SideView: Equatable, Sendable {
    case none
    case browse
    case search

    var isOpen: Bool { self != .none }

    var title: String {
        switch self {
        case .browse: "browse — all mail"
        case .search: "search"
        case .none: ""
        }
    }
}

/// How wide the right-hand panel is. ONE definition: the thread viewer insets
/// itself by exactly this much so an open panel stays visible beside the reader,
/// and two numbers drifting apart would leave a seam or a covered strip.
let sidePanelWidth: CGFloat = 460

/// Everything the sitrep's zones render, held here rather than in the zones: a
/// view's `@State` dies on unmount, so the last good answer is already on screen
/// when the dashboard re-mounts and the refresh happens UNDERNEATH it — rows are
/// only ever replaced by newer rows, never cleared first. `loadedAt` keeps that
/// honest in the other direction (see `refreshZones`).
struct SitrepZoneCache: Sendable {
    var calendar: [CalendarUpdate] = []
    var shipments: [Shipment] = []
    var banking: [BankingRecord] = []
    var receipts: [Receipt] = []
    var newsletters: [Newsletter] = []
    var rulesCount: Int?
    /// When the last full refresh COMPLETED. nil = never loaded.
    var loadedAt: Date?
}

/// The search panel's state, held OUT of the panel: SwiftUI discards a view's
/// `@State` on unmount, and parking it here is what makes `/` resumable — same
/// query, same hits, same selection, no refetch and no empty flash.
struct SearchSession: Sendable, Equatable {
    var query = ""
    var hits: [SearchHit] = []
    /// The armed row. -1 = nothing armed, focus semantically in the bar: Enter
    /// expands the panel instead of opening a hit. ArrowDown arms row 0.
    var index = -1
    /// Fullscreen results with larger previews (Enter in the bar). Collapses
    /// when a hit opens so the results stay in the strip beside the reader.
    var expanded = false
    var error: String?
    /// The term `hits` actually came from, so reopening on an unchanged query
    /// skips the round-trip. nil = what is on screen is not authoritative (never
    /// fetched, or the last fetch failed), so reopening retries.
    var fetchedQuery: String?
    /// Cursor for the page AFTER the ones in `hits`. nil = the server has no
    /// more (or nothing authoritative is on screen). Parked here with the rest
    /// so reopening resumes mid-scroll instead of dropping back to page one.
    var nextCursor: String?
}

// MARK: - connection

enum ConnStatus: Sendable, Equatable {
    case loading      // reading keychain on boot
    case disconnected // no settings -> Connect screen
    case connecting   // testing a candidate URL+token
    case connected
    case error
}

/// Why a refresh failed, so the UI can distinguish "daemon unreachable" (fix:
/// start squelchd) from "token rejected" (fix: open Settings).
struct RefreshError: Equatable, Sendable {
    var message: String
    var kind: APIErrorKind

    /// True when the failure is the token, not the transport.
    var isAuthFailure: Bool { kind == .unauthorized || kind == .forbidden }
}

// MARK: - undo / toasts

enum UndoKind: Sendable { case archive, done, label, ruleDelete }

/// A queued undo. `revert` is the exact inverse call to fire on `u`/toast-click;
/// the forward action has already gone out.
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

/// A live countdown ring on the auth rail icon, one per freshly-arrived auth
/// message, sweeping over `ringSeconds` then removing itself. The ring plus the
/// seen-set ARE the read state — read-marking is impossible under
/// gmail.readonly.
struct AuthRing: Identifiable, Sendable, Equatable {
    var id: Int
    var startedAt: Date
}

/// A queued code-modal entry. `code` is nil when extraction failed (the modal
/// shows an "Open Auth" affordance instead). In memory only, never persisted.
struct AuthCodeEntry: Identifiable, Sendable, Equatable {
    var meta: SealedMeta
    var code: String?
    var id: Int { meta.id }
}

/// How long an auth ring sweeps before it disappears.
let ringSeconds: TimeInterval = 60

// MARK: - compose

/// Draft + review state for the send ceremony. ONE type for both composers: the
/// `ComposePane` (new message, the right-hand pane) and the reader's inline
/// reply.
struct ComposeState: Sendable, Equatable {
    enum Phase: Sendable { case edit, review }

    /// WHICH COMPOSER THIS IS. Minted once when the state is built and carried
    /// through every copy of it, because this is a value type living in a slot
    /// (`AppStore.compose` / `AppStore.inlineReply`) that anything can replace:
    /// the only way an async continuation can tell "still mine" from "somebody
    /// else's draft is in the slot now" is an identity that copies along.
    ///
    /// The race it exists for: a send is awaited, the sender presses Escape
    /// (which flushes the draft and clears the slot) and opens a different
    /// composer, then the response lands and the continuation closes and
    /// un-marks THAT one — destroying whatever was typed into it. A 60s forward
    /// holds that window open for a minute. Every send continuation therefore
    /// checks this id against the slot before touching it.
    ///
    /// Being `let` with a default keeps it out of the memberwise init, so no
    /// caller can mint two states with one identity — and note this makes
    /// Equatable DISTINGUISH two composers holding identical text, which is
    /// exactly what the slot logic wants: same words, different draft.
    let id = UUID()

    var replyToMessageId: Int?
    /// The message this composer PASSES ON. Mutually exclusive with
    /// `replyToMessageId` — a send names a parent to answer or an original to
    /// forward, never both — and the daemon rejects a body carrying the pair.
    ///
    /// Nothing of the original travels with it: the daemon reads the message
    /// out of its own store, quotes it, re-attaches its files and starts a NEW
    /// thread. So this id is the whole of the forward on the wire, and the
    /// composer's body is only what the sender adds on top of it (which may be
    /// nothing at all).
    var forwardOfMessageId: Int?
    var to: String = ""
    var subject: String = ""
    var body: String = ""
    /// The server-side draft this composer is autosaving into, once one exists.
    /// Rides along to `send` so a successful send deletes the draft in the same
    /// transaction — otherwise the next `c` would restore mail already gone.
    var draftId: Int?
    /// "edit" = composing; "review" = guard verdict shown, second Enter fires.
    var phase: Phase = .edit
    /// Redacted guard kinds from a 422; empty means the guard passed (or hasn't
    /// been asked yet).
    var guardKinds: [String] = []
    var sending = false
    var error: String?
    /// Ask the daemon to mint a read-tracking pixel for this one send. Seeded
    /// from the account's stored default when the composer opens, then owned by
    /// the composer — the daemon applies no default of its own.
    var includeTracker = false
    /// Answer everyone on the parent rather than only its sender. Meaningless
    /// without `replyToMessageId`, and fixed when the composer opens: which key
    /// opened it is the whole choice, so there is no switch to flip afterwards.
    var replyAll = false
    /// What the forwarded original is called. DISPLAY ONLY: it NEVER reaches the
    /// wire — the daemon builds the quoted original from `forwardOfMessageId`
    /// alone, and a client that shipped its own copy of the subject would be
    /// inviting the two to disagree. It exists so the composer can name what
    /// rides along, and so `ComposePane.reviewSubject` can mirror the daemon's
    /// own `Fwd: …` titling for a sender who cleared the subject field.
    var forwardedSubject: String?
    /// THE FORWARDED ORIGINAL ITSELF, which the composer shows underneath the
    /// note, indented like quoted mail. DISPLAY ONLY, exactly like
    /// `forwardedSubject` and for exactly the same reason: the wire carries
    /// `forwardOfMessageId` and nothing else, and the daemon rebuilds the quote
    /// from its own RAW GMAIL FETCH of that id.
    ///
    /// So this is THE READER'S SANITIZED COPY of the same message, standing in
    /// for a quote that is assembled somewhere else. Without it the composer is
    /// an empty new message that inexplicably sends a fat email, and review —
    /// whose whole job is promising what goes out — promises the covering note
    /// alone.
    ///
    /// THE PREVIEW AND THE WIRE CAN DIFFER, in exactly the ways ingest
    /// sanitization differs from raw: this html has been through ammonia, its
    /// tracking pixels are stripped unless the sender is `sender_known`, and its
    /// `cid:` sources have been rewritten to point at the stored parts. What
    /// actually goes out is the original as Gmail holds it, pixels and all.
    /// Nothing else drifts — same message, same id, same files.
    var forwardedMessage: ClientMessage?

    /// The one word both compose events use for what this draft IS —
    /// `compose_opened` and `compose_send` must not disagree about that.
    /// Analytics only; nothing on the wire reads it.
    var analyticsKind: String {
        if forwardOfMessageId != nil { return "forward" }
        return replyToMessageId == nil ? "new" : "reply"
    }
}

/// The email currently being reclassified by the `v` palette.
struct TriageFixTarget: Sendable, Equatable {
    var messageId: Int
    var sender: String
    var subject: String
    /// Current values for the "was" labels. Double optional: an outer nil means
    /// the caller does not know, and that dimension is OMITTED rather than shown
    /// as "unset" — which would claim a fact we never fetched.
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
    /// TAKES THE SAVE OVER. Set, the editor hands it the body it built and
    /// stops there: no POST, no analytics, no rule. The onboarding tour is the
    /// only caller — its rule is a demonstration, and a real write would both
    /// invent a rule nobody asked for and 403 on a read-only daemon.
    var intercept: (@MainActor @Sendable (CreateRuleBody) -> Void)?
    /// Called after a successful save so the opener re-fetches its list.
    var onSaved: (@MainActor @Sendable () -> Void)?
}

/// The open thread, reduced to what the ⌘K agent needs to be told about it:
/// which email the person is looking at, and the id every thread-level verb in
/// the reader already targets. A named struct rather than a tuple because it is
/// an `@Observable` property, and lifted out of the viewer because the ask bar
/// is a modal ABOVE it with no other way to see what it holds.
struct OpenThreadSummary: Sendable, Equatable {
    /// WHICH THREAD THIS DESCRIBES. Carried so a reader can check the summary
    /// against the thread actually open rather than trust that the last writer
    /// won — a slow fetch lands whenever it lands. See `currentThreadSummary`.
    var threadId: String
    var subject: String
    /// The NEWEST message in the thread — see ThreadViewer's `newest`.
    var newestMessageId: Int
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

    /// A `passband://pair` link waiting to be acted on. ConnectView is its only
    /// consumer and clears it as it applies it — and applying it only ever
    /// FILLS THE FORM, never claims, so a link can be parked without it ever
    /// becoming a credential on its own.
    var pairLink: PairLink?

    /// The Add Account sheet. Raised by Settings, the rail's account menu, the
    /// Accounts menu, and by a pair link arriving at an install that already
    /// has an identity — that link names a SECOND daemon, and the sheet is
    /// where a second daemon is added. It is deliberately not a connection
    /// state: adding an account must not move `connStatus`, or the shell would
    /// unmount and the account already on screen would go with it.
    var addAccountSheetOpen = false

    // MARK: sitrep slice
    var sitrep = SitrepData()
    var lastRefresh: Date?
    var refreshError: RefreshError?

    /// Down = refreshes failing AND nothing has ever loaded this session, so the
    /// routed view is replaced by the down pane (Settings excepted: it must stay
    /// reachable to fix the token/URL). Once a sync HAS landed, failures degrade
    /// to a stale-data banner and the stale view stays up.
    var daemonDown: Bool { refreshError != nil && lastRefresh == nil }

    // MARK: routing slice
    var activeView: MainView = .sitrep
    var history: [HistoryEntry] = [HistoryEntry(view: .sitrep, selectedId: nil)]
    var historyIndex = 0

    // MARK: selection slice — stable by message id
    var selectedId: Int?

    // MARK: sitrep zones — see SitrepZoneCache
    var zones = SitrepZoneCache()

    // MARK: surfaces
    var sideView: SideView = .none
    /// Survives the search panel closing — see SearchSession.
    var search = SearchSession()
    /// The fullscreen email viewer. Orthogonal to sideView (it layers above),
    /// so closing the viewer restores whatever was underneath.
    var threadId: String?
    /// The ordered list the viewer was opened FROM, so "done + next" (e/d) can
    /// advance in place. Empty when opened from a surface without a queue.
    var threadQueue: [AttentionUpdate] = []
    /// WHERE THE READER IS IN ITS FLIGHT. `settled` is where it lives and is the
    /// default, because opening an email is navigation: a surface people enter
    /// and leave all day should just BE there. The other two are the done+next
    /// flight, and it is deliberately a position rather than a SwiftUI
    /// transition — a transition is chosen by the framework at the moment a view
    /// is inserted or removed, and when it declines to use the one you gave it
    /// you get its default crossfade and no way to argue. An offset is not
    /// negotiable.
    ///
    /// ThreadViewer drives it in two beats: `departing` lifts the email you
    /// finished off the top of the window, then the next one mounts `entering`
    /// from an edge and is walked home. See `doneAndNext`.
    var threadFlight: ThreadFlight = .settled

    enum ThreadFlight: Sendable, Equatable {
        /// In the window, where the reader belongs.
        case settled
        /// Lifted out through the top: the email you just finished.
        case departing
        /// Parked one window outside the frame, waiting to be walked in.
        case entering(ThreadEdge)
    }

    /// Which side a thread arrives from. The bottom is a new subject; the
    /// trailing edge is MORE MAIL FROM THE SAME SENDER, because that is the next
    /// item in a pile rather than the next thing to think about.
    enum ThreadEdge: Sendable, Equatable {
        case bottom
        case trailing
    }
    /// What that thread IS, once it has landed — written by the viewer. nil
    /// while a thread is loading. READ IT THROUGH `currentThreadSummary`: this
    /// is the raw last write, and the writer is a network callback.
    var openThreadSummary: OpenThreadSummary?
    /// The summary ONLY IF it describes the thread that is open. The viewer
    /// writes it from a fetch callback, so a slow load can land after the user
    /// has moved on; `openThread` clears the stale one on the way out, and this
    /// is the second lock on the same door — the ask bar pins what it reads
    /// here into a prompt, where a subject and a message id belonging to
    /// another email would be a lie told confidently.
    var currentThreadSummary: OpenThreadSummary? {
        guard let threadId, let summary = openThreadSummary, summary.threadId == threadId
        else { return nil }
        return summary
    }
    /// Bumped when a sync brings a message NEWER than the one the reader holds
    /// into the thread on screen — see SitrepPoller.performPull. The viewer
    /// watches it and refetches without moving the reading position.
    ///
    /// A token rather than the mail itself: the poller reads the attention
    /// bands, which say a thread moved but not what it now contains, and the
    /// viewer is the one place that knows how to adopt a thread. It is also why
    /// nothing resets it — a counter only has to CHANGE, so a thread switch
    /// under a live token is a viewer that simply never hears about the bump it
    /// no longer cares about.
    var openThreadRefreshToken = 0
    var compose: ComposeState?
    /// The reader's inline reply composer. Deliberately NOT part of
    /// `modalOverlayOpen`: it is a bar inside the reading surface, not an overlay
    /// on one, so the thread behind it must stay unblurred and clickable — you
    /// answer an email while reading it.
    var inlineReply: ComposeState?
    /// A reply the thread viewer should open its composer on the moment the
    /// thread lands. Set by `openThread(replyTo:)`, so `r` on a list row is one
    /// gesture: navigate, then compose. One-shot — the viewer clears it.
    var pendingReplyMessageId: Int?
    var triageFix: TriageFixTarget?
    var ruleEditor: RuleEditorRequest?
    var processModeOpen = false
    var askBarOpen = false
    /// The ⌘K agent's conversation, HELD HERE rather than in AskBar: the modal
    /// is conditionally mounted, so a view-owned session would be torn down —
    /// mid-answer — every time the bar closed. Living in the store, the
    /// transcript survives until the user asks for a new chat.
    let assistant = AssistantSession()
    var shortcutsOpen = false
    /// The first-run tour. Held here for the same reason the assistant is: it
    /// outlives every view it draws itself in, and the trigger that starts it
    /// (the first sync of the session) fires from the shell, not from a step.
    let tour = TourController()

    // MARK: undo / toasts
    var undos: [PendingUndo] = []
    var toasts: [Toast] = []

    // MARK: 2FA
    var authRings: [AuthRing] = []
    /// Newest-first queue of code-modal entries (only otp/verification).
    var authQueue: [AuthCodeEntry] = []

    // MARK: read tracking
    /// The daemon's tracking answer, fetched once on connect. nil = never
    /// asked (or the daemon did not answer), which reads exactly like
    /// unconfigured: no toggle anywhere, no receipts fetched.
    var tracking: TrackingConfig?

    // MARK: account switching

    /// Bumped ONCE per account switch. Every async writer of account-scoped
    /// state captures it before its first await and refuses to write once it
    /// no longer matches: a request built against account A's daemon can land
    /// after B is live, and message ids are per-daemon SQLite ints — so a
    /// stale answer does not read as stale, it reads as B's mail.
    private(set) var epoch = 0

    /// True for the length of a switch. THE re-entrancy guard: the sequence
    /// suspends twice (the draft settle, the keychain read) and a second
    /// switch starting inside one of those windows would configure the client
    /// out from under the first.
    private(set) var switching = false

    /// Whether `e` is still the live epoch — the post-await check every
    /// account-scoped writer outside this file makes.
    func isCurrent(_ e: Int) -> Bool { epoch == e }

    private init() {}

    // MARK: - settings

    func loadSettings() async {
        // The index says WHICH account's credentials to read, and repairs a
        // pre-multi-account install on the way through. Off the main actor for
        // the same reason the load below is.
        let index = await AccountIndex.loadOrMigrate()
        // Into the observable mirror before anything reads it: the manager may
        // have been built — and read a pre-repair index — during boot.
        AccountManager.shared.adopt(index)
        guard let active = index.active else {
            connStatus = .disconnected
            return
        }
        // Off the main actor: a keychain read can put up the system's "allow
        // access?" panel and block until answered.
        switch await SettingsStore.loadAsync(accountId: active.id) {
        case .success(let stored):
            if let stored {
                await APIClient.shared.configure(
                    baseURL: stored.serverURL, token: stored.apiToken)
                settings = stored
                connStatus = .connected
                connError = nil
                // A link that arrived during boot (the app was LAUNCHED by one)
                // races the keychain read and finds no Connect gate to land on.
                // On the Mac it is not dropped for that: this install having an
                // identity is precisely what makes the link an ADD rather than
                // a re-pair, so it goes to the same sheet a link arriving a
                // minute later would, parked for the sheet's ConnectView to
                // read as it mounts. On the phone nothing presents that sheet,
                // and a link left parked only goes stale — same policy as
                // `receivePairLink`, it is dropped instead.
                #if os(iOS)
                    pairLink = nil
                #else
                    if pairLink != nil { addAccountSheetOpen = true }
                #endif
            } else {
                connStatus = .disconnected
            }
        case .failure:
            connStatus = .disconnected
            connError = "settings load failed"
        }
    }

    /// Take a `passband://` URL the OS handed us. Never acted on here — it is
    /// parked for a ConnectView to pick up, and that view only ever fills its
    /// form from it. Which ConnectView depends on what this install already is:
    ///
    /// - No identity yet: the Connect gate is on screen and reads it there.
    /// - Already connected: the gate is not mounted, and the link is a SECOND
    ///   daemon asking to be added, so the Add Account sheet is opened for it.
    ///   Re-pairing over the identity this install holds is still impossible —
    ///   the sheet adds an account beside it, it does not replace it.
    func receivePairLink(_ url: URL) {
        guard let link = PairLink(url) else { return }
        #if os(iOS)
            // Nothing on the phone presents the Add Account sheet, so a link
            // arriving while connected would park here forever — and then be
            // applied, arbitrarily stale, by whatever Connect gate mounts
            // next. Dropped instead; a link at the gate still fills the form.
            guard connStatus != .connected else { return }
        #endif
        pairLink = link
        if connStatus == .connected { addAccountSheetOpen = true }
    }

    /// Test a candidate URL+token via /client/stats; on success persist + connect.
    ///
    /// THE GATE'S path, and the first account's: it moves `connStatus`, which
    /// is what swaps the Connect screen for the shell. Adding an account to an
    /// install that already has one is `addAccount` instead, precisely because
    /// this one moves connection state the live account is standing on.
    ///
    /// `label` is the optional name from the form. Empty means "unchanged"
    /// rather than "unnamed", so a re-connect through the gate cannot silently
    /// erase a label the account already had.
    @discardableResult
    func connect(serverURL: String, apiToken: String, label: String = "") async -> Bool {
        connStatus = .connecting
        connError = nil
        // Probe with a throwaway config so a bad token never gets persisted.
        await APIClient.shared.configure(baseURL: serverURL, token: apiToken)
        do {
            _ = try await APIClient.shared.getStats()  // 401 => bad token; network => bad url
            // Re-connecting to a daemon the index ALREADY NAMES — active or
            // not — keeps that record's id (and so its keychain slots and
            // scoped cursors); a daemon no record names mints a fresh one. The
            // gate can be reached with healthy accounts still in the index (a
            // failed switch, a denied keychain read at boot), and the two
            // wrong answers there are each other's mirror: reusing the active
            // record for a DIFFERENT daemon overwrites a survivor's
            // credentials, while minting a fresh record for a KNOWN daemon
            // splits one mailbox across two accounts and doubles its streams
            // and banners. The index entry is written only after the
            // credentials are in the keychain.
            let host = AccountRecord.host(from: serverURL)
            var account =
                AccountIndex.load().accounts.first { $0.displayHost == host } ?? AccountRecord()
            let named = label.trimmingCharacters(in: .whitespacesAndNewlines)
            if !named.isEmpty { account.label = named }
            // The name the switcher shows when there is no label, learned here
            // rather than from a keychain read later on the main actor.
            account.displayHost = AccountRecord.host(from: serverURL)
            let fresh = ConnectionSettings(serverURL: serverURL, apiToken: apiToken)
            try await SettingsStore.saveAsync(fresh, accountId: account.id).get()
            AccountIndex.upsert(account)
            // Into the observable mirror too: everything account-scoped — the
            // 2FA seen-set, the decisions ledger, the notification feeds —
            // derives its storage key (or its very existence) from
            // `AccountManager`, and a first connection is the moment that goes
            // from nothing to something.
            AccountManager.shared.reload()
            // The decisions ledger's key derives from the id that just went
            // live. Without this, a session that removed its last account and
            // connected a different one would still hold the OLD account's
            // in-memory verdicts — served under the new daemon's colliding
            // message ids, and persisted wholesale under the new key on the
            // first `set`.
            AuthDecisions.shared.reload()
            // The gate's world is empty, but not necessarily CLEAN: view state
            // parked before the disconnect — a thread id planted by a stale
            // notification tap, answers still in flight from the previous
            // daemon — would otherwise survive into this account. Same fence a
            // switch uses; costs nothing when there was truly nothing on
            // screen.
            epoch &+= 1
            wipeAccountState()
            wipeAccountCaches()
            // Arriving from the Connect gate the feeds are down, so this is a
            // no-op and the `.connected` transition below is what raises them
            // — the same route the single-account build took. Kept as a
            // defensive pair with `revalidate`'s, which handles the
            // stay-connected recredential arrival.
            AccountManager.shared.restartFeeds(account.id, with: fresh)
            settings = fresh
            connStatus = .connected
            connError = nil
            Analytics.capture("connect_succeeded")
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

    /// Settings-screen re-validate: test a candidate and, on success, persist and
    /// swap the live client — WITHOUT dropping connStatus out of "connected" on
    /// failure, so Settings stays mounted instead of bouncing to the Connect gate.
    func revalidate(serverURL: String, apiToken: String) async -> (ok: Bool, error: String?) {
        let prev = settings
        await APIClient.shared.configure(baseURL: serverURL, token: apiToken)
        do {
            _ = try await APIClient.shared.getStats()
            // Same account, new credentials — `activeOrNew` returns the live
            // record here, so this overwrites its slots rather than adding one.
            var account = AccountIndex.activeOrNew()
            // A moved daemon renames its own row in the switcher; the label,
            // if the human gave one, is untouched.
            account.displayHost = AccountRecord.host(from: serverURL)
            let fresh = ConnectionSettings(serverURL: serverURL, apiToken: apiToken)
            try await SettingsStore.saveAsync(fresh, accountId: account.id).get()
            AccountIndex.upsert(account)
            AccountManager.shared.reload()
            // The feed is still connected with the OLD token, and nothing else
            // will ever tell it otherwise: re-validating deliberately never
            // leaves `.connected`, so the transition that starts the streams
            // does not fire. A rotated token means the old connection is one
            // 401 away from a silent backoff loop, so it is replaced outright
            // — streams take their credentials at construction and keep them.
            AccountManager.shared.restartFeeds(account.id, with: fresh)
            settings = fresh
            return (true, nil)
        } catch {
            // Restore the prior working client — a fat-fingered token must not
            // leave the app pointed at a bad config. With no prior config to
            // restore, the rejected candidate must not linger either.
            if let prev {
                await APIClient.shared.configure(baseURL: prev.serverURL, token: prev.apiToken)
            } else {
                await APIClient.shared.deconfigure()
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

    /// Whether a daemon is already one of this install's accounts. Matched on
    /// host:port, which the index already holds and no keychain read is needed
    /// for; a different spelling of the same host (localhost for 127.0.0.1)
    /// slips through, and the cost of that is the duplicate the human
    /// explicitly asked for. Public because `ConnectView` must ask BEFORE
    /// claiming a pairing code — a claim mints a device token on the daemon,
    /// and a duplicate refused after that has already spent the code and
    /// orphaned the token server-side.
    func isKnownDaemon(_ serverURL: String) -> Bool {
        let host = AccountRecord.host(from: serverURL)
        return AccountManager.shared.accounts.contains { $0.displayHost == host }
    }

    /// Add a SECOND (or fifth) account and switch to it.
    ///
    /// The whole difference from `connect` is what it refuses to touch. This
    /// install already has an identity and a mailbox on screen, so:
    ///
    /// - the probe goes through `APIClient.probe`, which builds its one request
    ///   from the candidate credentials instead of installing them — the live
    ///   account's requests keep working while a stranger's token is tested,
    ///   and a bad one is never configured to find out that it is bad;
    /// - `connStatus` never moves, so the shell stays mounted (dropping to
    ///   `.connecting` would unmount the account already on screen and flash
    ///   the Connect gate under the sheet);
    /// - the credentials land under a NEW id via `AccountManager.add`, which
    ///   writes the keychain before the index entry that names it.
    ///
    /// Only then is the world torn down, and only through the ordinary switch —
    /// the one place that knows how to do it safely.
    func addAccount(serverURL: String, apiToken: String, label: String = "") async -> (
        ok: Bool, error: String?
    ) {
        // ONE DAEMON IS ONE MAILBOX, so the same daemon twice is not two
        // accounts — it is every banner delivered twice and one mailbox's state
        // split across two records.
        if isKnownDaemon(serverURL) {
            return (false, "that daemon is already one of your accounts")
        }
        do {
            try await APIClient.shared.probe(baseURL: serverURL, token: apiToken)
        } catch {
            return (false, Self.connectErrorText(error))
        }
        let fresh = ConnectionSettings(serverURL: serverURL, apiToken: apiToken)
        guard
            let record = await AccountManager.shared.add(
                label: label.trimmingCharacters(in: .whitespacesAndNewlines), settings: fresh)
        else {
            // The keychain refused the write and `add` rolled it back, so
            // nothing was recorded. Nothing to undo here either.
            return (false, "could not save to the keychain")
        }
        Analytics.capture("account_added")
        // Adding an account is asking to use it. Through the ordinary switch,
        // which flushes drafts, wipes every per-account cache and bumps the
        // epoch — the new account is a whole new daemon, and none of the ids on
        // screen mean anything there. Waited for AND held: a spin alone leaves
        // a gap where a switch starting between the loop and the call makes
        // `switchTo`'s guard silently decline, and "added but never shown"
        // would read as a failed add. Loop and flag are both MainActor, so the
        // handoff has no gap; `performSwitch` directly, because `switchTo`
        // would bounce off the very gate held here.
        while switching { try? await Task.sleep(for: .milliseconds(50)) }
        switching = true
        defer { switching = false }
        // The switch can fail its credential read-back (a denied access panel
        // on the slots just written). The account exists either way — saying
        // `ok` while the old mailbox stays on screen would read as a failed
        // add, and the retry it invites is refused as a duplicate daemon.
        guard await performSwitch(to: record) else {
            return (false, "account added, but the keychain refused its credentials; pick it from the account switcher to retry")
        }
        return (true, nil)
    }

    /// Forget one account, wherever it sits in the list. THE removal path:
    /// Settings' per-row Remove lands here (its last-account form wears the
    /// old Disconnect label), so there is exactly one answer to "what happens
    /// to the world when the mailbox on screen goes away".
    ///
    /// `AccountManager.remove` does the durable half — feed down, keychain
    /// cleared (off the main actor, a refusal parked for the boot sweep), index
    /// entry dropped, scoped defaults forgotten, the legacy slots sealed if
    /// that was the last account. What is left for here is the world on screen,
    /// and only when it belonged to the account that just went.
    func removeAccount(_ id: UUID) async {
        // Wait out any in-flight switch, then hold its gate for the duration:
        // a remove landing inside a switch's suspension windows would leave
        // the index, the configured client and the per-account-keyed stores
        // disagreeing about which mailbox is live — and `wasActive` below is
        // only meaningful once whoever was switching has finished doing so.
        // Both loop and flag are MainActor, so the handoff has no gap.
        while switching { try? await Task.sleep(for: .milliseconds(50)) }
        switching = true
        defer { switching = false }

        let wasActive = AccountManager.shared.activeId == id
        // Fenced BEFORE the index moves: `remove` suspends on keychain awaits,
        // and a sitrep pull of this daemon already in flight would otherwise
        // land under the old epoch — attributed to the survivor the index
        // reassigns `active` to, and folded into the survivor's persisted
        // seen-set (message ids are per-daemon ints; they collide).
        if wasActive {
            epoch &+= 1
            SitrepPoller.shared.stop()
        }
        await AccountManager.shared.remove(id)
        guard wasActive else { return }

        guard let survivor = AccountManager.shared.active else {
            // THE LAST ACCOUNT. Back to the Connect gate: every id on screen
            // addressed a daemon this install no longer holds credentials for.
            // The drafts are deliberately NOT flushed on the way out (a switch
            // flushes them) — there is nowhere to send them that we are still
            // entitled to talk to, and the human asked for this account to be
            // forgotten.
            await tearDownToGate(error: nil)
            return
        }
        // Straight to `performSwitch` rather than `AccountManager.switchTo`:
        // that one refuses a switch to the account already marked active, and
        // `AccountIndex.remove` handed `active` to this survivor on its way
        // through. The index has moved on; the store has not, and this is what
        // moves it — under the gate this function already holds. Drafts are
        // NOT flushed on the way: the daemon they would go to is the one the
        // human just asked to forget, same principle as the last-account
        // branch above.
        await performSwitch(to: survivor, flushDrafts: false, currentWorldGone: true)
    }

    // MARK: - switching accounts

    /// Make `record` the live account: tear the current world down, point the
    /// client at the new daemon, and start it up again. `connStatus` never
    /// leaves `.connected`, so the Connect gate does not flash in between —
    /// the restart at the end is by hand for exactly that reason.
    ///
    /// Entered through `AccountManager.switchTo`, which owns the "is this a
    /// real, different account" half of step (1).
    ///
    /// ACCEPTED RESIDUAL RACE, documented rather than fixed: an action Task
    /// created in the same instant as this one can read `APIClient.shared`'s
    /// config AFTER step (8) has replaced it, and send account A's verb to
    /// account B's daemon. The window is the sub-millisecond gap between that
    /// Task being created and its request being built. Closing it needs a
    /// generation INSIDE APIClient that every request checks and every caller
    /// passes — identified, not built, because it touches all 25 call sites.
    func switchAccount(to record: AccountRecord) async {
        // (1) Never two at once — the two awaits below are both windows a
        //     second switch could start in.
        guard !switching else { return }
        switching = true
        // (10) Whichever path we leave by.
        defer { switching = false }
        await performSwitch(to: record)
    }

    /// Steps (2)–(9), gate already held. Split out so `removeAccount` — which
    /// holds the same gate across its OWN awaits — can run a switch without
    /// tripping the guard that exists to keep everyone else out.
    ///
    /// `flushDrafts: false` is the removal path's variant: the daemon those
    /// drafts would go to is the one being forgotten. The settle still runs —
    /// a debounced save already in flight must finish before the client is
    /// reconfigured, whoever it was addressed to.
    @discardableResult
    private func performSwitch(
        to record: AccountRecord, flushDrafts: Bool = true, currentWorldGone: Bool = false
    ) async -> Bool {
        // (2) The target's credentials, BEFORE anything is torn down — the one
        //     step that can fail. Failing here, with the old world untouched,
        //     keeps the human on a working mailbox; checking after the wipe
        //     (the old shape) turned a single denied access panel into a
        //     logout, and the gate it landed on then endangered the surviving
        //     accounts' credentials. A keychain read can raise the access
        //     panel, but nothing has been suspended or wiped yet, so blocking
        //     here is safe.
        let loaded = await AccountManager.shared.credentialLoad(for: record.id)
        guard case .ok(let next) = loaded else {
            let why: String
            if case .unreadable = loaded {
                why = "the keychain refused to unlock \(record.displayName)"
            } else {
                why = "no credentials for \(record.displayName) in the keychain"
            }
            guard currentWorldGone else {
                // A plain switch: the world on screen is intact and the
                // credentials backing it were never touched. Stay — and say
                // so. A toast, not `connError`: that field renders only on
                // ConnectView, so writing it here would show nothing now and
                // then greet the next Add Account sheet with a stale failure.
                pushToast(why, .error)
                return false
            }
            // The removal path: the account on screen is already gone from the
            // index, so there is no working world to stay in — the Connect
            // gate is the honest answer.
            await tearDownToGate(error: why)
            return false
        }

        // (3) From here, every answer still in flight belongs to the old
        //     account and every writer that captured the old epoch is inert.
        epoch &+= 1

        // (4) Stop polling this daemon. The EventStreams are deliberately left
        //     alone — every account holds one, live or not, so that mail
        //     arriving anywhere still raises a banner. Switching changes which
        //     mailbox is on screen, not which ones are worth hearing from.
        SitrepPoller.shared.stop()

        // (5) Both composers' last save — and then WAIT for it. Reconfiguring
        //     the client underneath an in-flight draft PUT would post what the
        //     human typed into account A into account B's drafts, under an id
        //     that means something else there.
        if flushDrafts {
            DraftSaver.shared.flush(.compose, compose)
            DraftSaver.shared.flush(.inlineReply, inlineReply)
        }
        await DraftSaver.shared.settle()

        // (6) The ⌘K session: its transcript cites the old account's mail and
        //     its parked tool calls would act on it.
        assistant.clear()

        // (7) The world itself, in one synchronous pass so no frame is
        //     ever painted showing a mix of two mailboxes.
        wipeAccountState()
        wipeAccountCaches()

        // (8) The new credentials go live.
        settings = next
        connError = nil
        await APIClient.shared.configure(baseURL: next.serverURL, token: next.apiToken)
        AccountManager.shared.markActive(record.id)
        // AFTER `markActive`, deliberately: the ledger's UserDefaults key is
        // derived from the live account id, so reloading it with the singletons
        // in step (7) would have re-read the account that just went away.
        AuthDecisions.shared.reload()

        // (9) Restart by hand. `connStatus` was never dropped, so the
        //     `.connected` transition that normally does this never fires.
        //     Fire-and-forget: a slow daemon must not hold the switch (and so
        //     the next ⌘number) open for a round trip. Both are epoch-guarded.
        SitrepPoller.shared.start()
        Task { await refreshMail(.inbox) }
        Task { await refreshTrackingConfig() }
        return true
    }

    /// Tear the world down to the Connect gate: the shared tail of "the
    /// account on screen is gone" — the last account removed, or a removal's
    /// survivor unreadable. ONE copy, because a teardown-ordering fix that
    /// lands in one path and misses another leaves that path presenting a
    /// forgotten daemon's token.
    ///
    /// The ledger reload empties the in-memory verdicts (no live account, no
    /// key), so a later `connect` cannot inherit — and then persist — a gone
    /// account's decisions under a new daemon's colliding message ids. The
    /// deconfigure is the same principle for the client itself.
    private func tearDownToGate(error: String?) async {
        epoch &+= 1
        SitrepPoller.shared.stop()
        assistant.clear()
        wipeAccountState()
        wipeAccountCaches()
        AuthDecisions.shared.reload()
        await APIClient.shared.deconfigure()
        settings = nil
        connStatus = .disconnected
        connError = error
    }

    /// Everything in this store that belongs to ONE account. Message ids are
    /// per-daemon SQLite ints, so "looks plausible" is precisely the failure
    /// mode being prevented here: A's row 412 is B's row 412.
    private func wipeAccountState() {
        sitrep = SitrepData()
        zones = SitrepZoneCache()
        // The parked refresh tasks go too, not just their results: a joiner
        // arriving after the switch would otherwise wait on — and adopt the
        // silence of — a pass that has already been disowned.
        zoneRefresh = nil
        mailPages = [:]
        mailLoadedAt = [:]
        mailRefreshes = [:]
        search = SearchSession()
        resolvedIds = []
        selectedId = nil
        // A fresh sync history: a `lastRefresh` from the old account must not
        // make a silent new one look recently synced (the rule `connect` keeps).
        lastRefresh = nil
        refreshError = nil
        // The thread viewer and both composers, by assignment rather than
        // through `closeThread`/`closeCompose`: those flush drafts, and step
        // (4) already saved and settled everything there was to save.
        threadId = nil
        threadQueue = []
        pendingReplyMessageId = nil
        compose = nil
        inlineReply = nil
        sideView = .none
        // Overlays holding an account-scoped id. A rule editor left open
        // across a switch would save the old account's rule to the new
        // account's daemon.
        triageFix = nil
        ruleEditor = nil
        // Every revert closure targets the old account's daemon.
        undos = []
        authRings = []
        authQueue = []
        tracking = nil
        history = [HistoryEntry(view: .sitrep, selectedId: nil)]
        historyIndex = 0
        route(to: .sitrep)
    }

    /// The per-account caches that live OUTSIDE this store. Every one of them
    /// is keyed by something a daemon minted — message ids, attachment ids,
    /// thread ids — so every one of them collides across accounts.
    ///
    /// Deliberately NOT wiped: `PreparedBodies` (keyed by a hash of the html
    /// itself) and `ImageStore`'s files (keyed by sha256 of the url). Both are
    /// content-addressed, so a cross-account hit is the same bytes. The image
    /// store's message-id PINS do go ambiguous across accounts, which
    /// mis-prioritises eviction and nothing else — accepted.
    private func wipeAccountCaches() {
        ThreadPrefetch.shared.wipe()
        HeroCache.shared.wipe()
        AttachmentThumbs.shared.wipe()
        // The staged preview files, for the same reason and with more teeth: a
        // survivor here is another account's attachment served from this
        // account's id, straight into a preview.
        AttachmentFiles.shared.wipe()
        FrameHeights.shared.wipeAll()
        // The reader's live-frame pool. In EmailWebCore, which both targets
        // compile, so no platform fence.
        WebFramePool.shared.wipeAll()
        ImageWarmer.shared.resetForSwitch()
        AuthArrival.shared.resetForSwitch()
        // AuthDecisions.reload() is NOT here — see switchAccount step (8).
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

    /// Whether the last route came from the POINTER: the rail slides its selector
    /// for a click (a continuous gesture the eye follows) and snaps for
    /// everything else (`3` is instantaneous; animating it just puts 300ms
    /// between the keypress and the answer). Read by the rail in the same update
    /// that carries the new `activeView`, so the two cannot disagree.
    private(set) var routeWasPointer = false

    /// EVERY route goes through here, so the flag can never be left describing
    /// the previous one — assigning `activeView` directly would inherit whatever
    /// the last click set and slide the rail for a keyboard route.
    private func route(to view: MainView, viaPointer: Bool = false) {
        // On change only: the repeat-press no-op in setView re-routes to the
        // same view, and that is not a navigation worth an event.
        if view != activeView { Analytics.screen(view.rawValue) }
        routeWasPointer = viaPointer
        activeView = view
    }

    func setView(_ view: MainView, viaPointer: Bool = false) {
        // Navigating ANYWHERE dismisses an open thread viewer: the rail is visible
        // beside it, so a click there means "leave this email", not "change the
        // page underneath the overlay". Through closeThread() so the reader's
        // inline reply is torn down with it, wherever the exit was taken from.
        closeThread()
        // No-op on the same view+selection, so a repeat press cannot spam
        // identical history entries.
        let cur = history.indices.contains(historyIndex) ? history[historyIndex] : nil
        if let cur, cur.view == view, cur.selectedId == selectedId {
            route(to: view, viaPointer: viaPointer)
            return
        }
        route(to: view, viaPointer: viaPointer)
        pushHistory(HistoryEntry(view: view, selectedId: selectedId))
    }

    /// Switch to the Emails view showing one PAGE — the header's noise count and
    /// the sitrep's noise affordances are both this.
    func openMail(_ mode: MailMode) {
        mailMode = mode
        setView(.emails)
    }

    /// Switch to the Emails view with a specific update selected — the sitrep's
    /// "view" affordances hand off to the band list with the right row focused.
    func viewInEmails(_ id: Int) {
        // A hand-off always targets a signal row, which the noise page does not
        // hold: land on the inbox or the highlight would have nothing to find.
        mailMode = .inbox
        route(to: .emails)
        selectedId = id
        closeThread()
        pushHistory(HistoryEntry(view: .emails, selectedId: id))
    }

    /// The record zones' shared click routing: open the row's thread, falling
    /// back to the emails-page jump when an older daemon sent no thread_id —
    /// the click always does SOMETHING.
    func openRecord(thread: String?, message: Int) {
        if let thread, !thread.isEmpty {
            openThread(thread)
        } else {
            viewInEmails(message)
        }
    }

    var canGoBack: Bool { historyIndex > 0 }
    var canGoForward: Bool { historyIndex < history.count - 1 }

    func goBack() {
        guard historyIndex > 0 else { return }
        historyIndex -= 1
        let entry = history[historyIndex]
        route(to: entry.view)
        selectedId = entry.selectedId
    }

    func goForward() {
        guard historyIndex < history.count - 1 else { return }
        historyIndex += 1
        let entry = history[historyIndex]
        route(to: entry.view)
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

    /// Open the fullscreen reader. `replyTo` is the unified `r` verb: the message
    /// id the reader should open its inline composer on once the thread loads.
    func openThread(
        _ threadId: String, queue: [AttentionUpdate] = [], replyTo: Int? = nil,
        entering edge: ThreadEdge? = nil
    ) {
        // from_noise: an open from below the squelch line — someone digging for
        // mail the triage muted, which is the false-negative signal.
        Analytics.capture(
            "thread_opened",
            [
                "via_reply": replyTo != nil,
                "from_noise": activeView == .emails && mailMode == .noise,
            ])
        // A DIFFERENT thread drops the summary NOW rather than when the new one
        // lands: in that gap the reader is showing thread B while this still
        // described A, and the ask bar would pin B's id under A's subject and
        // A's message id. Same-thread reopens keep it — the viewer is already
        // mounted and may never re-adopt.
        if self.threadId != threadId { openThreadSummary = nil }
        self.threadId = threadId
        self.threadQueue = queue
        // An ordinary open puts the reader straight in the window — opening an
        // email is a jump. Only done+next passes an edge, and it walks the
        // reader home itself a frame later.
        self.threadFlight = edge.map { .entering($0) } ?? .settled
        // HERE, not in the search panel's open path: the reader can be opened
        // from anywhere (ask-bar cards, citations) while fullscreen search sits
        // underneath, and it insets by only the STRIP — a still-expanded panel
        // would peek out as a sliced-off edge behind it.
        search.expanded = false
        // Both cleared unconditionally: moving to ANOTHER thread (h/l, done+next)
        // must not carry the previous one's draft or its pending reply request
        // into a thread they do not belong to. The draft is SAVED on the way out
        // rather than dropped — the reply is keyed to the message it answers, so
        // walking away from it and coming back restores it.
        DraftSaver.shared.flush(.inlineReply, inlineReply)
        inlineReply = nil
        pendingReplyMessageId = replyTo
    }

    func closeThread() {
        threadId = nil
        threadQueue = []
        // The flight is deliberately left where it is: the last email of a queue
        // is mid-departure when this runs, and there is nothing left to put back
        // in the window. `openThread` resets it for whatever comes next.
        openThreadSummary = nil
        DraftSaver.shared.flush(.inlineReply, inlineReply)
        inlineReply = nil
        pendingReplyMessageId = nil
    }

    /// True while a modal owns the screen; the surfaces under it blur so the modal
    /// reads as focus rather than as a new page. Toasts are deliberately absent (a
    /// toast must never defocus the app), as are the side panels, the thread
    /// viewer, and the compose pane — those are surfaces you work beside, not
    /// overlays on one. The reader's `inlineReply` is absent for the same reason:
    /// it is part of the viewer, and blurring the email you are answering would
    /// be absurd.
    /// The tour joins on `wantsBlur` alone: its three talking steps are modals
    /// like any other, but its coach marks POINT AT the board, so blurring what
    /// they are ringing would defeat them.
    var modalOverlayOpen: Bool {
        askBarOpen || shortcutsOpen || processModeOpen
            || triageFix != nil || ruleEditor != nil || !authQueue.isEmpty
            || tour.wantsBlur
    }

    // MARK: - sitrep zones

    /// Refresh every zone CONCURRENTLY, replacing rows only once new ones arrive,
    /// and skip entirely while the last refresh is still fresh — that is what
    /// makes a revisit instant instead of five round-trips. `force` is for callers
    /// that just changed the data (an explicit sync, a rule save).
    func refreshZones(force: Bool = false) async {
        if !force, let loadedAt = zones.loadedAt,
            Date().timeIntervalSince(loadedAt) < Self.zoneTTL
        {
            return
        }
        // JOIN an in-flight refresh rather than starting a second one. The TTL
        // cannot do this job: `loadedAt` is stamped only when a refresh COMPLETES,
        // and on a cold load all five zones plus the dashboard sail past it from
        // their own `.task` and each fire a full copy of the five requests.
        //
        // A FORCED caller does not stop there: a pass already in flight may have
        // read the daemon BEFORE the change it forced for landed.
        if let running = zoneRefresh {
            await running.value
            if !force { return }
        }
        let refresh = Task { await performZoneRefresh() }
        zoneRefresh = refresh
        await refresh.value
        // Only clear our OWN marker: a forced caller can have replaced it while we
        // were suspended, and nil-ing that one lets the next joiner start a third
        // redundant pass.
        if zoneRefresh == refresh { zoneRefresh = nil }
    }

    /// In-flight zone refresh, so concurrent callers share one pass.
    private var zoneRefresh: Task<Void, Never>?

    /// One zone's answer, tagged so completion order can drive the writes.
    /// nil rows = that endpoint failed and its zone keeps what it had.
    private enum ZoneAnswer: Sendable {
        case calendar([CalendarUpdate]?)
        case shipments([Shipment]?)
        case banking([BankingRecord]?)
        case receipts([Receipt]?)
        case rules(Int?)
        case newsletters([Newsletter])
    }

    private func performZoneRefresh() async {
        let e = epoch
        // Six independent fetches racing in one group, each zone written the
        // moment ITS fetch answers — completion order, not a fixed await
        // order, which is what actually keeps one wedged endpoint riding out
        // its timeout from holding every later zone's paint hostage. Every
        // write is individually fenced on the epoch, so a switch landing
        // mid-pass cannot show half of each mailbox; a failing endpoint (nil
        // rows) leaves its own zone's last good rows rather than blanking the
        // column.
        await withTaskGroup(of: ZoneAnswer.self) { group in
            group.addTask { .calendar(try? await APIClient.shared.getCalendar()) }
            group.addTask {
                .shipments(try? await APIClient.shared.getShipments(includeDelivered: true))
            }
            group.addTask { .banking(try? await APIClient.shared.getBanking()) }
            group.addTask { .receipts(try? await APIClient.shared.getReceipts()) }
            group.addTask { .rules((try? await APIClient.shared.listRules())?.count) }
            group.addTask { .newsletters(await NewsletterFeed.load()) }
            for await answer in group {
                guard e == epoch else {
                    group.cancelAll()
                    return
                }
                switch answer {
                case .calendar(let rows?): zones.calendar = rows
                case .shipments(let rows?): zones.shipments = rows
                case .banking(let rows?): zones.banking = rows
                case .receipts(let rows?): zones.receipts = rows
                case .rules(let count?): zones.rulesCount = count
                case .newsletters(let rows):
                    if !rows.isEmpty || zones.newsletters.isEmpty {
                        zones.newsletters = rows
                    }
                case .calendar, .shipments, .banking, .receipts, .rules: break
                }
            }
        }
        guard e == epoch else { return }
        zones.loadedAt = Date()

        HeroCache.shared.preload(zones.newsletters.map(\.latestThreadId))
        warmZoneThreads()
        // The newsletter half of the launch image warm's input; the bands are the
        // other half, and it starts once both have landed.
        ImageWarmer.shared.noteZonesLanded()
    }

    /// How long a zone refresh stays good: long enough that flipping between views
    /// is free, short enough that a dashboard left open still tracks the mail (the
    /// 10s sitrep poll drives the bands, which change minute to minute).
    private static let zoneTTL: TimeInterval = 45

    /// Dismiss one shipment from the Shipments zone.
    ///
    /// OPTIMISTIC, then authoritative: the card leaves the rail on the click,
    /// because a records rail is polled on a 45s TTL and waiting for the round
    /// trip would read as the menu item doing nothing. The forced refresh behind
    /// it is what makes the daemon's list — not this local edit — the thing on
    /// screen a moment later.
    ///
    /// NO UNDO, deliberately: clearing hides a row rather than destroying it, and
    /// the daemon brings it straight back when the carrier or a new email reports
    /// something new. A failed request puts the row back instead.
    func clearShipment(_ id: Int) async {
        let e = epoch
        guard let index = zones.shipments.firstIndex(where: { $0.id == id }) else { return }
        let row = zones.shipments[index]
        zones.shipments.remove(at: index)
        do {
            try await APIClient.shared.clearShipment(id)
            // The write landed on the OLD account's daemon; this account's rail
            // knows nothing about it and must not be refreshed on its behalf.
            guard e == epoch else { return }
            Analytics.capture("shipment_cleared")
            pushToast("cleared \(Self.shipmentLabel(row)) · it returns if there's news", .info)
            await refreshZones(force: true)
        } catch {
            guard e == epoch else { return }
            // Put it back where it was — but only if it is still missing: a zone
            // refresh can have landed while the request was in flight, and that
            // list is newer than this snapshot of one row.
            if !zones.shipments.contains(where: { $0.id == id }) {
                zones.shipments.insert(row, at: min(index, zones.shipments.count))
            }
            if let api = error as? APIError, api.kind == .notFound {
                // Two ways to earn this: a row this account no longer has, or a
                // daemon older than the route. The daemon's own word for both is
                // "not found", which is not a sentence anyone can act on, so say
                // the one thing that fixes the likelier of the two.
                pushToast("could not clear that package · update squelchd", .error)
            } else {
                pushToast(errText(error, "could not clear that package"), .error)
            }
        }
    }

    /// Ask the daemon to poll every carrier now.
    ///
    /// NOTHING IS REFRESHED HERE. The daemon answers before the carrier round
    /// trips, so a forced zone refresh behind this would repaint the same rows and
    /// teach the user the button does nothing; the rail's own 45s poll is what
    /// carries the answers in a few seconds later.
    ///
    /// The toast says WHAT WE DID rather than what the carriers will say, because
    /// those are two different sentences and only the first one is true on a
    /// daemon with no carrier keys — carrier polling is BYOK, and such a daemon
    /// runs no poller and does nothing with the kick.
    func pollShipments() async {
        do {
            let kick = try await APIClient.shared.pollShipments()
            Analytics.capture("shipments_poll_kicked")
            // A daemon with no carrier credentials answers a normal 200 that
            // kicked nothing. Saying "checking" there would be a lie the user
            // cannot see through, since the pass they are waiting on will never
            // change a single row.
            if kick.kicked {
                pushToast("checking \(Self.carrierList(kick.carriers))", .info)
            } else {
                pushToast("no carrier keys configured · see docs/SHIPMENTS.md", .info)
            }
        } catch {
            if let api = error as? APIError, api.kind == .notFound {
                pushToast("this daemon has no carrier polling · update squelchd", .error)
            } else {
                pushToast(errText(error, "could not reach the carrier poller"), .error)
            }
        }
    }

    /// The carriers a kick actually reached, as prose. The daemon returns its
    /// own slugs, so they are labelled through the same `Carrier` vocabulary the
    /// cards use rather than printed raw.
    private static func carrierList(_ slugs: [String]) -> String {
        let names = slugs.map { Carrier(rawValue: $0)?.label ?? $0.uppercased() }
        switch names.count {
        case 0: return "the carriers"
        case 1: return names[0]
        case 2: return "\(names[0]) and \(names[1])"
        default: return names.dropLast().joined(separator: ", ") + ", and " + names[names.count - 1]
        }
    }

    /// What a cleared shipment is CALLED in its toast: the card's own title rule
    /// (item name, else the carrier), kept short enough to sit in one.
    private static func shipmentLabel(_ s: Shipment) -> String {
        let trimmed = s.item_name.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? s.carrier.label : trimmed
    }

    // MARK: - the flat mail pages

    /// One generous page — the read model is local, so this is cheap.
    private static let mailLimit = 500
    /// How many rows get their thread warmed ahead of any click. A bounded
    /// prefix: warming all 500 would stampede the daemon for mail nobody scrolls
    /// to, and hover warming picks up anything past it.
    private static let mailWarmRows = 40
    /// Only long enough to collapse a mount and a poll tick landing together;
    /// staleness past it is harmless because a reload keeps the rows on screen.
    private static let mailTTL: TimeInterval = 5

    /// The emails tab's pages, HELD HERE RATHER THAN IN THE VIEW. `@State` is
    /// discarded on navigate-away, so a view-owned list refetched from nothing on
    /// every visit and showed "loading mail…" over an empty page each time. Held in
    /// the store they survive navigation, and `Loadable` keeps the last rows through
    /// a reload, so even a stale revisit paints rows first and updates underneath.
    private var mailPages: [MailMode: Loadable<[AttentionUpdate]>] = [:]
    /// Which page the emails tab is on. Here for the same reason as the pages
    /// themselves: a trip to the sitrep and back has to land where you left off.
    var mailMode: MailMode = .inbox
    /// Freshness and in-flight fetch, PER PAGE — one shared stamp would let the
    /// inbox's TTL swallow the noise page's first fetch. Kept out of the
    /// `Loadable`s, which the list reads: these are bookkeeping, not rows.
    private var mailLoadedAt: [MailMode: Date] = [:]
    private var mailRefreshes: [MailMode: Task<Void, Never>] = [:]

    /// One page's rows. Never fetched reads as LOADING, so the first paint of a
    /// page shows "loading" rather than claiming it is empty.
    ///
    /// INBOX AND NOISE ONLY — `.sent` holds a different wire type and lives in
    /// `sentPage`; asking here for it answers a permanent "loading".
    func mailPage(_ mode: MailMode) -> Loadable<[AttentionUpdate]> {
        mailPages[mode] ?? .loading
    }

    /// Same TTL-plus-join shape as `refreshZones`, for the same reasons: a
    /// revisit inside the window is free, and concurrent callers share one pass
    /// instead of each firing a 500-row fetch.
    func refreshMail(_ mode: MailMode, force: Bool = false) async {
        if !force, let loadedAt = mailLoadedAt[mode],
            Date().timeIntervalSince(loadedAt) < Self.mailTTL
        {
            return
        }
        if let running = mailRefreshes[mode] {
            await running.value
            if !force { return }
        }
        let refresh = Task { await performMailRefresh(mode) }
        mailRefreshes[mode] = refresh
        await refresh.value
        if mailRefreshes[mode] == refresh { mailRefreshes[mode] = nil }
    }

    /// Drop a message from the cached pages outright. For a resolve that must not
    /// come back on the next poll's slower truth. EVERY page, not just the one on
    /// screen: the inbox filters no tier, so a noise row sits in both.
    func removeFromMail(_ messageId: Int) {
        for mode in MailMode.allCases {
            mailPages[mode]?.value?.removeAll { $0.id == messageId }
        }
    }

    private func performMailRefresh(_ mode: MailMode) async {
        let e = epoch
        withMailPage(mode) { $0.isLoading = true }
        do {
            let fetched = try await APIClient.shared.getUpdates(
                UpdatesParams(tier: mode.tier, limit: Self.mailLimit))
            // The rows belong to the account that asked for them. Both exits
            // below return rather than fall through, so the `isLoading` write
            // at the bottom is only ever reached by the live epoch.
            guard e == epoch else { return }
            // Done/archived mail leaves the inbox (gmail semantics), which also
            // keeps auto-resolved receipts out — they're rail records, not rows.
            let next =
                fetched.items
                .filter { $0.status != .done }
                .sorted { a, b in
                    let ta = Self.receivedTS(a)
                    let tb = Self.receivedTS(b)
                    return ta != tb ? ta > tb : a.id > b.id
                }
            withMailPage(mode) { page in
                // Only touch the value when the list actually CHANGED: the poll
                // re-runs this, and an identical assignment re-renders 500 rows.
                if next != page.value { page.value = next }
                page.error = nil
            }
            mailLoadedAt[mode] = Date()
            // Pull the head rows' threads before any click, so an open renders
            // from cache. Runs on the launch warm too, not just on a visit.
            ThreadPrefetch.shared.warm(
                next.prefix(Self.mailWarmRows).map(\.thread_id), immediate: 5)
        } catch {
            guard e == epoch else { return }
            withMailPage(mode) { $0.error = errText(error, "load failed") }
        }
        withMailPage(mode) { $0.isLoading = false }
    }

    /// Mutate one page in place. Every call is its own access, which is the point:
    /// a local copy held across the fetch would write back over a `removeFromMail`
    /// that landed while the request was in flight.
    private func withMailPage(
        _ mode: MailMode, _ mutate: (inout Loadable<[AttentionUpdate]>) -> Void
    ) {
        var page = mailPages[mode] ?? .loading
        mutate(&page)
        mailPages[mode] = page
    }

    /// Epoch for "order received": surfaced_at approximates arrival, and items
    /// the triage loop hasn't surfaced yet are the newest mail, so they sort to
    /// the top. Ties (and the nil bucket) break on id, which is ingest order.
    private static func receivedTS(_ u: AttentionUpdate) -> Double {
        guard let s = u.surfaced_at else { return .greatestFiniteMagnitude }
        return Fmt.date(s)?.timeIntervalSince1970 ?? 0
    }

    // MARK: - the sent page

    /// The sent page's rows, in their OWN cache rather than a third `mailPages`
    /// entry. `SentItem` is a different fact: recipients instead of a sender,
    /// opens instead of a tier, and no status at all — forcing it into
    /// `AttentionUpdate` would mean inventing triage answers for mail nothing
    /// triaged. It lives in the store for the same reason the other two pages
    /// do: `@State` dies on navigate-away, and this list would refetch from
    /// blank on every visit.
    private var sentRows: Loadable<[SentItem]> = .loading
    /// Freshness + in-flight fetch, the sent half of `mailLoadedAt`/`mailRefreshes`.
    private var sentLoadedAt: Date?
    private var sentRefresh: Task<Void, Never>?

    var sentPage: Loadable<[SentItem]> { sentRows }

    /// Same TTL-plus-join shape as `refreshMail`, for the same reasons: a
    /// revisit inside the window is free, and a mount landing on the same tick
    /// as the poll costs one fetch rather than two.
    func refreshSent(force: Bool = false) async {
        if !force, let loadedAt = sentLoadedAt,
            Date().timeIntervalSince(loadedAt) < Self.mailTTL
        {
            return
        }
        if let running = sentRefresh {
            await running.value
            if !force { return }
        }
        let refresh = Task { await performSentRefresh() }
        sentRefresh = refresh
        await refresh.value
        if sentRefresh == refresh { sentRefresh = nil }
    }

    private func performSentRefresh() async {
        sentRows.isLoading = true
        do {
            let page = try await APIClient.shared.listSent(limit: Self.mailLimit)
            // THE SERVER'S ORDER IS THE ORDER (received_at DESC, id DESC): no
            // client-side sort, and no `resolvedIds` subtraction either — sent
            // mail is a record of what went out, and none of the triage verbs
            // can take a row off it.
            //
            // Assigned only on a real change, like the mail pages: the poll
            // re-runs this, and an identical assignment re-renders every row.
            if page.items != sentRows.value { sentRows.value = page.items }
            sentRows.error = nil
            sentLoadedAt = Date()
            ThreadPrefetch.shared.warm(
                page.items.prefix(Self.mailWarmRows).map(\.thread_id), immediate: 5)
        } catch {
            sentRows.error = errText(error, "load failed")
        }
        sentRows.isLoading = false
    }

    /// Preload the emails behind the records the columns show, so clicking one
    /// opens instantly.
    private func warmZoneThreads() {
        ThreadPrefetch.shared.warm(zones.banking.prefix(6).compactMap(\.thread_id), immediate: 2)
        ThreadPrefetch.shared.warm(zones.shipments.prefix(6).compactMap(\.thread_id), immediate: 2)
        // Receipts rotate at local midnight, so match the cache TTL to that.
        let midnight =
            Calendar.current.nextDate(
                after: Date(), matching: DateComponents(hour: 0, minute: 0),
                matchingPolicy: .nextTime) ?? Date().addingTimeInterval(3600)
        let ttl = max(60, midnight.timeIntervalSinceNow)
        for id in zones.receipts.compactMap(\.thread_id) {
            ThreadPrefetch.shared.prefetch(id, fresh: ttl)
        }
    }

    func openSide(_ view: SideView) { sideView = view }
    func closeSide() { sideView = .none }

    /// Open search. By default it RESUMES the last one; `seed` forces a fresh
    /// term (`f` on a row or in the reader, seeding `from:<address>`).
    func openSearch(seed: String? = nil) {
        // A seed matching what is ALREADY fetched keeps the session: the hits
        // on screen are authoritative for exactly that term, and nilling
        // `fetchedQuery` here would not refetch anyway (the panel's task is
        // keyed on `query`, which does not change) — it would just blank the
        // highlights and kill the cursor under live results.
        if let seed, seed != search.fetchedQuery {
            search.query = seed
            // Nil BOTH: the fetched term is what gates the refetch, and a cursor
            // from the old term would page a search that is no longer on screen.
            search.fetchedQuery = nil
            search.nextCursor = nil
        } else if let seed {
            // Restore the field text in case the bar was edited since the fetch.
            search.query = seed
        }
        // Always reopen as the strip: resuming the query is a convenience,
        // resuming a fullscreen takeover is a mode trap.
        search.expanded = false
        // And always reopen DISARMED. The hits and query persist, but a row
        // armed in some earlier session would silently repurpose bar-Enter
        // from "expand results" to "open that stale row".
        search.index = -1
        sideView = .search
    }

    func openCompose(_ state: ComposeState) {
        var next = state
        // Seeded HERE rather than by the caller, so every way into a composer
        // starts at the account's default and none can forget to.
        next.includeTracker = trackingDefault
        // The signature, below where typing starts. Only into an empty body — a
        // caller arriving with content composed that content, not a signature.
        if next.body.isEmpty { next.body = Prefs.shared.signatureSeed }
        compose = next
        DraftSaver.shared.noteOpened(.compose)
        Analytics.capture("compose_opened", ["kind": next.analyticsKind])
    }

    /// `c` / ⌘N — the new-message composer, RESTORED. The account holds at most
    /// one new-message draft by construction, so there is one row to look for and
    /// no picker to show.
    ///
    /// The pane opens BLANK and immediately, because the keypress has to feel
    /// instant; the saved draft lands into it a round-trip later, and only if
    /// nothing has been typed in the meantime (see `restoreNewMessage`).
    func openComposeNew() {
        // A composer already up is not something a repeated `c` — or the menu
        // item behind ⌘N — gets to blank out. Same rule as the inline reply.
        guard compose == nil else { return }
        openCompose(ComposeState())
        Task { await restoreNewMessage() }
    }

    /// `f` in the reader — pass THIS message on to someone else, in the pane
    /// composer. The pane rather than the reader's inline slot because a
    /// forward needs the two fields a reply does not have: a recipient (nobody
    /// is derivable from the original) and a subject.
    ///
    /// THE WHOLE MESSAGE comes in, and only its `id` goes out: the daemon
    /// quotes the original and re-attaches its files from that id alone. The
    /// rest is what the composer SHOWS — the subject it titles itself with and
    /// the quoted body it renders underneath the note (see
    /// `ComposeState.forwardedMessage`).
    ///
    /// `fallbackSubject` is the CONVERSATION's title, used only when the message
    /// carries no subject header of its own (a daemon too old to send one). The
    /// message's own header wins, because that is the header the daemon quotes:
    /// titling the forward from the thread would name one email while sending
    /// another, on exactly the threads where the two differ (a retitled reply, a
    /// list that stamps its own prefix). Resolved HERE rather than at the call
    /// site so the outgoing subject and the shown one cannot drift.
    ///
    /// Two deliberate absences:
    ///
    /// - No `restoreNewMessage`. That draft is the account's ONE plain new
    ///   message, keyed `reply_to_message_id == nil` server-side, and adopting
    ///   it here would drop a stranger's half-written mail into a forward of
    ///   something else — and take its subject line with it.
    /// - No autosave at all, for the mirror of the same reason: the drafts wire
    ///   cannot record what is being forwarded, so a saved forward would come
    ///   back as an ordinary new message with the original silently gone. See
    ///   `DraftSaver.save`.
    func openComposeForward(message: ClientMessage, fallbackSubject: String) {
        // A composer already up is not something a repeated `f` gets to blank
        // out — the same rule `openComposeNew` and the inline reply keep.
        guard compose == nil else { return }
        // REFUSE RATHER THAN HOPE. An outdated daemon does not reject a forward,
        // it ignores the field it does not know and answers 200 "sent" having
        // mailed the covering note alone — the original, its quote and its
        // files gone, with the sender told it went. That is a lie a toast cannot
        // walk back, so the composer never opens at all. See
        // `forwardingAvailable` for why nil counts as no.
        guard forwardingAvailable else {
            // Two states hide behind one refusal and only one is the daemon's
            // fault: before the first /client/stats lands there is no verdict
            // yet, and sending the user to a terminal to "update squelchd"
            // over a half-second race would be the wrong sentence. Both still
            // REFUSE — hope is what mails the note alone.
            pushToast(
                sitrep.stats == nil
                    ? "still connecting · try forward again in a moment"
                    : "this daemon cannot forward mail · update squelchd",
                .error)
            return
        }
        var state = ComposeState()
        state.forwardOfMessageId = message.id
        // THE MESSAGE'S OWN SUBJECT, with the caller's thread title as the
        // fallback — see the note above on why the message wins.
        let subject = message.subject ?? fallbackSubject
        // The subject is SENT, not derived: the daemon only titles a forward
        // itself when the field is absent, and the composer shows one, so what
        // is on screen has to be what goes out.
        state.subject = ComposeCopy.forwardSubject(subject)
        // Nil rather than "" when the original was untitled, so the quote's
        // header has one thing to test.
        let original = subject.trimmed
        state.forwardedSubject = original.isEmpty ? nil : original
        // The reader's copy of what the daemon will quote, for the composer to
        // show. Never sent — see `ComposeState.forwardedMessage`.
        state.forwardedMessage = message
        // Through `openCompose` so the tracker default and the signature are
        // seeded by the one place that seeds them, and the open is counted.
        openCompose(state)
    }

    /// Fill the just-opened new-message composer from its saved draft. Silent on
    /// failure: a restore that cannot happen leaves a blank composer, which is
    /// what the reader asked for anyway.
    private func restoreNewMessage() async {
        let e = epoch
        guard let rows = try? await APIClient.shared.listDrafts(),
            let draft = rows.first(where: { $0.reply_to_message_id == nil })
        else { return }
        // Another account's drafts are not this composer's to restore.
        guard e == epoch else { return }
        // Must still be THE blank composer we opened: closed, replaced by a
        // reply, or typed into in the meantime all mean the draft has missed its
        // window, and overwriting live keystrokes is worse than not restoring.
        // "Untouched" rather than empty: the composer opens holding the seeded
        // signature, and a seed nobody typed must not block the restore.
        //
        // A FORWARD is not a candidate either, and it would otherwise pass every
        // test above: `f` opens a composer with no reply parent, no draft id and
        // an untouched body, so a `c` fired a moment earlier could land its
        // restore in it and quietly re-address someone else's mail. The subject
        // this seeds would survive as the only visible trace.
        guard var next = compose, next.replyToMessageId == nil, next.forwardOfMessageId == nil,
            next.draftId == nil,
            next.to.isEmpty, next.subject.isEmpty, Prefs.shared.isBodyUntouched(next.body)
        else { return }
        next.to = draft.to
        next.subject = draft.subject
        next.body = draft.body
        next.draftId = draft.id
        compose = next
    }

    func closeCompose() {
        // An unsent draft outlives its composer: the closing values go out as one
        // fire-and-forget save. Before the slot is cleared — `flush` reads it.
        DraftSaver.shared.flush(.compose, compose)
        compose = nil
    }

    /// Open the reader's inline reply on a specific message. Never resets a
    /// composer that is already open — a draft is not something a repeated `r`
    /// gets to throw away.
    ///
    /// `replyAll` is the Enter key's answer to `r`'s: same composer, same
    /// ceremony, and the daemon does the widening. It is fixed here rather than
    /// toggled later, so what the header says is what the opening keystroke
    /// asked for.
    func openInlineReply(replyTo messageId: Int, replyAll: Bool = false) {
        guard inlineReply == nil else { return }
        inlineReply = ComposeState(
            replyToMessageId: messageId, body: Prefs.shared.signatureSeed,
            includeTracker: trackingDefault, replyAll: replyAll)
        DraftSaver.shared.noteOpened(.inlineReply)
        // Both ways in (the reader's `r` and the list's hand-off) come through
        // here, so the restore is wired once.
        Task { await restoreReply(messageId) }
    }

    /// Fill the just-opened inline composer from the draft keyed to this parent.
    /// Body only: a reply carries nothing else — the daemon derives the recipient
    /// and `Re: <subject>` from the parent.
    private func restoreReply(_ messageId: Int) async {
        let e = epoch
        guard let rows = try? await APIClient.shared.listDrafts(),
            let draft = rows.first(where: { $0.reply_to_message_id == messageId })
        else { return }
        // Another account's drafts are not this composer's to restore — and
        // `reply_to_message_id` is a per-daemon id, so one WOULD match.
        guard e == epoch else { return }
        // Untouched, not empty — the seeded signature must not block the restore.
        guard var next = inlineReply, next.replyToMessageId == messageId, next.draftId == nil,
            Prefs.shared.isBodyUntouched(next.body)
        else { return }
        next.body = draft.body
        next.draftId = draft.id
        inlineReply = next
    }

    func closeInlineReply() {
        DraftSaver.shared.flush(.inlineReply, inlineReply)
        inlineReply = nil
    }

    // MARK: - assistant relay

    /// Whether the ⌘K assistant can go through the daemon instead of a local
    /// key. Read off the sitrep stats — /client/stats is already the connect
    /// probe and SitrepPoller keeps it fresh, so no extra fetch exists for
    /// this. A self-host daemon never says `assistant_relay`, and nil means
    /// the switch is simply not offered (same posture as `trackingAvailable`).
    var relayAvailable: Bool { sitrep.stats?.assistant_relay == true }

    // MARK: - forwarding

    /// Whether this daemon understands `forward_of_message_id`. Read off the
    /// same sitrep stats `relayAvailable` is, for the same reason: /client/stats
    /// is already the connect probe and SitrepPoller keeps it fresh, so this
    /// capability costs no fetch of its own.
    ///
    /// STRICTLY `== true`. nil covers two states — stats not fetched yet, and a
    /// daemon too old to answer — and BOTH have to refuse. The failure this
    /// prevents is not an error dialog: an old daemon silently ignores the
    /// unknown field, sends the covering note by itself and returns 200 "sent",
    /// so hoping would mean mailing a stranger an empty "look at this" and
    /// telling the sender it worked. A send has no undo, so the only safe read
    /// of "we do not know" is no.
    var forwardingAvailable: Bool { sitrep.stats?.forwarding == true }

    // MARK: - read tracking

    /// Whether a composer may offer the pixel at all. A daemon with no tracking
    /// base_url ignores `include_tracker` on every send, so offering a switch
    /// there would be offering a lie.
    var trackingAvailable: Bool { tracking?.configured == true }

    /// What a freshly-opened composer starts at. Unconfigured wins over the
    /// stored preference — a default of "on" that cannot happen is still off.
    var trackingDefault: Bool { trackingAvailable && tracking?.default_enabled == true }

    /// Read the daemon's tracking answer. Silent on failure: `tracking` staying
    /// nil means the feature simply is not offered this session, which is the
    /// same thing an unconfigured daemon means.
    func refreshTrackingConfig() async {
        let e = epoch
        let next = try? await APIClient.shared.getTrackingConfig()
        guard e == epoch else { return }
        tracking = next
    }

    /// Persist the default for future composers. The response is the daemon's
    /// post-write view, so `configured` is refreshed alongside it.
    func setTrackingDefault(_ enabled: Bool) async throws {
        tracking = try await APIClient.shared.setTrackingDefault(enabled)
    }

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
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.undoTTL))
            self?.undos.removeAll { $0.id == entry.id }
        }
    }

    /// Undo the given (or most recent) queued action.
    func fireUndo(_ id: UUID? = nil) async {
        let e = epoch
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
            // The revert landed on the OLD account's daemon; nothing about it
            // is news the new one's surfaces should be told.
            guard e == epoch else { return }
            // The message is open again, so it must stop being filtered out of
            // every list that hides resolved ids — otherwise an undo looks like
            // it did nothing until the next poll. Harmless for undo kinds that
            // never resolved anything: the id simply is not in the set.
            resolvedIds.remove(entry.messageId)
            pushToast("undone: \(entry.label)", .info)
            Analytics.capture("undo_fired", ["kind": String(describing: entry.kind)])
            // The bands dropped the row optimistically and the next poll is up
            // to 10s out — pull now, or the undo reads as broken on the sitrep.
            await SitrepPoller.shared.pull()
        } catch {
            guard e == epoch else { return }
            pushToast("undo failed: \(entry.label)", .error)
        }
    }

    // MARK: - toasts

    @discardableResult
    func pushToast(_ text: String, _ tone: Toast.Tone = .info) -> UUID {
        let toast = Toast(text: text, tone: tone)
        toasts.append(toast)
        // Auto-dismiss so the stack cannot accumulate; undos own their own 5s
        // window and a click target.
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

    /// Ids resolved during this session. The bands in `sitrep` update in place,
    /// but a list holding its own snapshot (EmailsView reloads only on the 10s
    /// `lastRefresh`) would keep showing a resolved row for up to a full poll —
    /// which reads as the action not working. This is the shared record such a
    /// list filters against; the poll still supplies the truth.
    private(set) var resolvedIds: Set<Int> = []

    /// Optimistically pull a message id out of whatever band holds it and keep
    /// the selection valid (advance to the next row, else the previous).
    /// Returns a `restore` thunk that re-inserts the removed rows on failure.
    func removeFromBands(_ messageId: Int) -> () -> Void {
        let prev = sitrep
        resolvedIds.insert(messageId)
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
            // An undone resolve must be undone EVERYWHERE, or the row returns to
            // the bands while the mail page keeps filtering it out.
            self.resolvedIds.remove(messageId)
        }
    }

    /// Record a resolve that did NOT go through the bands: the reader can finish a
    /// thread it was not opened from a queue with, and that mail may still be
    /// sitting in a list behind it.
    func noteResolved(_ messageId: Int) {
        resolvedIds.insert(messageId)
        sitrep.standing.removeAll { $0.id == messageId }
        sitrep.new.removeAll { $0.id == messageId }
        sitrep.open.removeAll { $0.id == messageId }
    }

    /// The same, for every message from one sender: the client half of the
    /// server's `resolve_sender`, so unsubscribing or blocking empties that
    /// sender NOW rather than one poll later.
    ///
    /// Matched on the BARE ADDRESS, because the bands carry raw sender strings
    /// ("Name <addr>") while the caller has whatever the action was performed
    /// against. Everything the server resolved must be recorded here too — a row
    /// that stays in `sitrep` but is done on the server reappears on no refresh
    /// and vanishes on the next, which reads as a glitch.
    func noteSenderResolved(_ senderAddr: String) {
        let target = SenderID.address(senderAddr)
        guard !target.isEmpty else { return }
        let matches = { (u: AttentionUpdate) in SenderID.address(u.sender) == target }
        for u in sitrep.standing + sitrep.new + sitrep.open where matches(u) {
            resolvedIds.insert(u.id)
        }
        for mode in MailMode.allCases {
            for u in mailPage(mode).value ?? [] where matches(u) { resolvedIds.insert(u.id) }
        }
        sitrep.standing.removeAll(where: matches)
        sitrep.new.removeAll(where: matches)
        sitrep.open.removeAll(where: matches)
    }

    /// Optimistically pull a message out of the STANDING band ONLY — the one a tier
    /// correction empties. Standing is tier-defined server-side (`tier IN
    /// ('past_due','deadline') AND status != 'done'`) while `new`/`open` come from
    /// surfaced_at and status, which a tier correction leaves alone; a fresh
    /// deadline email sits in both, so pulling all three blanks it out of Attention
    /// until the refresh. No restore thunk — the write has already succeeded.
    func removeFromStanding(_ messageId: Int) {
        sitrep.standing.removeAll { $0.id == messageId }
    }
}
