// WHAT A WINDOWED ZONE SHOWS: the last 24 hours, or everything since the user
// last had the app open — whichever reaches further back. Without a window the
// banking card held its latest 8 rows forever, some a week stale (issue #82).
//
// "Last open" is PER-DEVICE on purpose (Prefs' charter: view state, not account
// state): the stamp is written when the app resigns active, and the cutoff is
// recomputed only when it becomes active again — never mid-session, so a row on
// screen cannot evaporate while the user is looking at it. A crash or force-quit
// loses one stamp update, which errs toward showing MORE, the safe direction.

import Foundation
import Observation

@MainActor
@Observable
final class SitrepWindow {
    static let shared = SitrepWindow()

    /// The floor: rows younger than this always show, however recently the app
    /// was open.
    nonisolated static let floorInterval: TimeInterval = 24 * 3600

    private nonisolated static let stampKey = "passband.sitrep.lastActive"

    /// Rows at or after this instant are in the window. Frozen between
    /// activations — see the header.
    private(set) var cutoff: Date

    private let defaults = UserDefaults.standard

    /// Held only so the closures stay registered for the singleton's (process)
    /// lifetime; never removed.
    private var observers: [NSObjectProtocol] = []

    private init() {
        // First access may come after the launch activation already fired, so
        // the initial cutoff is computed here rather than waiting for the next
        // notification.
        cutoff = Self.cutoff(now: Date(), lastActive: Self.storedStamp(defaults))
        observers.append(
            NotificationCenter.default.addObserver(
                forName: Platform.didBecomeActiveNotification, object: nil, queue: .main
            ) { _ in
                Task { @MainActor in SitrepWindow.shared.recompute() }
            })
        observers.append(
            NotificationCenter.default.addObserver(
                forName: Platform.didResignActiveNotification, object: nil, queue: .main
            ) { _ in
                Task { @MainActor in SitrepWindow.shared.stamp() }
            })
        // ⌘Q from the foreground can terminate without ever resigning active,
        // and losing the whole session's stamp would replay the week on the
        // next launch.
        observers.append(
            NotificationCenter.default.addObserver(
                forName: Platform.willTerminateNotification, object: nil, queue: .main
            ) { _ in
                // Directly, not via a Task: the process is on its way out and a
                // hop to the next runloop turn never runs. willTerminate is
                // delivered on the main thread, so the assumeIsolated is honest.
                MainActor.assumeIsolated {
                    // ONLY when the app dies frontmost. A logout or shutdown
                    // also terminates an app that has sat inactive for days,
                    // and an unconditional stamp would swallow everything since
                    // the resign stamp — mail the user never had on screen.
                    // When inactive, the resign stamp already tells the truth.
                    guard Platform.isAppActive else { return }
                    UserDefaults.standard.set(
                        Date().timeIntervalSince1970, forKey: SitrepWindow.stampKey)
                }
            })
    }

    /// Whether a row's timestamp is in the window. `nil` (unparseable) is IN:
    /// hiding a row over a formatting quirk is the wrong failure.
    func admits(_ date: Date?) -> Bool {
        Self.admits(date, cutoff: cutoff)
    }

    // MARK: - the rule, pure for tests

    /// The earlier of (now − 24h) and the last-active stamp; no stamp (first
    /// run) leaves the floor alone, which is also what clears a backlog that
    /// predates the window existing.
    nonisolated static func cutoff(now: Date, lastActive: Date?) -> Date {
        let floor = now.addingTimeInterval(-floorInterval)
        guard let lastActive else { return floor }
        return min(floor, lastActive)
    }

    nonisolated static func admits(_ date: Date?, cutoff: Date) -> Bool {
        guard let date else { return true }
        return date >= cutoff
    }

    // MARK: - lifecycle plumbing

    private func recompute() {
        cutoff = Self.cutoff(now: Date(), lastActive: Self.storedStamp(defaults))
    }

    private func stamp() {
        defaults.set(Date().timeIntervalSince1970, forKey: Self.stampKey)
    }

    private static func storedStamp(_ defaults: UserDefaults) -> Date? {
        let t = defaults.double(forKey: stampKey)
        return t > 0 ? Date(timeIntervalSince1970: t) : nil
    }
}
