// WHEN TO ASK SOMEBODY TO SHARE PASSBAND, and — the harder half — when never to
// ask again.
//
// "Two weeks of usage" is two facts, not one, and using only the first is how a
// nudge becomes a nag. Installing the app and ignoring it for a fortnight is not
// two weeks of usage; it is a fortnight. So the gate is BOTH:
//
//   * `nudgeAfterDays` have passed since the first launch, and
//   * the app has been opened on `nudgeAfterActiveDays` DISTINCT days.
//
// The second is what makes the ask land on somebody who actually has an opinion
// about the product, which is the only person worth asking.
//
// ASKED ONCE, EVER. Both answers are answers: "share" opens the sheet, "not now"
// closes it, and neither brings it back. There is no second wave and no
// re-prompt after a version bump — a person who said no once and gets asked
// again has learned something true about how the product treats them. The
// share sheet stays reachable from Settings forever, which is where somebody
// who changes their mind goes.
//
// PER-DEVICE, like everything else in Prefs' charter: this is view state about
// this install, not account state. A user on two Macs may be asked on each, and
// that is the correct failure — the alternative is asking on neither.

import Foundation
import Observation

@MainActor
@Observable
final class ShareNudge {
    static let shared = ShareNudge()

    /// How long after the first launch the ask becomes possible.
    nonisolated static let nudgeAfterDays = 14

    /// And on how many distinct days the app has to have been opened in that
    /// time. Half the window: enough that the person has a habit, loose enough
    /// that a week away does not disqualify them.
    nonisolated static let nudgeAfterActiveDays = 7

    private nonisolated static let firstUseKey = "passband.share.firstUseAt"
    private nonisolated static let activeDaysKey = "passband.share.activeDays"
    private nonisolated static let lastActiveDayKey = "passband.share.lastActiveDay"
    private nonisolated static let askedKey = "passband.share.asked"

    private let defaults = UserDefaults.standard

    /// Whether the modal is up right now. Owned here rather than by the view
    /// so that the one place that decides "ask" is the one place that records
    /// "asked".
    var showingNudge = false

    /// Held only so the closure stays registered for the singleton's (process)
    /// lifetime; never removed.
    private var observers: [NSObjectProtocol] = []

    private init() {
        // The first access is the first launch, if nothing recorded one before.
        if defaults.object(forKey: Self.firstUseKey) == nil {
            defaults.set(Date(), forKey: Self.firstUseKey)
        }
        recordActiveDay()
        observers.append(
            NotificationCenter.default.addObserver(
                forName: Platform.didBecomeActiveNotification, object: nil, queue: .main
            ) { _ in
                Task { @MainActor in ShareNudge.shared.recordActiveDay() }
            })
    }

    /// Count today, once. Called at launch and on every activation, because a
    /// Mac app is frequently left running across midnight and a day it was used
    /// on should count as one.
    func recordActiveDay() {
        let today = Self.dayKey(Date())
        guard defaults.string(forKey: Self.lastActiveDayKey) != today else { return }
        defaults.set(today, forKey: Self.lastActiveDayKey)
        defaults.set(defaults.integer(forKey: Self.activeDaysKey) + 1, forKey: Self.activeDaysKey)
    }

    /// THE RULE, pure and with every input passed in, so a headless suite can
    /// pin it: the lifecycle around it (notifications, UserDefaults) is out of
    /// reach of one, and a nudge is only ever wrong LATER.
    ///
    /// `canShare` is the daemon's answer (see `StoreStats.invite_sharing`): a
    /// self-hosted daemon, or a tenant nobody has turned sharing on for, is
    /// never nudged toward a button that cannot work.
    ///
    /// `firstUse` of nil means nothing recorded a first launch, which can only
    /// happen if the stamp was cleared underneath us. It reads as "not yet",
    /// the conservative direction: an ask that never comes beats one that
    /// arrives on somebody's first afternoon.
    nonisolated static func earned(
        canShare: Bool,
        asked: Bool,
        firstUse: Date?,
        activeDays: Int,
        now: Date
    ) -> Bool {
        guard canShare, !asked, let firstUse else { return false }
        let elapsed = Calendar.current.dateComponents([.day], from: firstUse, to: now).day ?? 0
        return elapsed >= nudgeAfterDays && activeDays >= nudgeAfterActiveDays
    }

    /// [`earned`] over what this install has recorded.
    func shouldAsk(canShare: Bool, now: Date = Date()) -> Bool {
        Self.earned(
            canShare: canShare,
            asked: defaults.bool(forKey: Self.askedKey),
            firstUse: defaults.object(forKey: Self.firstUseKey) as? Date,
            activeDays: defaults.integer(forKey: Self.activeDaysKey),
            now: now)
    }

    /// Put the modal up, and record that we did. THE STAMP IS WRITTEN HERE, not
    /// when the user answers: an app that crashed while the modal was on screen
    /// has still asked, and asking again because it never saw an answer is the
    /// behaviour this whole file exists to avoid.
    func ask() {
        defaults.set(true, forKey: Self.askedKey)
        showingNudge = true
    }

    /// Ask if this install has earned it. The one entry point a view calls.
    func askIfEarned(canShare: Bool) {
        guard shouldAsk(canShare: canShare) else { return }
        ask()
    }

    /// A local day, as a stable string. Local rather than UTC deliberately: the
    /// question is "did this person use the app today", and today is theirs.
    private nonisolated static func dayKey(_ date: Date) -> String {
        let c = Calendar.current.dateComponents([.year, .month, .day], from: date)
        return "\(c.year ?? 0)-\(c.month ?? 0)-\(c.day ?? 0)"
    }
}
