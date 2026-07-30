// NEWSLETTER HERO CACHE — resolve each sender's thumbnail ONCE, off the main
// actor, and hand the grid something cheap to draw.
//
// WHY THIS EXISTS: the hero used to resolve itself inside the card. In a
// LazyVGrid that is a trap — cards are created and destroyed as they cross the
// viewport, so every recycle re-ran the whole chain, and the expensive half of
// it (decode the full-size art, draw it into a 16x16 buffer to sample the
// dominant colour) ran ON THE MAIN ACTOR. Scrolling past the zone therefore
// paid for a decode per card per pass, which is exactly what it felt like.
//
// Three things fix it, and all three matter:
//   1. RESOLVE ONCE, keyed by thread id, including a NEGATIVE result — a sender
//      with no art must not re-walk its thread on every recycle looking for
//      one it will not find.
//   2. DOWNSAMPLE TO THE SQUARE. Heroes are routinely 1200px wide and were
//      being rescaled into a 54pt box on every frame. ImageIO decodes straight
//      to thumbnail size, so the bytes we keep are the bytes we draw.
//   3. DO IT OFF THE MAIN ACTOR. Decode, downsample and sampling all happen in
//      a detached task; only Sendable values (the encoded thumbnail and three
//      colour components) come back across.
//
// Preloading is then just "resolve everything the zone is about to show" —
// see SitrepView, which kicks it off as soon as the newsletter list lands.

import AppKit
import CoreGraphics
import ImageIO
import SwiftUI
import UniformTypeIdentifiers

@MainActor
final class HeroCache {
    static let shared = HeroCache()

    /// A resolved hero: art already downsampled to its square, plus the colour
    /// to paint behind it.
    struct Hero {
        let image: NSImage
        let fill: Color?
    }

    /// The rendered size, in PIXELS: the 54pt square at 2x. Retina is the only
    /// display class this app ships on, and over-decoding "just in case" is the
    /// cost this whole type exists to remove.
    /// `nonisolated` because the detached render task reads it.
    private nonisolated static let maxPixel = 108

    /// How many heroes resolve at once during a preload. The work is network-
    /// bound per item but each one ends in a decode, so a wide fan-out just
    /// queues CPU behind the scroll it is meant to smooth.
    private static let preloadWidth = 4

    /// threadId -> resolved hero, or nil for "checked, has none". The outer
    /// Optional is cache membership; the inner one is the verdict.
    private var cache: [String: Hero?] = [:]
    /// In-flight resolves, so a preload and a card that scrolls into view at the
    /// same moment share one fetch instead of racing.
    private var inFlight: [String: Task<Hero?, Never>] = [:]

    private init() {}

    /// A cached hero for instant render, without starting any work. Cards read
    /// this on the way into `body` so a recycled card paints immediately rather
    /// than flashing empty while its `.task` re-confirms what we already know.
    func cached(_ threadId: String) -> Hero? {
        guard let entry = cache[threadId] else { return nil }
        return entry
    }

    /// Whether this thread has a VERDICT — art, or a cached "has none".
    /// `cached` cannot answer that: it flattens both to nil. A card checks this
    /// before starting its `.task`, because on a hit the resolve would only
    /// hand back what the card already drew, at the price of a suspension and a
    /// second body pass.
    func isResolved(_ threadId: String) -> Bool { cache.index(forKey: threadId) != nil }

    /// Resolve one hero, deduped and memoized.
    @discardableResult
    func resolve(_ threadId: String) async -> Hero? {
        if let entry = cache[threadId] { return entry }
        if let running = inFlight[threadId] { return await running.value }
        guard Prefs.shared.loadRemoteImages, !threadId.isEmpty else { return nil }

        let task = Task<Hero?, Never> { [weak self] in
            let hero = await Self.fetch(threadId)
            self?.cache[threadId] = hero
            self?.inFlight[threadId] = nil
            return hero
        }
        inFlight[threadId] = task
        return await task.value
    }

