// NEWSLETTER HERO CACHE — each sender's thumbnail resolved ONCE per thread id,
// negative results included (a sender with no art must not re-walk its thread on
// every LazyVGrid recycle), downsampled at decode time so the bytes kept are the
// bytes drawn, and decoded/sampled in a detached task with only Sendable values
// (the encoded thumbnail and three colour components) crossing back. Preloading
// is just "resolve everything the zone is about to show".

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

    /// The rendered size in PIXELS: the 54pt square at 2x, retina being the only
    /// display class this app ships on. `nonisolated` for the detached render task.
    private nonisolated static let maxPixel = 108

    /// How many heroes resolve at once during a preload. Each item is network-bound
    /// but ends in a decode, so a wide fan-out queues CPU behind the scroll it is
    /// meant to smooth.
    private static let preloadWidth = 4

    /// threadId -> resolved hero, or nil for "checked, has none". The outer
    /// Optional is cache membership; the inner one is the verdict.
    private var cache: [String: Hero?] = [:]
    /// In-flight resolves, so a preload and a card scrolling into view share a fetch.
    private var inFlight: [String: Task<Hero?, Never>] = [:]

    private init() {}

    /// A cached hero for instant render, without starting any work — a recycled card
    /// reads this in `body` instead of flashing empty while its `.task` re-confirms.
    func cached(_ threadId: String) -> Hero? {
        guard let entry = cache[threadId] else { return nil }
        return entry
    }

    /// Whether this thread has a VERDICT — art, or a cached "has none". `cached`
    /// flattens both to nil and cannot answer it; a card checks this before
    /// starting a `.task` that would only re-hand it what it already drew.
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

        // Off the main actor; only the thumbnail and colour components come back.
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

    /// ImageIO decodes DIRECTLY to thumbnail size, so a 1200px hero is never fully
    /// rasterized. PNG on the way out because a transparent logo needs its alpha to
    /// survive — that is what lets the sampled fill show through behind the mark.
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

/// Fetches hero bytes: ephemeral, cookie-free and referrer-suppressed, like every
/// other remote-image fetch here. Hands back BYTES rather than an image — decoding
/// belongs off the main actor, and HeroCache already keeps the decoded result.
@MainActor
final class HeroBytes {
    static let shared = HeroBytes()

    /// Every knob that could leak the reader is off — this fetches a URL an email
    /// chose. `urlCache` is nil because HeroCache holds the only copy worth keeping;
    /// a cache underneath would keep the full-size art on disk too, keyed by a
    /// mail-supplied URL.
    private let session: URLSession = {
        let cfg = URLSessionConfiguration.ephemeral
        cfg.httpShouldSetCookies = false
        cfg.httpCookieAcceptPolicy = .never
        cfg.httpCookieStorage = nil
        cfg.urlCache = nil
        cfg.requestCachePolicy = .reloadIgnoringLocalCacheData
        cfg.timeoutIntervalForRequest = 10
        return URLSession(configuration: cfg)
    }()

    /// 8MB per image.
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
