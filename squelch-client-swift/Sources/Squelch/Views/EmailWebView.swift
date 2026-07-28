// Renders ONE message's server-sanitized HTML in a hard-sandboxed WKWebView.
// This is the one place in the app a webview is genuinely required — everything
// else is native SwiftUI.
//
// SECURITY MODEL — five independent layers, each sufficient on its own:
//
//  1. The HTML was already sanitized server-side (ammonia) at ingest: no
//     <script>, no on* handlers, no javascript:/data:text URLs, no forms, no
//     nested frames.
//  2. JAVASCRIPT IS DISABLED for page content
//     (`defaultWebpagePreferences.allowsContentJavaScript = false`). Nothing in
//     the message can execute, whatever it contains. Our OWN measuring script
//     still runs, because `WKUserScript` injection is governed separately.
//  3. A `<meta http-equiv="Content-Security-Policy">` is injected as the FIRST
//     child of <head>, so it applies before any resource is fetched:
//        default-src 'none'; style-src 'unsafe-inline'; img-src <gate>
//     There is no script-src at all. The img-src gate is the per-message remote
//     image decision (see REMOTE IMAGES).
//  4. NAVIGATION IS REFUSED. The delegate allows exactly one load — the initial
//     `loadHTMLString` — and cancels every other navigation. A click inside the
//     frame therefore cannot take the view anywhere; instead we hand the URL to
//     the SYSTEM BROWSER via Opener (which re-guards to http/https only).
//  5. No cookies, no persistent storage: a NON-persistent
//     `WKWebsiteDataStore`, so the frame has no jar to read or write and
//     nothing survives the message being closed.
//
// REMOTE IMAGES. Images load by default (real images are the point of HTML
// mail) but tracking pixels are stripped in a preprocessing pass first (see
// Trackers) and `Referrer-Policy: no-referrer` is set in the document, so hosts
// learn nothing about which mail — or reader — asked for them. When the
// Settings "load on demand" pref is on, `img-src` collapses to `data:` and the
// message shows a per-email opt-in bar instead: NO network request is made for
// mail the reader has not opted into.
//
// LINKS. Because navigation is refused the in-frame links are inert. We extract
// the http(s) hrefs from the same sanitized html and render them as REAL native
// buttons below the frame, which is both safer and more legible than a dead
// link the reader has to guess about.
//
// HEIGHT. The webview is sized to its exact content and NEVER scrolls itself —
// the thread column is the single scroll surface. Our injected measuring script
// reports document height on load, on every image settling, and on a
// ResizeObserver tick. A remembered height (FrameHeights, keyed by message id)
// means a reopened message paints at its final size on the FIRST frame, which
// is what the reader perceives as "no flicker".
//
// QUOTED HISTORY. Collapsed by default, using the same conservative heuristic
// as the plain-text path (Quotes): the first top-level <blockquote> after which
// the document has no substantial text of its own anchors the history. Because
// page script cannot run, the collapse is done by OUR injected script.

import SwiftUI
import WebKit

struct EmailWebView: View {
    let html: String
    /// Stable id (message id) for the height memory.
    var cacheKey: String?

    @Environment(Prefs.self) private var prefs
    @Environment(AppStore.self) private var store

    @State private var height: CGFloat = 0
    @State private var optedIn = false
    @State private var hasQuoted = false
    @State private var quotedHidden = true
    @State private var measured = false

    /// The tracker-strip + link-extraction pass, computed ONCE per html string.
    ///
    /// These were computed as `var`s off `html` on every body evaluation, which
    /// meant a full regex scan of every message body in the thread on every
    /// single scroll frame — enough to make the whole viewer feel locked up.
    /// `Prepared` is cached by content so scrolling costs nothing.
    @State private var prepared: Prepared = .empty

    struct Prepared: Equatable {
        var sourceHash: Int
        var html: String
        var blocked: Int
        var hasRemoteCandidates: Bool
        var links: [EmailLink]

        static let empty = Prepared(
            sourceHash: 0, html: "", blocked: 0, hasRemoteCandidates: false, links: [])

