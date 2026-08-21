// MEASURING A THREAD BEFORE ANYBODY SCROLLS TO IT.
//
// A rendered email's height is not knowable without a layout engine. You can
// count its characters and be wrong by a factor of ten, because the tables and
// the pictures are where the height actually is. So the reader used to find out
// the only way it could: mount the message, let WebKit measure it, and take the
// correction as a shove. A first pass through a thread is one shove per
// message — and the minimap is drawn to a scale derived from the same bad
// numbers, so the window mark barely moves while you scroll.
//
// This measures the whole thread up front, off screen, through the SAME sandbox
// the reader renders it in: same stripped body, same CSP, same image policy,
// same column width, same collapsed quoted history. The heights go in
// FrameHeights, so by the time the reader reaches a message its size is a fact,
// and opening the thread again has nothing left to measure.
//
// ONE FRAME, LOADED SERIALLY. Twenty messages are twenty documents, not twenty
// web views: a web view is a content process (~76ms to stand up and a process
// to keep), while another document through a warm one is ~2ms. That is what
// makes this cheap enough to run every time a thread opens.
//
// AND THE HEIGHT IS ASKED FOR, NOT LISTENED FOR. The measuring script posts
// heights as it finds them, but a posted message says nothing about which
// document it came from: measured against a run of documents through one frame,
// the previous body's height lands in the next body's bucket about a quarter of
// the time, and no amount of waiting between loads reliably prevents it. So
// this waits for the navigation IT started to finish, then calls
// `__passbandHeight()` — a question whose answer can only be about the document
// that was asked.

import SwiftUI
import WebKit

@MainActor
final class FrameMeasurer {
    static let shared = FrameMeasurer()
    private init() {}

    /// The measuring frame's viewport. The width is the reading column's and is
    /// load-bearing — mail reflows, and the same body measured at 843 and at
    /// 500 differs by a third.
    ///
    /// The height very nearly is not: `height:100%`, a table at `height="100%"`,
    /// a float overflowing the body box and an absolutely positioned child were
    /// all measured to give the same answer at viewport 100, 800 and 2400,
    /// because the document is `overflow:hidden` and the body's own box is what
    /// is measured. `vh` units are the one exception (`min-height:100vh` is
    /// whatever this number says), and they are rare enough in mail — and
    /// equally circular in the reader, where the frame's height IS the content
    /// — that a screenful is as good an answer as exists.
    private static let viewportHeight: CGFloat = 800
    /// How long a document has to hold the same height before it is believed.
    /// Short, because the navigation has already done the waiting: `didFinish`
    /// does not fire until the subresources are in, so an image delayed two
    /// seconds is still counted before the first poll. This only catches a
    /// document that is still settling after that.
    private static let quiet: Duration = .milliseconds(25)
    /// The longest the POLLING may go on for a document that will not hold
    /// still.
    private static let deadline: Duration = .milliseconds(1200)
    /// And the longest we wait for the load itself. `didFinish` waiting for
    /// every subresource is what makes the poll trustworthy, and it is also
    /// what makes it hostage: with remote images turned on, one body pointing
    /// at a host that never answers stalls on the fetch timeout — measured
    /// still in flight at ten seconds — and takes the whole pass with it. The
    /// mail is not worth that; a guess is fine for one message.
    private static let loadDeadline: Duration = .seconds(2)

    private var frame: WKWebView?
    private var relay: MeasureRelay?
    private var pass: Task<Void, Never>?
    /// Whose pass this is — the thread id. See `cancel(token:)`.
    private var token: String?

    // MARK: - the pass

    /// Measure every message in this thread that has no height on file, and
    /// call back ONCE when the whole thread is measured. That includes calling
    /// back immediately with nothing to do, which is what a second open looks
    /// like.
    func measure(
        _ messages: [ClientMessage], width: CGFloat, allowRemote: Bool, token: String,
        onFinished: @escaping () -> Void
    ) {
        reset()
        guard width > 0 else { return }
        FrameHeights.shared.using(width: width)
        self.token = token

        // NEWEST FIRST: the thread opens on the newest message with the history
        // above it, so the next size anybody needs is the one just behind them.
        let jobs = messages.reversed().filter { message in
            guard let html = message.html, !html.isEmpty else { return false }
            return FrameHeights.shared.get(String(message.id)) == nil
        }
        guard !jobs.isEmpty else {
            onFinished()
            return
        }

        pass = Task { @MainActor [weak self] in
            // A document that reports nothing is a slow email; two in a row is
            // a frame that is not measuring at all, and the right answer to
            // that is to stop rather than to spend a second and a bit per
            // message learning it twenty times.
            var misses = 0
            for message in jobs {
                if Task.isCancelled { return }
                guard let self else { return }
                let height = await self.measure(message, allowRemote: allowRemote)
                if Task.isCancelled { return }
                if height > 0 {
                    FrameHeights.shared.set(String(message.id), height)
                    misses = 0
                } else {
                    misses += 1
                    if misses >= 2 { break }
                }
            }
            guard let self, !Task.isCancelled else { return }
            // The frame is a content process and the queue is empty: nothing
            // needs one until the next thread opens.
            self.reset()
            onFinished()
        }
    }

