// The NETWORK half of the send ceremony, lifted out of the pane composer so the
// reader's inline reply fires the identical request and maps the identical
// failures. What stays with each caller is only the UI half: which toast, which
// surface closes, where the verdict renders.
//
// The ceremony itself is unchanged and still lives in the views: submit once
// WITHOUT override to get the outbound-guard verdict, then a second, explicit
// act to override it.

import Foundation

/// What a send attempt came back as. `guardBlocked` is not a failure — it is the
/// verdict half of the ceremony, carrying the REDACTED kinds the guard matched.
enum SendOutcome: Sendable {
    case sent(SendResult)
    case guardBlocked([String])
    case forbidden
    case failure(String)
}

@MainActor
enum ComposeSubmit {
    /// Submit a draft. `override` is the explicit SECOND act after a
    /// `guardBlocked` verdict; never pass true on a first attempt, or the verdict
    /// never happens.
    ///
    /// `draftId` rides along so the daemon deletes the autosaved draft in the same
    /// transaction as the send: one round-trip, no window in which the mail has
    /// gone out and a draft of it is still restorable. nil means the composer never
    /// autosaved (nothing typed since it opened, or the save never landed), and
    /// then there is nothing to delete.
    static func fire(_ c: ComposeState, override: Bool) async -> SendOutcome {
        do {
            let result = try await APIClient.shared.actionSend(
                body: c.body, replyToMessageId: c.replyToMessageId,
                // Empty = "derive it": on a reply the daemon reads the recipient
                // off the parent message.
                to: c.to.isEmpty ? nil : c.to,
                // THE ONE FIELD WHERE `""` AND ABSENT DIFFER, and the flag is
                // what tells them apart: a composer whose fields state the
                // audience sends its copy list whatever it holds, emptied
                // included, while one still waiting on the daemon's derivation
                // sends nothing and lets the daemon derive. Asserting an empty
                // Cc we never had would narrow a reply-all to one person.
                cc: c.recipientsStated ? c.cc : nil,
                // Never derived, so this is simply what the sender typed.
                bcc: c.bcc.isEmpty ? nil : c.bcc,
                // nil, never "": the daemon reads Some("") as an explicit blank
                // subject and would send the reply untitled.
                subject: c.subject.isEmpty ? nil : c.subject,
                overrideGuard: override, draftId: c.draftId,
                // The composer's own switch, every time — the daemon's stored
                // default is a client preference it never applies itself.
                includeTracker: c.includeTracker,
                // Only a reply can widen: `reply_all` names a parent to widen
                // FROM, so a new message carrying it would be asking the daemon
                // to derive a recipient set out of nothing.
                replyAll: c.replyToMessageId != nil && c.replyAll,
                // The whole of a forward on the wire: the daemon quotes the
                // original and re-attaches its files from this id. Never set
                // beside `reply_to_message_id` — the two are mutually exclusive
                // server-side, and nothing in the client can produce both (a
                // composer is opened as one or the other and never converts).
                forwardOfMessageId: c.forwardOfMessageId)
            capture(c, override, "sent")
            return .sent(result)
        } catch let apiError as APIError where apiError.kind == .guardBlocked {
            capture(c, override, "guard_blocked")
            return .guardBlocked(apiError.guardKinds ?? [])
        } catch let apiError as APIError where apiError.kind == .forbidden {
            capture(c, override, "forbidden")
            return .forbidden
        } catch {
            capture(c, override, "failure")
            return .failure(errText(error, "send failed"))
        }
    }

    /// Outcome shape only — the draft's content never rides along.
    private static func capture(_ c: ComposeState, _ override: Bool, _ outcome: String) {
        Analytics.capture(
            "compose_send",
            [
                "kind": c.analyticsKind,
                "outcome": outcome,
                "override": override,
                "tracked": c.includeTracker,
                // Whether the send used the copy lists at all — booleans, so
                // nothing about WHO is anywhere near this (see Analytics'
                // closed string vocabulary).
                "copied": !c.cc.isEmpty,
                "blind": !c.bcc.isEmpty,
            ])
    }
}

// MARK: - shared copy

/// Sentences both composers say. Held together because a reply that starts in
/// the reader and a reply that starts in the pane must not describe the same
/// state in two different voices.
enum ComposeCopy {
    /// 403: the read credential cannot send.
    static let noWriteCredential = "no write credential — run `squelchd auth --write`"

    /// The review pane's line for an armed read-tracking pixel — the one thing
    /// about to go out that the body does not show.
    static let trackedSend = "read receipt pixel attached"

    /// Stands in for a subject the daemon will derive but that is NOT in reach to
    /// show: the pane composer works off an update, which carries an LLM summary
    /// rather than the real header.
    static let derivedSubject = "Re: (derived from thread)"

    /// Stands in for a reply-all's recipients while the lookup is in flight or
    /// after it failed. It states who decides rather than naming addresses the
    /// client does not have: the daemon derives the set at send time either way.
    static let derivedRecipients = "derived by the daemon at send"

    /// What the daemon will title a reply, mirrored for DISPLAY only — see
    /// `gmail_write::reply_subject`, which prefixes "Re: " exactly once, so an
    /// already-answered thread does not read as "Re: Re: …" here either.
    static func replySubject(_ parentSubject: String) -> String {
        let trimmed = parentSubject.trimmed
        guard !trimmed.isEmpty else { return derivedSubject }
        return trimmed.lowercased().hasPrefix("re:") ? trimmed : "Re: \(trimmed)"
    }

    /// What a forward is titled — the same mirror `replySubject` is, of the
    /// daemon's `gmail_write::forward_subject`, which prefixes "Fwd: " exactly
    /// once. Kept in step for a reason the reply side does not have: the daemon
    /// only titles a forward itself when the field arrives absent or holding
    /// nothing but whitespace (it trims before deriving), and the composer
    /// opens holding this string, so this is what actually goes out. The two
    /// must agree or a forwarded forward reads "Fwd: Fwd: …".
    ///
    /// `fw:` counts as prefixed alongside `fwd:` — plenty of clients write the
    /// short form — and both are matched case-insensitively, because no client
    /// agrees on the capitalization either.
    ///
    /// An untitled original stays untitled: "Fwd:" alone, rather than borrowing
    /// the reply side's "(derived from thread)" stand-in, which would be a lie
    /// here. Nothing derives this one. THE ONE PLACE the mirror is deliberately
    /// not byte-identical: the daemon's empty case keeps a trailing space.
    ///
    /// Called for two different jobs, and the second is why the mirror has to
    /// stay honest. The composer PRE-FILLS with this, and that string is what
    /// actually goes out. But a sender can clear the field (nil on the wire) or
    /// blank it to spaces (sent, then discarded by the daemon's trim) — either
    /// way titling lands back with the daemon — so the review pane calls this
    /// again purely to SAY what the daemon will title it (see
    /// `ComposePane.reviewSubject`). In that second use the trailing space is
    /// display-invisible; in the first it never reaches the wire, because a
    /// pre-filled field the sender left alone is non-empty and is sent verbatim.
    static func forwardSubject(_ originalSubject: String) -> String {
        let trimmed = originalSubject.trimmed
        guard !trimmed.isEmpty else { return "Fwd:" }
        let lowered = trimmed.lowercased()
        return lowered.hasPrefix("fwd:") || lowered.hasPrefix("fw:") ? trimmed : "Fwd: \(trimmed)"
    }
}
