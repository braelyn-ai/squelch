// ATTACHMENT ART CACHE — each attachment resolved ONCE per id per SIZE (negative
// results included, so a failed tile is not re-fetched on every scroll pass),
// downsampled at decode time so the bytes kept are the bytes drawn, and rendered
// in a detached task with only encoded Data crossing back. Bytes ride the
// authenticated door via APIClient — a bearer token cannot ride an <img src>,
// and this cache never learns it. The caller picks the bucket.
//
// TWO SIZES, two tables: the 38pt chip tile and the column-width inline
// rendering. They are not the same picture scaled — the tile is a recognition
// aid a mailbox-walk fills by the hundred, the inline art is the message itself
// and costs ~200x as much to keep, so they are bounded separately.

import CoreGraphics
import Foundation

@MainActor
final class AttachmentThumbs {
    static let shared = AttachmentThumbs()

    /// How to rasterize the bytes. Deliberately NOT derived from the mime here —
    /// the strip owns that, so "svg is never rendered inline" lives in one place.
    enum Source: Sendable { case image, pdf }

    /// A resolved tile. `.blank` is a real verdict ("this one produces no
    /// art"), not a missing entry, which is what makes the negative cache work.
    enum Tile { case art(PlatformImage), blank }

    /// The tile's size in PIXELS: the 38pt square at 2x, retina being the only
    /// display class this app ships on. `nonisolated` for the detached render task.
    private nonisolated static let maxPixel = 76

    /// The INLINE rendering's ceiling in PIXELS: the message column at 2x. Big
    /// enough to read a photo somebody sent, bounded so a 4000px original never
    /// sits in memory at full size. ImageIO does NOT upscale past a source's own
    /// dimensions, so a small signature logo asked for at this size comes back
    /// small rather than blurry — which is why one ceiling serves both.
    private nonisolated static let inlinePixel = 1200

    /// How many resolved tiles to keep. Each is a 76px thumbnail, so this is a
    /// few megabytes at worst — deep enough that walking a mailbox never
    /// re-downloads, bounded so a long session cannot grow forever.
    private static let cacheMax = 512

    /// Far shallower, because each entry is ~200x the size: a 1200px bitmap is a
    /// few megabytes DECODED. This is "the thread being read stays warm", not the
    /// mailbox — a thread with more photos than this re-fetches the oldest, which
    /// is the right trade against holding a gallery in memory.
    private static let inlineCacheMax = 8

    /// attachment id -> verdict, `.blank` included. Membership IS "already
    /// resolved", which is what makes the negative cache work.
    private let memo = AsyncMemo<Int, Tile>(limit: cacheMax)

    /// The same verdicts at column size. A separate table rather than a wider
    /// key: promoting a 38pt tile into the column would be an upscale of art we
    /// already know how to fetch properly.
    private let inlineMemo = AsyncMemo<Int, Tile>(limit: inlineCacheMax)

    private init() {}

    /// A resolved tile for instant render, without starting any work — a recycled
    /// card reads this in `body` instead of flashing a spinner while its `.task`
    /// re-confirms what we hold. nil means "not resolved yet", NOT "no art".
    func cached(_ id: Int) -> Tile? { memo.cached(id) }

    /// The column-sized art, without starting work. Same contract as `cached`.
    func cachedInline(_ id: Int) -> Tile? { inlineMemo.cached(id) }

    /// Forget every resolved tile. An account switch: attachment ids are one
    /// daemon's SQLite ints, so a surviving entry would not merely be stale —
    /// id 91 in the new account would render the old account's attachment. BOTH
    /// tables, for exactly that reason.
    func wipe() {
        memo.clear()
        inlineMemo.clear()
    }

    /// Resolve one tile, deduped and memoized.
    @discardableResult
    func resolve(_ attachment: Attachment, as source: Source) async -> Tile {
        await memo.resolve(attachment.id) {
            await Self.fetch(attachment, source, maxPixel: Self.maxPixel)
        }
    }

