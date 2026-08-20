// WHAT A WINDOWED ZONE SHOWS: the last 24 hours, or everything since the user
// last SAW the zone — whichever reaches further back. Without a window the
// banking card held its latest 8 rows forever, some a week stale (issue #82).
//
// The seen-stamp belongs to the SURFACE, not the app. The zone reports its own
// lifecycle (the Mac mounts it on the sitrep page, the phone on the Quick Look
// tab — one zone, so both platforms get the right signal for free), and the
// stamp advances only while one is on screen: at appearance, at disappearance,
// and when the app resigns active or terminates WITH one showing. A session
// spent entirely in Mail clears nothing, because nothing was seen.
//
// Per-device on purpose (Prefs' charter: view state, not account state), and the
// cutoff is recomputed only when the app becomes active — never mid-session, so
// a row on screen cannot evaporate while the user is looking at it. A crash or
// force-quit loses at most one stamp update, which errs toward showing MORE, the
// safe direction.

import Foundation
import Observation

@MainActor
@Observable
final class SitrepWindow {
    static let shared = SitrepWindow()

    /// The floor: rows younger than this always show, however recently the zone
    /// was seen.
    nonisolated static let floorInterval: TimeInterval = 24 * 3600

    private nonisolated static let stampKey = "passband.sitrep.lastSeen"

    /// Rows at or after this instant are in the window. Frozen between
    /// activations — see the header.
    private(set) var cutoff: Date

    /// How many windowed zones are on screen right now. A count, not a flag:
    /// the sitrep lays the zone out differently per width and a layout switch
    /// can overlap the two mounts.
    private var visibleSurfaces = 0

    private let defaults = UserDefaults.standard

    /// Held only so the closures stay registered for the singleton's (process)
    /// lifetime; never removed.
    private var observers: [NSObjectProtocol] = []

    private init() {
        // First access may come after the launch activation already fired, so
        // the initial cutoff is computed here rather than waiting for the next
        // notification.
        cutoff = Self.cutoff(now: Date(), lastSeen: Self.storedStamp(defaults))
        observers.append(
            NotificationCenter.default.addObserver(
                forName: Platform.didBecomeActiveNotification, object: nil, queue: .main
            ) { _ in
                Task { @MainActor in SitrepWindow.shared.recompute() }
            })
        // Resigning active is a seen-moment only when a zone is showing:
        // cmd-tabbing away from the Mail tab says nothing about banking.
        observers.append(
            NotificationCenter.default.addObserver(
                forName: Platform.didResignActiveNotification, object: nil, queue: .main
            ) { _ in
                Task { @MainActor in SitrepWindow.shared.stampIfSurfaceShowing() }
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
                    // ONLY when the app dies frontmost with a zone showing. A
                    // logout or shutdown also terminates an app that has sat
                    // backgrounded for days with the sitrep still mounted, and
                    // an unconditional stamp would swallow everything since the
                    // last true sighting.
                    SitrepWindow.shared.stampIfSurfaceShowing(requireActive: true)
                }
            })
    }

    // MARK: - the seen-signal

    /// A windowed zone landed on screen. The rows it is about to render are
    /// being seen right now.
    func surfaceAppeared() {
        visibleSurfaces += 1
        stamp()
    }

    /// The zone left the screen — the last moment it was seen, which is the
    /// truthful stamp for a sitrep that sat open all afternoon.
    func surfaceDisappeared() {
        stamp()
        visibleSurfaces = max(0, visibleSurfaces - 1)
    }

    /// Whether a row's timestamp is in the window. `nil` (unparseable) is IN:
    /// hiding a row over a formatting quirk is the wrong failure.
    func admits(_ date: Date?) -> Bool {
        Self.admits(date, cutoff: cutoff)
    }

    // MARK: - the rule, pure for tests

    /// The earlier of (now − 24h) and the last-seen stamp; no stamp (first
    /// run) leaves the floor alone, which is also what clears a backlog that
    /// predates the window existing.
    nonisolated static func cutoff(now: Date, lastSeen: Date?) -> Date {
        let floor = now.addingTimeInterval(-floorInterval)
        guard let lastSeen else { return floor }
        return min(floor, lastSeen)
    }

    nonisolated static func admits(_ date: Date?, cutoff: Date) -> Bool {
        guard let date else { return true }
        return date >= cutoff
    }

    // MARK: - lifecycle plumbing

    private func recompute() {
        cutoff = Self.cutoff(now: Date(), lastSeen: Self.storedStamp(defaults))
    }

    private func stampIfSurfaceShowing(requireActive: Bool = false) {
        guard visibleSurfaces > 0 else { return }
        if requireActive, !Platform.isAppActive { return }
        stamp()
    }

    private func stamp() {
        defaults.set(Date().timeIntervalSince1970, forKey: Self.stampKey)
    }

    private static func storedStamp(_ defaults: UserDefaults) -> Date? {
        let t = defaults.double(forKey: stampKey)
        return t > 0 ? Date(timeIntervalSince1970: t) : nil
    }
}
