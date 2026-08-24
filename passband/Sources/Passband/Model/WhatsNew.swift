// Whether this launch owes the human a changelog, and which entries.
//
// The rule is one sentence: a version's notes are shown once, on the first
// launch after taking that version, and never again. Everything below is the
// ways that sentence can be got wrong.
//
// IT NEVER MEETS A NEW USER. `tourCompleted` is the gate, and it is exactly the
// right bit: it says the app has already introduced itself to whoever is
// sitting here. A fresh install has not been introduced, so it gets the tour
// and no changelog, and the two overlays can never stack, because one runs only
// where the other has finished. It also settles the transition INTO this
// feature, which has no other honest signal: an install that predates the stamp
// but has finished the tour is an established install, owed the release it just
// took and nothing older (see ReleaseNotes.unseen).

import Foundation
import Observation

@MainActor
@Observable
final class WhatsNew {
    /// Env escape hatch for development and screenshots, matching the tour's.
    /// It takes the card past the STAMP, not past the "must be on the board,
    /// must have synced" part of the gate, and it fabricates nothing: with no
    /// note for this build there is still no card.
    static let forced = ProcessInfo.processInfo.environment["PASSBAND_FORCE_WHATS_NEW"] == "1"

    /// What the card draws, newest release first. Empty means no card.
    private(set) var notes: [ReleaseNote] = []

    var active: Bool { !notes.isEmpty }

    /// Set by closing the card, so a reconnect (which resets `lastRefresh`, the
    /// trigger) cannot raise the same notes again in one session. The stamp
    /// covers the next LAUNCH; this covers the next sync.
    private var dismissedThisSession = false

    /// Which version this copy of the app actually is. Read from the bundle
    /// rather than passband/VERSION so it cannot drift from what shipped, and
    /// nil where there is no bundled version (a test binary), for which the
    /// honest answer is to show nothing.
    private var runningVersion: String? {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String
    }

    /// The trigger, called when the first sync of the session lands. Every
    /// clause is a reason NOT to interrupt: already up, already answered, a new
    /// user who is owed the tour instead, a tour actually running, or the human
    /// is somewhere other than the board this is shown over.
    func maybeShow() {
        let store = AppStore.shared
        guard !active, !dismissedThisSession else { return }
        guard Prefs.shared.tourCompleted, !store.tour.active else { return }
        guard store.activeView == .sitrep, !store.daemonDown else { return }
        guard let running = runningVersion else { return }

        let stamp = Self.forced ? nil : Prefs.shared.lastSeenReleaseNotes
        let unseen = ReleaseNotes.unseen(lastSeen: stamp, running: running)
        guard !unseen.isEmpty else { return }
        notes = unseen
        Analytics.capture("whats_new_shown", ["releases": unseen.count])
    }

    /// Settings' "what's new in this version": on demand, ignoring the stamp.
    /// ONE release, because that is the question the button asks. The back
    /// catalogue belongs in the changelog, not behind a button labelled with
    /// the word "this".
    func replay() {
        guard let running = runningVersion,
            let current = ReleaseNotes.newest(atOrBelow: running)
        else { return }
        dismissedThisSession = false
        notes = [current]
    }

    /// Closing the card. The stamp is the newest note ACTUALLY SHOWN, never the
    /// running version: a build carrying no note for some release must leave it
    /// unstamped so a later build can still announce it, instead of swallowing
    /// it on the reader's behalf.
    func dismiss() {
        guard active else { return }
        stamp(ReleaseNotes.newestShown(notes))
        notes = []
        dismissedThisSession = true
    }

    /// Advance the stamp, never walk it back. `replay` shows a release
    /// regardless of the stamp, and closing THAT card must not re-arm the app
    /// for notes the human has already read past.
    private func stamp(_ version: String?) {
        guard let version, let seen = ReleaseVersion(version) else { return }
        if let current = Prefs.shared.lastSeenReleaseNotes.flatMap(ReleaseVersion.init),
            seen <= current
        {
            return
        }
        Prefs.shared.lastSeenReleaseNotes = version
    }
}
