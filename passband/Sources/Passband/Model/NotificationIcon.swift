// The picture on a banner: the SENDER's mark, not ours.
//
// macOS draws the posting app's icon at the left of every notification and
// nothing an app does can change that. What it can add is an ATTACHMENT, which
// the system renders as a thumbnail beside the copy — so the thing worth
// putting there is the one the app icon can never say: who the mail is from.
//
// The artwork is the reader's own `Avatar`, rendered off-screen, which is the
// whole reason this file is short: a service sender resolves to its domain
// logo, a human correspondent to initials over a deterministic colour. THAT
// SPLIT IS THE PRIVACY RULE (see SenderIdentity), not a style — the human
// correspondent graph must not leave the device, so a person's tile is drawn
// locally and only a brand's is fetched.

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

    /// The banner's thumbnail for one sender. An array because that is the
    /// shape `UNMutableNotificationContent.attachments` wants, and "nothing to
    /// draw" is an ordinary outcome here: a failed render must post the banner
    /// anyway, never swallow it.
    ///
    /// `id` is the notification's own request identifier — unique per banner,
    /// which is all an attachment identifier has to be.
    static func attachments(for sender: String, id: String) async -> [UNNotificationAttachment] {
        await warmLogo(sender)
        guard let image = render(sender), let url = write(image) else { return [] }
        guard
            let attachment = try? UNNotificationAttachment(
                identifier: id, url: url,
                options: [UNNotificationAttachmentOptionsTypeHintKey: UTType.png.identifier])
        else {
            // On success the system MOVES the file into its own store. On
            // rejection it stays behind, so a refused attachment cleans up
            // after itself rather than leaving a PNG per banner in the
            // container's tmp for the life of the install.
            try? FileManager.default.removeItem(at: url)
            return []
        }
        return [attachment]
    }

    /// Resolve the sender's domain logo BEFORE rendering, so the tile draws it
    /// instead of the initials underneath. Only ever for a service sender —
    /// `eligibleFaviconDomain` returns nil for a human, and that nil is the
    /// privacy rule rather than an optimisation.
    ///
    /// This is the one place a banner waits on the network, and the wait is
    /// bounded three ways: the loader's own 8s session timeout, one fetch per
    /// domain per launch (`FaviconLoader` memoizes and joins), and a failure
    /// remembered for a week so a logo-less domain is not re-asked per email.
    /// It is also the same fetch the sitrep rows make, so a brand already on
    /// screen costs nothing at all.
    private static func warmLogo(_ sender: String) async {
        guard let domain = SenderID.eligibleFaviconDomain(sender),
            FaviconLoader.shared.cached(domain) == nil,
            FaviconCache.shared.verdict(domain) != .failed,
            let url = SenderID.faviconURL(domain)
        else { return }
        _ = await FaviconLoader.shared.load(url: url, domain: domain)
    }

    /// The reader's own avatar, off-screen.
    ///
    /// ALWAYS the light palette. The tile is baked into the notification at
    /// post time and then outlives the appearance it was posted under — a
    /// banner drawn in dark mode is still sitting in Notification Center at
    /// noon — and it sits beside brand logos that carry their own colours
    /// whatever the system is doing. A tile that picks its own colours once is
    /// the honest version of that.
    private static func render(_ sender: String) -> CGImage? {
        let renderer = ImageRenderer(
            content: Avatar(sender: sender, size: edge).environment(\.colorScheme, .light))
        renderer.scale = 2
        return renderer.cgImage
    }

    /// PNG to a file of its own. UNNotificationAttachment takes a URL and takes
    /// OWNERSHIP of what it finds there, so two banners cannot share a path:
    /// the second would find the first's file already carried off.
    private static func write(_ image: CGImage) -> URL? {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("notification-icons", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let url = dir.appendingPathComponent("\(UUID().uuidString).png")
        guard
            let destination = CGImageDestinationCreateWithURL(
                url as CFURL, UTType.png.identifier as CFString, 1, nil)
        else { return nil }
        CGImageDestinationAddImage(destination, image, nil)
        guard CGImageDestinationFinalize(destination) else {
            try? FileManager.default.removeItem(at: url)
            return nil
        }
        return url
    }
}
