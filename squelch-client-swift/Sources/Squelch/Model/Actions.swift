// ACTION DISPATCH — the canonical verb API the read views call.
//
// Design law: UNDO-FIRST. archive/done/label fire instantly (the API client
// already sends confirm:true). The row leaves its band the instant the key is
// pressed (optimistic), a 5s undo toast is queued, and the inverse call is the
// toast's `revert`. `send` is the only ceremony path and lives in the compose
// overlay, reached here via reply().
//
// Nothing here logs the token or any sealed body; errors surface as toasts.
//
// Ported from squelch-desktop/src/actions/{useActions,blockSender}.ts.

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
            // The message leaves the working set — drop its remembered height.
            FrameHeights.shared.clear(String(u.id))
            store.pushUndo(kind: .done, messageId: u.id, label: "done \(u.sender)") {
                try await APIClient.shared.setStatus(u.id, .open)
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
                // Inverse: swap add <-> remove.
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
    static func reply(_ u: AttentionUpdate) {
        let subject =
            u.one_line.lowercased().hasPrefix("re:") ? u.one_line : "Re: \(u.one_line)"
        store.openCompose(
            ComposeState(replyToMessageId: u.id, to: u.sender, subject: subject, body: ""))
    }

    /// Tune a sender: open the rule editor prefilled with *@domain.
    static func tune(sender: String) {
        store.openRuleEditor(RuleEditorRequest(sender: sender))
    }

    /// Create a squelch rule matching `sender` EXACTLY (not *@domain) — this one
    /// sender abused the situation, not necessarily its whole domain. Shared by
    /// the unsubscribe-violation prompt and the thread viewer's no-link
    /// fallback, so the rule shape lives in exactly one place.
    static func createBlockRule(sender: String) async throws {
        try await APIClient.shared.createRule(
            CreateRuleBody(
                match_pattern: sender.trimmingCharacters(in: .whitespaces).lowercased(),
                want: "", disposition: .squelch))
    }
}
