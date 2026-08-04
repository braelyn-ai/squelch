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
/// the default. `minimal` keeps sessions and screen views but drops the action
/// verbs; `none` sends nothing at all.
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
        static let theme = "passband.pref.theme"
        static let userName = "passband.name"
        static let assistantModel = "passband.assistant.model"
        static let telemetry = TelemetryLevel.prefKey
    }

    private init() {
        defaults.register(defaults: [
            Key.loadRemoteImages: true,
            Key.settingsSection: SettingsSection.general.rawValue,
            Key.rankWeight: defaultRankWeight,
            Key.developerMode: false,
            Key.theme: ThemeChoice.system.rawValue,
            Key.telemetry: TelemetryLevel.full.rawValue,
        ])
        _loadRemoteImages = defaults.bool(forKey: Key.loadRemoteImages)
        _settingsSection =
            SettingsSection(rawValue: defaults.string(forKey: Key.settingsSection) ?? "")
            ?? .general
        _rankWeight = defaults.double(forKey: Key.rankWeight)
        _developerMode = defaults.bool(forKey: Key.developerMode)
        _theme = ThemeChoice(rawValue: defaults.string(forKey: Key.theme) ?? "") ?? .system
        _telemetry =
            TelemetryLevel(rawValue: defaults.string(forKey: Key.telemetry) ?? "") ?? .full
        _userName = defaults.string(forKey: Key.userName) ?? ""
        _assistantModel =
            AssistantModel(rawValue: defaults.string(forKey: Key.assistantModel) ?? "")
            ?? .haiku
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

    private var _assistantModel: AssistantModel
    var assistantModel: AssistantModel {
        get { _assistantModel }
        set {
            _assistantModel = newValue
            defaults.set(newValue.rawValue, forKey: Key.assistantModel)
        }
    }

    /// Flip light <-> dark. `system` resolves against the current appearance
    /// first so the toggle always visibly changes something.
    func flipTheme() {
        switch theme {
        case .light: theme = .dark
        case .dark: theme = .light
        case .system:
            let isDark = NSApp?.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            theme = isDark ? .light : .dark
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
