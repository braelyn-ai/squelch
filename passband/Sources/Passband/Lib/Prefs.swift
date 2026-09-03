// Local UI preferences — UserDefaults-backed, app-wide, reactive. Per-device
// view state, not account state. Views read through @Observable, so a change in
// Settings re-renders every open consumer (e.g. the email frames) immediately.

import Foundation
import SwiftUI

// `SettingsSection` — the sub-nav's own enum, which the stored section below is
// typed as — lives in Lib/SettingsSearch.swift, beside the card index that
// files every setting under one. The taxonomy is one thing; this file only
// remembers which of it you were last looking at.

/// HOW SEARCH ORDERS ITS RESULTS, and the reader's standing answer to it.
/// Sent on every search as `sort=`; the daemon ranks accordingly.
///
/// A preference rather than a control on the search bar itself. Somebody who
/// wants one of these wants it every time, and a per-search picker is a
/// decision re-asked on every keystroke. `recent` is the default because mail
/// is not a document corpus: the thread you are looking for is usually the one
/// that moved.
enum SearchSortChoice: String, CaseIterable, Sendable {
    case recent, bestMatch = "best_match"

    var label: String {
        switch self {
        case .recent: "Recent"
        case .bestMatch: "Best match"
        }
    }

    /// What the setting actually promises, in one line, under the picker.
    var blurb: String {
        switch self {
        case .recent: "Newer mail ranks higher when matches are close."
        case .bestMatch: "Rank on the words alone, however old the mail is."
        }
    }
}

/// How much developer telemetry (PostHog) leaves the app. Opt-out: `full` is
/// the default. `minimal` keeps sessions, screen views, and the anonymous
/// counter events (sends, triage volume, corrections, connection health — see
/// `Analytics.minimalEvents`) but drops the remaining action verbs; `none`
/// sends nothing at all. No level carries content at any time: every string
/// that leaves is in `Analytics.allowedStrings`.
///
/// The key is public because Analytics reads the raw value straight from
/// UserDefaults — capture can fire off the main actor, where Prefs lives.
enum TelemetryLevel: String, CaseIterable, Sendable {
    case full, minimal, none

    static let prefKey = "passband.pref.telemetry"

    var label: String {
        switch self {
        case .full: "Full"
        case .minimal: "Minimal"
        case .none: "None"
        }
    }
}

/// The banner chime. `system` is the macOS default alert; the rest are bundled
/// CAFs named for what they are on the air. Referenced by file name because
/// UNNotificationSound only resolves names, never paths — Notifier installs the
/// files where the system looks (see `Notifier.installSounds`).
enum NotificationSound: String, CaseIterable, Sendable {
    case system, squelch, `static`, morse, carrier

    var label: String {
        switch self {
        case .system: "Default"
        case .squelch: "Squelch"
        case .static: "Static"
        case .morse: "Morse"
        case .carrier: "Carrier"
        }
    }

    /// Bundle resource name under Resources/Sounds, nil for the system sound.
    var resourceName: String? {
        self == .system ? nil : label
    }

    /// The file name in ~/Library/Sounds — prefixed, because that folder is
    /// shared by every app on the machine and shows up in system sound pickers.
    var installedFileName: String? {
        resourceName.map { "Passband \($0).caf" }
    }
}

/// Two palettes selected explicitly; `system` follows the OS and is the default.
enum ThemeChoice: String, CaseIterable, Sendable {
    case system, light, dark

    var label: String {
        switch self {
        case .system: "Auto"
        case .light: "Light"
        case .dark: "Dark"
        }
    }

    var colorScheme: ColorScheme? {
        switch self {
        case .system: nil
        case .light: .light
        case .dark: .dark
        }
    }
}

@MainActor
@Observable
final class Prefs {
    static let shared = Prefs()

    private let defaults = UserDefaults.standard

    private enum Key {
        static let loadRemoteImages = "passband.pref.loadRemoteImages"
        static let settingsSection = "passband.pref.settingsSection"
        static let rankWeight = "passband.pref.rankWeight"
        static let developerMode = "passband.pref.developerMode"
        static let tourCompleted = "passband.pref.tourCompleted"
        static let lastSeenReleaseNotes = "passband.pref.lastSeenReleaseNotes"
        static let theme = "passband.pref.theme"
        static let threadStyle = "passband.pref.threadStyle"
        static let searchSort = "passband.pref.searchSort"
        static let notificationSound = "passband.pref.notificationSound"
        static let userName = "passband.name"
        static let nameChosen = "passband.name.chosen"
        static let signature = "passband.pref.signature"
        static let assistantModel = "passband.assistant.model"
        static let assistantTransport = "passband.assistant.transport"
        static let telemetry = TelemetryLevel.prefKey
    }

