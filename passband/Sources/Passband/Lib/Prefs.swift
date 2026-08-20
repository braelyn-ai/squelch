// Local UI preferences — UserDefaults-backed, app-wide, reactive. Per-device
// view state, not account state. Views read through @Observable, so a change in
// Settings re-renders every open consumer (e.g. the email frames) immediately.

import Foundation
import SwiftUI

/// The Settings sub-nav sections; the last-active one is restored on reopen.
enum SettingsSection: String, CaseIterable, Sendable {
    case general, mail, triage, assistant, privacy, account

    var label: String {
        switch self {
        case .general: "General"
        case .mail: "Mail"
        case .triage: "Triage"
        case .assistant: "Assistant"
        case .privacy: "Privacy"
        case .account: "Account"
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
        static let theme = "passband.pref.theme"
        static let threadStyle = "passband.pref.threadStyle"
        static let notificationSound = "passband.pref.notificationSound"
        static let userName = "passband.name"
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
            Key.threadStyle: ThreadStyle.classic.rawValue,
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
        _theme = ThemeChoice(rawValue: defaults.string(forKey: Key.theme) ?? "") ?? .system
        _threadStyle =
            ThreadStyle(rawValue: defaults.string(forKey: Key.threadStyle) ?? "") ?? .classic
        _notificationSound =
            NotificationSound(rawValue: defaults.string(forKey: Key.notificationSound) ?? "")
            ?? .system
        _telemetry =
            TelemetryLevel(rawValue: defaults.string(forKey: Key.telemetry) ?? "") ?? .full
        _userName = defaults.string(forKey: Key.userName) ?? ""
        _signature = defaults.string(forKey: Key.signature) ?? ""
        _assistantModel =
            AssistantModel(rawValue: defaults.string(forKey: Key.assistantModel) ?? "")
            ?? .haiku
        _assistantTransport =
            AssistantTransport(rawValue: defaults.string(forKey: Key.assistantTransport) ?? "")
            ?? .relay
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

    /// How threads are drawn where nothing else has been said. A thread the
    /// reader has switched by hand keeps its own answer (ThreadStyleLedger) and
    /// ignores this.
    private var _threadStyle: ThreadStyle
    var threadStyle: ThreadStyle {
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
    private var _userName: String
    var userName: String {
        get { _userName }
        set {
            _userName = newValue.trimmingCharacters(in: .whitespaces)
            defaults.set(_userName, forKey: Key.userName)
        }
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
enum AssistantModel: String, CaseIterable, Sendable {
    case haiku = "claude-haiku-4-5"
    case opus = "claude-opus-4-8"

    var shortLabel: String {
        switch self {
        case .haiku: "Haiku"
        case .opus: "Opus"
        }
    }

    var label: String {
        switch self {
        case .haiku: "Haiku 4.5 — fast & cheap (recommended)"
        case .opus: "Opus 4.8 — smartest, pricier"
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
