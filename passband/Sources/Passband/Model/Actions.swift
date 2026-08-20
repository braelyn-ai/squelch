// ACTION DISPATCH — the canonical verb API the read views call.
//
// UNDO-FIRST: archive/done/label fire instantly, the row leaves its band
// optimistically, and a 5s undo toast carries the inverse call as its `revert`.
// `send` is the only ceremony path, and it does not live here: reply() opens the
// email and the reader's inline composer runs the two-phase send inside it.

import Foundation

private let inboxLabel = "INBOX"

@MainActor
enum Actions {
    private static var store: AppStore { AppStore.shared }

    /// Archive (undo-first): the row leaves instantly, an INBOX-relabel is the
    /// revert.
    static func archive(_ u: AttentionUpdate) async {
        let restore = store.removeFromBands(u.id)
        do {
            try await APIClient.shared.actionArchive(u.id)
            Analytics.capture("email_archived")
            store.pushUndo(kind: .archive, messageId: u.id, label: "archived \(u.sender)") {
                // archive undo = re-add the INBOX label; the poll refreshes.
                try await APIClient.shared.actionLabel(u.id, add: [inboxLabel])
            }
        } catch {
            restore()
            if let api = error as? APIError, api.kind == .forbidden {
                store.pushToast("no write credential · run squelchd auth --write", .error)
            } else {
                store.pushToast(errText(error, "archive failed"), .error)
            }
        }
    }

    /// Done (undo-first): status->done, revert resets status to open.
    static func done(_ u: AttentionUpdate) async {
        let restore = store.removeFromBands(u.id)
        do {
            try await APIClient.shared.setStatus(u.id, .done)
            Analytics.capture("email_done")
            // The message leaves the working set: drop its remembered height and
            // unpin its images. The bytes stay on disk — the undo below is five
            // seconds away and re-pins them.
            FrameHeights.shared.clear(String(u.id))
            await ImageStore.shared.release(messageId: u.id)
            store.pushUndo(kind: .done, messageId: u.id, label: "done \(u.sender)") {
                try await APIClient.shared.setStatus(u.id, .open)
                await ImageStore.shared.repin(messageId: u.id)
            }
        } catch {
            restore()
            store.pushToast(errText(error, "done failed"), .error)
        }
    }

    /// Remind (undo-first): park the thread until `date`. The daemon resolves it
    /// exactly the way `done` does and re-opens the message when the stamp comes
    /// due, so this row leaves every band NOW — a reminder you still have to
    /// look at is not a reminder, it is a second inbox.
    ///
    /// `label` is the phrase the undo chip shows, and callers pass the row's
    /// ABSOLUTE `detail` ("tomorrow 9:00 AM") rather than the words that were
    /// typed: the one thing the confirmation has to answer is when this comes
    /// back, and "next week" does not answer it.
    /// Takes a message id rather than a row because the reader can press `H` on
    /// mail it never had an `AttentionUpdate` for (a thread opened from search
    /// has no queue). The row is looked up where it exists, and where it does
    /// not the parked list is simply invalidated instead of guessed at.
    ///
    /// RETURNS WHETHER THE STAMP LANDED. Callers choreograph off this: the
    /// palette is still on screen when this returns, and the reader's walk to
    /// the next email is the app saying "the reminder took". Both of those on a
    /// 400 would leave the user watching the mail leave while a toast says it
    /// did not. Deliberately NOT `@discardableResult`: swallowing this is the
    /// exact bug the return value exists to prevent.
    static func remind(_ messageId: Int, at date: Date, label: String) async -> Bool {
        let row = store.update(id: messageId)
        // The stamp this is moving OFF, read BEFORE the call: a second `H` on
        // already-parked mail is a reschedule, and after the POST the row's own
        // remind_at is the new one. See the undo below.
        let prior = Fmt.date(row?.remind_at)
        let restore = store.removeFromBands(messageId)
        do {
            let result = try await APIClient.shared.setReminder(messageId, at: date)
            Analytics.capture("email_remind")
            // Off the mail pages too: `removeFromBands` only covers the sitrep,
            // and the row is done from this moment on either way.
            store.removeFromMail(messageId)
            // The server's own stamp when it sent one — it is the value the
            // parked list will be re-fetched with, and a local string that
            // disagrees would make the row jump on the next poll.
            let stamp = result.remind_at ?? APIClient.rfc3339(date)
            if let row {
                store.noteReminder(row, remindAt: stamp)
            } else {
                store.invalidateReminders()
            }
            // Same as `done`: the message leaves the working set. The undo five
            // seconds away re-pins it.
            FrameHeights.shared.clear(String(messageId))
            await ImageStore.shared.release(messageId: messageId)
            store.pushUndo(kind: .remind, messageId: messageId, label: "reminder set for \(label)")
            {
                // A RESCHEDULE UNDOES TO THE REMINDER IT MOVED OFF. "Move
                // tomorrow's reminder to next week" undone has to mean parked
                // until tomorrow again — clearing it outright would take the
                // undo of a MOVE and turn it into a delete of something the
                // user never asked to lose. Only a stamp still in the future is
                // worth restoring; one that came due while the toast was up is
                // just the clear below.
                if let prior, prior > Date() {
                    let back = try await APIClient.shared.setReminder(messageId, at: prior)
                    let stamp = back.remind_at ?? APIClient.rfc3339(prior)
                    await MainActor.run {
                        if let row {
                            AppStore.shared.noteReminder(row, remindAt: stamp)
                        } else {
                            AppStore.shared.invalidateReminders()
                        }
                    }
                    // Still parked, so the mail stays out of the working set:
                    // no repin, and the row belongs on the parked list.
                    return
                }
                // BOTH halves, REOPEN FIRST: a failure between the two then
                // leaves an open row still carrying its stamp, and a stamp that
                // fires on mail already back in the inbox costs nothing. The
                // other order leaves the mail marked done with no reminder left
                // to unpark it, which is mail that silently never comes back.
                try await APIClient.shared.setStatus(messageId, .open)
                try await APIClient.shared.clearReminder(messageId)
                await ImageStore.shared.repin(messageId: messageId)
                await MainActor.run { AppStore.shared.dropReminder(messageId) }
            }
            return true
        } catch {
            restore()
            store.pushToast(errText(error, "could not set the reminder"), .error)
            return false
        }
    }