        static func make(from html: String) -> Prepared {
            let stripped = Trackers.strip(html)
            return Prepared(
                sourceHash: html.hashValue,
                html: stripped.html,
                blocked: stripped.blocked,
                hasRemoteCandidates: Trackers.hasNetworkImages(stripped.html),
                links: Trackers.extractLinks(stripped.html))
        }
    }

    private var allowRemote: Bool { prefs.loadRemoteImages || optedIn }
    private static let maxLinks = 8

    /// Frame height shown before the first successful measurement.
    private static let placeholderHeight: CGFloat = 120

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            imageBar

            EmailWebViewRepresentable(
                html: prepared.html,
                allowRemote: allowRemote,
                collapseQuotes: quotedHidden,
                onHeight: { h in
                    guard h > 0 else { return }
                    height = h
                    measured = true
                    // The memory stores the DEFAULT (collapsed) state only.
                    if let cacheKey, quotedHidden { FrameHeights.shared.set(cacheKey, h) }
                },
                onQuotedFound: { hasQuoted = $0 },
                onLink: { Opener.open($0) }
            )
            .frame(height: displayHeight)
            // PAINT-HOLD: an unmeasured frame keeps its placeholder space but
            // stays invisible until the first measurement lands, so the reader
            // never sees a half-laid-out document snap to size.
            .opacity(measured || rememberedHeight != nil ? 1 : 0)
            .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))

            if hasQuoted {
                Button {
                    quotedHidden.toggle()
                } label: {
                    Text(quotedHidden ? "··· show quoted history" : "hide quoted history")
                        .font(Typo.micro)
                        .padding(.horizontal, 9)
                        .padding(.vertical, 3)
                }
                .buttonStyle(.glass)
                .foregroundStyle(Palette.inkFaint)
                .help("the quoted reply chain below this message")
            }

            linkRow
        }
        .onAppear {
            if let cacheKey, let remembered = FrameHeights.shared.get(cacheKey) {
                height = remembered
            }
        }
        // Prepare once per content change, never per render.
        .task(id: html) {
            if prepared.sourceHash != html.hashValue { prepared = Prepared.make(from: html) }
        }
    }

    private var rememberedHeight: CGFloat? { cacheKey.flatMap { FrameHeights.shared.get($0) } }
    private var displayHeight: CGFloat {
        height > 0 ? height : (rememberedHeight ?? Self.placeholderHeight)
    }

    @ViewBuilder
    private var imageBar: some View {
        if !allowRemote && prepared.hasRemoteCandidates {
            Button {
                optedIn = true
            } label: {
                Label("remote images blocked — load for this email", systemImage: "photo")
                    .font(Typo.micro)
                    .padding(.horizontal, 9)
                    .padding(.vertical, 4)
            }
            .buttonStyle(.glass)
            .foregroundStyle(Palette.inkFaint)
            .help(
                "remote images are off by default (Settings → Mail); load them for this email only")
        } else if prepared.blocked > 0 {
            Label(
                "\(prepared.blocked) tracker\(prepared.blocked == 1 ? "" : "s") blocked",
                systemImage: "eye.slash"
            )
            .font(Typo.micro)
            .foregroundStyle(Palette.positive)
            .padding(.horizontal, 9)
            .padding(.vertical, 4)
            .glassCapsule(tint: Palette.positiveSoft, interactive: false)
            .help(
                "tracking pixels removed before render; links open externally and no referrer is ever sent"
            )
        }
    }

    @ViewBuilder
    private var linkRow: some View {
        let shown = Array(prepared.links.prefix(Self.maxLinks))
        if !shown.isEmpty {
            VStack(alignment: .leading, spacing: 5) {
                Text("links open externally")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                FlowLayout(spacing: 6) {
                    ForEach(shown) { link in
                        Button {
                            Opener.open(link.href)
                        } label: {
                            Text(Newsletters.truncate(link.text, 42))
                                .font(Typo.micro)
                                .lineLimit(1)
                                .padding(.horizontal, 8)
                                .padding(.vertical, 3)
                        }
                        .buttonStyle(.glass)
                        .foregroundStyle(Palette.accent)
                        .help(link.href)
                    }
                    if prepared.links.count > shown.count {
                        Text("+\(prepared.links.count - shown.count) more")
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaintest)
                            .padding(.vertical, 4)
                    }
                }
            }
            .padding(.top, 2)
        }
    }
}