    /// The reader leaving. TOKEN-SCOPED because a thread switch is two events
    /// in an order SwiftUI does not promise: the incoming reader may start its
    /// pass before the outgoing one says goodbye, and a bare cancel would then
    /// throw away the pass that had just begun.
    func cancel(token: String) {
        guard self.token == token else { return }
        reset()
    }

    /// Stop, and let the frame go. Heights already taken stay taken — they are
    /// true regardless of what is on screen.
    private func reset() {
        pass?.cancel()
        pass = nil
        token = nil
        // Anything awaiting a navigation that will now never finish has to be
        // let go, or the task parks on a continuation forever.
        relay?.abort()
        frame?.stopLoading()
        frame?.navigationDelegate = nil
        frame?.configuration.userContentController.removeAllUserScripts()
        frame = nil
        relay = nil
    }

    // MARK: - one message

    /// Load this body and hand back what it measures, or 0 if it never says.
    private func measure(_ message: ClientMessage, allowRemote: Bool) async -> CGFloat {
        // Prepared the same way the reader prepares it (strip, dedupe, cid,
        // proxy) — a document measured is not a document rendered otherwise.
        // Off the main actor, and filed in the shared cache, so the card that
        // eventually mounts this message finds the work already done.
        let prepared = await Self.prepare(message)
        guard !prepared.html.isEmpty, !Task.isCancelled else { return 0 }

        let view = liveFrame()
        guard let relay else { return 0 }
        view.frame = CGRect(
            x: 0, y: 0, width: FrameHeights.shared.width, height: Self.viewportHeight)
        // Claimed BEFORE the load, because the policy gate can be asked before
        // `loadHTMLString` has returned.
        relay.expectOwnLoad()
        guard
            let navigation = view.loadHTMLString(
                EmailFrameCoordinator.document(html: prepared.html, allowRemote: allowRemote),
                baseURL: nil)
        else {
            relay.forgetOwnLoad()
            return 0
        }
        // THE NAVIGATION IS THE TOKEN. Until this one finishes, the frame is
        // still showing the previous message and anything it reports is that
        // message's answer.
        //
        // On a clock, because the load is the unbounded part (see
        // `loadDeadline`): the watchdog releases the wait, and the frame is
        // told to stop so a dead fetch is not still running under the next
        // message.
        let watchdog = Task { @MainActor [weak relay] in
            try? await Task.sleep(for: Self.loadDeadline)
            guard !Task.isCancelled else { return }
            relay?.abort()
        }
        let landed = await relay.settled(navigation)
        watchdog.cancel()
        guard landed else {
            view.stopLoading()
            return 0
        }

        var previous: CGFloat = -1
        var silent = 0
        let started = ContinuousClock.now
        while ContinuousClock.now - started < Self.deadline {
            let height = await pull(view)
            if height > 0, height == previous { return height }
            // The script defines the function before the navigation finishes,
            // and an empty body is still 28 points of padding — so a document
            // answering zero is a document that did not run it. Three of those
            // is not a slow email, and waiting out the deadline for it only
            // delays the nineteen behind it.
            silent = height > 0 ? 0 : silent + 1
            if silent >= 3 { return 0 }
            previous = height
            try? await Task.sleep(for: Self.quiet)
            if Task.isCancelled { return 0 }
        }
        // Out of time with a number that was still moving: the last one it
        // said beats nothing, and beats a guess.
        return max(0, previous)
    }

    /// What the document says it is right now. The injected script defines
    /// `__passbandHeight` on the collapsed document, so this is the size the
    /// reader will show — not the size with the quoted chain unrolled.
    private func pull(_ view: WKWebView) async -> CGFloat {
        await withCheckedContinuation { continuation in
            view.evaluateJavaScript("window.__passbandHeight ? window.__passbandHeight() : 0") {
                value, _ in
                let height = (value as? NSNumber).map { CGFloat($0.doubleValue) } ?? 0
                continuation.resume(returning: height)
            }
        }
    }

