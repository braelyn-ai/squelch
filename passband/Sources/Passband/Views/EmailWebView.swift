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

        let (config, relay) = EmailFrame.makeConfiguration()

        let webView = PassthroughWebView(frame: .zero, configuration: config)
        // WKWebView holds its delegates WEAKLY; the relay survives because the
        // content controller retains its message handlers and the frame retains
        // the controller. That retention is load-bearing — removing the handler
        // (WebFramePool.discard) is what unwires the delegates for good.
        webView.navigationDelegate = relay
        webView.uiDelegate = relay
        webView.setValue(false, forKey: "drawsBackground")
        webView.allowsBackForwardNavigationGestures = false
        webView.allowsMagnification = false

        context.coordinator.attach(relay)
        context.coordinator.load(webView, html: html, allowRemote: allowRemote, poolKey: poolKey)
        return webView
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