// MARK: - the webview itself

/// A WKWebView that does NOT eat the scroll wheel.
///
/// Each message is sized to its exact content and its document is
/// `overflow: hidden`, so the web view has nothing of its own to scroll — but it
/// still swallowed every wheel event that landed on it, which meant the thread
/// viewer simply would not scroll while the pointer was over an email body
/// (i.e. almost always). Forwarding to the next responder hands the gesture to
/// the SwiftUI ScrollView that owns the column, which is the single intended
/// scroll surface.
private final class PassthroughWebView: WKWebView {
    override func scrollWheel(with event: NSEvent) {
        nextResponder?.scrollWheel(with: event)
    }
}

private struct EmailWebViewRepresentable: NSViewRepresentable {
    let html: String
    let allowRemote: Bool
    let collapseQuotes: Bool
    let onHeight: (CGFloat) -> Void
    let onQuotedFound: (Bool) -> Void
    let onLink: (String) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(onHeight: onHeight, onQuotedFound: onQuotedFound, onLink: onLink)
    }

    /// ONE ephemeral data store shared by every email view.
    ///
    /// `WKWebsiteDataStore.nonPersistent()` mints a BRAND NEW store per call, so
    /// giving each message its own meant nothing was ever cached: every image in
    /// every message re-fetched from the network on every open, and re-opening
    /// the same mail was as slow as the first time. Sharing one store restores
    /// the URL cache (and lets messages from the same CDN warm each other) while
    /// keeping the property that actually matters — non-persistent, so nothing
    /// touches disk and no cookie jar survives the app.
    @MainActor
    private static let sharedDataStore: WKWebsiteDataStore = .nonPersistent()

    func makeNSView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()

        // LAYER 2: page content cannot execute script, whatever it contains.
        // Our injected user script is governed separately and still runs.
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        // LAYER 5: no cookie jar, no persistent storage, nothing survives close.
        config.websiteDataStore = Self.sharedDataStore
        config.suppressesIncrementalRendering = true

        let controller = WKUserContentController()
        controller.add(context.coordinator, name: "squelch")
        controller.addUserScript(
            WKUserScript(
                source: Self.measuringScript, injectionTime: .atDocumentEnd,
                forMainFrameOnly: true))
        config.userContentController = controller

        let webView = PassthroughWebView(frame: .zero, configuration: config)
        webView.navigationDelegate = context.coordinator
        webView.uiDelegate = context.coordinator
        webView.setValue(false, forKey: "drawsBackground")
        webView.allowsBackForwardNavigationGestures = false
        webView.allowsMagnification = false

        context.coordinator.load(webView, html: html, allowRemote: allowRemote)
        return webView
    }

    func updateNSView(_ webView: WKWebView, context: Context) {
        context.coordinator.onHeight = onHeight
        context.coordinator.onQuotedFound = onQuotedFound
        context.coordinator.onLink = onLink
        // A content/policy change is a genuine reload; a quote toggle is not
        // (reloading for it would flash an empty frame).
        context.coordinator.load(webView, html: html, allowRemote: allowRemote)
        context.coordinator.setQuotesCollapsed(webView, collapsed: collapseQuotes)
    }

    /// Injected AFTER document end. Does four things, none of which the message
    /// can influence (it cannot run script to interfere):
    ///   1. collapses trailing quoted history, using the same conservative
    ///      heuristic as the plain-text path,
    ///   2. reports the document height on load / on each image settling / on a
    ///      ResizeObserver tick,
    ///   3. reports link clicks up to native rather than navigating,
    ///   4. exposes a collapse toggle the host calls on the quote chip.
    private static let measuringScript = """
        (function () {
          var send = function (payload) {
            try { window.webkit.messageHandlers.squelch.postMessage(payload); } catch (e) {}
          };

          // ---- quoted history --------------------------------------------
          // Mirrors Quotes.swift: the first TOP-LEVEL <blockquote> after which
          // the document has no substantial text of its own anchors the
          // history. A blockquote with real reply text after it (bottom-posting)
          // never qualifies, because collapsing it would hide real content.
          var ATTRIBUTION = [
            /^On .{1,200} wrote:\\s*$/,
            /^-{2,}\\s*(original|forwarded) message\\s*-{0,}$/i,
            /^Begin forwarded message:?\\s*$/i
          ];
          function isAttribution(t) {
            t = (t || '').trim();
            if (!t) return false;
            return ATTRIBUTION.some(function (re) { return re.test(t); });
          }
          function textLengthAfter(root, node) {
            var len = 0;
            var walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
            var t;
            while ((t = walker.nextNode())) {
              if (node.contains(t)) continue;
              if (node.compareDocumentPosition(t) & Node.DOCUMENT_POSITION_FOLLOWING) {
                len += (t.textContent || '').trim().length;
              }
            }
            return len;
          }
          function findQuoteNodes() {
            var body = document.body;
            if (!body) return [];
            var quotes = Array.prototype.slice
              .call(body.querySelectorAll('blockquote'))
              .filter(function (b) {
                return !(b.parentElement && b.parentElement.closest('blockquote'));
              });
            for (var i = 0; i < quotes.length; i++) {
              var q = quotes[i];
              if (textLengthAfter(body, q) > 200) continue;
              var nodes = [];
              var prev = q.previousElementSibling ||
                (q.parentElement && q.parentElement.previousElementSibling);
              if (prev && (prev.textContent || '').trim().length < 300 &&
                  isAttribution((prev.textContent || '').replace(/\\s+/g, ' '))) {
                nodes.push(prev);
              }
              nodes.push(q);
              for (var j = 0; j < quotes.length; j++) {
                var r = quotes[j];
                if (r !== q &&
                    (q.compareDocumentPosition(r) & Node.DOCUMENT_POSITION_FOLLOWING)) {
                  nodes.push(r);
                }
              }
              return nodes;
            }
            return [];
          }

          var quoteNodes = findQuoteNodes();
          window.__squelchSetQuotes = function (collapsed) {
            for (var i = 0; i < quoteNodes.length; i++) {
              quoteNodes[i].style.display = collapsed ? 'none' : '';
            }
            measure();
          };
          // Collapsed by default, BEFORE the first measure, so the frame sizes
          // to the collapsed content and never flashes the full chain.
          window.__squelchSetQuotes(true);
          send({ kind: 'quoted', value: quoteNodes.length > 0 });

          // ---- height ------------------------------------------------------
          var last = -1;
          function measure() {
            var h = Math.max(
              document.documentElement ? document.documentElement.scrollHeight : 0,
              document.body ? document.body.scrollHeight : 0
            );
            if (h > 0 && Math.abs(h - last) > 1) {
              last = h;
              send({ kind: 'height', value: h });
            }
          }
          measure();
          requestAnimationFrame(measure);
          window.addEventListener('load', measure);
          // Images arriving are the only post-load height change (no page
          // script can run) — re-measure as each settles.
          Array.prototype.forEach.call(document.images, function (img) {
            img.decoding = 'async';
            img.addEventListener('load', measure);
            img.addEventListener('error', measure);
          });
          if (window.ResizeObserver && document.body) {
            new ResizeObserver(measure).observe(document.body);
          }

          // ---- links -------------------------------------------------------
          // Navigation is refused by the delegate anyway; reporting the href up
          // to native is what makes an in-frame click actually DO something.
          document.addEventListener('click', function (e) {
            var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
            if (!a) return;
            e.preventDefault();
            var href = a.getAttribute('href') || '';
            if (/^https?:\\/\\//i.test(href)) send({ kind: 'link', value: href });
          }, true);
        })();
        """

    @MainActor
    final class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate,
        WKScriptMessageHandler
    {
        var onHeight: (CGFloat) -> Void
        var onQuotedFound: (Bool) -> Void
        var onLink: (String) -> Void

        /// What is currently loaded, so `updateNSView` only reloads on a real
        /// content/policy change (a reload flashes an empty frame).
        private var loadedSignature: String?
        /// Set for exactly the initial load; every other navigation is refused.
        private var allowingInitialLoad = false

        init(
            onHeight: @escaping (CGFloat) -> Void, onQuotedFound: @escaping (Bool) -> Void,
            onLink: @escaping (String) -> Void
        ) {
            self.onHeight = onHeight
            self.onQuotedFound = onQuotedFound
            self.onLink = onLink
        }

        func load(_ webView: WKWebView, html: String, allowRemote: Bool) {
            let signature = "\(allowRemote)|\(html.hashValue)"
            guard signature != loadedSignature else { return }
            loadedSignature = signature
            allowingInitialLoad = true
            webView.loadHTMLString(
                Self.document(html: html, allowRemote: allowRemote),
                // A nil base URL gives the document a unique opaque origin, so
                // relative references resolve to nothing rather than to us.
                baseURL: nil)
        }

        func setQuotesCollapsed(_ webView: WKWebView, collapsed: Bool) {
            webView.evaluateJavaScript(
                "window.__squelchSetQuotes && window.__squelchSetQuotes(\(collapsed))")
        }

        /// LAYER 4: allow exactly the initial in-memory load; refuse everything
        /// else. A link click cannot navigate this view anywhere.
        ///
        /// NOTE the exact signature — the closure must be `@MainActor @Sendable`
        /// or Swift treats this as an unrelated near-miss method, the delegate
        /// requirement goes unimplemented, and WebKit silently defaults to
        /// ALLOWING every navigation. This is a security layer; it has to match.
        func webView(
            _ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
            decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void
        ) {
            if allowingInitialLoad {
                allowingInitialLoad = false
                decisionHandler(.allow)
                return
            }
            // A user-initiated link becomes a SYSTEM BROWSER open (re-guarded to
            // http/https by Opener); anything else is silently dropped.
            if navigationAction.navigationType == .linkActivated,
                let url = navigationAction.request.url
            {
                onLink(url.absoluteString)
            }
            decisionHandler(.cancel)
        }

        /// No popups, ever.
        func webView(
            _ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
            for navigationAction: WKNavigationAction, windowFeatures: WKWindowFeatures
        ) -> WKWebView? {
            if let url = navigationAction.request.url { onLink(url.absoluteString) }
            return nil
        }

        func userContentController(
            _ controller: WKUserContentController, didReceive message: WKScriptMessage
        ) {
            guard let payload = message.body as? [String: Any],
                let kind = payload["kind"] as? String
            else { return }
            switch kind {
            case "height":
                if let value = payload["value"] as? Double { onHeight(CGFloat(value)) }
            case "quoted":
                if let value = payload["value"] as? Bool { onQuotedFound(value) }
            case "link":
                if let value = payload["value"] as? String { onLink(value) }
            default: break
            }
        }

        /// Build the full document. The CSP meta MUST be the first thing in
        /// <head> so it governs every subsequent resource.
        ///
        /// The reading surface is deliberately OPAQUE white: long-form mail is a
        /// reading page, not a glass panel, and body copy over a live wallpaper
        /// is unreadable. Mail also ships its own colors assuming a white
        /// canvas, so anything else breaks a large fraction of real messages.
        nonisolated static func document(html: String, allowRemote: Bool) -> String {
            let imgSrc = allowRemote ? "http: https: data:" : "data:"
            let csp = "default-src 'none'; style-src 'unsafe-inline'; img-src \(imgSrc)"
            return """
                <!doctype html><html><head>\
                <meta http-equiv="Content-Security-Policy" content="\(csp)">\
                <meta name="referrer" content="no-referrer">\
                <meta charset="utf-8">\
                <style>\
                html,body{margin:0;padding:14px;background:#fff;color:#111;\
                font:14px/1.55 -apple-system,BlinkMacSystemFont,'SF Pro Text',sans-serif;\
                word-break:break-word;overflow-wrap:anywhere;overflow:hidden;}\
                img{max-width:100%;height:auto;}\
                a{color:#2b7fd4;}\
                table{max-width:100%;}\
                blockquote{margin:8px 0;padding-left:10px;border-left:2px solid #d8dee6;color:#555;}\
                </style>\
                </head><body>\(html)</body></html>
                """
        }
    }
}