    private static func prepare(_ message: ClientMessage) async -> EmailWebView.Prepared {
        let html = message.html ?? ""
        let allow = message.allowsTrackers
        let parts = message.attachmentList
        let key = EmailWebView.Prepared.cacheKey(html, allow, parts)
        if let warm = PreparedBodies.shared.get(key) { return warm }
        let made = await Task.detached(priority: .utility) {
            EmailWebView.Prepared.make(from: html, allowTrackers: allow, attachments: parts)
        }.value
        PreparedBodies.shared.set(key, made)
        return made
    }

    /// The one frame, built on demand. NO WINDOW AND NO SUPERVIEW: a web view
    /// held in a variable loads, runs the injected script and lays out exactly
    /// as one in a window does (checked against three kinds of offscreen
    /// hosting — same height to the point, and no window can steal focus if
    /// there is no window).
    private func liveFrame() -> WKWebView {
        if let frame { return frame }
        let config = WKWebViewConfiguration()
        // The same layers the reader renders under, because a document measured
        // under a weaker policy is a different document: no page script, no
        // persistent store, images only through our own handlers, and every
        // navigation but ours refused.
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        config.websiteDataStore = EmailFrame.sharedDataStore
        config.setURLSchemeHandler(ImageSchemeHandler.shared, forURLScheme: ImageProxy.scheme)
        config.setURLSchemeHandler(ImageSchemeHandler.shared, forURLScheme: CidProxy.scheme)

        // The measuring script rides along for what it does BEFORE it reports:
        // it collapses the quoted history, so the height is the one the reader
        // opens at. Its messages have nowhere to go — no handler is registered
        // under that name and `send` swallows the failure — which is the point.
        let controller = WKUserContentController()
        controller.addUserScript(
            WKUserScript(
                source: EmailFrame.measuringScript, injectionTime: .atDocumentEnd,
                forMainFrameOnly: true))
        config.userContentController = controller

        let relay = MeasureRelay()
        let view = WKWebView(
            frame: CGRect(
                x: 0, y: 0, width: max(1, FrameHeights.shared.width),
                height: Self.viewportHeight),
            configuration: config)
        view.navigationDelegate = relay
        self.frame = view
        self.relay = relay
        return view
    }
}

/// The measuring frame's navigation delegate, and the reason a height can be
/// trusted: it says when the navigation WE started has finished, so the pull
/// that follows can only be about the document we loaded. It also keeps layer 4
/// intact — every navigation the frame did not ask for is refused.
@MainActor
private final class MeasureRelay: NSObject, WKNavigationDelegate {
    private var pendingOwnLoads = 0
    private var waiting: (navigation: WKNavigation, resume: (Bool) -> Void)?
    /// A navigation that landed before anybody asked about it. Calling an async
    /// method is a suspension point, so "load, then wait" can be two turns of
    /// the run loop and the answer can arrive in between.
    private var settledEarly: (navigation: WKNavigation, ok: Bool)?

    /// Wait for this navigation to finish. False means it failed, or that the
    /// pass was abandoned while it was in flight.
    func settled(_ navigation: WKNavigation) async -> Bool {
        if let early = settledEarly, early.navigation === navigation {
            settledEarly = nil
            return early.ok
        }
        return await withCheckedContinuation { continuation in
            waiting = (navigation, { continuation.resume(returning: $0) })
        }
    }

    /// Let go of a wait that will never be answered.
    func abort() {
        let pending = waiting
        waiting = nil
        settledEarly = nil
        pending?.resume(false)
    }

    private func land(_ navigation: WKNavigation?, ok: Bool) {
        guard let navigation else { return }
        if let pending = waiting, pending.navigation === navigation {
            waiting = nil
            pending.resume(ok)
            return
        }
        settledEarly = (navigation, ok)
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        land(navigation, ok: true)
    }

    func webView(
        _ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error
    ) {
        land(navigation, ok: false)
    }

    func webView(
        _ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!,
        withError error: Error
    ) {
        land(navigation, ok: false)
    }

    /// LAYER 4, unchanged for being off screen: only the loads this frame
    /// started are allowed, and a document that tried to navigate itself
    /// (a meta refresh is the one way it could, with script off) is refused.
    func webView(
        _ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void
    ) {
        if pendingOwnLoads > 0 {
            pendingOwnLoads -= 1
            decisionHandler(.allow)
            return
        }
        decisionHandler(.cancel)
    }

    func expectOwnLoad() { pendingOwnLoads += 1 }
    /// A load that was claimed and then never started. Left standing, the claim
    /// would be spent on somebody else's navigation.
    func forgetOwnLoad() { pendingOwnLoads = max(0, pendingOwnLoads - 1) }
}
