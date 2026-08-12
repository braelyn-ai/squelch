// The first-run tour's state machine: seven steps over the real dashboard,
// skippable from every one of them and replayable from Settings.
//
// IT OWNS NO MAIL. The practice email is this file's own `demoUpdate` (id -1),
// rendered inside the tour card and NEVER injected into `store.sitrep` — the
// poller rebuilds those bands wholesale every 10s, so anything parked there
// would blink out mid-lesson, and every verb in the app would try to POST it.
// For the same reason the rule the tour teaches is never saved and the `e` it
// teaches never leaves the machine (see TourOverlay).

import Foundation
import Observation
import SwiftUI

/// The seven steps, in order. `Int`-backed because the progress footer counts
/// them and the analytics events report the step the user left on.
enum TourStep: Int, CaseIterable, Sendable {
    case welcome, records, newsletters, philosophy, demoRule, demoDone, wrap

    /// 1-based position for the "N / 7" footer.
    var position: Int { rawValue + 1 }
}

/// A dashboard region a coach mark can ring. The sitrep tags its real zones
/// with these (see `tourTarget`), so the ring lands on whatever the current
/// layout actually put on screen rather than on a hardcoded rectangle.
enum TourTarget: Hashable, Sendable {
    case records, newsletters, eyes
}

/// How far through the interactive demo the user is. Drives the copy on the two
/// demo steps and nothing else.
enum TourDemoPhase: Sendable {
    case waiting, ruleSaved, done, undone
}

@MainActor
@Observable
final class TourController {
    /// Env escape hatch for development and screenshots: takes the tour past the
    /// completed flag, not past the "must be on the sitrep, must have synced"
    /// part of the gate.
    static let forced = ProcessInfo.processInfo.environment["PASSBAND_FORCE_TOUR"] == "1"

    private(set) var active = false
    private(set) var step: TourStep = .welcome
    private(set) var demoPhase: TourDemoPhase = .waiting

    /// Measured region rects, in the tour's named coordinate space. Written
    /// only while the tour is up — see `report`.
    private(set) var targets: [TourTarget: CGRect] = [:]

    /// The same rects, kept fresh at ALL times and observed by nobody. The
    /// sitrep measures its zones whether or not a tour is running, and an
    /// observable write per layout pass would invalidate readers on every
    /// window resize forever. This shadow copy is what makes the first coach
    /// mark land: the tour starts long after the zones stopped moving, so
    /// there is no later layout change to learn their positions from.
    @ObservationIgnored private var measured: [TourTarget: CGRect] = [:]

    /// Set by skip/finish so a reconnect (which resets `lastRefresh`, the
    /// trigger) cannot start the tour a second time in one session.
    private var dismissedThisSession = false

    /// The practice email. `id: -1` is deliberately not a real message id: the
    /// tour's verbs are client-only, and anything that leaked to the API would
    /// 404 loudly rather than act on somebody's actual mail.
    static let demoUpdate = AttentionUpdate(
        id: -1,
        thread_id: "tour-demo",
        tier: .noise,
        importance: 12,
        sender: "legal@brightly.example",
        one_line: "We have updated our Terms of Service",
        reason: "routine policy notice, no action required",
        deadline: nil,
        matched_rule: nil,
        field_reasons: nil,
        has_attachments: false,
        from_name: "Brightly",
        status: .new,
        surfaced_at: ISO8601DateFormatter().string(from: Date()),
        resolved_at: nil)

    /// Blur belongs to the three steps that are MODALS. The coach marks point
    /// at real regions, so blurring the board would hide the thing being
    /// pointed at.
    var wantsBlur: Bool {
        active && (step == .welcome || step == .philosophy || step == .wrap)
    }

    // MARK: - lifecycle

    /// The trigger, called when the first sync of the session lands. Every
    /// clause is a reason NOT to interrupt: already running, already dismissed,
    /// already seen, or the user is somewhere other than the board the tour
    /// talks about.
    func maybeStart() {
        let store = AppStore.shared
        guard !active, !dismissedThisSession else { return }
        guard !Prefs.shared.tourCompleted || Self.forced else { return }
        guard store.activeView == .sitrep, !store.daemonDown else { return }
        start()
    }

    /// Settings' "replay the tour": bypasses the completed flag AND the
    /// once-a-session rule, and lands on the sitrep first, since four of the
    /// seven steps are about what is on that page.
    func replay(store: AppStore) {
        dismissedThisSession = false
        store.setView(.sitrep)
        start()
    }

    private func start() {
        step = .welcome
        demoPhase = .waiting
        targets = measured
        active = true
    }

    func advance() {
        guard let next = TourStep(rawValue: step.rawValue + 1) else {
            finish()
            return
        }
        step = next
    }

    func back() {
        guard let prev = TourStep(rawValue: step.rawValue - 1) else { return }
        step = prev
    }

    func skip() {
        guard active else { return }
        Analytics.capture("tour_skipped", ["step": step.position])
        end()
        AppStore.shared.pushToast("tour skipped · replay it any time from Settings", .info)
    }

    func finish() {
        guard active else { return }
        Analytics.capture("tour_completed", ["step": step.position])
        end()
    }

    private func end() {
        // The practice undo dies WITH the lesson. Its chip is a real one on a
        // real stack, and one left behind outlives the controller it reverts
        // into: a click a second after "skip" would revert nothing, visibly.
        AppStore.shared.undos.removeAll { $0.messageId == Self.demoUpdate.id }
        active = false
        dismissedThisSession = true
        Prefs.shared.tourCompleted = true
    }

    // MARK: - demo state

    // All three note* callbacks arrive from things that outlive the step that
    // armed them — a saved editor, a 5s undo chip — so each one checks that the
    // tour is still on the step it belongs to. Without that, an undo fired from
    // the wrap step advances past the end and finishes the tour silently.

    func noteRuleSaved() {
        guard active, step == .demoRule else { return }
        demoPhase = .ruleSaved
        advance()
    }

    func noteDone() {
        guard active, step == .demoDone else { return }
        demoPhase = .done
    }

    /// The undo landed (from the real chip, or from the fallback after its 5s
    /// window closed). Advancing from here is what makes `u` feel like the last
    /// beat of the lesson rather than a dead end.
    func noteUndone() {
        guard active, step == .demoDone else { return }
        demoPhase = .undone
        advance()
    }

    // MARK: - measured regions

    /// A sitrep zone reporting where it is. The observable copy is written only
    /// while the tour is up; the shadow copy always is (see `measured`).
    func report(_ id: TourTarget, _ rect: CGRect) {
        measured[id] = rect
        guard active, targets[id] != rect else { return }
        targets[id] = rect
    }
}
