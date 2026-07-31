// ACTION DISPATCH — the canonical verb API the read views call.
//
// UNDO-FIRST: archive/done/label fire instantly, the row leaves its band
// optimistically, and a 5s undo toast carries the inverse call as its `revert`.
// `send` is the only ceremony path (the compose overlay, reached via reply()).

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

    /// Reopen a done item. No undo toast; it's already a recovery.
    static func reopen(_ u: AttentionUpdate) async {
        do {
            try await APIClient.shared.setStatus(u.id, .open)
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

    /// Reply: open the compose/review ceremony prefilled from the update.
    /// Subject stays EMPTY — `u.one_line` is the LLM's summary, not the real
    /// subject, and the daemon derives `Re: <parent subject>` when the send body
    /// omits `subject` entirely. Typing one here overrides that derivation.
    static func reply(_ u: AttentionUpdate) {
        store.openCompose(
            ComposeState(replyToMessageId: u.id, to: u.sender, subject: "", body: ""))
    }

    /// Tune a sender: open the rule editor prefilled with *@domain.
    static func tune(sender: String) {
        store.openRuleEditor(RuleEditorRequest(sender: sender))
    }

    /// Create a squelch rule matching `sender` EXACTLY (not *@domain) — this one
    /// sender abused the situation, not necessarily its whole domain.
    static func createBlockRule(sender: String) async throws {
        try await APIClient.shared.createRule(
            CreateRuleBody(
                match_pattern: sender.trimmingCharacters(in: .whitespaces).lowercased(),
                want: "", disposition: .squelch))
    }
}