    /// Warm the cache for a whole zone's worth of senders, `preloadWidth` at a
    /// time. Fire-and-forget: already-cached and in-flight ids fall straight
    /// through `resolve`, so calling this on every refresh is near-free.
    func preload(_ threadIds: [String]) {
        let pending = threadIds.filter { !$0.isEmpty && cache[$0] == nil }
        guard !pending.isEmpty, Prefs.shared.loadRemoteImages else { return }
        Task { [weak self] in
            await withTaskGroup(of: Void.self) { group in
                var next = 0
                var running = 0
                while next < pending.count || running > 0 {
                    while running < Self.preloadWidth, next < pending.count {
                        let id = pending[next]
                        next += 1
                        running += 1
                        group.addTask { [weak self] in
                            await self?.resolve(id)
                        }
                    }
                    await group.next()
                    running -= 1
                }
            }
        }
    }

    // MARK: - resolution

    /// Thread -> hero src -> bytes -> thumbnail + fill. Every failure is a
    /// negative cache entry, not a retry loop.
    private static func fetch(_ threadId: String) async -> Hero? {
        guard let view = try? await ThreadPrefetch.shared.fetch(threadId, fresh: 600),
            let newest = view.messages.last, let html = newest.html,
            let src = Trackers.extractHeroSrc(html), let url = URL(string: src),
            let bytes = await HeroBytes.shared.load(url)
        else { return nil }

        // Decode, downsample and sample OFF the main actor; only the encoded
        // thumbnail and the colour components come back.
        let rendered = await Task.detached(priority: .utility) {
            render(bytes, maxPixel: maxPixel)
        }.value

        guard let rendered, let image = NSImage(data: rendered.thumbnail) else { return nil }
        return Hero(image: image, fill: rendered.rgb.map(\.color))
    }

    /// What crosses back from the detached task — Sendable by construction.
    private struct Rendered: Sendable {
        let thumbnail: Data
        let rgb: ImageFill.RGB?
    }

    /// ImageIO decodes DIRECTLY to thumbnail size, so a 1200px hero never gets
    /// fully rasterized. PNG on the way out because the transparent-logo case
    /// depends on the alpha surviving — that transparency is what lets the
    /// sampled fill show through behind the mark.
    private nonisolated static func render(_ bytes: Data, maxPixel: Int) -> Rendered? {
        guard let source = CGImageSourceCreateWithData(bytes as CFData, nil) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maxPixel,
        ]
        guard let thumb = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
        else { return nil }

        let rgb = ImageFill.dominantRGB(thumb)

        let out = NSMutableData()
        guard
            let dest = CGImageDestinationCreateWithData(
                out as CFMutableData, UTType.png.identifier as CFString, 1, nil)
        else { return nil }
        CGImageDestinationAddImage(dest, thumb, nil)
        guard CGImageDestinationFinalize(dest) else { return nil }
        return Rendered(thumbnail: out as Data, rgb: rgb)
    }
}

/// Fetches hero bytes. Referrer-suppressed and cookie-free, matching the posture
/// of every other remote-image fetch in the app. It hands back BYTES rather than
/// an image: decoding belongs off the main actor, and the decoded result is
/// cached by HeroCache anyway, so caching whole images here would be a second
/// copy of the same art at full size.
@MainActor
final class HeroBytes {
    static let shared = HeroBytes()

    private let session: URLSession = {
        let cfg = URLSessionConfiguration.default
        cfg.timeoutIntervalForRequest = 10
        cfg.httpShouldSetCookies = false
        return URLSession(configuration: cfg)
    }()

    /// 8MB per image, matching the Rust shell's cap.
    private static let maxBytes = 8 * 1024 * 1024

    private init() {}

    func load(_ url: URL) async -> Data? {
        guard url.scheme == "http" || url.scheme == "https" else { return nil }
        var req = URLRequest(url: url)
        req.setValue("", forHTTPHeaderField: "Referer")
        guard let (data, response) = try? await session.data(for: req),
            let http = response as? HTTPURLResponse, http.statusCode == 200,
            data.count <= Self.maxBytes,
            let mime = http.value(forHTTPHeaderField: "Content-Type"),
            mime.lowercased().hasPrefix("image/")
        else { return nil }
        return data
    }
}