    private init() {
        defaults.register(defaults: [
            Key.loadRemoteImages: true,
            Key.settingsSection: SettingsSection.general.rawValue,
            Key.rankWeight: defaultRankWeight,
            Key.developerMode: false,
            Key.tourCompleted: false,
            Key.theme: ThemeChoice.system.rawValue,
            Key.threadStyle: ThreadStyleDefault.auto.rawValue,
            Key.searchSort: SearchSortChoice.recent.rawValue,
            Key.notificationSound: NotificationSound.system.rawValue,
            Key.telemetry: TelemetryLevel.full.rawValue,
        ])
        _loadRemoteImages = defaults.bool(forKey: Key.loadRemoteImages)
        _settingsSection =
            SettingsSection(rawValue: defaults.string(forKey: Key.settingsSection) ?? "")
            ?? .general
        _rankWeight = defaults.double(forKey: Key.rankWeight)
        _developerMode = defaults.bool(forKey: Key.developerMode)
        _tourCompleted = defaults.bool(forKey: Key.tourCompleted)
        _lastSeenReleaseNotes = defaults.string(forKey: Key.lastSeenReleaseNotes)
        _theme = ThemeChoice(rawValue: defaults.string(forKey: Key.theme) ?? "") ?? .system
        _threadStyle =
            ThreadStyleDefault(rawValue: defaults.string(forKey: Key.threadStyle) ?? "") ?? .auto
        _searchSort =
            SearchSortChoice(rawValue: defaults.string(forKey: Key.searchSort) ?? "") ?? .recent
        _notificationSound =
            NotificationSound(rawValue: defaults.string(forKey: Key.notificationSound) ?? "")
            ?? .system
        _telemetry =
            TelemetryLevel(rawValue: defaults.string(forKey: Key.telemetry) ?? "") ?? .full
        let storedName = defaults.string(forKey: Key.userName) ?? ""
        _userName = storedName
        // A NAME TYPED BEFORE THE FLAG EXISTED is a chosen name. Deliberately
        // NOT in `register(defaults:)` above: a registered default reads exactly
        // like a stored one, and this needs to see absence — the same reason
        // `tourCompleted` is written the way it is.
        _nameChosen = defaults.object(forKey: Key.nameChosen) as? Bool ?? !storedName.isEmpty
        _signature = defaults.string(forKey: Key.signature) ?? ""
        _assistantModel =
            AssistantModel.migrating(rawValue: defaults.string(forKey: Key.assistantModel) ?? "")
            ?? .haiku
        _assistantTransport =
            AssistantTransport(rawValue: defaults.string(forKey: Key.assistantTransport) ?? "")
            ?? .relay
        // LAST, because it writes: the verdict above has to be PINNED on the
        // launch that reaches it, not re-derived on the next one. A seed lands
        // in the name key with no flag beside it, so a re-derivation would read
        // the app's own guess as the human's answer and quietly retire the
        // pencil that exists to correct it. Runs once per install.
        if defaults.object(forKey: Key.nameChosen) == nil {
            defaults.set(_nameChosen, forKey: Key.nameChosen)
        }
    }

    /// Load remote (network) images in email HTML automatically. When false,
    /// each message shows a per-email "load images" opt-in instead.
    private var _loadRemoteImages: Bool
    var loadRemoteImages: Bool {
        get { _loadRemoteImages }
        set {
            _loadRemoteImages = newValue
            defaults.set(newValue, forKey: Key.loadRemoteImages)
        }
    }

    private var _settingsSection: SettingsSection
    var settingsSection: SettingsSection {
        get { _settingsSection }
        set {
            _settingsSection = newValue
            defaults.set(newValue.rawValue, forKey: Key.settingsSection)
        }
    }

    /// Blend weight (0..1) for the Sitrep "For your eyes" ranking: the urgency
    /// (time) share of the score; the remainder is severity.
    private var _rankWeight: Double
    var rankWeight: Double {
        get { _rankWeight }
        set {
            _rankWeight = newValue
            defaults.set(newValue, forKey: Key.rankWeight)
        }
    }

    /// How search orders its results. Read at FETCH time rather than captured
    /// when a panel opens, so changing it in Settings is in force on the very
    /// next search without anything having to observe anything.
    private var _searchSort: SearchSortChoice
    var searchSort: SearchSortChoice {
        get { _searchSort }
        set {
            _searchSort = newValue
            defaults.set(newValue.rawValue, forKey: Key.searchSort)
        }
    }

    /// Developer mode: exposes re-triage affordances (masthead + thread viewer).
    private var _developerMode: Bool
    var developerMode: Bool {
        get { _developerMode }
        set {
            _developerMode = newValue
            defaults.set(newValue, forKey: Key.developerMode)
        }
    }

