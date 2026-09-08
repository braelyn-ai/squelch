// THE APP ICON'S NUMBER: how many obligations are overdue or due by end of
// today, said to someone who is not in the app.
//
// It is the same `NeedToday.count` the sitrep's headline is built from, off the
// same `store.sitrep.standing`, because a badge is read WITHOUT the sentence
// that would correct it. Two counts would eventually differ and the badge is the
// one nobody can check.
//
// WRITTEN FROM TWO PLACES, and the second one is not redundant.
//
// `AppStore.sitrep`'s `didSet` catches every CHANGE: the poller's pull, a
// resolve, a re-triage, the wipe on disconnect all end in an assignment to that
// property, so hooking it rather than the callers is what stops a sixth path
// from shipping a stale number by forgetting to call anything.
//
// What a didSet cannot catch is a CONFIRMATION. `SitrepPoller` assigns only when
// the read model actually differs — writing an identical one every ten seconds
// would re-lay out the dashboard for nothing — so "the daemon agrees with what
// we already had" produces no assignment and no observer. That is fine for
// anything whose only writer is this process, and wrong for the badge, whose
// other writer runs while this process does not. A launch begins with an empty
// standing band; if the extension left a 3 on the icon and the daemon now agrees
// the band is empty, nothing is assigned and the 3 survives with the app open in
// front of it. So the poller refreshes unconditionally after each successful
// pull, which is the only place that learns the number is still right.
//
// AND A SECOND WRITER THAT IS NOT THIS FILE. The badge has to be right while the
// app is CLOSED, which is exactly when this code is not running, so the
// notification service extension stamps its own count onto each push it
// enriches (Sources/PassbandiOSNotify/NotificationService.swift). That is the
// only other thing that may set it, it uses the same pure count, and it reads
// the band with `peek` so refreshing a number never marks mail as seen.
//
// TWO PLATFORMS, TWO MECHANISMS. A Mac dock tile takes a STRING and asks nobody
// for permission; an iOS badge is a notification capability and shows nothing
// unless the user granted `.badge` (see PushRegistration). So a Mac always gets
// its number and a phone may silently get none, which is a permission state and
// not a bug to code around.

import Foundation

#if os(macOS)
    import AppKit
#else
    import UserNotifications
#endif

@MainActor
enum Badge {
    /// The whole contract: given the standing band, put its due-today count on
    /// the app icon. Zero clears the badge rather than drawing a "0" — an icon
    /// wearing a zero is a notification that nothing happened.
    static func refresh(_ standing: [AttentionUpdate]) {
        set(NeedToday.count(standing))
    }

    static func set(_ count: Int) {
        #if os(macOS)
            // `badgeLabel` is the dock's own text, so the empty case is nil and
            // not "0" or "": either of those draws the red pill anyway.
            NSApplication.shared.dockTile.badgeLabel = count > 0 ? String(count) : nil
        #else
            // Fails quietly when the user never granted `.badge`, which is the
            // behavior we want: nothing to report and nothing to report it to.
            UNUserNotificationCenter.current().setBadgeCount(count)
        #endif
    }
}
