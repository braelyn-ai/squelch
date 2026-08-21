// THE PHONE HALF of the reading surface — the UIKit twin of the Mac's
// EmailWebView.swift, and nothing more than that. The sandbox, the body
// preparation, the measuring bridge, the frame pool and the SwiftUI
// `EmailWebView` that hosts them are all in Views/EmailWebCore.swift and are
// literally the same code on both platforms; this file exists because
// NSViewRepresentable and UIViewRepresentable are two protocols.
//
// NO SCROLL PASSTHROUGH HACK HERE. The Mac has to forward the wheel because an
// NSScrollView eats it; a UIScrollView with `isScrollEnabled = false` simply
// does not claim the pan, so the gesture reaches the thread column on its own.
// The frame is sized to the height the bridge reports and never scrolls itself,
// exactly as on the Mac — one scroll surface per thread, always the outer one.

import SwiftUI
import WebKit

struct EmailWebViewRepresentable: UIViewRepresentable {
    let html: String
    let allowRemote: Bool
    let collapseQuotes: Bool
    /// Message id, or nil for a body that must never be pooled.
    let poolKey: String?
    let onHeight: (CGFloat) -> Void
    let onQuotedFound: (Bool) -> Void
    let onLink: (String) -> Void

    func makeCoordinator() -> EmailFrameCoordinator {
        EmailFrameCoordinator(onHeight: onHeight, onQuotedFound: onQuotedFound, onLink: onLink)
    }

    func makeUIView(context: Context) -> WKWebView {
        // POOL HIT: this document, for this message, under this image policy,
        // still parsed and laid out in a live frame. Adopt it and DO NOT LOAD —
        // `adopt` seeds the load guard so the updateUIView that follows is a
        // no-op instead of a reload that would throw the rendered frame away.
        if let key = EmailFrame.renderKey(poolKey: poolKey, html: html, allowRemote: allowRemote),
            let entry = WebFramePool.shared.checkout(key)
        {
            context.coordinator.adopt(entry, key: key)
            return entry.webView
        }

        // A WARM SPARE if there is one: the same frame this would have built,
        // minus the seventy milliseconds of blank box while its content process
        // comes up. Taking one is what makes the next one worth building.
        // The FALLBACK builds a frame WITHOUT warming it: there is no time for
        // a warming load to help a frame that is being used this instant, and
        // an empty document racing the real one is only something to get wrong.
        let entry = WebFramePool.shared.takeSpare() ?? Self.buildFrame()
        WebFramePool.shared.replenishSpares(Self.buildSpare)

        let webView = entry.webView
        let relay = entry.relay
        // WIRED HERE, not at build time (see `buildSpare`). WKWebView holds its
        // delegates WEAKLY; the relay survives because the content controller
        // retains its message handlers and the frame retains the controller.
        // That retention is load-bearing — removing the handler
        // (WebFramePool.discard) is what unwires the delegates for good.
        webView.navigationDelegate = relay
        webView.uiDelegate = relay
        webView.allowsBackForwardNavigationGestures = false
        // No 3D-touch/long-press link preview: a peek would navigate a surface
        // whose whole security posture is that it never navigates.
        webView.allowsLinkPreview = false

        // The document paints its own opaque white, so this is only about the
        // frames BEFORE it lands: an opaque web view flashes system white (or
        // black in dark mode) over the reader's ground while the paint-hold is
        // still up.
        webView.isOpaque = false
        webView.backgroundColor = .clear
        webView.scrollView.backgroundColor = .clear

        // THE ONE SCROLL RULE. The height bridge sizes this frame to its content
        // and the thread column scrolls; an inner scroll view that still claimed
        // the pan would trap the gesture inside whichever message the finger
        // happened to land on.
        webView.scrollView.isScrollEnabled = false
        webView.scrollView.bounces = false
        webView.scrollView.showsVerticalScrollIndicator = false
        // The frame is inset by its host, and a safe-area inset applied down here
        // would be added to the measured height as blank space at the bottom of
        // every message.
        webView.scrollView.contentInsetAdjustmentBehavior = .never

        context.coordinator.attach(relay)
        context.coordinator.load(webView, html: html, allowRemote: allowRemote, poolKey: poolKey)
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {
        context.coordinator.onHeight = onHeight
        context.coordinator.onQuotedFound = onQuotedFound
        context.coordinator.onLink = onLink
        // A content/policy change is a genuine reload; a quote toggle is not
        // (reloading for it would flash an empty frame).
        context.coordinator.load(webView, html: html, allowRemote: allowRemote, poolKey: poolKey)
        context.coordinator.setQuotesCollapsed(webView, collapsed: collapseQuotes)
    }

    /// Return the frame to the pool rather than dropping it — that is what makes
    /// reopening a read message instant.
    static func dismantleUIView(_ webView: WKWebView, coordinator: EmailFrameCoordinator) {
        coordinator.release(webView)
    }

    /// A blank frame with its process already awake, and DELIBERATELY UNWIRED:
    /// layer 4 refuses every navigation whose relay has no owner, so a spare
    /// that had its navigation delegate attached could not load the empty
    /// document that warms it. Nothing else can reach the frame in the
    /// meantime — it is in no view and holds nothing — and the delegates go on
    /// before any mail does.
    @MainActor
    static func buildSpare() -> WebFramePool.Entry {
        let entry = buildFrame()
        // THE WARMING IS THE LOAD, not the construction: a frame built and
        // never loaded still pays the full content-process launch when it is
        // finally used. The claim is what keeps this load from being refused
        // by layer 4 once the frame has a delegate.
        entry.relay.expectWarmingLoad()
        entry.webView.loadHTMLString("<html><body></body></html>", baseURL: nil)
        return entry
    }

    /// A frame with nothing loaded. Everything UIKit-side is set when it is
    /// taken, since those are plain view properties and setting them twice
    /// costs nothing.
    @MainActor
    static func buildFrame() -> WebFramePool.Entry {
        let (config, relay) = EmailFrame.makeConfiguration()
        return WebFramePool.Entry(
            webView: WKWebView(frame: .zero, configuration: config), relay: relay)
    }
}
