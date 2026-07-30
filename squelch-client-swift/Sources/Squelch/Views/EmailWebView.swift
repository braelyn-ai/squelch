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
//     There is no script-src at all, and NO `http:`/`https:` anywhere in the
//     policy: the frame cannot reach the network directly at all. The img-src
//     gate is the per-message remote image decision (see REMOTE IMAGES).
//  4. NAVIGATION IS REFUSED. The delegate allows exactly one load — the initial
//     `loadHTMLString` — and cancels every other navigation. A click inside the
//     frame therefore cannot take the view anywhere; instead we hand the URL to
//     the SYSTEM BROWSER via Opener (which re-guards to http/https only).
//  5. No cookies, no persistent storage, and no WebKit-driven image loads: a
//     NON-persistent `WKWebsiteDataStore`, so the frame has no jar to read or
//     write and nothing survives the message being closed. Images do not ride
//     that loader at all — they ride ours (see REMOTE IMAGES).
//
// REMOTE IMAGES. Images load by default (real images are the point of HTML
// mail) but tracking pixels are stripped in a preprocessing pass first (see
// Trackers) and every surviving reference is REWRITTEN to `squelch-img:` (see
// ImageProxy), so the fetch happens in our own audited path — ImageStore:
// ephemeral session, no cookies ever, empty referrer, image/* responses only,
// redirects re-guarded per hop, bytes cached to disk under our own eviction
// policy — rather than inside WebKit where we could only watch. That is also
// what lets `img-src` drop `http: https:` and close the CSS-background tracking
// gap the sanitizer documents (squelch-core/src/sync/html.rs, "KNOWN
// TRADE-OFF"): a `url()` in a kept `<style>` block goes through the same proxy
// as an `<img>`, and anything the rewrite misses is simply blocked.
// `Referrer-Policy: no-referrer` remains set in the document too, so hosts learn
// nothing about which mail — or reader — asked for an image. When the Settings
// "load on demand" pref is on, `img-src` collapses to `data:` and the message
// shows a per-email opt-in bar instead: NO network request is made for mail the
// reader has not opted into, because the proxy scheme itself is refused by the
// CSP before any request reaches the handler.
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
// FRAME REUSE. A rendered frame is POOLED on teardown (WebFramePool) and handed
// back to the SAME message on reopen, so the second read costs no content-process
// attach, no parse, no layout and no measuring pass — FrameHeights makes a
// reopened message the right size, this makes it already drawn. It is also the
// one thing that outlives "the message being closed" (LAYER 5), so it is fenced
// rather than merely capped: the key is the message id AND the exact document
// AND the remote-image policy, so a frame can only ever be re-shown to the mail
// it already held, under the CSP it was built with. A body with no message id —
// the sealed-record reveal, which promises the opposite — is never pooled at
// all. Nothing new reaches disk: the store is the same non-persistent one, and
// the html is already resident in PreparedBodies either way.
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
    let cacheKey: String?
    /// Image srcs already shown by a CHRONOLOGICALLY EARLIER message in this
    /// thread; each is dropped from this one.
    ///
    /// The default is empty, not "off": a lone message still de-duplicates
    /// against ITSELF, which is what collapses the stack of signature copies a
    /// quoted history drags along behind it. Only cross-message suppression
    /// needs a thread to supply this.
    let seenEarlier: Set<String>

    @Environment(Prefs.self) private var prefs
    @Environment(AppStore.self) private var store

    @State private var height: CGFloat = 0
    @State private var optedIn = false
    @State private var hasQuoted = false
    @State private var quotedHidden = true
    @State private var measured = false

    /// The tracker-strip + dedupe + link-extraction + image-proxy pass for THIS
    /// body.
    ///
    /// Warmed at PREFETCH time (ThreadPrefetch fills PreparedBodies off the
    /// main actor as soon as a thread lands in the cache) and read back in
    /// `init`, so the ordinary open hands the frame a finished document on its
    /// first evaluation. Two separate costs are being removed, not one: the
    /// scans, which are a full regex walk of the body, and the runloop beat a
    /// `.task` needs before it can deliver anything — and the Coordinator
    /// refuses to load a placeholder, so that beat WAS the blank frame.
    ///
    /// The `.task` below is the cold path (opened before the warmer finished,
    /// or evicted), not the normal one.
    @State private var prepared: Prepared

    /// Explicit because `prepared` is seeded from the warm cache here, in the
    /// initializer — the earliest point that exists. `@State` cannot be
    /// assigned from `body`, and every later hook is a frame too late.
    init(html: String, cacheKey: String? = nil, seenEarlier: Set<String> = []) {
        self.html = html
        self.cacheKey = cacheKey
        self.seenEarlier = seenEarlier
        _prepared = State(
            initialValue: PreparedBodies.shared.get(Prepared.cacheKey(html, seenEarlier)) ?? .empty)
    }

    struct Prepared: Equatable, Sendable {
        var sourceHash: Int
        var html: String
        var blocked: Int
        var hasRemoteCandidates: Bool
        var links: [EmailLink]
        /// The ORIGINAL http(s) urls behind this body's proxied references, in
        /// document order, CAPPED (ImageProxy.maxWarmURLs). What the launch
        /// warmer pre-fetches and pins — not the full set of what the body
        /// renders, which is uncapped and loads on demand.
        var imageURLs: [String]

        static let empty = Prepared(
            sourceHash: 0, html: "", blocked: 0, hasRemoteCandidates: false, links: [],
            imageURLs: [])

        static func make(from html: String, seenEarlier: Set<String>) -> Prepared {
            // ORDER MATTERS: trackers come out FIRST, so a tracking pixel can
            // never be the "first occurrence" that suppresses a real image
            // further down the thread. Dedupe is layered on top of the security
            // pass, never in place of it.
            let stripped = Trackers.strip(html)
            let deduped = ImageRepeats.dropRepeats(stripped.html, alreadySeen: seenEarlier)
            // Read off the DEDUPED html: a message whose only images were
            // repeats has nothing left to fetch, so it must not offer the
            // "load remote images" bar for images it will never show.
            let hasRemoteCandidates = Trackers.hasNetworkImages(deduped)
            let links = Trackers.extractLinks(deduped)
            // LAST, and only after the two reads above: the proxy rewrite
            // replaces every http(s) image reference with a `squelch-img:` one,
            // which those scans would no longer recognise as remote.
            let proxied = ImageProxy.rewrite(deduped)
            return Prepared(
                sourceHash: cacheKey(html, seenEarlier),
                html: proxied.html,
                blocked: stripped.blocked,
                hasRemoteCandidates: hasRemoteCandidates,
                links: links,
                imageURLs: proxied.urls)
        }

        /// The suppression set is an input, so it has to be part of the identity
        /// of what was prepared — keying on the html alone would hand a message
        /// the previous thread's dedupe result.
        static func cacheKey(_ html: String, _ seenEarlier: Set<String>) -> Int {
            var hasher = Hasher()
            hasher.combine(html)
            hasher.combine(seenEarlier)
            return hasher.finalize()
        }
    }

    private var allowRemote: Bool { prefs.loadRemoteImages || optedIn }
    private static let maxLinks = 8

    /// Spin WebKit up BEFORE the first email is opened.
    ///
    /// The thread itself is already prefetched (ThreadPrefetch), so opening one
    /// costs no network — but the FIRST WKWebView in a process still pays for
    /// launching the content process, and that landed squarely on the first
    /// message the reader opened. This burns that cost at boot, on a throwaway
    /// frame that shares the real one's data store (and therefore its process
    /// pool and URL cache), then keeps it alive so the process is not reaped.
    ///
    /// Idempotent, and deliberately loads NOTHING but an empty document: it is
    /// a process warmer, not a cache.
    @MainActor
    static func warmProcess() {
        guard warmFrame == nil else { return }
        let config = WKWebViewConfiguration()
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        config.websiteDataStore = EmailWebViewRepresentable.sharedDataStore
        let frame = WKWebView(frame: .zero, configuration: config)
        frame.loadHTMLString("<html><body></body></html>", baseURL: nil)
        warmFrame = frame
    }

    @MainActor private static var warmFrame: WKWebView?

    /// Frame height shown before the first successful measurement.
    private static let placeholderHeight: CGFloat = 120
    /// Height used when measurement never arrives at all — generous on purpose:
    /// too tall leaves whitespace, too short silently hides mail.
    private static let unmeasuredFallbackHeight: CGFloat = 900

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            imageBar

            EmailWebViewRepresentable(
                html: prepared.html,
                allowRemote: allowRemote,
                collapseQuotes: quotedHidden,
                // The pool's message identity. nil is not "unkeyed", it is
                // NOT POOLABLE — see WebFramePool.Key.
                poolKey: cacheKey,
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
            // PAINT-HOLD: the frame keeps its (placeholder or remembered) space
            // but stays invisible until the first measurement lands, so the
            // reader never sees a half-laid-out document snap to size. This is
            // THE anti-incremental-paint mechanism — WebKit's own
            // `suppressesIncrementalRendering` is deliberately NOT used, because
            // it refuses to paint until every subresource (each remote image)
            // has finished downloading, which held fully-parsed mail hostage to
            // the slowest CDN. The measurement fires at document end, before
            // images: one coherent text+layout paint, images settle into it.
            .opacity(measured ? 1 : 0)
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
                // Same text-on-hover treatment as the header actions: this is
                // the only control INSIDE the reading surface, so a glass pill
                // here read as a second piece of chrome stapled to the mail.
                .buttonStyle(.textAction)
                .help("the quoted reply chain below this message")
            }

            linkRow
        }
        .onAppear {
            if let cacheKey, let remembered = FrameHeights.shared.get(cacheKey) {
                height = remembered
            }
        }
        // COLD PATH ONLY — `init` already seeded `prepared` from the warmer,
        // so this runs for a body opened before its thread finished warming
        // (or one whose entry has since been evicted). Off the main actor,
        // because the scans are exactly as expensive here as they are there,
        // and the result goes back into the cache so the next open is warm.
        .task(id: preparedKey) {
            guard prepared.sourceHash != preparedKey else { return }
            let key = preparedKey
            if let warm = PreparedBodies.shared.get(key) {
                prepared = warm
                return
            }
            let source = html
            let seen = seenEarlier
            let made = await Task.detached(priority: .userInitiated) {
                Prepared.make(from: source, seenEarlier: seen)
            }.value
            PreparedBodies.shared.set(key, made)
            guard !Task.isCancelled else { return }
            prepared = made
        }
        // SAFETY NET: the view is sized purely from what the measuring script
        // reports, so if that never arrives (a pathological document, a load
        // failure) the message would sit clipped at the placeholder height with
        // no way to read the rest. After a beat, fall back to a tall box.
        .task(id: prepared.sourceHash) {
            try? await Task.sleep(for: .seconds(2))
            guard !Task.isCancelled, !measured else { return }
            // A remembered height is still a good size; only a never-measured
            // first open needs the tall fallback. Either way the frame must
            // become VISIBLE — opacity is gated on `measured` alone now.
            if height == 0 { height = Self.unmeasuredFallbackHeight }
            measured = true
        }
    }

    private var preparedKey: Int { Prepared.cacheKey(html, seenEarlier) }
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
    /// Message id, or nil for a body that must never be pooled.
    let poolKey: String?
    let onHeight: (CGFloat) -> Void
    let onQuotedFound: (Bool) -> Void
    let onLink: (String) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(onHeight: onHeight, onQuotedFound: onQuotedFound, onLink: onLink)
    }

    /// What a LIVE frame would have to be holding to be reusable here, or nil
    /// if this render cannot use one (nothing to render yet, or no message id).
    private var renderKey: WebFramePool.Key? {
        guard let poolKey, !html.isEmpty else { return nil }
        return WebFramePool.Key(
            message: poolKey, allowRemote: allowRemote, document: html.hashValue)
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
    fileprivate static let sharedDataStore: WKWebsiteDataStore = .nonPersistent()

    func makeNSView(context: Context) -> PassthroughWebView {
        // POOL HIT: this exact document, for this exact message, under this
        // exact image policy, still parsed and laid out in a live frame. Adopt
        // it and DO NOT LOAD — `adopt` seeds the load guard so the updateNSView
        // that immediately follows is a no-op instead of a reload, which is the
        // entire point (a reload would throw the rendered frame away and flash
        // an empty one on the way back to the same pixels).
        if let key = renderKey, let entry = WebFramePool.shared.checkout(key) {
            context.coordinator.adopt(entry, key: key)
            return entry.webView
        }

        let config = WKWebViewConfiguration()

        // LAYER 2: page content cannot execute script, whatever it contains.
        // Our injected user script is governed separately and still runs.
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        // LAYER 5: no cookie jar, no persistent storage, nothing survives close.
        config.websiteDataStore = Self.sharedDataStore
        // Images are the ONE resource an email body is allowed to load, and they
        // come from here rather than from WebKit's loader — see ImageProxy. One
        // shared handler across every configuration is legal and keeps the
        // in-flight bookkeeping in a single place.
        config.setURLSchemeHandler(ImageSchemeHandler.shared, forURLScheme: ImageProxy.scheme)

        // The relay, not the coordinator, is what gets wired to the frame — it
        // outlives every coordinator that borrows the frame. See FrameRelay.
        let relay = FrameRelay()
        let controller = WKUserContentController()
        controller.add(relay, name: FrameRelay.name)
        controller.addUserScript(
            WKUserScript(
                source: Self.measuringScript, injectionTime: .atDocumentEnd,
                forMainFrameOnly: true))
        config.userContentController = controller

        let webView = PassthroughWebView(frame: .zero, configuration: config)
        // WKWebView holds its delegates WEAKLY; the relay stays alive because
        // the content controller retains its message handlers and the frame
        // retains the controller. That retention is load-bearing — removing the
        // handler (WebFramePool.discard) is therefore also what unwires the
        // delegates for good.
        webView.navigationDelegate = relay
        webView.uiDelegate = relay
        webView.setValue(false, forKey: "drawsBackground")
        webView.allowsBackForwardNavigationGestures = false
        webView.allowsMagnification = false

        context.coordinator.attach(relay)
        context.coordinator.load(webView, html: html, allowRemote: allowRemote, poolKey: poolKey)
        return webView
    }

    func updateNSView(_ webView: PassthroughWebView, context: Context) {
        context.coordinator.onHeight = onHeight
        context.coordinator.onQuotedFound = onQuotedFound
        context.coordinator.onLink = onLink
        // A content/policy change is a genuine reload; a quote toggle is not
        // (reloading for it would flash an empty frame).
        context.coordinator.load(webView, html: html, allowRemote: allowRemote, poolKey: poolKey)
        context.coordinator.setQuotesCollapsed(webView, collapsed: collapseQuotes)
    }

    /// Return the frame to the pool rather than dropping it — the whole reason
    /// reopening a read message is instant.
    static func dismantleNSView(_ webView: PassthroughWebView, coordinator: Coordinator) {
        coordinator.release(webView)
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

          // Declared UP HERE because the quoted-history collapse below runs
          // before the height section and calls measure() itself. Left where it
          // read more naturally, `last` was still undefined on that first call,
          // every comparison against it was NaN, and the first measurement — the
          // one that sizes the frame — was silently thrown away.
          var last = -1;

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
          // Measure the CONTENT, never documentElement.scrollHeight.
          //
          // The root's scrollHeight is floored at the VIEWPORT height, and the
          // viewport here IS the frame we are trying to size. Once the frame had
          // grown to show quoted history, collapsing it again measured the grown
          // frame (2240) instead of the shrunken content (112) — an unchanged
          // number, which `last` then swallowed, so the host was never told and
          // the mail kept a screenful of blank space under it. Only the body
          // reports what the document actually needs.
          //
          // Both readings are content-based: scrollHeight also covers children
          // that overflow the body box (floats, absolutely positioned tables),
          // which the bounding rect alone would miss.
          function contentHeight() {
            var b = document.body;
            if (!b) {
              return document.documentElement ? document.documentElement.scrollHeight : 0;
            }
            return Math.max(b.scrollHeight, Math.ceil(b.getBoundingClientRect().height));
          }
          function measure() {
            var h = contentHeight();
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

          // Called by the host when a POOLED frame is handed to a new view.
          // Nothing here re-runs on a document that was not reloaded, so the
          // new host would otherwise know neither the height nor whether there
          // is a quoted chain to offer. Clearing `last` is the whole trick:
          // measure() suppresses an unchanged height, and for a frame being
          // re-shown unchanged is exactly what it is.
          window.__squelchResend = function () {
            send({ kind: 'quoted', value: quoteNodes.length > 0 });
            last = -1;
            measure();
          };

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
    final class Coordinator: NSObject {
        var onHeight: (CGFloat) -> Void
        var onQuotedFound: (Bool) -> Void
        var onLink: (String) -> Void

        /// The frame's permanent hinge, borrowed for as long as this
        /// representable owns the frame. Held strongly so `release` can still
        /// hand it to the pool after WebKit has let go of everything else.
        private var relay: FrameRelay?

        /// What is currently loaded, so `updateNSView` only reloads on a real
        /// content/policy change (a reload flashes an empty frame).
        private var loadedSignature: String?
        /// The pool identity of what the frame CURRENTLY HOLDS — re-derived on
        /// every real load, never remembered from checkout. The two diverge
        /// exactly when getting it wrong would matter: the remote-image opt-in
        /// reloads a frame that is already checked out, so a frame borrowed
        /// under `allowRemote: false` can go back holding a permissive-CSP
        /// document, and must be filed as such.
        private var loadedPoolKey: WebFramePool.Key?
        /// Set for exactly the initial load; every other navigation is refused.
        /// Navigations WE started that have not yet been through the policy
        /// gate. A COUNTER, not a flag: `loadHTMLString` is asynchronous, so two
        /// loads in quick succession produce two policy callbacks racing for one
        /// permission. With a bool, whichever arrived first consumed it and the
        /// OTHER navigation was cancelled — and when the loser was the real
        /// document, the frame stayed blank forever while still reporting a
        /// height. Counting means every load we start is allowed exactly once.
        private var pendingOwnLoads = 0

        init(
            onHeight: @escaping (CGFloat) -> Void, onQuotedFound: @escaping (Bool) -> Void,
            onLink: @escaping (String) -> Void
        ) {
            self.onHeight = onHeight
            self.onQuotedFound = onQuotedFound
            self.onLink = onLink
        }

        /// The load guard and the pool key say the same thing about a document,
        /// so they are derived from one place: if they could drift, an adopted
        /// frame would fail its own guard and reload the document it is already
        /// showing.
        private static func signature(allowRemote: Bool, document: Int) -> String {
            "\(allowRemote)|\(document)"
        }

        func load(_ webView: WKWebView, html: String, allowRemote: Bool, poolKey: String?) {
            // NEVER LOAD THE PLACEHOLDER. `prepared` starts empty and is filled
            // by a `.task`, so the first body pass used to load a blank document
            // and the real one landed as a SECOND navigation a beat later. That
            // is what made the race below reachable at all.
            guard !html.isEmpty else { return }
            let document = html.hashValue
            let signature = Self.signature(allowRemote: allowRemote, document: document)
            guard signature != loadedSignature else { return }
            loadedSignature = signature
            loadedPoolKey = poolKey.map {
                WebFramePool.Key(message: $0, allowRemote: allowRemote, document: document)
            }
            relay?.noteLoadStarted()
            pendingOwnLoads += 1
            webView.loadHTMLString(
                Self.document(html: html, allowRemote: allowRemote),
                // A nil base URL gives the document a unique opaque origin, so
                // relative references resolve to nothing rather than to us.
                baseURL: nil)
        }

        /// Take ownership of a freshly built frame.
        func attach(_ relay: FrameRelay) {
            self.relay = relay
            relay.owner = self
        }

        /// Take ownership of a frame that is ALREADY showing this document.
        ///
        /// Re-pointing `relay.owner` is the entire handover: the frame's
        /// message handler and both delegates are the relay, permanently, so
        /// there is nothing on the frame to rewire and no window in which a
        /// callback could reach the previous message's coordinator.
        func adopt(_ entry: WebFramePool.Entry, key: WebFramePool.Key) {
            attach(entry.relay)
            // The frame is already loaded, so no load may be started for it —
            // and `pendingOwnLoads` stays 0, which is correct: LAYER 4 has no
            // permission outstanding to grant.
            loadedSignature = Self.signature(allowRemote: key.allowRemote, document: key.document)
            loadedPoolKey = key

            // A document that was not reloaded never re-runs the measuring
            // script, so a reused frame would sit at zero height behind the
            // paint-hold and the quoted-history chip would vanish. Two answers,
            // both wanted: the relay already knows what THIS document last
            // reported, which releases the hold on the very next main-actor
            // turn with no round trip; and the page is asked to re-send, which
            // is what makes it right if the frame comes back at a new width.
            //
            // The hop is not optional — makeNSView runs inside a view update,
            // and these callbacks write @State.
            let height = entry.relay.lastHeight
            let quoted = entry.relay.lastQuoted
            Task { @MainActor [weak self] in
                guard let self else { return }
                self.onQuotedFound(quoted)
                if height > 0 { self.onHeight(height) }
            }
            entry.webView.evaluateJavaScript("window.__squelchResend && window.__squelchResend()")
        }

        /// Give the frame up: to the pool if it is worth keeping, otherwise to
        /// the teardown path.
        func release(_ webView: PassthroughWebView) {
            guard let relay else { return }
            relay.owner = nil
            let entry = WebFramePool.Entry(webView: webView, relay: relay)
            self.relay = nil
            // An unfinished or failed load is not merely worthless to keep, it
            // is a trap: the next owner adopts with no pending-load permission,
            // so the still-in-flight navigation would be refused by LAYER 4 and
            // the frame would come back blank. `loadedPoolKey` is nil for a body
            // with no message id, which is what keeps sealed reveals out.
            guard relay.loaded, let key = loadedPoolKey else {
                WebFramePool.discard(entry)
                return
            }
            WebFramePool.shared.checkIn(entry, key: key)
        }

        func setQuotesCollapsed(_ webView: WKWebView, collapsed: Bool) {
            webView.evaluateJavaScript(
                "window.__squelchSetQuotes && window.__squelchSetQuotes(\(collapsed))")
        }

        /// LAYER 4: allow exactly the in-memory loads WE started; refuse everything
        /// else. A link click cannot navigate this view anywhere.
        ///
        /// Called by the relay, which is the frame's actual delegate — the
        /// counter lives here because only the owner starts loads.
        func decideNavigation(_ navigationAction: WKNavigationAction) -> WKNavigationActionPolicy {
            if pendingOwnLoads > 0 {
                pendingOwnLoads -= 1
                return .allow
            }
            // A user-initiated link becomes a SYSTEM BROWSER open (re-guarded to
            // http/https by Opener); anything else is silently dropped.
            if navigationAction.navigationType == .linkActivated,
                let url = navigationAction.request.url
            {
                onLink(url.absoluteString)
            }
            return .cancel
        }

        /// Build the full document. The CSP meta MUST be the first thing in
        /// <head> so it governs every subsequent resource.
        ///
        /// The reading surface is deliberately OPAQUE white: long-form mail is a
        /// reading page, not a glass panel, and body copy over a live wallpaper
        /// is unreadable. Mail also ships its own colors assuming a white
        /// canvas, so anything else breaks a large fraction of real messages.
        nonisolated static func document(html: String, allowRemote: Bool) -> String {
            // `squelch-img:` and NOT `http: https:` — every remote image was
            // rewritten to the proxy scheme (ImageProxy), so the network is
            // reachable only through ImageSchemeHandler and anything the rewrite
            // missed fails closed. `data:` stays for inline art.
            //
            // This is ALSO the load-on-demand gate, and it is the whole gate: an
            // un-opted message has no `squelch-img:` in its policy, so the
            // request is refused by the document and the handler is never
            // reached. Nothing downstream has to re-check the pref.
            let imgSrc = allowRemote ? "squelch-img: data:" : "data:"
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

// MARK: - the live frame pool

/// A frame's ONE permanent connection to native code: script message handler,
/// navigation delegate and UI delegate, installed at construction and never
/// replaced.
///
/// A pooled frame outlives the representable that built it, so the naive
/// handover — re-`add` each new Coordinator as the "squelch" handler — is not
/// available: `WKUserContentController` retains its handlers and will not hold
/// two under one name, so that path either throws or pins the previous
/// message's coordinator (and its closures into a torn-down SwiftUI view) to
/// the frame for good. Instead the wiring never changes and OWNERSHIP does, in
/// one assignment. A callback can therefore never reach a stale coordinator:
/// there is exactly one owner slot, and it is nil while the frame sits in the
/// pool.
///
/// Being unowned also has to be SAFE, not merely quiet, so this is where LAYER
/// 4 is anchored: a frame with no owner refuses every navigation outright. And
/// the height/quoted values keep being recorded with no owner attached, which
/// is what lets a check-in settle the frame back to its collapsed default and
/// still leave the pool a truthful measurement for the next open.
@MainActor
private final class FrameRelay: NSObject, WKScriptMessageHandler, WKNavigationDelegate,
    WKUIDelegate
{
    static let name = "squelch"

    /// Weak on purpose: SwiftUI owns coordinators, and a frame must never be
    /// the reason a dead view's callbacks stay alive.
    weak var owner: EmailWebViewRepresentable.Coordinator?

    /// What THIS document last told us. Read at checkout, because the measuring
    /// script does not re-run for a frame that was not reloaded.
    private(set) var lastHeight: CGFloat = 0
    private(set) var lastQuoted = false
    /// Whether the current document finished loading. Only a settled frame is
    /// worth pooling, and only a settled frame is safe to pool — see
    /// Coordinator.release.
    private(set) var loaded = false

    func noteLoadStarted() {
        loaded = false
        lastHeight = 0
        lastQuoted = false
    }

    func userContentController(
        _ controller: WKUserContentController, didReceive message: WKScriptMessage
    ) {
        guard let payload = message.body as? [String: Any],
            let kind = payload["kind"] as? String
        else { return }
        switch kind {
        case "height":
            guard let value = payload["value"] as? Double, value > 0 else { return }
            lastHeight = CGFloat(value)
            owner?.onHeight(lastHeight)
        case "quoted":
            guard let value = payload["value"] as? Bool else { return }
            lastQuoted = value
            owner?.onQuotedFound(value)
        // Dropped when there is no owner, which is right: a pooled frame is off
        // screen and cannot be clicked, so the only such message would be a
        // stale one.
        case "link":
            if let value = payload["value"] as? String { owner?.onLink(value) }
        default: break
        }
    }

    /// LAYER 4 — see Coordinator.decideNavigation for the policy itself.
    ///
    /// NOTE the exact signature — the closure must be `@MainActor @Sendable`
    /// or Swift treats this as an unrelated near-miss method, the delegate
    /// requirement goes unimplemented, and WebKit silently defaults to ALLOWING
    /// every navigation. This is a security layer; it has to match.
    func webView(
        _ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void
    ) {
        // No owner means the frame is parked in the pool: nobody is entitled to
        // navigate it, so nothing may.
        guard let owner else {
            decisionHandler(.cancel)
            return
        }
        decisionHandler(owner.decideNavigation(navigationAction))
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        loaded = true
    }

    func webView(
        _ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error
    ) {
        loaded = false
    }

    func webView(
        _ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!,
        withError error: Error
    ) {
        loaded = false
    }

    /// No popups, ever.
    func webView(
        _ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
        for navigationAction: WKNavigationAction, windowFeatures: WKWindowFeatures
    ) -> WKWebView? {
        if let url = navigationAction.request.url { owner?.onLink(url.absoluteString) }
        return nil
    }
}

/// LRU pool of LIVE, ALREADY-RENDERED email frames.
///
/// Reopening a message the reader just read used to redo all of it: content
/// process attach, parse, layout, measuring pass, image reattachment.
/// FrameHeights already made the reopened frame the right SIZE on the first
/// paint; this makes it the same frame, still drawn.
@MainActor
private final class WebFramePool {
    static let shared = WebFramePool()

    /// SMALL ON PURPOSE. Each entry is a live WebKit content-process attachment
    /// plus a retained layout tree for a whole email — the expensive thing we
    /// are keeping is precisely the thing that costs memory, so this is not a
    /// cache to be generous with. Six covers walking a thread back and forth
    /// and flipping between the last few messages, which is the motion the pool
    /// exists for; beyond that the parse we are avoiding is cheaper than the
    /// residency we would be paying for.
    private static let capacity = 6

    /// The identity of a RENDERED DOCUMENT, not of a message.
    ///
    /// All three fields, exactly, or it is not a match and a fresh frame is
    /// built:
    ///  - `message` makes cross-message reuse structurally impossible. The
    ///    document hash alone would already have to collide for two bodies to
    ///    be confused, but "one sender's mail inside another's frame" is not a
    ///    thing to leave to a hash, so the message id is part of the key.
    ///  - `allowRemote` IS the CSP. It is baked into the document text, and a
    ///    WKWebView's configuration is immutable after creation besides, so a
    ///    frame rendered under `img-src squelch-img: data:` is unreachable from
    ///    a render that asked for `img-src data:`. The remote-image opt-in is a
    ///    pool MISS, never a silently permissive reuse.
    ///  - `document` is the exact html handed to the frame, which already folds
    ///    in the tracker strip, the cross-message image dedupe and the proxy
    ///    rewrite (Prepared) — it is the same value Coordinator's load guard
    ///    uses, for the same reason.
    ///
    /// Which together give the property the reuse actually rests on: what the
    /// frame is showing is `Coordinator.document(html:allowRemote:)`, a pure
    /// function of two of these three fields, so a key match means the parked
    /// frame is displaying a BYTE-IDENTICAL document — same CSP meta, same
    /// referrer policy, same everything. Nothing about a recycled frame is
    /// weaker than a fresh one; the rest of the sandbox rides its configuration,
    /// which WebKit will not let anyone change after construction anyway.
    ///
    /// Quote collapse is deliberately NOT in the key: it is a JS call on the
    /// live document, never a reload, so it cannot invalidate a frame. Frames
    /// are checked in collapsed instead, so a hit matches a fresh open's
    /// default state and nothing has to snap on reopen.
    struct Key: Hashable {
        let message: String
        let allowRemote: Bool
        let document: Int
    }

    struct Entry {
        let webView: PassthroughWebView
        let relay: FrameRelay
    }

    private var frames: [Key: Entry] = [:]
    /// Least-recently-used first.
    private var order: [Key] = []

    private init() {}

    /// A parked frame holding exactly this document, or nil.
    ///
    /// Checkout REMOVES it, which is the whole answer to a frame being in use:
    /// a borrowed frame is not in the pool, so two views of one message (a
    /// reader and a reveal, say) can never be handed the same WKWebView — the
    /// second builds fresh, and whichever is torn down last is the one that
    /// ends up parked.
    func checkout(_ key: Key) -> Entry? {
        guard let entry = frames.removeValue(forKey: key) else { return nil }
        order.removeAll { $0 == key }
        return entry
    }

    func checkIn(_ entry: Entry, key: Key) {
        entry.relay.owner = nil
        entry.webView.removeFromSuperview()
        // Settle back to the state a fresh open expects. Without this a frame
        // parked with its quoted history expanded would be re-shown tall and
        // then snap shorter when the new view applied its own (collapsed)
        // default — a flash, in the one code path whose entire purpose is not
        // having one. The relay records the resulting height with no owner
        // attached, so it is the height the next checkout reports.
        entry.webView.evaluateJavaScript(
            "window.__squelchSetQuotes && window.__squelchSetQuotes(true)")

        // Two frames of one document: keep the one just used.
        if let incumbent = frames.removeValue(forKey: key) {
            order.removeAll { $0 == key }
            Self.discard(incumbent)
        }
        frames[key] = entry
        order.append(key)

        while order.count > Self.capacity {
            let oldest = order.removeFirst()
            if let evicted = frames.removeValue(forKey: oldest) { Self.discard(evicted) }
        }
    }

    /// A dropped frame has to be UNWIRED, not just released: the content
    /// controller retains the relay and the relay is the frame's delegate, so
    /// simply letting go leaves a live object WebKit can still deliver a late
    /// navigation or script callback into.
    static func discard(_ entry: Entry) {
        entry.relay.owner = nil
        entry.webView.stopLoading()
        entry.webView.navigationDelegate = nil
        entry.webView.uiDelegate = nil
        let controller = entry.webView.configuration.userContentController
        controller.removeScriptMessageHandler(forName: FrameRelay.name)
        controller.removeAllUserScripts()
        entry.webView.removeFromSuperview()
    }
}
