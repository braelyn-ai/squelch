// Downsample-at-decode + PNG-encode, shared by every thumbnail cache here.
// Free of actor isolation on purpose: both callers run this inside a detached
// task, and `CGImage` is not Sendable — it must never cross back out.

import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

enum Raster {
    /// ImageIO decodes DIRECTLY to thumbnail size, so a 1200px hero (or a 2MB
    /// photo) is never fully rasterized to fill a small square.
    static func thumbnail(_ data: Data, maxPixel: Int) -> CGImage? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maxPixel,
        ]
        return CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
    }

    /// PNG on the way out because `CGImage` is not Sendable and `Data` is — that
    /// re-encode is what lets the render stay off the main actor. PNG rather than
    /// JPEG because a transparent logo needs its alpha to survive.
    static func png(_ image: CGImage) -> Data? {
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
