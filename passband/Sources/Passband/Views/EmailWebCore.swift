// THE READING SURFACE, minus the platform. Everything a rendered email is made
// of lives here — the body-preparation pipeline, the five sandbox layers wired
// into a WKWebViewConfiguration, the injected measuring script, the message
// relay and the live-frame pool — and the ONLY thing left outside is the
// representable that hands a WKWebView to a view system: `EmailWebView.swift`
// (AppKit) and `PassbandiOS/Views/EmailWebViewiOS.swift` (UIKit). Both define
// `EmailWebViewRepresentable` with the same initializer, so the SwiftUI view
// below is one view on both platforms rather than a pair that can drift.
//
// Five independent sandbox layers (page JS off, injected CSP, navigation
// refused, non-persistent store, images proxied through our own fetch path):
// see docs/SECURITY.md. The frame is sized to its measured content and never
// scrolls itself; the thread column is the one scroll.

import SwiftUI
import WebKit

struct EmailWebView: View {
    let html: String
    /// Stable id (message id) for the height memory.
    let cacheKey: String?
    /// The sender is somebody the user has written to (`sender_known` on the
    /// wire), so this body's tracking pixels are left in the document: a trusted
    /// correspondent is allowed to learn the mail was opened. Everyone else is
    /// stripped exactly as before, and the default is the strict one.
    let allowTrackers: Bool
    /// This message's parts, for the `cid:` references in the body. Empty is a
    /// real answer, not a missing one: a body with nothing to resolve against
    /// drops every cid reference it carries, which is what sealed mail wants.
    let attachments: [Attachment]
    /// WHAT THIS BODY'S TEXT IS GUESSED TO DRAW AS (MinimapGeometry.textHeight),
    /// or 0 from a caller with no guess to offer.
    ///
    /// It is the height the frame is given until the document measures itself,
    /// and that is a scrolling concern, not a cosmetic one: a message that
    /// mounts at a flat placeholder and then snaps to its real height moves
    /// every message after it by the difference, under somebody who is in the
    /// middle of reading. A guess that is roughly right turns that shove into a
    /// nudge.
    let textHeight: CGFloat

    @Environment(Prefs.self) private var prefs
    @Environment(AppStore.self) private var store

    @State private var height: CGFloat = 0
    @State private var optedIn = false
    @State private var hasQuoted = false
    @State private var quotedHidden = true
    @State private var measured = false
    @State private var sizing: SizingPhase = .opening
    /// Whether the current sizing round was started by the READER doing
    /// something — see `apply`, which defers to the measuring pass except when
    /// this is set.
    @State private var sizingIsUserDriven = false

    /// HOW MANY OF THIS DOCUMENT'S REPORTED HEIGHTS ARE ALLOWED TO BECOME THE
    /// FRAME'S HEIGHT, and when. Exactly two, and both at a moment that means
    /// something.
    ///
    /// The frame's height is the document's layout viewport, so applying every
    /// report is a closed loop for any body whose height depends on the
    /// viewport (see `EmailFrameCoordinator.document`). Applying two bounds the
    /// error at one round trip — measured, that is 28 points — instead of
    /// letting it run at 840 points a second.
    ///
    /// `opening` takes the first thing the document says, which is what gets a
    /// message on screen at roughly the right size. Then nothing is applied
    /// until WebKit reports the navigation finished, which is AFTER its images,
    /// at which point the host asks the document once what it came to and stops
    /// listening. A user action that genuinely changes the content — unfolding
    /// the quoted history, letting the remote images in — puts it back to
    /// `opening`, because that is a new document to size.
    private enum SizingPhase { case opening, waiting, settling, done }

    /// The tracker-strip + dedupe + link-extraction + image-proxy pass for THIS
    /// body, warmed off the main actor at prefetch time (ThreadPrefetch) and read
    /// back in `init` — the Coordinator refuses to load a placeholder, so an
    /// unprepared body is a blank frame. The `.task` below is the cold path.
    @State private var prepared: Prepared

    /// `prepared` is seeded from the warm cache HERE: `@State` cannot be assigned
    /// from `body`, and every later hook is a frame too late.
    init(
        html: String, cacheKey: String? = nil, allowTrackers: Bool = false,
        attachments: [Attachment] = [], textHeight: CGFloat = 0
    ) {
        self.html = html
        self.cacheKey = cacheKey
        self.allowTrackers = allowTrackers
        self.attachments = attachments
        self.textHeight = textHeight
        _prepared = State(
            initialValue: PreparedBodies.shared.get(
                Prepared.cacheKey(html, allowTrackers, attachments)) ?? .empty)
    }

    struct Prepared: Equatable, Sendable {
        var sourceHash: Int
        var html: String
        /// Tracking pixels the strip pass FOUND. Removed from `html` unless
        /// `trackersAllowed`, in which case they are still in the document and
        /// this is only what the badge reports.
        var trackers: Int
        var trackersAllowed: Bool
        var hasRemoteCandidates: Bool
        /// The ORIGINAL http(s) urls behind this body's proxied references, in
        /// document order, capped (ImageProxy.maxWarmURLs) — what the launch
        /// warmer pre-fetches and pins, not the full uncapped render set.
        var imageURLs: [String]

        static let empty = Prepared(
            sourceHash: 0, html: "", trackers: 0, trackersAllowed: false,
            hasRemoteCandidates: false, imageURLs: [])

