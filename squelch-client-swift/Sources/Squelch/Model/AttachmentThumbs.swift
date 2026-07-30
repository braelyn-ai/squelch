// ATTACHMENT TILE CACHE — each attachment's 38pt tile resolved ONCE per id
// (negative results included, so a failed tile is not re-fetched on every scroll
// pass), downsampled at decode time so the bytes kept are the bytes drawn, and
// rendered in a detached task with only encoded Data crossing back. Bytes ride
// the authenticated door via APIClient — a bearer token cannot ride an <img src>,
// and this cache never learns it. The caller picks the bucket.

import AppKit
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

@MainActor
final class AttachmentThumbs {
    static let shared = AttachmentThumbs()

    /// How to rasterize the bytes. Deliberately NOT derived from the mime here —
    /// the strip owns that, so "svg is never rendered inline" lives in one place.
    enum Source: Sendable { case image, pdf }

    /// A resolved tile. `.blank` is a real verdict ("this one produces no
    /// art"), not a missing entry, which is what makes the negative cache work.
    enum Tile { case art(NSImage), blank }

    /// The tile's size in PIXELS: the 38pt square at 2x, retina being the only
    /// display class this app ships on. `nonisolated` for the detached render task.
    private nonisolated static let maxPixel = 76

    /// attachment id -> verdict. Membership IS "already resolved".
    private var cache: [Int: Tile] = [:]
    /// In-flight resolves, so two cards for one attachment share a download.
    private var inFlight: [Int: Task<Tile, Never>] = [:]

    private init() {}

    /// A resolved tile for instant render, without starting any work — a recycled
    /// card reads this in `body` instead of flashing a spinner while its `.task`
    /// re-confirms what we hold. nil means "not resolved yet", NOT "no art".
    func cached(_ id: Int) -> Tile? { cache[id] }

    /// Resolve one tile, deduped and memoized.
    @discardableResult
    func resolve(_ attachment: Attachment, as source: Source) async -> Tile {
        let id = attachment.id
        if let entry = cache[id] { return entry }
        if let running = inFlight[id] { return await running.value }

        let task = Task<Tile, Never> { [weak self] in
            let tile = await Self.fetch(attachment, source)
            self?.cache[id] = tile
            self?.inFlight[id] = nil
            return tile
        }
        inFlight[id] = task
        return await task.value
    }

    // MARK: - resolution

    /// Bytes -> rasterized tile. Every failure is a negative cache entry, not a
    /// retry loop: a PDF that CoreGraphics refuses once will refuse forever.
    private static func fetch(_ attachment: Attachment, _ source: Source) async -> Tile {
        guard
            let fetched = try? await APIClient.shared.fetchAttachment(
                attachment.id, fallbackName: attachment.filename)
        else { return .blank }

        let bytes = fetched.bytes
        let png = await Task.detached(priority: .utility) { () -> Data? in
            switch source {
            case .image: return downsample(bytes, maxPixel: maxPixel)
            case .pdf: return renderFirstPage(bytes, maxPixel: maxPixel)
            }
        }.value

        guard let png, let image = NSImage(data: png) else { return .blank }
        return .art(image)
    }

    /// ImageIO decodes DIRECTLY to thumbnail size, so a 2MB photo is never fully
    /// rasterized to fill a 38pt square.
    private nonisolated static func downsample(_ bytes: Data, maxPixel: Int) -> Data? {
        guard let source = CGImageSourceCreateWithData(bytes as CFData, nil) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maxPixel,
        ]
        guard
            let thumb = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
        else { return nil }
        return encodePNG(thumb)
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
        return encodePNG(image)
    }

    /// PNG on the way out because `CGImage` is not Sendable and `Data` is — that
    /// re-encode is what lets the render stay off the main actor.
    private nonisolated static func encodePNG(_ image: CGImage) -> Data? {
        let out = NSMutableData()
        guard
            let dest = CGImageDestinationCreateWithData(
                out as CFMutableData, UTType.png.identifier as CFString, 1, nil)
        else { return nil }
        CGImageDestinationAddImage(dest, image, nil)
        guard CGImageDestinationFinalize(dest) else { return nil }
        return out as Data
    }
}