    /// Resolve one image attachment at column size. Images only: a PDF's first
    /// page is a recognition aid at tile size, not something the thread pastes
    /// across its full width.
    ///
    /// This is also the ONE resolve that keeps the bytes it downloaded. A picture
    /// big enough to render in the column is a picture somebody is about to click,
    /// and the original is already in hand — see AttachmentFiles. The 38pt tile
    /// path deliberately does not: it runs for every attachment in a mailbox walk,
    /// and staging hundreds of files would evict the handful worth holding.
    @discardableResult
    func resolveInline(_ attachment: Attachment) async -> Tile {
        await inlineMemo.resolve(attachment.id) {
            await Self.fetch(attachment, .image, maxPixel: Self.inlinePixel, keepBytes: true)
        }
    }

    // MARK: - resolution

    /// Bytes -> rasterized tile. Every failure is a negative cache entry, not a
    /// retry loop: a PDF that CoreGraphics refuses once will refuse forever.
    private static func fetch(
        _ attachment: Attachment, _ source: Source, maxPixel: Int, keepBytes: Bool = false
    ) async -> Tile {
        guard
            let fetched = try? await APIClient.shared.fetchAttachment(
                attachment.id, fallbackName: attachment.filename)
        else { return .blank }

        // Before the rasterize, not after: the file is what a click needs, and it
        // should be on disk from the earliest moment it can be.
        if keepBytes {
            AttachmentFiles.shared.keep(
                id: attachment.id, bytes: fetched.bytes, filename: fetched.filename)
        }

        let bytes = fetched.bytes
        let png = await Task.detached(priority: .utility) { () -> Data? in
            switch source {
            case .image: return downsample(bytes, maxPixel: maxPixel)
            case .pdf: return renderFirstPage(bytes, maxPixel: maxPixel)
            }
        }.value

        guard let png, let image = PlatformImage(data: png) else { return .blank }
        return .art(image)
    }

    private nonisolated static func downsample(_ bytes: Data, maxPixel: Int) -> Data? {
        guard let thumb = Raster.thumbnail(bytes, maxPixel: maxPixel) else { return nil }
        return Raster.png(thumb)
    }

    /// Rasterize page 1 at tile size. CoreGraphics rather than PDFKit on purpose:
    /// `CGPDFDocument` draws into a context we own on whatever thread we are on,
    /// while PDFKit hands back an `NSImage` that cannot cross out of the task.
    private nonisolated static func renderFirstPage(_ bytes: Data, maxPixel: Int) -> Data? {
        guard let provider = CGDataProvider(data: bytes as CFData),
            let document = CGPDFDocument(provider),
            let page = document.page(at: 1)
        else { return nil }

        let box = page.getBoxRect(.cropBox)
        guard box.width > 0, box.height > 0 else { return nil }

        // /Rotate is a page ATTRIBUTE, not something already applied to the crop
        // box: a landscape scan stored as a quarter-turned portrait page must be
        // measured after the turn or the tile gets the wrong aspect.
        let quarterTurned = page.rotationAngle % 180 != 0
        let width = quarterTurned ? box.height : box.width
        let height = quarterTurned ? box.width : box.height

        let scale = min(CGFloat(maxPixel) / width, CGFloat(maxPixel) / height)
        let pxWidth = max(1, Int((width * scale).rounded()))
        let pxHeight = max(1, Int((height * scale).rounded()))

        guard
            let ctx = CGContext(
                data: nil, width: pxWidth, height: pxHeight, bitsPerComponent: 8, bytesPerRow: 0,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue)
        else { return nil }

        // A PDF page paints no background of its own; without this the tile is
        // transparent black-on-nothing text, which over a dark card reads as noise.
        let full = CGRect(x: 0, y: 0, width: pxWidth, height: pxHeight)
        ctx.setFillColor(CGColor(gray: 1, alpha: 1))
        ctx.fill(full)

        ctx.interpolationQuality = .high
        // `rotate: 0` means "no EXTRA turn" — the transform folds in /Rotate.
        ctx.concatenate(
            page.getDrawingTransform(
                .cropBox, rect: full, rotate: 0, preserveAspectRatio: true))
        // Content outside the crop box is not part of the page; without the clip a
        // document with bleed marks paints them over the tile.
        ctx.clip(to: box)
        ctx.drawPDFPage(page)

        guard let image = ctx.makeImage() else { return nil }
        return Raster.png(image)
    }
}