        static func make(
            from html: String, allowTrackers: Bool = false, attachments: [Attachment] = []
        ) -> Prepared {
            // The pass runs either way — one regex sweep, and its COUNT is what
            // the badge reports — but a known sender's body keeps its pixels.
            // ORDER MATTERS when it does strip: trackers come out FIRST, so a
            // tracking pixel can never be the "first occurrence" that suppresses
            // a real image further down the thread. Dedupe layers on top, never
            // in place of it.
            let stripped = Trackers.strip(html)
            let body = allowTrackers ? html : stripped.html
            // A repeated sender logo is still part of THIS message. Suppressing
            // it because an older message used the same URL made current mail
            // look broken (and could erase every image in a short reply). Keep
            // the within-body pass for quoted-history duplicates, but do not let
            // an earlier message remove content from this one.
            let deduped = ImageRepeats.dropRepeats(body)
            // Read off the DEDUPED html: a message whose only images were
            // repeats has nothing left to fetch, so it must not offer the
            // "load remote images" bar.
            let hasRemoteCandidates = Trackers.hasNetworkImages(deduped)
            // BETWEEN the two, and it has to be: the dedupe never touches a cid
            // reference (they are message-specific, not the repeated chrome it
            // removes), while the proxy must not see what this mints — a
            // `passband-cid:` src is not http(s), so it would be left alone, but
            // running before it is what keeps that a fact rather than a
            // coincidence of ImageProxy's prefix test.
            let inlined = CidImages.rewrite(html: deduped, attachments: attachments)
            // LAST, after the read above: the rewrite replaces every http(s)
            // image reference with a `passband-img:` one, which those scans would
            // no longer recognise as remote.
            let proxied = ImageProxy.rewrite(inlined)
            return Prepared(
                sourceHash: cacheKey(html, allowTrackers, attachments),
                html: proxied.html,
                trackers: stripped.blocked,
                trackersAllowed: allowTrackers,
                hasRemoteCandidates: hasRemoteCandidates,
                imageURLs: proxied.urls)
        }

        /// Tracker policy is part of the identity: the two policies produce
        /// different documents from the same body.
        ///
        /// So are the PARTS, for the same reason: the cid rewrite resolves
        /// against them, so one body quoted into two messages carrying different
        /// attachments prepares into two different documents. Everything the
        /// rewrite reads is folded in — the id and content-id it matches on, and
        /// the three fields of the inline gate — because a hit on a stale key
        /// would paste one message's photo into another's.
        static func cacheKey(
            _ html: String, _ allowTrackers: Bool, _ attachments: [Attachment]
        ) -> Int {
            var hasher = Hasher()
            hasher.combine(html)
            hasher.combine(allowTrackers)
            for att in attachments {
                hasher.combine(att.id)
                hasher.combine(att.content_id)
                hasher.combine(att.downloadable)
                hasher.combine(att.mime)
                hasher.combine(att.size)
            }
            return hasher.finalize()
        }
    }

    private var allowRemote: Bool { prefs.loadRemoteImages || optedIn }

    /// Spin WebKit up BEFORE the first email is opened. Two separate costs are
    /// being paid here, and only one of them used to be: the FIRST WKWebView the
    /// app ever builds pays a one-off framework initialisation (measured at
    /// 53-58ms), and EVERY new frame pays some seventy milliseconds of content
    /// process launch before it can draw anything.
    ///
    /// It used to be a throwaway frame, which bought the first of those and
    /// nothing else — the first message opened still faced a blank box while its
    /// own process came up. The warmer is a SPARE instead: one real frame, the
    /// same cost, held in the drawer the reader takes from. Idempotent.
    @MainActor
    static func warmProcess() {
        WebFramePool.shared.seedSpare(EmailWebViewRepresentable.buildSpare)
    }

