// The NETWORK half of the send ceremony, lifted out of the modal composer so the
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
    /// No `draftId` yet: `actionSend` can delete a server-side draft on success,
    /// but `ComposeState` carries no draft id, so there is nothing to pass. When
    /// it grows one, it is threaded through here and both composers get it.
    static func fire(_ c: ComposeState, override: Bool) async -> SendOutcome {
        do {
            let result = try await APIClient.shared.actionSend(
                body: c.body, replyToMessageId: c.replyToMessageId,
                // Empty = "derive it": on a reply the daemon reads the recipient
                // off the parent message.
                to: c.to.isEmpty ? nil : c.to,
                // nil, never "": the daemon reads Some("") as an explicit blank
                // subject and would send the reply untitled.
                subject: c.subject.isEmpty ? nil : c.subject,
                overrideGuard: override)
            return .sent(result)
        } catch let apiError as APIError where apiError.kind == .guardBlocked {
            return .guardBlocked(apiError.guardKinds ?? [])
        } catch let apiError as APIError where apiError.kind == .forbidden {
            return .forbidden
        } catch {
            return .failure(errText(error, "send failed"))
        }
    }
}

// MARK: - shared copy

/// Sentences both composers say. Held together because a reply that starts in
/// the reader and a reply that starts in the modal must not describe the same
/// state in two different voices.
enum ComposeCopy {
    /// 403: the read credential cannot send.
    static let noWriteCredential = "no write credential — run `squelchd auth --write`"

    /// Stands in for a subject the daemon will derive but that is NOT in reach to
    /// show: the modal composer works off an update, which carries an LLM summary
    /// rather than the real header.
    static let derivedSubject = "Re: (derived from thread)"

    /// What the daemon will title a reply, mirrored for DISPLAY only — see
    /// `gmail_write::reply_subject`, which prefixes "Re: " exactly once, so an
    /// already-answered thread does not read as "Re: Re: …" here either.
    static func replySubject(_ parentSubject: String) -> String {
        let trimmed = parentSubject.trimmed
        guard !trimmed.isEmpty else { return derivedSubject }
        return trimmed.lowercased().hasPrefix("re:") ? trimmed : "Re: \(trimmed)"
    }
}
