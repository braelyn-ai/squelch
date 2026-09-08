// THE "NEEDS YOU TODAY" COUNT, in one place because three surfaces say it and
// they must say the same number.
//
// The sitrep's headline states it in words ("Three items need you today"), the
// masthead states it as a figure, and the app icon's badge states it to someone
// who is not looking at either. A badge that disagreed with the sentence behind
// it would be worse than no badge — it is read WITHOUT the context that could
// correct it, so it is the one of the three that cannot be allowed to drift.
//
// PURE, AND FOUNDATION-ONLY, because the third caller is not the app. The
// notification service extension refreshes the badge when a push arrives, and it
// compiles a hand-picked list of files with no SwiftUI on it (see project.yml).
// This lived on `SitrepView` until the badge needed it, which put a count behind
// a view the extension could never load.
//
// OVERDUE COUNTS AS TODAY. The test is "due by the end of today", and something
// that was due on Tuesday satisfies it more urgently than something due tonight.
// A dateless row is never counted at all: the standing band carries live
// correspondence too (see STANDING_BAND in the daemon), and a reply you owe a
// colleague is not a thing with a clock on it.

import Foundation

enum NeedToday {
    /// How many standing rows either surface reads before it stops counting.
    ///
    /// IT LIVES HERE BECAUSE TWO CALLERS MUST AGREE ON IT, and they are in
    /// different targets: the app's poller fetches the band for the dashboard,
    /// and the notification extension fetches it again for the badge. The daemon
    /// pages this route and defaults to FIFTY when the caller says nothing, so
    /// an extension that simply omitted the parameter counted a different set of
    /// rows than the headline it is supposed to agree with — and quietly
    /// under-reported, since the band is ordered by importance rather than by
    /// deadline and the rows past the cut are not the least due ones.
    ///
    /// The daemon clamps to 500, so this is a real ceiling and not a suggestion.
    static let bandLimit = 200

    /// How many of these obligations are overdue or due by the end of today.
    ///
    /// `now` is injected so the boundary can be tested rather than trusted: the
    /// end of "today" is a local-calendar question, and the interesting cases
    /// are all within a minute of a date the machine picks.
    static func count(_ items: [AttentionUpdate], now: Date = Date()) -> Int {
        let endOfDay =
            Calendar.current.date(bySettingHour: 23, minute: 59, second: 59, of: now) ?? now
        return items.filter { u in
            guard let due = Fmt.date(u.deadline) else { return false }
            return due <= endOfDay
        }.count
    }
}