    /// Frame height shown before the first successful measurement, for a body
    /// whose caller offered no guess at all. A LAST RESORT: a frame that opens
    /// at this and measures at ten times it is the thread lurching.
    private static let placeholderHeight: CGFloat = 120
    /// The document's own top and bottom padding (the 14px in `document`),
    /// which a caller counting lines of text knows nothing about.
    private static let documentPadding: CGFloat = 28
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
                    apply(h)
                },
                onLoaded: {
                    // The document and its subresources are in. Whatever it
                    // says next is the size it came to; after that the frame
                    // stops being a party to its own layout.
                    if sizing == .waiting { sizing = .settling }
                },
                onQuotedFound: { hasQuoted = $0 },
                onLink: { Opener.open($0) }
            )
            .frame(height: displayHeight)
            // PAINT-HOLD: the frame keeps its space but stays invisible until the
            // first measurement lands, so no half-laid-out document snaps to size.
            // WebKit's `suppressesIncrementalRendering` is deliberately NOT used:
            // it waits for every subresource, holding parsed mail hostage to the
            // slowest CDN. Measurement fires at document end, before images.
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
                // Text-on-hover like the header actions: the only control INSIDE
                // the reading surface, where a glass pill reads as chrome.
                .buttonStyle(.textAction)
                .help("the quoted reply chain below this message")
            }
        }
        .onAppear {
            if let cacheKey, let remembered = FrameHeights.shared.get(cacheKey) {
                height = remembered
            }
        }
        // A NEW DOCUMENT TO SIZE. Unfolding the quoted history and letting the
        // remote images in are the two things that legitimately change what
        // this message is; everything else that reaches the frame is the frame
        // talking to itself.
        .onChange(of: quotedHidden) { _, _ in restartSizing(userDriven: true) }
        .onChange(of: allowRemote) { _, _ in restartSizing(userDriven: true) }
        // And a different BODY in the same frame, which is not a user action
        // but is still a new document to size: the cold path finishing its
        // preparation, or this view being reused for another message as the
        // stack recycles. Without it the new document's first measurement
        // arrives into a machine that has already stopped listening.
        .onChange(of: prepared.sourceHash) { _, _ in restartSizing(userDriven: false) }
        // COLD PATH ONLY — `init` already seeded `prepared` from the warmer; this
        // runs for a body opened before its thread warmed (or since evicted). Off
        // the main actor: the scans are a full regex walk of the body.
        .task(id: preparedKey) {
            guard prepared.sourceHash != preparedKey else { return }
            let key = preparedKey
            if let warm = PreparedBodies.shared.get(key) {
                prepared = warm
                return
            }
            let source = html
            let allow = allowTrackers
            let parts = attachments
            let made = await Task.detached(priority: .userInitiated) {
                Prepared.make(from: source, allowTrackers: allow, attachments: parts)
            }.value
            PreparedBodies.shared.set(key, made)
            guard !Task.isCancelled else { return }
            prepared = made
        }
        // SAFETY NET: the view is sized purely from what the measuring script
        // reports, so if that never arrives fall back to a tall box rather than
        // leaving the message clipped at the placeholder height.
        .task(id: prepared.sourceHash) {
            try? await Task.sleep(for: .seconds(2))
            guard !Task.isCancelled, !measured else { return }
            // A REMEMBERED MEASUREMENT STANDS: the frame is already that size,
            // so it becomes visible without also lurching — which is what
            // reaching straight for the fallback did to every message that had
            // a perfectly good size on file.
            //
            // With nothing measured, the tall fallback still wins, and it wins
            // over the guess too. A frame that never reported is a frame
            // nothing verified, and the guess counts text and scores pictures
            // at zero: too tall leaves whitespace, too short silently hides
            // mail, and only one of those is recoverable by scrolling.
            if height == 0 {
                height = rememberedHeight ?? max(guessedHeight ?? 0, Self.unmeasuredFallbackHeight)
            }
            measured = true
        }
    }

    /// A new document to size, and whether the reader asked for it.
    private func restartSizing(userDriven: Bool) {
        sizing = .opening
        sizingIsUserDriven = userDriven
    }

    /// Take a reported height, or decline it. THE ONE PLACE a measurement ever
    /// becomes the frame's size.
    private func apply(_ h: CGFloat) {
        guard h > 0, h <= Self.sanityCeiling else {
            trace("refused \(Int(h))")
            return
        }
        // A MEASUREMENT TAKEN AT A FIXED VIEWPORT BEATS ANYTHING THIS FRAME CAN
        // WORK OUT, and for one kind of body it is the only real answer there
        // is. A document written in `vh` units has no intrinsic height — it is
        // one viewport tall, whatever viewport you ask at, so `h = h + padding`
        // has no solution — and a frame deriving it from its own box just
        // returns wherever it started plus a couple of round trips. Measured:
        // opened at the placeholder, a full-height email settled at 176 points
        // against 828 for the same document asked at a real window.
        //
        // Not for a user action, though: unfolding the quoted history or
        // letting the images in makes this a different document from the one
        // the pass measured, and then the frame in front of somebody is the
        // only thing that knows.
        if !sizingIsUserDriven, let key = cacheKey,
            let measuredHeight = FrameHeights.shared.authoritative(key)
        {
            height = measuredHeight
            measured = true
            sizing = .done
            trace("deferred to measured \(Int(measuredHeight)), offered \(Int(h))")
            return
        }
        switch sizing {
        case .opening: sizing = .waiting
        case .settling: sizing = .done
        // Between the opening measurement and the navigation finishing, the
        // document is still arriving and every report it makes is a report
        // about a frame we sized from its last one. Nothing to learn there.
        case .waiting, .done:
            trace("ignored \(Int(h)) in \(sizing)")
            return
        }
        trace("applied \(Int(height)) -> \(Int(h)) (\(sizing))")
        height = h
        measured = true
        // The memory stores the DEFAULT (collapsed) state only — and stores
        // this as PROVISIONAL, because it was taken inside a box this frame had
        // sized itself. It holds the message at a sane size until the pass
        // reaches it, and never displaces what the pass finds.
        if let cacheKey, quotedHidden { FrameHeights.shared.set(cacheKey, h) }
    }

    /// Nothing this tall is an email. A number past here is a runaway — the
    /// frame growing on its own layout — and letting one reach the cache is
    /// what used to make a poisoned message stay poisoned for the session.
    private static let sanityCeiling: CGFloat = 60_000

    /// EVERY HEIGHT THIS FRAME IS OFFERED, and what became of it, in developer
    /// mode. A message resizing itself is the one bug here that is invisible
    /// from the outside until it is enormous, and a line per decision is the
    /// difference between seeing it happen and theorising about it: a healthy
    /// message prints two `applied` lines and stops, and a message fighting its
    /// own layout prints a wall of `ignored` ones climbing by 28.
    private func trace(_ what: String) {
        guard prefs.developerMode, let cacheKey else { return }
        print("[passband.height] msg \(cacheKey) \(what)")
    }

    private var preparedKey: Int { Prepared.cacheKey(html, allowTrackers, attachments) }
    private var rememberedHeight: CGFloat? { cacheKey.flatMap { FrameHeights.shared.get($0) } }
    /// What the caller's line count says this document will come to, or nil
    /// when it offered none. Never remembered as a height: it is a guess, and
    /// `FrameHeights` holds measurements.
    private var guessedHeight: CGFloat? {
        textHeight > 0 ? textHeight + Self.documentPadding : nil
    }
    /// MEASURED, then REMEMBERED, then GUESSED. The order is the confidence
    /// order, and everything above the last resort exists so that the frame
    /// which lands its measurement mid-scroll corrects by a little instead of
    /// shoving the rest of the thread down the window.
    private var displayHeight: CGFloat {
        height > 0 ? height : (rememberedHeight ?? guessedHeight ?? Self.placeholderHeight)
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
        } else if prepared.trackers > 0, prepared.trackersAllowed {
            // The count is honest about what was LEFT IN: this sender is someone
            // the user writes to, so they are allowed to see the open. Muted, not
            // green — nothing was protected here.
            Label(
                "trackers allowed (known sender)",
                systemImage: "eye"
            )
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaint)
            .padding(.horizontal, 9)
            .padding(.vertical, 4)
            .glassCapsule(interactive: false)
            .help(
                "\(prepared.trackers) tracking pixel\(prepared.trackers == 1 ? "" : "s") left in place: you have emailed this sender, so they may see that you opened this"
            )
        } else if prepared.trackers > 0 {
            Label(
                "\(prepared.trackers) tracker\(prepared.trackers == 1 ? "" : "s") blocked",
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

}

// MARK: - the frame, before a view system touches it

/// Everything about a mail frame that has no opinion about AppKit or UIKit: the
/// one ephemeral data store, the sandbox configuration built on top of it, the
/// injected measuring script, and the pool identity of a rendered document. The
/// two representables differ only in what they wrap this in.
enum EmailFrame {
    /// ONE ephemeral data store shared by every email view. `nonPersistent()`
    /// mints a BRAND NEW store per call, so a per-message store cached nothing —
    /// every image re-fetched on every open. Sharing restores the URL cache while
    /// keeping what matters: nothing touches disk, no cookie jar survives.
    @MainActor
    static let sharedDataStore: WKWebsiteDataStore = .nonPersistent()

    /// A configuration with all five layers wired, plus the relay that will be
    /// the built frame's permanent hinge. Returned together because the relay is
    /// registered INTO the configuration — a frame cannot be handed one later.
    @MainActor
    static func makeConfiguration() -> (WKWebViewConfiguration, FrameRelay) {
        let config = WKWebViewConfiguration()

        // LAYER 2: page content cannot execute script, whatever it contains.
        // Our injected user script is governed separately and still runs.
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        // LAYER 5: no cookie jar, no persistent storage, nothing survives close.
        config.websiteDataStore = sharedDataStore
        // Images are the ONE resource a body may load, and they ride our handler
        // rather than WebKit's loader; one shared handler keeps the in-flight
        // bookkeeping in a single place. TWO schemes reach it: remote art
        // rewritten by ImageProxy, and this message's own attachment parts
        // rewritten by CidImages. Both configurations register both — a frame
        // that knew only one would draw the other's images as broken glyphs.
        config.setURLSchemeHandler(ImageSchemeHandler.shared, forURLScheme: ImageProxy.scheme)
        config.setURLSchemeHandler(ImageSchemeHandler.shared, forURLScheme: CidProxy.scheme)

        // The relay, not the coordinator, is wired to the frame — it outlives
        // every coordinator that borrows it. See FrameRelay.
        let relay = FrameRelay()
        let controller = WKUserContentController()
        controller.add(relay, name: FrameRelay.name)
        controller.addUserScript(
            WKUserScript(
                source: measuringScript, injectionTime: .atDocumentEnd,
                forMainFrameOnly: true))
        config.userContentController = controller

        return (config, relay)
    }

    /// What a LIVE frame would have to be holding to be reusable for this render,
    /// or nil if this render cannot use one (nothing to render yet, or no message
    /// id).
    static func renderKey(poolKey: String?, html: String, allowRemote: Bool) -> WebFramePool.Key? {
        guard let poolKey, !html.isEmpty else { return nil }
        return WebFramePool.Key(
            message: poolKey, allowRemote: allowRemote, document: html.hashValue)
    }

    /// Injected AFTER document end (the message cannot run script to interfere):
    /// collapses trailing quoted history, reports document height on load / each
    /// image settling / a ResizeObserver tick, reports link clicks up to native
    /// rather than navigating, and exposes the collapse toggle the host calls.
    static let measuringScript = """
        (function () {
          var send = function (payload) {
            try { window.webkit.messageHandlers.passband.postMessage(payload); } catch (e) {}
          };

          // Declared UP HERE because the quoted-history collapse below calls
          // measure() before the height section: an undefined `last` makes the
          // comparison NaN and throws away the measurement that sizes the frame.
          var last = -1;

          // ---- quoted history --------------------------------------------
          // Mirrors Quotes.swift: the first TOP-LEVEL <blockquote> after which
          // the document has no substantial text of its own anchors the history,
          // so a bottom-posted reply never qualifies.
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
          window.__passbandSetQuotes = function (collapsed) {
            for (var i = 0; i < quoteNodes.length; i++) {
              quoteNodes[i].style.display = collapsed ? 'none' : '';
            }
            measure();
          };
          // Collapsed by default, BEFORE the first measure, so the frame sizes
          // to the collapsed content and never flashes the full chain.
          window.__passbandSetQuotes(true);
          send({ kind: 'quoted', value: quoteNodes.length > 0 });

          // ---- height ------------------------------------------------------
          // Measure the CONTENT, never documentElement.scrollHeight: the root's
          // scrollHeight is floored at the VIEWPORT height, and the viewport here
          // IS the frame being sized, so a shrinking document (collapsing quoted
          // history) could never report smaller. max(scrollHeight, rect) also
          // covers children overflowing the body box (floats, positioned tables).
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
          // THE SAME NUMBER, ASKED FOR RATHER THAN SENT. A frame being
          // measured off screen (FrameMeasurer) loads one document after
          // another through one web view, and a pushed message carries no
          // sign of which document it came from — the previous body's height
          // lands in the next body's lap about a quarter of the time. A call
          // has an answer, and the answer belongs to the document that was
          // asked. Defined on the collapsed document, so it reports what the
          // reader will actually show.
          window.__passbandHeight = contentHeight;
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

          // Called by the host when a POOLED frame is handed to a new view:
          // nothing here re-runs on a document that was not reloaded, so the new
          // host knows neither the height nor whether a quoted chain exists.
          // Clearing `last` is the trick — measure() suppresses an unchanged
          // height, and a re-shown frame is exactly unchanged.
          window.__passbandResend = function () {
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
}

// MARK: - the coordinator

/// The owner of a frame while a representable is showing it: load guard, pool
/// bookkeeping, navigation policy. Free-standing rather than nested in a
/// representable, because there are two representables and only one of these.
@MainActor
final class EmailFrameCoordinator: NSObject {
    var onHeight: (CGFloat) -> Void
    /// The navigation this frame started has finished — subresources and all.
    var onLoaded: () -> Void
    var onQuotedFound: (Bool) -> Void
    var onLink: (String) -> Void

    /// The frame's permanent hinge, borrowed while this representable owns the
    /// frame. Strong, so `release` can still hand it to the pool after WebKit
    /// has let go of everything else.
    private var relay: FrameRelay?

    /// What is currently loaded, so `updateNSView` only reloads on a real
    /// content/policy change (a reload flashes an empty frame).
    private var loadedSignature: String?
    /// The pool identity of what the frame CURRENTLY HOLDS — re-derived on
    /// every real load, never remembered from checkout: the remote-image
    /// opt-in reloads an already-checked-out frame, so one borrowed under
    /// `allowRemote: false` can go back holding a permissive-CSP document and
    /// must be filed as such.
    private var loadedPoolKey: WebFramePool.Key?
    /// Navigations WE started that have not yet passed the policy gate. A
    /// COUNTER, not a flag: `loadHTMLString` is async, so two loads in quick
    /// succession race for one permission, and with a bool the loser is
    /// cancelled — a permanently blank frame that still reports a height.
    private var pendingOwnLoads = 0

    /// The quoted-history state the HOST wants, and the one the DOCUMENT is
    /// actually in. They are kept apart because `updateNSView` runs for reasons
    /// that have nothing to do with quoted history — a new height, a re-render
    /// of the reader, a fresh set of callback closures — and firing the toggle
    /// each time is a script round trip into every open message's content
    /// process, each one forcing that document to lay itself out again to
    /// answer with a height it already reported. `nil` = the document's state
    /// is unknown, which is what a load in flight leaves behind.
    private var wantedCollapse = true
    private var appliedCollapse: Bool?

    init(
        onHeight: @escaping (CGFloat) -> Void, onLoaded: @escaping () -> Void,
        onQuotedFound: @escaping (Bool) -> Void, onLink: @escaping (String) -> Void
    ) {
        self.onHeight = onHeight
        self.onLoaded = onLoaded
        self.onQuotedFound = onQuotedFound
        self.onLink = onLink
    }

    /// The load guard and the pool key must say the same thing about a
    /// document, so both derive from here: drift would make an adopted frame
    /// fail its own guard and reload what it is already showing.
    private static func signature(allowRemote: Bool, document: Int) -> String {
        "\(allowRemote)|\(document)"
    }

    func load(_ webView: WKWebView, html: String, allowRemote: Bool, poolKey: String?) {
        // NEVER LOAD THE PLACEHOLDER: `prepared` can start empty, and loading
        // a blank document lands the real one as a SECOND navigation — which
        // is what makes the load race reachable at all.
        guard !html.isEmpty else { return }
        let document = html.hashValue
        let signature = Self.signature(allowRemote: allowRemote, document: document)
        guard signature != loadedSignature else { return }
        loadedSignature = signature
        loadedPoolKey = poolKey.map {
            WebFramePool.Key(message: $0, allowRemote: allowRemote, document: document)
        }
        // The document about to be replaced is the one whose quote state we
        // knew. Until the new one lands there is nothing to toggle — the
        // function the toggle calls does not exist yet — and `documentDidLoad`
        // is what applies the host's wish once it does.
        appliedCollapse = nil
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

    /// Take ownership of a frame ALREADY showing this document. Re-pointing
    /// `relay.owner` is the entire handover: the message handler and both
    /// delegates are the relay permanently, so nothing on the frame needs
    /// rewiring and no callback can reach the previous coordinator.
    func adopt(_ entry: WebFramePool.Entry, key: WebFramePool.Key) {
        attach(entry.relay)
        // The frame is already loaded, so no load may be started — and
        // `pendingOwnLoads` stays 0, which is correct: layer 4 has no
        // permission outstanding to grant.
        loadedSignature = Self.signature(allowRemote: key.allowRemote, document: key.document)
        loadedPoolKey = key
        // A frame is parked COLLAPSED (WebFramePool.checkIn settles it there),
        // which is also a fresh host's default — so the update that follows
        // this has nothing to toggle and does not say so twice.
        appliedCollapse = true

        // A document that was not reloaded never re-runs the measuring script,
        // so a reused frame would sit at zero height behind the paint-hold
        // with no quoted-history chip. The relay's last values release the
        // hold with no round trip; the resend is what makes it right if the
        // frame comes back at a new width. The main-actor hop is not optional
        // — makeNSView runs inside a view update and these callbacks write
        // @State.
        let height = entry.relay.lastHeight
        let quoted = entry.relay.lastQuoted
        Task { @MainActor [weak self] in
            guard let self else { return }
            self.onQuotedFound(quoted)
            if height > 0 { self.onHeight(height) }
            // A POOLED FRAME NEVER FIRES `didFinish` AGAIN — it finished
            // loading for its previous host. Saying so here is what leaves the
            // host willing to take one more measurement, which is the whole
            // point of the resend below: the frame may be coming back at a
            // different column width, and the size it was parked at is then the
            // wrong one.
            self.onLoaded()
        }
        entry.webView.evaluateJavaScript("window.__passbandResend && window.__passbandResend()")
    }

    /// Give the frame up: to the pool if it is worth keeping, otherwise to
    /// the teardown path.
    func release(_ webView: WKWebView) {
        guard let relay else { return }
        relay.owner = nil
        let entry = WebFramePool.Entry(webView: webView, relay: relay)
        self.relay = nil
        // An unfinished or failed load is a trap, not merely worthless: the
        // next owner adopts with no pending-load permission, so the in-flight
        // navigation is refused by layer 4 and the frame comes back blank. A
        // body with no message id has a nil `loadedPoolKey` — that is what
        // keeps sealed reveals out of the pool.
        guard relay.loaded, let key = loadedPoolKey else {
            WebFramePool.discard(entry)
            return
        }
        WebFramePool.shared.checkIn(entry, key: key)
    }

    func setQuotesCollapsed(_ webView: WKWebView, collapsed: Bool) {
        wantedCollapse = collapsed
        applyCollapse(webView)
    }

    /// The document just finished loading, so it IS collapsed: the injected
    /// script does that before its first measurement. That is what makes the
    /// state knowable again — and what re-applies an expanded history the
    /// reader had open when something (the remote-image opt-in) reloaded the
    /// frame out from under them.
    ///
    /// It is also the moment the host has been waiting for. WebKit does not
    /// call this until the subresources are in, so the height asked for here
    /// is the one the message came to, images included — and asking is the
    /// point: a pushed height says nothing about which state of the document
    /// produced it, while an answer belongs to the question.
    func documentDidLoad(_ webView: WKWebView) {
        appliedCollapse = true
        applyCollapse(webView)
        settle(webView)
    }

    /// A navigation that failed will never report anything, and a host still
    /// waiting for it would refuse every later height this frame ever offers —
    /// including the ones a retry or a resize produces. Releasing the wait
    /// costs nothing when there is genuinely nothing more to come.
    func documentDidFail() {
        onLoaded()
    }

    /// TELL THE HOST TO TAKE ONE LAST MEASUREMENT, and hand it the number.
    ///
    /// Asking is the point: a pushed height says nothing about which state of
    /// the document produced it, while an answer belongs to the question. Used
    /// wherever a document has finished becoming whatever it is about to be —
    /// the navigation landing, and the quoted history folding or unfolding,
    /// which changes the content just as much but starts no navigation at all,
    /// so nothing else would ever tell the host it had stopped moving.
    private func settle(_ webView: WKWebView) {
        onLoaded()
        webView.evaluateJavaScript("window.__passbandHeight ? window.__passbandHeight() : 0") {
            [weak self] value, _ in
            guard let self, let number = value as? NSNumber, number.doubleValue > 0 else { return }
            self.onHeight(CGFloat(number.doubleValue))
        }
    }

    private func applyCollapse(_ webView: WKWebView) {
        guard appliedCollapse != wantedCollapse else { return }
        appliedCollapse = wantedCollapse
        webView.evaluateJavaScript(
            "window.__passbandSetQuotes && window.__passbandSetQuotes(\(wantedCollapse))"
        ) { [weak self] _, _ in
            // Folding the history is a content change with no navigation
            // behind it, so this is the only thing that ever tells the host
            // the document has finished moving.
            self?.settle(webView)
        }
    }

    /// LAYER 4: allow exactly the in-memory loads WE started, refuse everything
    /// else — a link click cannot navigate this view anywhere. Called by the
    /// relay; the counter lives here because only the owner starts loads.
    func decideNavigation(_ navigationAction: WKNavigationAction) -> WKNavigationActionPolicy {
        if pendingOwnLoads > 0 {
            pendingOwnLoads -= 1
            return .allow
        }
        // A user-initiated link becomes a system-browser open (Opener
        // re-guards to http/https); anything else is silently dropped.
        if navigationAction.navigationType == .linkActivated,
            let url = navigationAction.request.url
        {
            onLink(url.absoluteString)
        }
        return .cancel
    }

    /// Build the full document. The CSP meta MUST be the first thing in <head>
    /// so it governs every subsequent resource. The reading surface is
    /// deliberately OPAQUE white: body copy over a live wallpaper is
    /// unreadable, and mail ships its own colors assuming a white canvas.
    ///
    /// THE HEIGHT RULE IS LOAD-BEARING, and it is not cosmetic. This frame is
    /// sized to what the document measures, so a document whose own height is a
    /// function of the viewport is a closed loop with gain 1: it reports the
    /// viewport's height, the frame becomes that, the viewport grows by the
    /// padding above, it reports again. Measured at exactly +28 points a pass,
    /// about eighty passes a second, without bound — a message that grows until
    /// you leave the thread, on the same main thread that moves the scroll.
    ///
    /// `html,body{height:100%}` is how a great many email templates open, and
    /// our stylesheet used to set no height at all, so theirs applied
    /// unopposed. `!important` beats an ordinary author rule wherever it sits.
    /// Verified across documents up to 16,842 points not to move an ordinary
    /// body by a single point.
    ///
    /// It does NOT cover `vh` units, which resolve against the viewport no
    /// matter what html and body are told to be. Those are bounded on the host
    /// side instead — see EmailWebView's sizing phases.
    nonisolated static func document(html: String, allowRemote: Bool) -> String {
        // Which schemes an image may come from, and why the two custom ones are
        // gated differently, is MailCSP's — the policy lives where it can be
        // asserted without a WebKit process.
        let csp = MailCSP.policy(allowRemote: allowRemote)
        // A PHONE lays a viewport-less document out at 980 CSS px and then
        // scales the result down to fit, which renders every email at about a
        // third size. Pinning the layout viewport to the frame's own width is
        // also what makes the height the bridge reports arrive in units the
        // outer SwiftUI ScrollView can use as points. A Mac frame IS its layout
        // width already, so the tag would be a no-op there and is left out.
        #if os(macOS)
            let viewport = ""
        #else
            let viewport = "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
        #endif
        return """
            <!doctype html><html><head>\
            <meta http-equiv="Content-Security-Policy" content="\(csp)">\
            <meta name="referrer" content="no-referrer">\
            <meta charset="utf-8">\(viewport)\
            <style>\
            html,body{margin:0;padding:14px;background:#fff;color:#111;\
            font:14px/1.55 -apple-system,BlinkMacSystemFont,'SF Pro Text',sans-serif;\
            word-break:break-word;overflow-wrap:anywhere;overflow:hidden;}\
            html,body{height:auto!important;min-height:0!important;}\
            img{max-width:100%;height:auto;}\
            a{color:#2b7fd4;}\
            table{max-width:100%;}\
            blockquote{margin:8px 0;padding-left:10px;border-left:2px solid #d8dee6;color:#555;}\
            </style>\
            </head><body>\(html)</body></html>
            """
    }
}

// MARK: - the live frame pool

/// A frame's ONE permanent connection to native code: script message handler,
/// navigation delegate and UI delegate, installed at construction and never
/// replaced. A pooled frame outlives its representable, and one
/// `WKUserContentController` will not hold two handlers under a single name, so
/// re-`add`ing each new Coordinator would either throw or pin a torn-down view's
/// closures to the frame — instead the wiring never changes and OWNERSHIP does,
/// in one assignment, nil while the frame sits in the pool. Layer 4 is anchored
/// here: an unowned frame refuses every navigation. Height/quoted keep being
/// recorded with no owner, so a checked-in frame still leaves the pool a truthful
/// measurement for the next open.
@MainActor
final class FrameRelay: NSObject, WKScriptMessageHandler, WKNavigationDelegate,
    WKUIDelegate
{
    static let name = "passband"

    /// Weak on purpose: SwiftUI owns coordinators, and a frame must never be
    /// the reason a dead view's callbacks stay alive.
    weak var owner: EmailFrameCoordinator?

    /// What THIS document last told us. Read at checkout, because the measuring
    /// script does not re-run for a frame that was not reloaded.
    private(set) var lastHeight: CGFloat = 0
    private(set) var lastQuoted = false
    /// Whether the current document finished loading. Only a settled frame is safe
    /// to pool — see Coordinator.release.
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
        // Dropped with no owner: a pooled frame is off screen and cannot be
        // clicked, so such a message could only be stale.
        case "link":
            if let value = payload["value"] as? String { owner?.onLink(value) }
        default: break
        }
    }

    /// The empty document a SPARE is warmed with, which is loaded before this
    /// relay is anybody's delegate and may still be at the policy gate when the
    /// spare is handed out. It gets its own claim so that it can never spend
    /// the COORDINATOR's: both are "a load we started", they are told apart by
    /// who started them, and a warming load that consumed the real one's
    /// permission would leave the message that took the spare permanently
    /// blank.
    private var pendingWarmLoads = 0
    func expectWarmingLoad() { pendingWarmLoads += 1 }

    /// LAYER 4 — the policy itself is Coordinator.decideNavigation. NOTE the exact
    /// signature: the closure must be `@MainActor @Sendable` or Swift treats this
    /// as an unrelated near-miss method, the delegate requirement goes
    /// unimplemented, and WebKit silently defaults to ALLOWING every navigation.
    func webView(
        _ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void
    ) {
        if pendingWarmLoads > 0 {
            pendingWarmLoads -= 1
            decisionHandler(.allow)
            return
        }
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
        // The new document is collapsed and toggleable again; only the owner
        // knows which state its host is asking for.
        owner?.documentDidLoad(webView)
    }

    func webView(
        _ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error
    ) {
        loaded = false
        owner?.documentDidFail()
    }

    func webView(
        _ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!,
        withError error: Error
    ) {
        loaded = false
        owner?.documentDidFail()
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

/// LRU pool of LIVE, ALREADY-RENDERED email frames: a reopened message skips the
/// content-process attach, parse, layout, measuring pass and image reattachment.
/// FrameHeights makes it the right size; this makes it already drawn.
@MainActor
final class WebFramePool {
    static let shared = WebFramePool()

    /// SMALL ON PURPOSE: each entry is a live WebKit content-process attachment
    /// plus a whole email's retained layout tree. Six covers walking a thread back
    /// and forth and flipping between the last few messages; past that the parse
    /// being avoided is cheaper than the residency being paid for.
    private static let capacity = 6

    /// The identity of a RENDERED DOCUMENT, not of a message: all three fields
    /// must match, so a hit is showing a BYTE-IDENTICAL document under an
    /// identical CSP. Dropping any one weakens the sandbox (see docs/SECURITY.md)
    /// — `allowRemote` in particular IS the CSP, so the remote-image opt-in must
    /// be a pool MISS. Quote collapse is deliberately NOT in the key: it is a JS
    /// call on the live document, and frames are checked in collapsed so a hit
    /// matches a fresh open's default state.
    struct Key: Hashable {
        let message: String
        let allowRemote: Bool
        let document: Int
    }

    /// The frame is held as the BASE class: the Mac subclasses WKWebView to give
    /// the scroll wheel back to the column that owns the scroll, and a phone has
    /// nothing to override, so the pool must not care which it is holding.
    struct Entry {
        let webView: WKWebView
        let relay: FrameRelay
    }

    private var frames: [Key: Entry] = [:]
    /// Least-recently-used first.
    private var order: [Key] = []

    private init() {}

    /// A parked frame holding exactly this document, or nil. Checkout REMOVES it,
    /// which is the whole answer to a frame being in use: two views of one message
    /// can never be handed the same WKWebView — the second builds fresh, and
    /// whichever is torn down last is the one that ends up parked.
    func checkout(_ key: Key) -> Entry? {
        guard let entry = frames.removeValue(forKey: key) else { return nil }
        order.removeAll { $0 == key }
        return entry
    }

    func checkIn(_ entry: Entry, key: Key) {
        entry.relay.owner = nil
        entry.webView.removeFromSuperview()
        // Settle back to the state a fresh open expects: a frame parked with its
        // quoted history expanded would be re-shown tall and then snap shorter
        // under the new view's collapsed default. The relay records the resulting
        // height with no owner attached, so it is what the next checkout reports.
        entry.webView.evaluateJavaScript(
            "window.__passbandSetQuotes && window.__passbandSetQuotes(true)")

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

    /// Drop every parked frame. An account switch: the key's `message` is a
    /// message id, which is one daemon's — and a live WebKit attachment still
    /// holding the old account's document is not something to hand the new one
    /// on a key collision.
    func wipeAll() {
        for (_, entry) in frames { Self.discard(entry) }
        frames.removeAll()
        order.removeAll()
        for spare in spares { Self.discard(spare) }
        spares.removeAll()
    }

    // MARK: - warm spares

    /// BLANK FRAMES, ALREADY WARM. A pool hit is a message you have already
    /// read; this is for the ones you have not.
    ///
    /// Building a WKWebView is cheap (about three milliseconds of main thread).
    /// What is not cheap is what happens after: a brand new frame takes some
    /// seventy milliseconds to put anything on screen, because its content
    /// process is starting up. That time is not a freeze — the main thread
    /// stays responsive throughout — it is a BLANK BOX where a message should
    /// be, and on a first scroll through a thread there is one per message.
    ///
    /// A frame that has already had a document through it renders the next one
    /// in about five. So spares are built ahead of the scroll, put through an
    /// empty document to wake their process, and handed out the moment a
    /// message needs one. Construction alone warms nothing — measured: a spare
    /// built and never loaded still pays the full seventy — so the empty load
    /// IS the warming, and it holds: a spare that has sat idle for ninety
    /// seconds is still within a millisecond or two of a hot one.
    private static let spareTarget = 2
    private var spares: [Entry] = []
    private var refilling = false

    /// ONE warm frame in the drawer, at launch, so the first message of the
    /// session is not the one that waits for a content process. Does nothing
    /// once there is anything to hand out.
    func seedSpare(_ build: @MainActor () -> Entry) {
        guard spares.isEmpty else { return }
        spares.append(build())
    }

    /// A warm blank frame, or nil if none is ready. The caller wires its own
    /// delegates: a spare is deliberately handed out UNWIRED (see the builders
    /// in the two representables) — layer 4 refuses every navigation a frame's
    /// relay does not own, which would include the empty one that warms it.
    func takeSpare() -> Entry? {
        spares.popLast()
    }

    /// Top the spares back up, NEXT TURN — never inside the mount that just
    /// took one. The build is only a few milliseconds, but they are the same
    /// few milliseconds the scroll is trying to use.
    func replenishSpares(_ build: @escaping @MainActor () -> Entry) {
        guard !refilling, spares.count < Self.spareTarget else { return }
        refilling = true
        Task { @MainActor [weak self] in
            guard let self else { return }
            defer { self.refilling = false }
            while self.spares.count < Self.spareTarget {
                self.spares.append(build())
                // One per turn: two content processes starting at once, during
                // a scroll, is the thing being avoided.
                await Task.yield()
            }
        }
    }

    /// A dropped frame has to be UNWIRED, not just released: the content controller
    /// retains the relay and the relay is the frame's delegate, so letting go
    /// leaves a live target for a late navigation or script callback.
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
