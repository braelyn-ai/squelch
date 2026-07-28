// Newsletter-thumb fill colour: sample the hero's pixels and reduce them to the
// ONE colour the rest of the square should be painted.
//
// Ported 1:1 from squelch-desktop/src/lib/imageFill.ts, minus its transport —
// the Tauri build had to fetch bytes through a Rust command and decode them in a
// same-origin blob to dodge webview CORS taint. Here the image is already an
// NSImage in hand, so this starts at the pixels.
//
// Every failure path returns nil and the card keeps its neutral well; nothing
// here ever throws to the caller.

import AppKit
import SwiftUI

enum ImageFill {
    /// Downsample size. 256 samples is enough to find a dominant region and
    /// cheap enough to run per card.
    private static let side = 16

    /// A dominant-region bucket. Counts stay separate from the sums so the
    /// winner reports its TRUE average rather than the quantized key.
    private struct Bucket {
        var r = 0, g = 0, b = 0, n = 0
    }

    /// DOMINANT colour of an image, not the average: newsletter heroes are
    /// mostly white margin, so a plain average washes out to near-white.
    ///
    /// 16x16 downsample; skip transparent, near-white and near-black pixels
    /// (margins, backgrounds, text ink); coarse-quantize the rest to 3 bits per
    /// channel and take the biggest bucket's true average. Falls back to the
    /// overall average when nothing mid-tone dominated, and to nil if the image
    /// could not be sampled at all.
    static func dominant(_ image: NSImage) -> Color? {
        guard let px = samples(image) else { return nil }

        var buckets: [Int: Bucket] = [:]
        var ar = 0, ag = 0, ab = 0, an = 0

        var i = 0
        while i + 3 < px.count {
            defer { i += 4 }
            let a = Int(px[i + 3])
            if a < 128 { continue }
            // CGContext hands back PREMULTIPLIED bytes; the canvas this was
            // ported from hands back straight ones. Undo it, or every pixel
            // with partial alpha reads darker than it looks.
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
            return Color(
                .sRGB,
                red: Double(best.r) / Double(best.n) / 255,
                green: Double(best.g) / Double(best.n) / 255,
                blue: Double(best.b) / Double(best.n) / 255,
                opacity: 1)
        }

        guard an > 0 else { return nil }

        // NO DOMINANT MID-TONE REGION => a monochrome logo, and this is the
        // transparent-background case: transparent pixels were skipped
        // entirely, so a WHITE mark on transparency averages to near-white and
        // would vanish against the card. Stage it on its OPPOSITE instead, and
        // mirror that for an all-black mark so it never sits on near-black.
        let lum = (0.299 * Double(ar) + 0.587 * Double(ag) + 0.114 * Double(ab)) / Double(an)
        if lum > 225 { return Color(hex: 0x26_31_3C) }
        if lum < 30 { return Color(hex: 0xE9_F1_F9) }
        return Color(
            .sRGB,
            red: Double(ar) / Double(an) / 255,
            green: Double(ag) / Double(an) / 255,
            blue: Double(ab) / Double(an) / 255,
            opacity: 1)
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
    private static func samples(_ image: NSImage) -> [UInt8]? {
        guard let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil) else {
            return nil
        }
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
