// AUTOSAVE FOR BOTH COMPOSERS. A reply you were halfway through is not something
// an Esc, a queue step, or a closed window gets to destroy — there is no undo for
// a lost draft — so every keystroke arms a debounced PUT /client/drafts and every
// exit path flushes one last save on the way out.
//
// It is SILENT, without exception: no toasts, no error text, no spinner. A save
// that failed is invisible, because an autosave the reader did not ask for must
// never interrupt what they are writing to report on itself. The only thing a
// failure costs is the restore.
//
// Two slots, saved independently — the pane composer and the reader's inline
// reply are two live drafts at once, keyed differently server-side (nil vs. the
// parent message id), so they debounce and flush on their own timers.

import Foundation

@MainActor
final class DraftSaver {
    static let shared = DraftSaver()

    /// Which composer a save belongs to. Mirrors the two `ComposeState` slots on
    /// the store; nothing else has a draft.
    enum Slot: Hashable {
        case compose
        case inlineReply
    }

    /// Long enough that a normal typing rhythm writes once per pause rather than
    /// once per word, short enough that anything worth keeping is on disk within a
    /// breath of the reader stopping.
    private static let debounce: Duration = .seconds(1)

    private var store: AppStore { AppStore.shared }

    /// Armed debounce per slot. A fresh keystroke cancels the previous one.
    private var pending: [Slot: Task<Void, Never>] = [:]
    /// Slots edited since they opened. THE gate on flushing: a composer that was
    /// opened and closed without a keystroke — including one that opened onto a
    /// restored draft and left it alone — must not write anything, or every glance
    /// at a draft would bump its `updated_at`.
    private var touched: Set<Slot> = []
    /// The save currently on the wire per slot, so the next one waits for it. Two
    /// PUTs racing on one key can land out of order, and the loser would be the
    /// newer text. This closes the overlap that actually happens — a debounced save
    /// still in flight when the composer closes and flushes — rather than being a
    /// general queue.
    private var inflight: [Slot: Task<Void, Never>] = [:]

    private init() {}

    // MARK: - lifecycle

    /// A slot just opened. Any timer left by its previous occupant is stale, and
    /// the fresh draft has nothing to save yet — a restore is not an edit.
    func noteOpened(_ slot: Slot) {
        pending[slot]?.cancel()
        pending[slot] = nil
        touched.remove(slot)
    }

    /// One field of one composer changed. Called from the composers' binding
    /// setters — the single funnel every edit passes through, so there is no way
    /// to type into a draft without arming its save.
    func noteChange(_ slot: Slot) {
        touched.insert(slot)
        pending[slot]?.cancel()
        pending[slot] = Task { [weak self] in
            try? await Task.sleep(for: Self.debounce)
            guard !Task.isCancelled, let self else { return }
            // Before clearing the marker: a keystroke during the sleep already
            // cancelled us and installed its own task, and nil-ing that one would
            // strand it.
            self.pending[slot] = nil
            // Read the slot NOW rather than capturing values at arm time — the
            // reader kept typing for a second after the change that armed this.
            //
            // Never while a send is on the wire: that request carries the draft's
            // id and deletes the row, so a save landing behind it would resurrect
            // a draft of mail that has already gone out. A failed send leaves the
            // composer open and `touched` set, so nothing is lost by waiting.
            guard let state = self.state(of: slot), !state.sending else { return }
            await self.write(slot, state)
        }
    }

    /// A closing composer's LAST save, fire-and-forget: the caller is about to
    /// clear the slot, so the values are passed in rather than read back.
    func flush(_ slot: Slot, _ state: ComposeState?) {
        pending[slot]?.cancel()
        pending[slot] = nil
        // Nothing typed => nothing to write. `remove` both tests and clears, so a
        // second exit path firing on the same close cannot double-save.
        guard touched.remove(slot) != nil, let state else { return }
        Task { await write(slot, state) }
    }

    /// The send SUCCEEDED, which already deleted this draft server-side. Drop the
    /// timer and the pending flush with it: the composer closes next, and a flush
    /// there would faithfully re-create the draft for mail that has gone out.
    func noteSent(_ slot: Slot) {
        pending[slot]?.cancel()
        pending[slot] = nil
        touched.remove(slot)
    }

    // MARK: - the write

    private func state(of slot: Slot) -> ComposeState? {
        switch slot {
        case .compose: store.compose
        case .inlineReply: store.inlineReply
        }
    }

    private func write(_ slot: Slot, _ state: ComposeState) async {
        // Wait out the slot's previous save — see `inflight`.
        if let running = inflight[slot] { await running.value }
        let task = Task { await save(slot, state) }
        inflight[slot] = task
        await task.value
        // Only our OWN marker: a later save may have replaced it while we were
        // suspended, and nil-ing that one would let the next writer overlap it.
        if inflight[slot] == task { inflight[slot] = nil }
    }

    private func save(_ slot: Slot, _ state: ComposeState) async {
        let blank =
            state.to.trimmed.isEmpty && state.subject.trimmed.isEmpty
            && state.body.trimmed.isEmpty
        if blank {
            // EMPTIED, not composed: a draft cleared back to nothing is discarded
            // rather than saved as a blank row that would restore as one. With no
            // id yet there was never a row, so there is nothing to discard either.
            guard let id = state.draftId else { return }
            try? await APIClient.shared.deleteDraft(id)
            adopt(nil, slot: slot, key: state.replyToMessageId)
            return
        }
        do {
            let saved = try await APIClient.shared.putDraft(
                replyToMessageId: state.replyToMessageId, to: state.to,
                subject: state.subject, body: state.body)
            adopt(saved.id, slot: slot, key: state.replyToMessageId)
        } catch {
            // Silent, always. A 404 here is a parent sealed since the composer
            // opened — the draft is deliberately unsavable, and the reader is told
            // nothing because there is nothing for them to do about it.
        }
    }

    /// Write the server's id back into the slot — the thing `send` needs so a
    /// successful send takes the draft with it.
    ///
    /// The slot may have moved on while the request was in flight: closed, or
    /// re-opened on a different message. `key` is what it was saved for, so a
    /// mismatch means the id belongs to a draft nobody is editing any more and is
    /// dropped rather than stamped onto the current one.
    private func adopt(_ id: Int?, slot: Slot, key: Int?) {
        switch slot {
        case .compose:
            guard var next = store.compose, next.replyToMessageId == key else { return }
            next.draftId = id
            store.compose = next
        case .inlineReply:
            guard var next = store.inlineReply, next.replyToMessageId == key else { return }
            next.draftId = id
            store.inlineReply = next
        }
    }
}
