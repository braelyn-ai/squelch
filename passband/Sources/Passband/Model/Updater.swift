// Sparkle auto-update wearing Passband's own face. One updater for the process
// (Sparkle asserts if a second one starts for the same bundle), and one card
// inside the app instead of Sparkle's separate window: a new version arrives as
// an in-app alert whose single button installs it and relaunches. "Update" and
// then "Install and Relaunch" in a second dialog is one decision asked twice.
//
// This is a full SPUUserDriver rather than the standard driver plus its
// gentle-reminder delegate. That delegate can only DEFER Sparkle's own alert,
// and that alert is exactly the second click being removed. What the standard
// driver used to own is reproduced here on purpose, not dropped: the permission
// prompt below, so a fresh install still never phones home undisclosed, and the
// two answers a check the human asked for has to get (up to date / it failed).
// A BACKGROUND failure stays silent, which is the one thing a driver with no
// window of its own can do that Sparkle's cannot.
//
// The vendored framework is prebuilt (vendor/fetch-sparkle.sh); the feed URL
// and the EdDSA public key live in Info.plist.

import AppKit
import Combine
import Sparkle

/// Where the update flow has got to, as the card needs to see it. `available`
/// is the ONLY phase that waits on a human; everything past it is progress the
/// card narrates while Sparkle works, and every one of them ends at a relaunch.
enum UpdatePhase: Equatable {
    case idle
    /// A check the human asked for, in flight. Never entered for a scheduled
    /// check: the whole point of those is that they cost no attention until
    /// there is something to say.
    case checking
    /// Sparkle has an update and is holding for an answer.
    case available(version: String)
    /// nil fraction: the feed declared no length, so there is a download but no
    /// honest way to draw how much of it is done.
    case downloading(Double?)
    case extracting(Double)
    /// Past the point of no return: the app is being replaced and relaunched.
    case installing

    /// Whether the card is on screen at all.
    var visible: Bool { self != .idle }

    /// Whether the flow is running itself, i.e. the card narrates and its
    /// buttons are gone.
    var busy: Bool {
        switch self {
        case .idle, .checking, .available: false
        case .downloading, .extracting, .installing: true
        }
    }
}

@MainActor
final class Updater: NSObject, ObservableObject, SPUUserDriver {
    static let shared = Updater()

    /// Sparkle's engine. Implicitly unwrapped because it is constructed with
    /// `self` as its user driver, which cannot happen until `super.init()` has
    /// run and every stored property already has a value.
    private var engine: SPUUpdater!

    /// Mirrors Sparkle's own gate (KVO), so the menu item disables itself
    /// mid-check instead of stacking a second session on a click.
    @Published private(set) var canCheck = false

    /// What the card draws.
    @Published private(set) var phase: UpdatePhase = .idle

    /// Sparkle's question, parked until the human answers it. `install` is the
    /// card's one button; `dismiss` is Later, which brings the update back on
    /// the next scheduled check rather than burying the version forever, which
    /// is what `skip` would do. Nil whenever nothing is waiting on an answer.
    private var choice: ((SPUUserUpdateChoice) -> Void)?

    /// Set the moment the human presses Update. This is what makes the second
    /// prompt unnecessary: `showReady(toInstallAndRelaunch:)` answers itself
    /// only because consent for this exact update was already given, and a
    /// relaunch nobody asked for is the one failure mode worth guarding.
    private var consented = false

    /// True only while a check the human started is running. Errors and "no new
    /// version" are reported ONLY under it: a scheduled check that cannot reach
    /// the feed is not news, it is a laptop on a plane.
    private var userInitiated = false

    private var downloadTotal: UInt64 = 0
    private var downloadSoFar: UInt64 = 0

    private override init() {
        super.init()
        let engine = SPUUpdater(
            hostBundle: .main, applicationBundle: .main, userDriver: self, delegate: nil)
        self.engine = engine
        // PASSBAND ALWAYS ASKS FIRST, and this line is what makes that true.
        // Sparkle's automatic-download mode does not merely skip the download
        // prompt: SPUAutomaticUpdateDriver never calls the user driver at all,
        // so there is no showUpdateFound, no card, and the new version lands on
        // the next quit with nobody told. That is the exact opposite of the
        // thing this file exists to draw. The preference persists in user
        // defaults — Sparkle's own permission prompt used to offer it, and an
        // install that answered yes years ago still carries the yes — so it is
        // overridden on EVERY launch rather than written once.
        engine.automaticallyDownloadsUpdates = false
        do {
            try engine.start()
        } catch {
            // A misconfigured feed is a build mistake, not a runtime condition
            // to nag about: the app works fine, it just cannot update itself.
            // Log it and leave `canCheck` false, which greys the menu item.
            NSLog("passband: updater did not start: \(error.localizedDescription)")
            return
        }
        engine.publisher(for: \.canCheckForUpdates)
            .receive(on: DispatchQueue.main)
            .assign(to: &$canCheck)
    }

    // MARK: - what the app calls

    /// User-initiated check, from the app menu.
    func check() {
        userInitiated = true
        engine.checkForUpdates()
    }

    /// THE button. Consent for this update, once, covering both of the answers
    /// Sparkle asks for: install it, and relaunch into it.
    func installAndRelaunch() {
        guard let reply = choice else { return }
        consented = true
        choice = nil
        phase = .downloading(nil)
        reply(.install)
    }

    /// Later. `dismiss`, not `skip`: the version comes back around on the next
    /// scheduled check, because "not now" is not "never".
    func later() {
        guard let reply = choice else {
            phase = .idle
            return
        }
        choice = nil
        phase = .idle
        reply(.dismiss)
    }

    // MARK: - SPUUserDriver: permission