    /// Whether the first-run tour has been seen. Set by finishing OR skipping
    /// it: both are answers, and re-asking somebody who said no is worse than
    /// never asking. Settings' replay ignores it.
    private var _tourCompleted: Bool
    var tourCompleted: Bool {
        get { _tourCompleted }
        set {
            _tourCompleted = newValue
            defaults.set(newValue, forKey: Key.tourCompleted)
        }
    }

    /// The newest release whose notes this install has been shown, or nil for
    /// an install that has never seen the card. NOT registered with a default:
    /// absence is a real state (see WhatsNew), and a registered "" would be
    /// indistinguishable from a stamp somebody wrote.
    private var _lastSeenReleaseNotes: String?
    var lastSeenReleaseNotes: String? {
        get { _lastSeenReleaseNotes }
        set {
            _lastSeenReleaseNotes = newValue
            defaults.set(newValue, forKey: Key.lastSeenReleaseNotes)
        }
    }

    /// Developer telemetry level. Analytics gates on the UserDefaults value
    /// this writes, so a change here takes effect on the very next event.
    private var _telemetry: TelemetryLevel
    var telemetry: TelemetryLevel {
        get { _telemetry }
        set {
            _telemetry = newValue
            defaults.set(newValue.rawValue, forKey: Key.telemetry)
        }
    }

    private var _theme: ThemeChoice
    var theme: ThemeChoice {
        get { _theme }
        set {
            _theme = newValue
            defaults.set(newValue.rawValue, forKey: Key.theme)
        }
    }

    /// How threads are drawn where nothing else has been said — including
    /// `auto`, which is an instruction to read the thread rather than a style
    /// (see ThreadStyle.automatic). A thread the reader has switched by hand
    /// keeps its own answer (ThreadStyleLedger) and ignores this.
    private var _threadStyle: ThreadStyleDefault
    var threadStyle: ThreadStyleDefault {
        get { _threadStyle }
        set {
            _threadStyle = newValue
            defaults.set(newValue.rawValue, forKey: Key.threadStyle)
        }
    }

    private var _notificationSound: NotificationSound
    var notificationSound: NotificationSound {
        get { _notificationSound }
        set {
            _notificationSound = newValue
            defaults.set(newValue.rawValue, forKey: Key.notificationSound)
        }
    }

    /// The human's display name, for the Sitrep greeting only. Client-side —
    /// the human door has no such field.
    ///
    /// SETTING THIS TO A NAME IS A CHOICE, and the setter records it as one:
    /// whoever assigns here is a human at a text field (the Settings row, or
    /// the greeting's own inline editor), which is what retires the pencil
    /// beside the greeting for good. The seed below writes the stored value
    /// directly, precisely so a guess the app made about you never counts as
    /// your answer.
    ///
    /// EMPTY IS NOT A CHOICE, and that is not pedantry: the Settings row binds
    /// straight to this pref, so it lands here on every keystroke — including
    /// the empty string in the middle of select-all-and-retype. Counting that
    /// as an answer would strand anyone who got distracted mid-edit with a bare
    /// greeting AND no pencil, the one state with no way back but Settings.
    private var _userName: String
    var userName: String {
        get { _userName }
        set {
            _userName = newValue.trimmingCharacters(in: .whitespaces)
            defaults.set(_userName, forKey: Key.userName)
            if !_nameChosen, !_userName.isEmpty {
                _nameChosen = true
                defaults.set(true, forKey: Key.nameChosen)
            }
        }
    }

    /// Whether the human has ever named themselves. False means the greeting is
    /// running on a seeded guess (or on nothing), which is the one state that
    /// offers an edit affordance — a permanent pencil on a greeting is chrome,
    /// and the name lives in Settings like every other pref.
    private var _nameChosen: Bool
    var nameChosen: Bool { _nameChosen }

    /// First-run display name: the mailbox's local part, the first time a
    /// daemon tells us which mailbox this is.
    ///
    /// Fires AT MOST ONCE per install. The guard is the stored key's absence,
    /// not emptiness, so clearing the name in Settings stays cleared and an
    /// account switch never re-guesses over a name you already have. A nil
    /// address (daemon too old to say) and anything that is not address-shaped
    /// both mean the same thing here: seed nothing, greet the way we always did.
    ///
    /// SPLIT WOULD BE WRONG. `split(separator:)` omits empty subsequences, so
    /// "@gmail.com" hands back the DOMAIN and a bare "nobody" hands back
    /// itself — and self-host never checks the shape of `account_email` (the
    /// config only requires it non-empty), so both are reachable. The guess is
    /// one-shot and keyed on the key's absence, so a wrong one is permanent:
    /// this takes the text before the first "@" and insists there was one.
    func seedUserName(fromEmail email: String?) {
        guard !_nameChosen, defaults.object(forKey: Key.userName) == nil else { return }
        guard let email, let at = email.firstIndex(of: "@") else { return }
        let local = String(email[..<at])
        guard !local.isEmpty else { return }
        _userName = local
        defaults.set(local, forKey: Key.userName)
    }

