// ATTACHMENT BYTES ON DISK, KEPT WARM. Quick Look reads a file, so every preview
// ends here; the point of this cache is that it is usually already full by the
// time somebody clicks.
//
// It exists because the same bytes were being pulled twice. Rendering a photo in
// the message column downloads the ORIGINAL and keeps only a 1200px raster of it,
// so clicking that photo — the one already on screen — went back to the daemon
// for bytes the app had held minutes earlier, and did not start until the click.
// Now the render path hands its bytes here on the way past, and the click finds a
// file already written.
//
// LIFETIME, stated plainly, because it is looser than the rule it replaced.
// Bytes used to die with the panel that showed them. They now live in a bounded
// on-disk cache: at most `keepMax` files, oldest evicted first, never the one a
// panel is currently showing. The cache is emptied on account switch and the
// whole staging root is swept at launch, so nothing crosses a session or an
// account — but within a session, a picture you looked at is still on disk.
// That is the trade the instant second open is bought with.
//
// IDS ARE ONE DAEMON'S SQLITE INTS. A file surviving an account switch would not
// merely be stale, it would be another account's attachment under this account's
// id, so `wipe()` empties the table AND fences the fetches already in flight
// behind a generation counter.

import Foundation

@MainActor
final class AttachmentFiles {
    static let shared = AttachmentFiles()

    /// How many staged files to keep. Comfortably more than one message's worth
    /// of attachments, so arrowing through a strip in the panel never evicts a
    /// neighbour it is about to come back to.
    private static let keepMax = 16

    private var staged: [Int: StagedAttachment] = [:]
    /// LRU order, least recent first.
    private var order: [Int] = []
    /// One download per id, however many callers ask at once — a hover and the
    /// click that follows it must not become two requests.
    private var inFlight: [Int: Task<StagedAttachment, Error>] = [:]
    /// Bumped by `wipe()`. A fetch that started before the switch must not file
    /// its bytes under an id that now means something else.
    private var generation = 0

    /// What a preview is showing right now. Eviction steps over it: pulling the
    /// file out from under an open panel would blank the thing being looked at.
    var pinned: Int?

    private init() {}

    // MARK: - reading

    /// The file for this attachment if it is already on disk, without starting
    /// any work. This is the fast path the whole file exists for.
    func cached(_ id: Int) -> StagedAttachment? {
        guard let file = staged[id] else { return nil }
        touch(id)
        return file
    }

    /// The file for this attachment, fetching and staging it if needed.
    func file(for attachment: Attachment) async throws -> StagedAttachment {
        if let hit = cached(attachment.id) { return hit }

        // Read the generation HERE, before any awaiting, and re-check it after.
        // Every caller does this — including the one that merely joined a fetch
        // somebody else started, which is the case that makes it necessary: the
        // starter's own check does not cover the joiner, and a joiner that
        // skipped it would file a just-deleted path under an id that now belongs
        // to a different account.
        let gen = generation
        let task: Task<StagedAttachment, Error>
        if let running = inFlight[attachment.id] {
            task = running
        } else {
            task = Task {
                let fetched = try await APIClient.shared.fetchAttachment(
                    attachment.id, fallbackName: attachment.filename)
                return try StagedAttachment.stage(
                    id: attachment.id, bytes: fetched.bytes, filename: fetched.filename)
            }
            inFlight[attachment.id] = task
        }

        // Clear the slot only if it is still OURS: a later caller may already
        // have started a fresh fetch under this id.
        defer { if inFlight[attachment.id] == task { inFlight[attachment.id] = nil } }

        let file = try await task.value
        guard gen == generation else {
            file.cleanUp()
            throw CancellationError()
        }
        insert(file)
        return file
    }

    /// Start the fetch without waiting for it — the hover ahead of a click.
    /// Silent by design: a warm-up that fails costs nothing, because the click
    /// that follows will ask again and report properly.
    func warm(_ attachment: Attachment) {
        guard staged[attachment.id] == nil, inFlight[attachment.id] == nil else { return }
        Task { _ = try? await file(for: attachment) }
    }

    // MARK: - writing

    /// Stage bytes the caller already has. The render path calls this on its way
    /// past: it just downloaded the original to make column art, and a second
    /// download of the same picture is pure latency.
    func keep(id: Int, bytes: Data, filename: String) {
        guard staged[id] == nil else {
            touch(id)
            return
        }
        guard let file = try? StagedAttachment.stage(id: id, bytes: bytes, filename: filename)
        else { return }
        insert(file)
    }

    /// Drop everything and cancel what is in flight. An account switch, for the
    /// reason in the file header.
    func wipe() {
        generation &+= 1
        for task in inFlight.values { task.cancel() }
        inFlight.removeAll()
        for file in staged.values { file.cleanUp() }
        staged.removeAll()
        order.removeAll()
        pinned = nil
    }

    // MARK: - bookkeeping

    private func insert(_ file: StagedAttachment) {
        staged[file.id] = file
        touch(file.id)
        evict()
    }

    private func touch(_ id: Int) {
        order.removeAll { $0 == id }
        order.append(id)
    }

    private func evict() {
        while order.count > Self.keepMax {
            guard let victim = order.first(where: { $0 != pinned }) else { return }
            order.removeAll { $0 == victim }
            staged.removeValue(forKey: victim)?.cleanUp()
        }
    }
}
