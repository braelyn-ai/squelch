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
    /// Flushes that have been STARTED but have not reached `write` yet. A
    /// flush hands its save to a fresh Task, and that task does not enter
    /// `inflight` until it runs — so `inflight` alone cannot answer "is
    /// anything still going out?", which is the only question `settle` asks.
    private var flushes: [Int: Task<Void, Never>] = [:]
    private var nextFlush = 0

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
        // A FORWARD IS NEVER AUTOSAVED — see `save`. Refused at the door rather
        // than at the write, so the slot is never even marked touched: a flush
        // is gated on that mark, and an untouched slot cannot be talked into a
        // save by any exit path.
        guard state(of: slot)?.forwardOfMessageId == nil else { return }
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
        // Forwards do not autosave — see `save`. Unreachable while `noteChange`
        // refuses to mark them touched, and kept because a flush takes the
        // state from the CALLER: the day something hands this a forward it did
        // not type into, the answer must still be no.
        guard state.forwardOfMessageId == nil else { return }
        let ticket = nextFlush
        nextFlush &+= 1
        flushes[ticket] = Task { [weak self] in
            await self?.write(slot, state)
            self?.flushes[ticket] = nil
        }
    }

    /// Wait until nothing this saver started is still on the wire.
    ///
    /// The account switch's one hard ordering rule: the draft PUTs go to the
    /// daemon that is configured when they are BUILT, so reconfiguring
    /// APIClient while one is parked would post what the human wrote in
    /// account A into account B's drafts, keyed to a `reply_to_message_id`
    /// that means something else there.
    ///
    /// Call it after flushing every slot: `flush` cancels that slot's debounce
    /// timer, and this drains the two tables the timers feed rather than
    /// racing them.
    func settle() async {
        // Flushes first: each one ends inside `write`, which enters (and
        // clears) `inflight` on its way through.
        for (_, task) in flushes { await task.value }
        for (_, task) in inflight { await task.value }
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
        // A FORWARD IS NEVER SAVED. THE reason, stated once here because every
        // other guard in this file points at it: `PUT /client/drafts` has no
        // field for the forwarded message, and the row it writes is keyed by
        // `reply_to_message_id` — which a forward leaves nil, exactly like a
        // plain new message. So an autosaved forward would come back as the
        // account's one new-message draft on the next `c`, wearing its
        // "Fwd: …" subject with the original it was forwarding silently gone,
        // and the reader would send a quote of nothing. Losing an unsent
        // forward is the cheaper failure: the original is still in the mailbox,
        // one `f` away.
        guard state.forwardOfMessageId == nil else { return }
        // A body that is only the seeded signature counts as blank: saving it
        // would mint a draft of nothing, and every later `c` would restore it.
        let blank =
            state.to.trimmed.isEmpty && state.bcc.trimmed.isEmpty
            && state.subject.trimmed.isEmpty
            && Prefs.shared.isBodyUntouched(state.body)
        if blank {
            // EMPTIED, not composed: a draft cleared back to nothing is discarded
            // rather than saved as a blank row that would restore as one. With no
            // id yet there was never a row, so there is nothing to discard either.
            guard let id = state.draftId else { return }
            try? await APIClient.shared.deleteDraft(id)
            adopt(nil, slot: slot, key: state.id)
            return
        }
        do {
            let saved = try await APIClient.shared.putDraft(
                replyToMessageId: state.replyToMessageId, to: state.to, bcc: state.bcc,
                subject: state.subject, body: state.body)
            adopt(saved.id, slot: slot, key: state.id)
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
    /// re-opened on a different message. `key` is the IDENTITY of the composer
    /// the save was taken from (`ComposeState.id`), so a mismatch means the id
    /// belongs to a draft nobody is editing any more and is dropped rather than
    /// stamped onto the current one.
    ///
    /// Identity, not the reply key: `replyToMessageId` is nil for a plain new
    /// message AND for a forward, and identical for two successive composers on
    /// the same message — so a stale flush's id could stamp onto a SUCCESSOR
    /// composer, whose send would then carry it and DELETE a draft holding mail
    /// that never went out. The UUID tells every composer from every other.
    private func adopt(_ id: Int?, slot: Slot, key: UUID) {
        switch slot {
        case .compose:
            guard var next = store.compose, next.id == key else { return }
            next.draftId = id
            store.compose = next
        case .inlineReply:
            guard var next = store.inlineReply, next.id == key else { return }
            next.draftId = id
            store.inlineReply = next
        }
    }
}