    /// The only modal in this driver, and it earns it: it is asked once, before
    /// the first scheduled check, and the answer decides whether this install
    /// ever talks to passband.app on its own.
    func show(
        _ request: SPUUpdatePermissionRequest,
        reply: @escaping (SUUpdatePermissionResponse) -> Void
    ) {
        let alert = NSAlert()
        alert.messageText = "Check for Passband updates automatically?"
        alert.informativeText =
            "Passband can look at passband.app now and then and tell you when a new "
            + "version is ready. Nothing about your mail or your accounts is sent."
        alert.addButton(withTitle: "Check Automatically")
        alert.addButton(withTitle: "Don't Check")
        let allowed = alert.runModal() == .alertFirstButtonReturn
        // The system profile is never sent, whatever the answer: it is a
        // hardware and locale fingerprint, and none of it makes the feed
        // better at telling this app there is a newer zip.
        reply(SUUpdatePermissionResponse(automaticUpdateChecks: allowed, sendSystemProfile: false))
    }

    // MARK: - SPUUserDriver: the check

    func showUserInitiatedUpdateCheck(cancellation: @escaping () -> Void) {
        userInitiated = true
        phase = .checking
    }

    func showUpdateFound(
        with appcastItem: SUAppcastItem,
        state: SPUUserUpdateState,
        reply: @escaping (SPUUserUpdateChoice) -> Void
    ) {
        // An information-only update has no zip to install — the appcast is
        // pointing at a page instead. There is no honest "Update" button for
        // that, so hand it to the browser and end the session.
        if appcastItem.isInformationOnlyUpdate {
            if let url = appcastItem.infoURL { NSWorkspace.shared.open(url) }
            phase = .idle
            reply(.dismiss)
            return
        }
        choice = reply
        consented = false
        phase = .available(version: appcastItem.displayVersionString)
    }

    func showUpdateNotFoundWithError(_ error: Error, acknowledgement: @escaping () -> Void) {
        phase = .idle
        // Silence unless someone is waiting to hear it.
        if userInitiated {
            let alert = NSAlert()
            alert.messageText = "Passband is up to date."
            alert.informativeText = "You're on the newest version."
            alert.addButton(withTitle: "OK")
            alert.runModal()
        }
        userInitiated = false
        acknowledgement()
    }

    func showUpdaterError(_ error: Error, acknowledgement: @escaping () -> Void) {
        phase = .idle
        choice = nil
        // `consented` as well as `userInitiated`: pressing Update is a request
        // too, and a download that dies afterwards would otherwise take the
        // card off screen and say nothing, which reads as the update having
        // been applied. Read before the reset below.
        let owed = userInitiated || consented
        consented = false
        if owed {
            let alert = NSAlert()
            alert.messageText = "Couldn't check for updates."
            alert.informativeText = error.localizedDescription
            alert.addButton(withTitle: "OK")
            alert.runModal()
        } else {
            NSLog("passband: update check failed: \(error.localizedDescription)")
        }
        userInitiated = false
        acknowledgement()
    }

    // MARK: - SPUUserDriver: release notes

    // Nothing to do with either: the card is one line and a button, and the
    // appcast this app publishes carries no notes to put in it. Sparkle calls
    // these whether or not anyone is listening.
    func showUpdateReleaseNotes(with downloadData: SPUDownloadData) {}
    func showUpdateReleaseNotesFailedToDownloadWithError(_ error: Error) {}

    // MARK: - SPUUserDriver: the work

    func showDownloadInitiated(cancellation: @escaping () -> Void) {
        downloadTotal = 0
        downloadSoFar = 0
        phase = .downloading(nil)
    }

    func showDownloadDidReceiveExpectedContentLength(_ expectedContentLength: UInt64) {
        downloadTotal = expectedContentLength
    }

    func showDownloadDidReceiveData(ofLength length: UInt64) {
        downloadSoFar += length
        // A zip that turns out longer than advertised must not read as 140%.
        phase = .downloading(
            downloadTotal > 0
                ? min(1, Double(downloadSoFar) / Double(downloadTotal)) : nil)
    }

    func showDownloadDidStartExtractingUpdate() {
        phase = .extracting(0)
    }

    func showExtractionReceivedProgress(_ progress: Double) {
        phase = .extracting(min(1, max(0, progress)))
    }

    /// The second question, answered from the consent already given for THIS
    /// update. Without `consented` a relaunch could arrive unasked — from an
    /// update downloaded in a previous session, say — so the guard stands and
    /// the card (which Sparkle raised through `showUpdateFound` first) keeps
    /// waiting for a human.
    func showReady(toInstallAndRelaunch reply: @escaping (SPUUserUpdateChoice) -> Void) {
        guard consented else {
            choice = reply
            return
        }
        phase = .installing
        reply(.install)
    }

    func showInstallingUpdate(
        withApplicationTerminated applicationTerminated: Bool,
        retryTerminatingApplication: @escaping () -> Void
    ) {
        phase = .installing
    }

    func showUpdateInstalledAndRelaunched(
        _ relaunched: Bool, acknowledgement: @escaping () -> Void
    ) {
        phase = .idle
        acknowledgement()
    }

    // MARK: - SPUUserDriver: session end

    func dismissUpdateInstallation() {
        // Sparkle is done with this session, one way or another. Anything the
        // card was narrating is over; a parked reply here is a question nobody
        // will answer any more.
        choice = nil
        consented = false
        userInitiated = false
        phase = .idle
    }

    /// Sparkle asking for the update it already raised to be put back in front
    /// of the human — a second ⌘-check while the card is up. The card lives in
    /// the window, so bringing the window forward IS showing it.
    func showUpdateInFocus() {
        MainWindow.show()
    }
}
