// A small circular sender avatar.
//
// HUMAN senders: initials over a deterministic, theme-aware background derived
// purely from the sender string — LOCAL-ONLY, no network fetch ever. Remote
// avatars would leak the human correspondent graph, which is the one thing this
// app must never do.
//
// ROBOT / BRAND senders (no-reply@, notifications@, billing@, eBay@eBay.com):
// the domain's favicon, fetched at most once per domain with the verdict cached
// across launches. On failure we fall back to initials seamlessly. Robot
// mailboxes name a service, not a person, so this leaks nothing about who a
// human talks to.
//
// Ported from squelch-desktop/src/components/Avatar.tsx.

import SwiftUI

struct Avatar: View {
    let sender: String
    var size: CGFloat = 22
    /// Draw a subtle accent ring (e.g. a known contact).
    var known = false

    @State private var favicon: NSImage?
    @State private var failed = false

    private var domain: String? { SenderID.eligibleFaviconDomain(sender) }

    var body: some View {
        Group {
            if let favicon, !failed {
                Image(nsImage: favicon)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fit)
                    .frame(width: size, height: size)
                    .clipShape(RoundedRectangle(cornerRadius: size * 0.24, style: .continuous))
            } else {
                initialsAvatar
            }
        }
        .overlay {
            if known {
                Circle().strokeBorder(Palette.accent.opacity(0.6), lineWidth: 1.5)
            }
        }
        .help(sender)
        .task(id: sender) { await loadFavicon() }
    }

    private var initialsAvatar: some View {
        let colors = Palette.avatarColors(for: sender)
        return Text(SenderID.initials(sender))
            .font(.system(size: size * 0.42, weight: .semibold))
            .foregroundStyle(colors.fg)
            .frame(width: size, height: size)
            .background(Circle().fill(colors.bg))
    }

    private func loadFavicon() async {
        guard let domain else { return }
        // A previously-failed domain never re-fetches.
        if FaviconCache.shared.verdict(domain) == .failed {
            failed = true
            return
        }
        guard let url = SenderID.faviconURL(domain) else { return }
        guard let image = await FaviconLoader.shared.load(url: url, domain: domain) else {
            failed = true
            return
        }
        favicon = image
    }
}

/// Fetches + memoizes favicons. One request per domain per launch, with the
/// ok/failed verdict persisted so a dead domain is never retried across
/// sessions either. Sends no referrer and no credentials.
@MainActor
final class FaviconLoader {
    static let shared = FaviconLoader()

    private var images: [String: NSImage] = [:]
    private var inflight: [String: Task<NSImage?, Never>] = [:]

    private let session: URLSession = {
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = 8
        cfg.httpShouldSetCookies = false
        return URLSession(configuration: cfg)
    }()

    private init() {}

    func load(url: URL, domain: String) async -> NSImage? {
        if let cached = images[domain] { return cached }
        if let task = inflight[domain] { return await task.value }

        let task = Task<NSImage?, Never> { [session] in
            var req = URLRequest(url: url)
            req.setValue("", forHTTPHeaderField: "Referer")
            guard let (data, response) = try? await session.data(for: req),
                let http = response as? HTTPURLResponse, http.statusCode == 200,
                let image = NSImage(data: data)
            else { return nil }
            // Blank/tiny responses (DDG's fallback) aren't real logos.
            guard image.size.width > 1, image.size.height > 1 else { return nil }
            return image
        }
        inflight[domain] = task
        let result = await task.value
        inflight[domain] = nil

        if let result {
            images[domain] = result
            FaviconCache.shared.record(domain, .ok)
        } else {
            FaviconCache.shared.record(domain, .failed)
        }
        return result
    }
}
