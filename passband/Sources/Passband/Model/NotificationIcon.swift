// The picture on a banner: the SENDER's mark, not ours.
//
// macOS draws the posting app's icon at the left of a notification and nothing
// an app does can change that. What it can add is an ATTACHMENT, which the
// system renders as a thumbnail beside the copy — so the thing worth putting
// there is the one the app icon can never say: who the mail is from.
//
// The artwork is the reader's own `Avatar`, rendered off-screen, which is the
// whole reason this file is short: a service sender resolves to its domain
// logo, a human correspondent to initials over a deterministic colour. THAT
// SPLIT IS THE PRIVACY RULE (see SenderIdentity), not a style choice — the
// human correspondent graph must not leave the device, so a person's tile is
// drawn locally and only a brand's is fetched.
//
// NOTHING HERE WAITS ON THE NETWORK. `attachments` is synchronous and draws
// with whatever logo is already in hand; `warm` fetches the missing one for
// NEXT time. An earlier cut awaited the fetch, and a banner that waits is a
// banner that can be lost: both callers write their "already seen" bookkeeping
// before they post, so a deferred post that a quit interrupts never happens and
// never retries — and the banner most likely to be first-of-its-domain is a
// login code, which is the one that cannot afford to arrive eight seconds late.
// A brand's first mail wears initials. Its second, and every row of the sitrep
// in between, wears the logo.
//
// THE DIRECTORY IS THE CONTRACT, the same one StagedAttachment states: every
// tile lands under one root and `purgeRoot()` at launch clears whatever the
// system did not carry off.

import ImageIO
import SwiftUI
import UniformTypeIdentifiers
import UserNotifications

@MainActor
enum NotificationIcon {
    /// Tile edge in points, rendered at 2x. Notification Center draws the
    /// thumbnail small, but it also draws it enlarged in an expanded banner,
    /// and a favicon upscaled from 16px has no more resolution to give either
    /// way — so this is sized for the big case and costs a few KB.
    private static let edge: CGFloat = 128

    /// Rendered tiles, by sender and by which artwork they got. A reconnect
    /// replays every event queued behind it, so one newsletter can post forty
    /// banners in a second and each one would otherwise pay a fresh SwiftUI
    /// render and a PNG encode on the main actor.
    ///
    /// The logo half of the key is what keeps this honest: the same sender
    /// draws a different tile once `warm` lands its logo, and a key of the
    /// sender alone would pin the initials that were drawn before it.
    private static var memo = LRUMap<String, Data>(limit: 256)

    /// The single root every tile lands under, so one sweep empties them all.
    private static var root: URL {
        FileManager.default.temporaryDirectory
            .appendingPathComponent("notification-icons", isDirectory: true)
    }

    /// The banner's thumbnail for one sender. An array because that is the
    /// shape `UNMutableNotificationContent.attachments` wants, and "nothing to
    /// draw" is an ordinary outcome here: a failed render must post the banner
    /// anyway, never swallow it. An empty `sender` means the caller has decided
    /// there is nobody to draw — a read receipt on your own mail.
    ///
    /// `id` is the notification's own request identifier — unique per banner,
    /// which is all an attachment identifier has to be.
    static func attachments(for sender: String, id: String) -> [UNNotificationAttachment] {
        guard !sender.isEmpty, let png = render(sender), let url = write(png) else { return [] }
        guard
            let attachment = try? UNNotificationAttachment(
                identifier: id, url: url,
                options: [UNNotificationAttachmentOptionsTypeHintKey: UTType.png.identifier])
        else {
            // The system MOVES the file into its own store, but only once the
            // request carrying it is accepted. Until then the bytes are ours to
            // clean up — here for a refused attachment, and in `Notifier.send`
            // for a refused request.
            discard([url])
            return []
        }
        return [attachment]
    }

    /// Drop tiles the system never took. Safe on a file already carried off:
    /// the removal simply fails.
    static func discard(_ urls: [URL]) {
        for url in urls { try? FileManager.default.removeItem(at: url) }
    }

    /// Fetch this sender's domain logo for the NEXT banner. Fire and forget, on
    /// purpose — see the header. Only ever for a service sender:
    /// `eligibleFaviconDomain` answers nil for a human, and that nil is the
    /// privacy rule rather than an optimisation.
    ///
    /// The same fetch the sitrep rows make, memoized per domain per launch and
    /// with a failure remembered for a week, so a brand already on screen costs
    /// nothing and a logo-less domain is not re-asked per email.
    static func warm(_ sender: String) {
        guard let domain = SenderCache.resolved(sender).faviconDomain,
            FaviconLoader.shared.cached(domain) == nil,
            FaviconCache.shared.verdict(domain) != .failed,
            let url = SenderID.faviconURL(domain)
        else { return }
        Task { _ = await FaviconLoader.shared.load(url: url, domain: domain) }
    }

    /// Empty the tile root. Called once at launch, for the tiles a crash or a
    /// hard quit stranded between writing one and the system taking it.
    static func purgeRoot() {
        let fm = FileManager.default
        let aside = fm.temporaryDirectory
            .appendingPathComponent("notification-icons-sweeping", isDirectory: true)
        try? fm.removeItem(at: aside)
        guard (try? fm.moveItem(at: root, to: aside)) != nil else { return }
        Task.detached(priority: .utility) { try? fm.removeItem(at: aside) }
    }

    /// The reader's own avatar, off-screen, as PNG bytes.
    ///
    /// ALWAYS the light palette. The tile is baked into the notification at
    /// post time and then outlives the appearance it was posted under — a
    /// banner drawn in dark mode is still sitting in Notification Center at
    /// noon — and it sits beside brand logos that carry their own colours
    /// whatever the system is doing. A tile that picks its own colours once is
    /// the honest version of that. (Verified rather than assumed: the palette's
    /// colours are dynamic NSColor/UIColor, and `ImageRenderer` does resolve
    /// them against the injected `colorScheme` and not the process appearance.)
    private static func render(_ sender: String) -> Data? {
        let resolved = SenderCache.resolved(sender)
        let key = "\(sender)|\(resolved.faviconDomain.flatMap(FaviconLoader.shared.cached) != nil)"
        if let hit = memo.get(key) { return hit }

        let renderer = ImageRenderer(
            content: Avatar(sender: sender, size: edge).environment(\.colorScheme, .light))
        renderer.scale = 2
        guard let image = renderer.cgImage, let png = Raster.png(image) else { return nil }
        memo.set(key, png)
        return png
    }

    /// PNG to a file of its own. UNNotificationAttachment takes a URL and takes
    /// OWNERSHIP of what it finds there, so two banners cannot share a path:
    /// the second would find the first's file already carried off. Which is
    /// also why the memo above holds BYTES and not a URL.
    private static func write(_ png: Data) -> URL? {
        let fm = FileManager.default
        try? fm.createDirectory(at: root, withIntermediateDirectories: true)
        let url = root.appendingPathComponent("\(UUID().uuidString).png")
        guard (try? png.write(to: url, options: .atomic)) != nil else { return nil }
        return url
    }
}
