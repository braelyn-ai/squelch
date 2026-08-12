// Newsletter-thumb fill colour: sample the hero's pixels and reduce them to the
// ONE colour the rest of the square is painted.
//
// Every failure path returns nil and the card keeps its neutral well; nothing
// here ever throws to the caller.

import CoreGraphics
import SwiftUI

enum ImageFill {
    /// A sampled colour as plain components so it can cross actor boundaries:
    /// sampling runs off the main actor, and `Color` is a view type.
    struct RGB: Sendable {
        let r: Double, g: Double, b: Double

        var color: Color { Color(.sRGB, red: r, green: g, blue: b, opacity: 1) }

        init(r: Double, g: Double, b: Double) {
            self.r = r
            self.g = g
            self.b = b
        }

        /// From 0-255 sums and their count.
        init(sum r: Int, _ g: Int, _ b: Int, over n: Int) {
            self.init(
                r: Double(r) / Double(n) / 255,
                g: Double(g) / Double(n) / 255,
                b: Double(b) / Double(n) / 255)
        }

        /// From a 24-bit literal, for the staged monochrome cases.
        init(hex: UInt32) {
            self.init(
                r: Double((hex >> 16) & 0xFF) / 255,
                g: Double((hex >> 8) & 0xFF) / 255,
                b: Double(hex & 0xFF) / 255)
        }
    }

    /// Downsample size. 256 samples is enough to find a dominant region and
    /// cheap enough to run per card.
    private static let side = 16

    /// A dominant-region bucket. Counts stay separate from the sums so the
    /// winner reports its TRUE average rather than the quantized key.
    private struct Bucket {
        var r = 0, g = 0, b = 0, n = 0
    }

    /// DOMINANT colour, not the average: heroes are mostly white margin, so an
    /// average washes out to near-white. 16x16 downsample, skip transparent /
    /// near-white / near-black, then the biggest 3-bit bucket's true average.
    nonisolated static func dominantRGB(_ image: CGImage) -> RGB? {
        guard let px = samples(image) else { return nil }

        var buckets: [Int: Bucket] = [:]
        var ar = 0, ag = 0, ab = 0, an = 0

        var i = 0
        while i + 3 < px.count {
            defer { i += 4 }
            let a = Int(px[i + 3])
            if a < 128 { continue }
            // CGContext hands back PREMULTIPLIED bytes. Undo that, or every
            // pixel with partial alpha reads darker than it looks.
            let r = straighten(px[i], a)
            let g = straighten(px[i + 1], a)
            let b = straighten(px[i + 2], a)

            ar += r
            ag += g
            ab += b
            an += 1

            let lum = luminance(r, g, b)
            if lum > 235 || lum < 20 { continue }  // margins / text ink

            let key = ((r >> 5) << 6) | ((g >> 5) << 3) | (b >> 5)
            var bucket = buckets[key] ?? Bucket()
            bucket.r += r
            bucket.g += g
            bucket.b += b
            bucket.n += 1
            buckets[key] = bucket
        }

        // A real dominant region — at least ~3% of the samples.
        if let best = buckets.values.max(by: { $0.n < $1.n }), best.n >= 8 {
            return RGB(sum: best.r, best.g, best.b, over: best.n)
        }

        guard an > 0 else { return nil }

        // NO DOMINANT MID-TONE REGION => a monochrome logo, and this is the
        // transparent-background case: transparent pixels were skipped
        // entirely, so a WHITE mark on transparency averages to near-white and
        // would vanish against the card. Stage it on its OPPOSITE instead, and
        // mirror that for an all-black mark so it never sits on near-black.
        let lum = (0.299 * Double(ar) + 0.587 * Double(ag) + 0.114 * Double(ab)) / Double(an)
        if lum > 225 { return RGB(hex: 0x26_31_3C) }
        if lum < 30 { return RGB(hex: 0xE9_F1_F9) }
        return RGB(sum: ar, ag, ab, over: an)
    }

    private static func luminance(_ r: Int, _ g: Int, _ b: Int) -> Double {
        0.299 * Double(r) + 0.587 * Double(g) + 0.114 * Double(b)
    }

    /// Undo premultiplication for one channel.
    private static func straighten(_ value: UInt8, _ alpha: Int) -> Int {
        guard alpha > 0, alpha < 255 else { return Int(value) }
        return min(255, Int(value) * 255 / alpha)
    }

    /// Draw the image into a 16x16 RGBA buffer. Alpha is preserved so the
    /// transparent-logo path above can tell "no pixel" from "white pixel".
    private nonisolated static func samples(_ cg: CGImage) -> [UInt8]? {
        var data = [UInt8](repeating: 0, count: side * side * 4)
        let drew = data.withUnsafeMutableBytes { raw -> Bool in
            guard let base = raw.baseAddress,
                let ctx = CGContext(
                    data: base, width: side, height: side, bitsPerComponent: 8,
                    bytesPerRow: side * 4, space: CGColorSpaceCreateDeviceRGB(),
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
            else { return false }
            ctx.interpolationQuality = .medium
            ctx.draw(cg, in: CGRect(x: 0, y: 0, width: side, height: side))
            return true
        }
        return drew ? data : nil
    }
}
