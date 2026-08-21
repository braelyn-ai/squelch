// THE MAC HALF of the reading surface. Everything a rendered email is actually
// made of — the `Prepared` pipeline, the sandbox configuration, the measuring
// script, the relay, the frame pool and the `EmailWebView` SwiftUI view itself —
// lives in EmailWebCore.swift and is shared with the phone. What is left here is
// only what AppKit demands: an NSViewRepresentable, and a WKWebView subclass
// that gives the scroll wheel back to the column that owns the scroll.
//
// The iOS twin is PassbandiOS/Views/EmailWebViewiOS.swift. It declares the same
// `EmailWebViewRepresentable` initializer, which is what lets one SwiftUI view
// serve both platforms; the two files are never compiled into the same target.

import SwiftUI
import WebKit

/// A WKWebView that does NOT eat the scroll wheel: each message is sized to its
/// content and its document is `overflow: hidden`, so forwarding the wheel to the
/// next responder hands the gesture to the ScrollView that owns the column.
final class PassthroughWebView: WKWebView {
    override func scrollWheel(with event: NSEvent) {
        nextResponder?.scrollWheel(with: event)
    }
}

struct EmailWebViewRepresentable: NSViewRepresentable {
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

    /// Typed as the BASE class, not `PassthroughWebView`: the pool hands back
    /// whatever it parked, and the subclass exists for one event override rather
    /// than for anything this representable calls.
    func makeNSView(context: Context) -> WKWebView {
        // POOL HIT: this document, for this message, under this image policy,
        // still parsed and laid out in a live frame. Adopt it and DO NOT LOAD —
        // `adopt` seeds the load guard so the updateNSView that follows is a no-op
        // instead of a reload that would throw the rendered frame away.
        if let key = EmailFrame.renderKey(poolKey: poolKey, html: html, allowRemote: allowRemote),
            let entry = WebFramePool.shared.checkout(key)
        {
            context.coordinator.adopt(entry, key: key)
            return entry.webView
        }

        // A WARM SPARE if there is one: same frame this would have built, minus
        // the seventy milliseconds of blank box while its content process comes
        // up. Taking one is what makes the next spare worth building, hence the
        // top-up on the way out.
        // The FALLBACK builds a frame WITHOUT warming it: there is no time for
        // a warming load to help a frame that is being used this instant, and
        // an empty document racing the real one is only something to get wrong.
        let entry = WebFramePool.shared.takeSpare() ?? Self.buildFrame()
        WebFramePool.shared.replenishSpares(Self.buildSpare)

        let webView = entry.webView
        // WIRED HERE, not at build time. WKWebView holds its delegates WEAKLY;
        // the relay survives because the content controller retains its message
        // handlers and the frame retains the controller. That retention is
        // load-bearing — removing the handler (WebFramePool.discard) is what
        // unwires the delegates for good.
        webView.navigationDelegate = entry.relay
        webView.uiDelegate = entry.relay

        context.coordinator.attach(entry.relay)
        context.coordinator.load(webView, html: html, allowRemote: allowRemote, poolKey: poolKey)
        return webView
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

    /// A frame with every AppKit-side setting on it and nothing loaded.
    @MainActor
    static func buildFrame() -> WebFramePool.Entry {
        let (config, relay) = EmailFrame.makeConfiguration()
        let webView = PassthroughWebView(frame: .zero, configuration: config)
        webView.setValue(false, forKey: "drawsBackground")
        webView.allowsBackForwardNavigationGestures = false
        webView.allowsMagnification = false
        return WebFramePool.Entry(webView: webView, relay: relay)
    }

    func updateNSView(_ webView: WKWebView, context: Context) {
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
    static func dismantleNSView(_ webView: WKWebView, coordinator: EmailFrameCoordinator) {
        coordinator.release(webView)
    }
}