    /// Cancel a pending reminder WITHOUT reopening the mail. The thread was
    /// resolved when the reminder was set, and cancelling is a statement about
    /// the reminder only: "no, I do not need to see this again."
    static func cancelReminder(_ u: AttentionUpdate) async {
        store.dropReminder(u.id)
        do {
            try await APIClient.shared.clearReminder(u.id)
            // The local list is now ahead of the cached one's TTL. Make the
            // next tick go and ask, so the truth arrives on its own rather than
            // up to a TTL later.
            store.invalidateReminders()
            store.pushToast("reminder cancelled", .info)
        } catch {
            // Put it back: the daemon still holds the stamp, and a row missing
            // from this list is a reminder the user believes is gone.
            store.noteReminder(u, remindAt: u.remind_at ?? "")
            store.pushToast(errText(error, "could not cancel the reminder"), .error)
        }
    }

    /// Reopen a done item. No undo toast; it's already a recovery.
    static func reopen(_ u: AttentionUpdate) async {
        do {
            try await APIClient.shared.setStatus(u.id, .open)
            Analytics.capture("email_reopened")
            store.pushToast("reopened \(u.sender)", .info)
        } catch {
            store.pushToast(errText(error, "reopen failed"), .error)
        }
    }

    /// Add/remove labels (undo-first for the common archive-restore inverse).
    /// The row is NOT pulled from its band for a plain label edit.
    static func label(_ u: AttentionUpdate, add: [String] = [], remove: [String] = []) async {
        do {
            try await APIClient.shared.actionLabel(u.id, add: add, remove: remove)
            Analytics.capture("email_labeled")
            store.pushUndo(kind: .label, messageId: u.id, label: "labeled \(u.sender)") {
                try await APIClient.shared.actionLabel(u.id, add: remove, remove: add)
            }
        } catch {
            if let api = error as? APIError, api.kind == .forbidden {
                store.pushToast("no write credential · run squelchd auth --write", .error)
            } else {
                store.pushToast(errText(error, "label failed"), .error)
            }
        }
    }

    /// Reply: open the EMAIL and compose inside it. One reply surface for the
    /// whole app — you answer where you read, with the thread on screen, rather
    /// than in a modal that hides the mail you are answering.
    ///
    /// Nothing is prefilled: the request carries `reply_to_message_id` and the
    /// body, and the daemon derives the recipient and `Re: <parent subject>` from
    /// the parent message (`u.one_line` is the LLM's summary, not the header, so
    /// it was never a subject worth sending).
    ///
    /// `queue` is the caller's own row order, so "done + next" still walks it
    /// once the reader is open.
    static func reply(_ u: AttentionUpdate, queue: [AttentionUpdate] = []) {
        store.openThread(u.thread_id, queue: queue, replyTo: u.id)
    }

    /// Tune a sender: open the rule editor prefilled with *@domain.
    static func tune(sender: String) {
        store.openRuleEditor(RuleEditorRequest(sender: sender))
    }

    /// Create a squelch rule matching `sender` EXACTLY (not *@domain) — this one
    /// sender abused the situation, not necessarily its whole domain. Pass the
    /// message the block was invoked from (when one is on screen) and the
    /// server resolves it to done — blocking IS that email's disposition.
    ///
    /// The disposition is stated EXPLICITLY and must stay that way: this path
    /// has no want text to infer from, and the daemon only sweeps on an explicit
    /// squelch. Do not "simplify" it to the inferred (absent) form.
    ///
    /// This is also the ONE caller that sets `sweep`, and it should: blocking a
    /// sender means their open mail goes too, not just the message on screen.
    /// Every other create path (the rule editor's save, an undo recreating a
    /// deleted rule) leaves the flag absent and touches nothing already in the
    /// bands.
    static func createBlockRule(sender: String, sourceMessageId: Int? = nil) async throws {
        try await APIClient.shared.createRule(
            CreateRuleBody(
                match_pattern: sender.trimmingCharacters(in: .whitespaces).lowercased(),
                want: "", disposition: .squelch,
                source_message_id: sourceMessageId, sweep: true))
        Analytics.capture("block_rule_created")
    }
}