    /// The email signature, as markdown — same dialect as the compose body.
    /// Stored raw (no trimming in the setter: it is bound to a live editor, and
    /// normalizing under the caret would fight every trailing newline typed).
    private var _signature: String
    var signature: String {
        get { _signature }
        set {
            _signature = newValue
            defaults.set(newValue, forKey: Key.signature)
        }
    }

    /// What a fresh compose body starts as: empty when no signature is stored,
    /// otherwise the signature two newlines down, so typing starts above it.
    var signatureSeed: String {
        let sig = signature.trimmed
        return sig.isEmpty ? "" : "\n\n" + sig
    }

    /// True while a compose body holds nothing the user typed: blank, or exactly
    /// the seeded signature block. The draft machinery treats such a body as
    /// empty — it must neither block a restore nor be worth saving as a draft.
    func isBodyUntouched(_ body: String) -> Bool {
        body.trimmed.isEmpty || body == signatureSeed
    }

    private var _assistantModel: AssistantModel
    var assistantModel: AssistantModel {
        get { _assistantModel }
        set {
            _assistantModel = newValue
            defaults.set(newValue.rawValue, forKey: Key.assistantModel)
        }
    }

    /// WHAT THE USER PICKED, not what runs. The effective transport is relay
    /// only when this says relay AND the daemon advertises `assistant_relay`
    /// (see `AssistantSession.run`); on a self-host daemon that never does, the
    /// relay default is inert and every ask is BYOK regardless.
    private var _assistantTransport: AssistantTransport
    var assistantTransport: AssistantTransport {
        get { _assistantTransport }
        set {
            _assistantTransport = newValue
            defaults.set(newValue.rawValue, forKey: Key.assistantTransport)
        }
    }

    /// Flip light <-> dark. `system` resolves against the current appearance
    /// first so the toggle always visibly changes something.
    func flipTheme() {
        switch theme {
        case .light: theme = .dark
        case .dark: theme = .light
        case .system:
            theme = Platform.isDarkAppearance ? .light : .dark
        }
    }
}

/// Model options offered in Settings. Cheap-first: Haiku is the default.
///
/// These raw values are BARE ids, which is correct for BYOK (Anthropic's own
/// endpoint rejects a provider prefix) and is deliberately not the whole story
/// on hosted: the daemon's relay qualifies the id before it reaches the
/// gateway, because only the daemon knows which endpoint is downstream. See
/// `qualify_model_for_gateway` in squelch-api.
///
/// A raw value here must also appear in the hosted allow-lists
/// (`SQUELCH_CONTROL_ASSISTANT_MODELS` and the gateway's provider key, which
/// `squelch-control llm sync` writes) or a hosted turn 403s. `claude-opus-4-8`
/// was offered here long after it had fallen out of both, so the Opus option
/// could not have worked on hosted even once the prefix was right.
enum AssistantModel: String, CaseIterable, Sendable {
    case haiku = "claude-haiku-4-5"
    case opus = "claude-opus-5"

    /// Raw values that used to name a still-offered option, mapped forward.
    /// Without this, `AssistantModel(rawValue:)` fails for anyone whose stored
    /// preference predates the rename and they are silently moved to Haiku,
    /// which is a quieter kind of wrong than an error.
    static func migrating(rawValue: String) -> AssistantModel? {
        switch rawValue {
        case "claude-opus-4-8": .opus
        default: AssistantModel(rawValue: rawValue)
        }
    }

    var shortLabel: String {
        switch self {
        case .haiku: "Haiku"
        case .opus: "Opus"
        }
    }

    var label: String {
        switch self {
        case .haiku: "Haiku 4.5 — fast & cheap (recommended)"
        case .opus: "Opus 5 — smartest, pricier"
        }
    }

    /// Rough $/1M rates, for the local cost estimate on the Usage page.
    var rates: (input: Double, output: Double) {
        switch self {
        case .haiku: (1, 5)
        case .opus: (5, 25)
        }
    }
}

/// Where a ⌘K ask travels: through the user's own daemon (the hosted relay) or
/// straight to the provider with their own key. `relay` is the default because
/// on hosted the relay IS the product, and BYOK users flip the switch once and
/// it sticks. Harmless on self-host: a daemon that never advertises the
/// capability never runs relay whatever this says.
enum AssistantTransport: String, CaseIterable, Sendable {
    case relay = "relay"
    case byok = "byok"

    var label: String {
        switch self {
        case .relay: "Passband relay"
        case .byok: "My own key"
        }
    }
}
