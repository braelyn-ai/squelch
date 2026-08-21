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
            // The message leaves the working set: drop its remembered heights and
            // unpin its images. The bytes stay on disk — the undo below is five
            // seconds away and re-pins them.
            FrameHeights.shared.clear(messageId: u.id)
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
