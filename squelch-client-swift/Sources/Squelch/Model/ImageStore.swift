// THE EMAIL IMAGE CACHE — bytes on disk, fetched by us (ImageProxy mints the
// references, ImageSchemeHandler answers them), pinned to the mail that needs them
// and kept across launches. AN ACTOR, not a lock: every operation awaits, and
// in-flight dedupe PARKS a caller on someone else's task, which a lock cannot
// express without holding it across an await. Files are named sha256(url) and the
// manifest holds no URLs — see docs/SECURITY.md §3. A pin is privilege, not
// exemption: pinned bytes have a budget of their own and evict oldest-first past
// it, because nothing on the wire bounds how many images one message can cite.

import CryptoKit
import Foundation

actor ImageStore {
    static let shared = ImageStore()

    /// One image, as the handler needs to answer with it.
    struct Fetched: Sendable {
        var data: Data
        var mime: String
    }

    /// Per-hash cache metadata, holding NO URLS. `mime` is kept because a disk hit
    /// still has to answer with a content type, and sniffing the bytes back into one
    /// is slower and less accurate than what the server said at fetch time.
    private struct Entry: Codable {
        var size: Int
        var mime: String
        var lastAccess: Double
        /// Message ids pinning this entry. Empty = LRU-evictable.
        var refs: Set<Int>
    }

    private struct Manifest: Codable {
        var entries: [String: Entry] = [:]
    }

    /// One image, capped. Larger than any legitimate email image and small
    /// enough that a hostile host cannot spend our disk in one response.
    private static let maxImageBytes = 25 * 1024 * 1024
    /// Total bytes of UNPINNED entries. Pinned entries sit outside this.
    private static let unpinnedCap = 256 * 1024 * 1024
    /// Total bytes of PINNED entries — the ceiling on the exemption. Evicting a
    /// pinned entry drops only its bytes: the mail is still pinned in spirit and
    /// the next warm re-fetches it, so the cost of being wrong here is one
    /// re-download, not a broken read.
    private static let pinnedCap = 128 * 1024 * 1024
    /// Concurrent fetches during a warm. Matches HeroCache's preload width: wide
    /// enough to hide latency, narrow enough not to saturate the link at launch.
    private static let warmWidth = 4
    /// How long a manifest write is deferred after a change. lastAccess churns on
    /// every image of every opened message, and the manifest is a rebuildable index.
    private static let saveDelay: Duration = .seconds(3)
    /// How many released message ids keep their tombstone (see `release`).
    private static let releaseMemory = 32
    /// Ceiling on the buffer a declared Content-Length may reserve. The header is
    /// attacker-controlled: honouring it to the 25MB cap would let a request that
    /// transfers nothing still allocate 25MB.
    private static let reserveCeiling = 1 << 20

    private let dir: URL
    private let manifestURL: URL
    private var manifest = Manifest()
    private var loaded = false
    private var dirty = false
    private var saveTask: Task<Void, Never>?
    /// url -> the fetch already running for it, so ten <img> tags pointing at
    /// one signature logo cost one request.
    private var inflight: [String: Task<Fetched?, Never>] = [:]
    /// messageId -> hashes it was pinning when released, so the five-second undo
    /// knows what to re-reference. In memory only: a tombstone outliving the launch
    /// would re-pin mail the reader finished with.
    private var released: [Int: Set<String>] = [:]
    private var releaseOrder: [Int] = []
    /// url -> message ids that asked to pin it while its fetch was still in
    /// flight, folded in by `store`. A caller that parks on someone else's
    /// in-flight fetch has no entry to pin at the instant it resumes, and the
    /// order the two resume in is not ours to choose — so the pin is recorded
    /// BEFORE the await and applied by whoever writes the entry.
    private var pendingPins: [String: Set<Int>] = [:]

    /// Ephemeral, and every knob that could leak the reader is off. `urlCache`
    /// is nil because WE are the cache — a second one underneath would hold a
    /// copy of the same bytes with none of the pinning.
    private let session: URLSession = {
        let cfg = URLSessionConfiguration.ephemeral
        cfg.httpShouldSetCookies = false
        cfg.httpCookieAcceptPolicy = .never
        cfg.httpCookieStorage = nil
        cfg.urlCache = nil
        cfg.requestCachePolicy = .reloadIgnoringLocalCacheData
        cfg.timeoutIntervalForRequest = 20
        // The WALL CLOCK for one image, which the line above is NOT: that one is
        // an idle gap, and a host dribbling a byte a second never idles out.
        // Unset, this defaults to seven days.
        cfg.timeoutIntervalForResource = 60
        return URLSession(configuration: cfg)
    }()

    /// Per-task delegate: re-guards every redirect hop.
    private let policy = FetchPolicy()

    /// Paths only — the disk work is in `ensureLoaded`, so it lands on the
    /// actor rather than on whichever thread first mentioned `shared`.
    private init() {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first ?? URL(fileURLWithPath: NSTemporaryDirectory())
        dir = base.appendingPathComponent("Squelch/ImageCache", isDirectory: true)
        manifestURL = dir.appendingPathComponent("manifest.json")
    }

    // MARK: - reads

    /// Bytes for a url: a disk hit answers immediately, a miss fetches through
    /// and stores. nil means "no image" — the handler turns that into a failed
    /// task, i.e. a broken-image glyph, which is the honest outcome.
    ///
    /// `pin` is the warmer's; the scheme handler passes none. Pinning belongs
    /// HERE rather than in a pass afterwards because the entry has to be pinned
    /// before `store` runs the LRU over it.
    func data(for url: String, pin messageId: Int? = nil) async -> (Data, mime: String)? {
        ensureLoaded()
        let hash = Self.hash(url)

        if let entry = manifest.entries[hash] {
            if let bytes = try? Data(contentsOf: fileURL(hash)) {
                touch(hash)
                if let messageId { manifest.entries[hash]?.refs.insert(messageId) }
                return (bytes, mime: entry.mime)
            }
            // Manifest says present, disk disagrees (a purge, a half-written
            // file): forget it and re-fetch rather than serving nothing forever.
            manifest.entries.removeValue(forKey: hash)
            scheduleSave()
        }

        if let messageId { pendingPins[url, default: []].insert(messageId) }

        if let running = inflight[url] {
            guard let fetched = await running.value else { return nil }
            return (fetched.data, mime: fetched.mime)
        }

        let task = Task.detached { [self] in await fetch(url) }
        inflight[url] = task
        let fetched = await task.value
        inflight.removeValue(forKey: url)
        guard let fetched else {
            // The owner of the fetch clears the pins for everyone parked on it:
            // there is no entry to attach them to and never will be.
            pendingPins.removeValue(forKey: url)
            return nil
        }
        store(url, hash, fetched)
        return (fetched.data, mime: fetched.mime)
    }

    // MARK: - warming + pins

    /// Fetch-through every url, `warmWidth` at a time, and pin the results to
    /// `messageId` when one is given. Urls already on disk are pinned without
    /// being read back — the warmer wants the entry, not the bytes.
    func warm(urls: [String], pin messageId: Int?) async {
        ensureLoaded()
        var unique: [String] = []
        var seen = Set<String>()
        for url in urls where seen.insert(url).inserted { unique.append(url) }
        guard !unique.isEmpty else { return }

        var toFetch: [String] = []
        for url in unique {
            let hash = Self.hash(url)
            if manifest.entries[hash] != nil, FileManager.default.fileExists(atPath: fileURL(hash).path) {
                touch(hash)
                if let messageId { manifest.entries[hash]?.refs.insert(messageId) }
            } else {
                toFetch.append(url)
            }
        }

        if !toFetch.isEmpty {
            await withTaskGroup(of: Void.self) { group in
                var next = 0
                var running = 0
                while next < toFetch.count || running > 0 {
                    while running < Self.warmWidth, next < toFetch.count {
                        let url = toFetch[next]
                        next += 1
                        running += 1
                        // Pinned AS IT LANDS, not in a pass after the batch:
                        // every `store` runs the LRU, so an entry fetched early
                        // can be gone before the batch ends, and a pin applied
                        // to an entry that is no longer there is a silent no-op.
                        group.addTask { [weak self] in
                            _ = await self?.data(for: url, pin: messageId)
                        }
                    }
                    await group.next()
                    running -= 1
                }
            }
        }
        scheduleSave()
    }

    /// Drop this message's pins. Deliberately does NOT delete bytes: the action
    /// that calls this is undoable for five seconds, and an entry that is merely
    /// unpinned is still a cache hit until the LRU actually needs the room.
    func release(messageId: Int) {
        ensureLoaded()
        var dropped: Set<String> = []
        for (hash, entry) in manifest.entries where entry.refs.contains(messageId) {
            manifest.entries[hash]?.refs.remove(messageId)
            dropped.insert(hash)
        }
        guard !dropped.isEmpty else { return }
        released[messageId] = dropped
        releaseOrder.removeAll { $0 == messageId }
        releaseOrder.append(messageId)
        while releaseOrder.count > Self.releaseMemory {
            released.removeValue(forKey: releaseOrder.removeFirst())
        }
        scheduleSave()
    }

    /// The inverse of `release`, for an undone "done". A tombstone we no longer
    /// hold is not an error — the next launch's warm re-pins whatever the sitrep
    /// still shows.
    func repin(messageId: Int) {
        ensureLoaded()
        guard let hashes = released.removeValue(forKey: messageId) else { return }
        releaseOrder.removeAll { $0 == messageId }
        for hash in hashes {
            manifest.entries[hash]?.refs.insert(messageId)
        }
        scheduleSave()
    }

    /// Forget pins held by message ids the read model no longer shows — mail
    /// marked done (or archived, or re-triaged away) while the app was closed,
    /// which would otherwise hold its images out of the LRU forever.
    ///
    /// Warming a thread pins every message in it, so a thread's older siblings
    /// are unpinned again by the NEXT launch's reconcile. That is the intent:
    /// only what the sitrep still surfaces earns an exemption.
    func reconcile(activePins: Set<Int>) {
        ensureLoaded()
        var changed = false
        for (hash, entry) in manifest.entries where !entry.refs.isEmpty {
            let kept = entry.refs.intersection(activePins)
            guard kept != entry.refs else { continue }
            manifest.entries[hash]?.refs = kept
            changed = true
        }
        guard changed else { return }
        scheduleSave()
        evictIfNeeded()
    }

    // MARK: - fetch

    /// `nonisolated` so the network work does not occupy the actor: only the
    /// result comes back inside.
    ///
    /// STREAMED, and the cap is enforced AS THE BYTES ARRIVE. `session.data(for:)`
    /// cannot do that at any price: it buffers the whole body itself and never
    /// asks a delegate what to do with the response, so a `data.count` check
    /// afterwards has already paid for the memory it is refusing. Measured on
    /// that shape: an endless chunked `image/png` cost +2.5GB of RSS in under
    /// half a second, and a 407KB gzip bomb with an entirely honest
    /// Content-Length cost 419MB resident — which is also why a header-only
    /// check is not a substitute. Reading the stream lets us stop at the cap and
    /// drop the connection.
    private nonisolated func fetch(_ url: String) async -> Fetched? {
        guard let target = URL(string: url), let scheme = target.scheme?.lowercased(),
            scheme == "http" || scheme == "https"
        else { return nil }

        var request = URLRequest(url: target)
        request.httpShouldHandleCookies = false
        // Empty rather than absent: the document already sets
        // `Referrer-Policy: no-referrer`, and this keeps the same promise on a
        // path WebKit is no longer driving.
        request.setValue("", forHTTPHeaderField: "Referer")
        request.setValue("image/*,*/*;q=0.5", forHTTPHeaderField: "Accept")

        // The declared length is a hint, never a promise — -1 when the response
        // is chunked, and a fraction of the truth when it is compressed. It can
        // only ever save us a transfer we were going to refuse anyway.
        guard let (stream, response) = try? await session.bytes(for: request, delegate: policy),
            let http = response as? HTTPURLResponse, http.statusCode == 200,
            let mime = response.mimeType?.lowercased(), mime.hasPrefix("image/"),
            response.expectedContentLength <= Int64(Self.maxImageBytes)
        else { return nil }

        var bytes: [UInt8] = []
        bytes.reserveCapacity(
            min(max(Int(response.expectedContentLength), 0), Self.reserveCeiling))
        do {
            for try await byte in stream {
                bytes.append(byte)
                // Leaving the loop drops the iterator, which cancels the task
                // and closes the connection. The cap has to stop us RECEIVING,
                // not merely counting.
                if bytes.count > Self.maxImageBytes { return nil }
            }
        } catch { return nil }
        guard !bytes.isEmpty else { return nil }
        return Fetched(data: Data(bytes), mime: mime)
        // What this does NOT bound is CFNetwork's own decompression: a
        // content-coded body is inflated below us, so a burst that arrives all
        // at once is inflated all at once, whatever we do with the stream
        // afterwards (measured identical against a delegate-driven data task
        // that cancels in `didReceive data:`). Once the stream is flowing the
        // inflater is backpressured by our reads — an ENDLESS gzip stream costs
        // single-digit MB — so what is left is one transient spike per hostile
        // response, not a leak and not something that accumulates.
    }

    // MARK: - disk

    private func store(_ url: String, _ hash: String, _ fetched: Fetched) {
        // Claimed whatever happens: a write that fails must not leave a pin
        // waiting for an entry that is never coming.
        let pending = pendingPins.removeValue(forKey: url) ?? []
        guard (try? fetched.data.write(to: fileURL(hash), options: .atomic)) != nil else { return }
        manifest.entries[hash] = Entry(
            size: fetched.data.count, mime: fetched.mime,
            lastAccess: Date().timeIntervalSince1970,
            // Every pin taken while the fetch was in flight, applied BEFORE the
            // eviction below so a warmed image is not evicted as unpinned in the
            // same breath it was stored.
            refs: (manifest.entries[hash]?.refs ?? []).union(pending))
        scheduleSave()
        evictIfNeeded()
    }

    private func touch(_ hash: String) {
        manifest.entries[hash]?.lastAccess = Date().timeIntervalSince1970
        scheduleSave()
    }

    private func fileURL(_ hash: String) -> URL { dir.appendingPathComponent(hash) }

    /// TWO budgets, each evicted oldest-first past its own cap. Pinned entries
    /// are not exempt, only privileged — see the header.
    private func evictIfNeeded() {
        evict(where: { $0.refs.isEmpty }, cap: Self.unpinnedCap)
        evict(where: { !$0.refs.isEmpty }, cap: Self.pinnedCap)
    }

    private func evict(where isMember: (Entry) -> Bool, cap: Int) {
        let members = manifest.entries.filter { isMember($0.value) }
        var total = members.values.reduce(0) { $0 + $1.size }
        guard total > cap else { return }
        for (hash, entry) in members.sorted(by: { $0.value.lastAccess < $1.value.lastAccess }) {
            try? FileManager.default.removeItem(at: fileURL(hash))
            manifest.entries.removeValue(forKey: hash)
            total -= entry.size
            if total <= cap { break }
        }
        scheduleSave()
    }

    private func scheduleSave() {
        dirty = true
        guard saveTask == nil else { return }
        saveTask = Task { [weak self] in
            try? await Task.sleep(for: Self.saveDelay)
            await self?.saveNow()
        }
    }

    private func saveNow() {
        saveTask = nil
        guard dirty else { return }
        dirty = false
        guard let data = try? JSONEncoder().encode(manifest) else { return }
        try? data.write(to: manifestURL, options: .atomic)
    }

    /// First touch: make the directory, read the manifest, and reconcile it with
    /// what is actually on disk. Lazy rather than in `init` so the work lands on
    /// the actor's executor instead of whichever thread first said
    /// `ImageStore.shared`.
    private func ensureLoaded() {
        guard !loaded else { return }
        loaded = true

        let fm = FileManager.default
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        // A cache of other people's imagery has no business in Time Machine or
        // iCloud backups.
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        var mutable = dir
        try? mutable.setResourceValues(values)

        if let data = try? Data(contentsOf: manifestURL),
            let decoded = try? JSONDecoder().decode(Manifest.self, from: data)
        {
            manifest = decoded
        }

        // Both directions of drift are self-healing: an entry whose file is gone
        // is forgotten, and a file no entry knows about is deleted. Without the
        // second half, a lost manifest would leave orphaned bytes that nothing
        // can ever account for or evict.
        let names = Set((try? fm.contentsOfDirectory(atPath: dir.path)) ?? [])
        for hash in manifest.entries.keys where !names.contains(hash) {
            manifest.entries.removeValue(forKey: hash)
        }
        for name in names where Self.isHashName(name) && manifest.entries[name] == nil {
            try? fm.removeItem(at: dir.appendingPathComponent(name))
        }
    }

    private static func isHashName(_ name: String) -> Bool {
        name.count == 64 && name.allSatisfy { $0.isHexDigit && !$0.isUppercase }
    }

    private static func hash(_ url: String) -> String {
        SHA256.hash(data: Data(url.utf8)).map { String(format: "%02x", $0) }.joined()
    }
}

/// Transfer policy for image fetches, applied per task.
///
/// ONE job: re-check every redirect hop for http/https, so a 302 cannot walk the
/// fetch onto `file:` or any other scheme this process can reach.
///
/// It is the only job because it is the only callback that ARRIVES. The async
/// APIs resolve response disposition and body delivery internally and never ask
/// a delegate — `didReceive response:` and `didReceive data:` are both dead on
/// this path, whether the object is installed as the task delegate or the
/// session's (measured: zero calls to either). Status, type and size are
/// therefore checked in `fetch`, against the stream, which is the only place a
/// transfer can still be stopped.
private final class FetchPolicy: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(
        _ session: URLSession, task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        let scheme = request.url?.scheme?.lowercased()
        guard scheme == "http" || scheme == "https" else {
            completionHandler(nil)
            return
        }
        completionHandler(request)
    }
}
