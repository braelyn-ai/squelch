// Small circular sender avatar. Human senders get initials over a deterministic
// theme-aware background derived from the sender string — local only, never a
// network fetch, because a remote avatar leaks the human correspondent graph.
// Robot/brand senders (no-reply@, notifications@, billing@) get the domain
// favicon, fetched once per domain, falling back to initials on failure.

import SwiftUI

struct Avatar: View {
    let sender: String
    var size: CGFloat = 22
    /// Draw a subtle accent ring (e.g. a known contact).
    var known = false

    @State private var favicon: PlatformImage?
    @State private var failed = false

    private var resolved: SenderID.Resolved { SenderCache.resolved(sender) }
    private var domain: String? { resolved.faviconDomain }

    /// Prefer this frame's image, else a synchronous cache read: a row rebuilt
    /// because a selection flip switched which branch of a conditional modifier
    /// it lives in loses `@State`, and would flash initials for the frame
    /// `.task` takes to hand the same cached image back.
    private var image: PlatformImage? {
        favicon ?? domain.flatMap { FaviconLoader.shared.cached($0) }
    }

    var body: some View {
        Group {
            if let favicon = image, !failed {
                Image(platformImage: favicon)
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
        let r = resolved
        let colors = Palette.avatarPalette[r.slot % Palette.avatarPalette.count]
        return Text(r.initials)
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

/// Fetches + memoizes favicons: one request per domain per launch, verdict
/// persisted so a dead domain is never retried across sessions. Ephemeral
/// session, no cookies, no referrer.
@MainActor
final class FaviconLoader {
    static let shared = FaviconLoader()

    /// A logo per domain, so this only bounds how many distinct BRANDS a session
    /// keeps art for. Icons are tiny; the cap is here so the table cannot grow
    /// without end, not because it is expected to fill.
    private static let cacheMax = 512

    /// FAILURES ARE NOT MEMOIZED HERE: `FaviconCache` owns the negative verdict
    /// and ages it out after a week, so a domain that was merely offline stays
    /// re-fetchable rather than pinned dead for the life of the process.
    private let memo = AsyncMemo<String, PlatformImage?>(limit: cacheMax, keep: { $0 != nil })

    private let session = Sessions.ephemeral(timeout: 8, cookies: .neverSent)

    private init() {}

    /// Already-loaded image, no async hop — lets a rebuilt `Avatar` draw on its
    /// first frame.
    func cached(_ domain: String) -> PlatformImage? { memo.cached(domain) ?? nil }

    /// Fetch once per domain per launch, joiners included. The verdict is
    /// recorded from INSIDE the fetch so it lands before any joiner resumes.
    func load(url: URL, domain: String) async -> PlatformImage? {
        await memo.resolve(domain) { [session] in
            let image = await Self.fetch(url, session)
            FaviconCache.shared.record(domain, image == nil ? .failed : .ok)
            return image
        }
    }

    private static func fetch(_ url: URL, _ session: URLSession) async -> PlatformImage? {
        var req = URLRequest(url: url)
        req.setValue("", forHTTPHeaderField: "Referer")
        guard let (data, response) = try? await session.data(for: req),
            let http = response as? HTTPURLResponse, http.statusCode == 200,
            let image = PlatformImage(data: data)
        else { return nil }
        // Blank/tiny responses (DDG's fallback) aren't real logos.
        guard image.size.width > 1, image.size.height > 1 else { return nil }
        return image
    }
}
