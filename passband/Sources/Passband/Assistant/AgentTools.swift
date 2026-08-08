// WHAT THE AGENT CAN DO, and the one place it is allowed to do it: the tool
// definitions the model sees, plus the dispatcher that answers them. Every call
// goes through the human door (APIClient), so sealed mail is structurally
// absent from every result here — the assistant cannot read a 2FA code because
// the door it knocks on has none.
//
// THREE TIERS, and the descriptions say which is which because the model reads
// them:
//   * READS take nothing back and are never gated.
//   * SAFE WRITES touch Passband's LOCAL store only (status, triage rules,
//     drafts). Reversible in the app, invisible to Gmail, so no card.
//   * GATED WRITES touch the mailbox. They park on a confirmation card and only
//     an approval reaches the API. The daemon demands `confirm: true` on those
//     routes and APIClient sends it — which is honest ONLY because the tap has
//     already happened by the time the call is made.
//
// Results are compact JSON summaries, never dumps: the model pays for every
// byte, and a full mailbox row carries fields it has no use for. Failures come
// back in-band as `{"error": …}` with is_error set, so the model can adapt
// (retry with a different id, tell the user it can't) rather than the loop
// dying.

import Foundation

/// One tool's answer: the JSON the model gets, whether it reads as a failure,
/// and one human line for the tray's activity chip.
struct ToolOutcome: Sendable {
    var content: String
    var isError: Bool
    var summary: String
}

enum AgentTools {

    // MARK: - the inventory

    /// Every tool, by wire name. Spelling lives HERE so the session, the chips
    /// and the dispatcher can never disagree about it.
    enum Tool: String, CaseIterable, Sendable {
        case searchMail = "search_mail"
        case getThread = "get_thread"
        case getUpdates = "get_updates"
        case getRecords = "get_records"
        case searchContacts = "search_contacts"
        case showEmails = "show_emails"
        case setStatus = "set_status"
        case createSenderRule = "create_sender_rule"
        case saveDraft = "save_draft"
        case archiveMessage = "archive_message"
        case labelMessage = "label_message"
        case sendEmail = "send_email"
        case unsubscribeSender = "unsubscribe_sender"

        /// The chip's glyph. On the tool rather than in the view: a second
        /// stringly-typed list of the same twelve names is a list that drifts.
        var symbol: String {
            switch self {
            case .searchMail: "magnifyingglass"
            case .getThread: "envelope.open"
            case .getUpdates: "tray"
            case .getRecords: "shippingbox"
            case .searchContacts: "person.crop.circle"
            case .showEmails: "mail.stack"
            case .setStatus: "checkmark.circle"
            case .createSenderRule: "line.3.horizontal.decrease.circle"
            case .saveDraft: "doc"
            case .archiveMessage: "archivebox"
            case .labelMessage: "tag"
            case .sendEmail: "paperplane"
            case .unsubscribeSender: "hand.raised"
            }
        }
    }

    /// What a chip says while the call's arguments are still arriving — the
    /// bare tool name, because that is genuinely all we know yet.
    static func openingSummary(_ name: String) -> String { name }

    // MARK: - dispatch

    /// Execute one tool call. `cite` collects sources as they are seen; `show`
    /// hands the session email cards to put in the transcript; `gate` is the
    /// confirm ceremony for the four tools that touch the mailbox.
    @MainActor
    static func run(
        name: String, input: [String: JSONValue], gate: ActionGate,
        cite: (ToolCitation) -> Void, show: ([EmailCard]) -> Void
    ) async -> ToolOutcome {
        guard let tool = Tool(rawValue: name) else {
            return failure("unknown tool \(name)", summary: "unknown tool")
        }
        do {
            switch tool {
            case .searchMail: return try await searchMail(input, cite: cite)
            case .getThread: return try await getThread(input, cite: cite)
            case .getUpdates: return try await getUpdates(input, cite: cite)
            case .getRecords: return try await getRecords(input)
            case .searchContacts: return try await searchContacts(input)
            case .showEmails: return await showEmails(input, show: show)
            case .setStatus: return try await setStatus(input)
            case .createSenderRule: return try await createSenderRule(input)
            case .saveDraft: return try await saveDraft(input)
            case .archiveMessage: return await archiveMessage(input, gate)
            case .labelMessage: return await labelMessage(input, gate)
            case .sendEmail: return await sendEmail(input, gate)
            case .unsubscribeSender: return await unsubscribeSender(input, gate)
            }
        } catch {
            // Compact, so the model can adapt (a sealed message reads as a 404).
            let text = errText(error, "\(tool.rawValue) failed")
            return failure(text, summary: "\(tool.rawValue) failed")
        }
    }

    // MARK: - target verification

    /// A message the DAEMON confirmed, resolved before any card is parked.
    private struct VerifiedTarget {
        var messageId: Int
        var threadId: String
        var subject: String
        var sender: String
    }

    private enum Verification {
        case ok(VerifiedTarget)
        case refused(ToolOutcome)
    }

    /// Resolve a message id THROUGH its thread and prove the two belong
    /// together, before anything is shown to the human.
    ///
    /// WHY THIS EXISTS: the tap is the security boundary, so everything the
    /// card states has to come from the daemon rather than from the model. A
    /// card that names its target only by opaque id asks somebody to authorize
    /// something they cannot audit, and one that names it in the model's own
    /// words asks them to authorize the model's description of the act instead
    /// of the act. The membership check is the second half: a message id the
    /// model invented — or lifted out of an injected instruction — cannot ride
    /// along under a card that describes some other message.
    ///
    /// Both ids are required because the client door is thread-id-only: there
    /// is no "fetch message N" route, so the proof is a membership test.
    @MainActor
    private static func verify(
        _ input: [String: JSONValue], summary: String
    ) async -> Verification {
        guard let messageId = int(input, "message_id") else {
            return .refused(failure("missing message_id", summary: summary))
        }
        guard let threadId = string(input, "thread_id") else {
            return .refused(
                failure(
                    "missing thread_id — pass the thread_id this message came back with",
                    summary: summary))
        }
        return await verify(messageId: messageId, threadId: threadId, summary: summary)
    }

    @MainActor
    private static func verify(
        messageId: Int, threadId: String, summary: String
    ) async -> Verification {
        let view: ClientThreadView
        do {
            view = try await APIClient.shared.getThread(threadId)
        } catch {
            return .refused(
                failure(errText(error, "could not read thread \(threadId)"), summary: summary))
        }
        guard let message = view.messages.first(where: { $0.id == messageId }) else {
            return .refused(
                failure(
                    "message \(messageId) is not in thread \(threadId) — re-read the thread "
                        + "and use a message_id it returned",
                    summary: summary))
        }
        return .ok(
            VerifiedTarget(
                messageId: messageId, threadId: view.thread_id,
                subject: view.subject.isEmpty ? "(no subject)" : view.subject,
                sender: senderLine(message)))
    }

    /// Name AND address. A display name on its own is the easiest thing in mail
    /// to forge, and the card is where that would go unnoticed.
    private static func senderLine(_ message: ClientMessage) -> String {
        guard let name = message.from_name?.trimmed, !name.isEmpty else { return message.from_addr }
        return "\(name) <\(message.from_addr)>"
    }

    // MARK: - reads

    @MainActor
    private static func searchMail(
        _ input: [String: JSONValue], cite: (ToolCitation) -> Void
    ) async throws -> ToolOutcome {
        guard let query = string(input, "query") else {
            return failure("empty query", summary: "search failed")
        }
        let limit = min(max(int(input, "limit") ?? 8, 1), 20)
        let page = try await APIClient.shared.search(query, limit: limit, mode: .hybrid)
        let rows = page.items.map { hit -> [String: Any] in
            cite(
                ToolCitation(
                    threadId: hit.thread_id,
                    subject: hit.subject.isEmpty ? "(no subject)" : hit.subject,
                    sender: hit.from_name ?? hit.from_addr, date: hit.received_at))
            return row([
                "thread_id": hit.thread_id,
                "from": hit.from_name ?? hit.from_addr,
                "subject": hit.subject,
                "date": hit.received_at,
                "snippet": hit.snippet,
            ])
        }
        return ok(["results": rows], summary: "searched \(quoted(query))")
    }

    @MainActor
    private static func getThread(
        _ input: [String: JSONValue], cite: (ToolCitation) -> Void
    ) async throws -> ToolOutcome {
        guard let id = string(input, "thread_id") else {
            return failure("missing thread_id", summary: "read failed")
        }
        let view = try await APIClient.shared.getThread(id)
        if let first = view.messages.first {
            cite(
                ToolCitation(
                    threadId: view.thread_id,
                    subject: view.subject.isEmpty ? "(no subject)" : view.subject,
                    sender: first.from_name ?? first.from_addr, date: first.received_at))
        }
        let messages = view.messages.map { message in
            row([
                "message_id": message.id,
                "from": message.from_name ?? message.from_addr,
                "date": message.received_at,
                "text": message.content,
            ])
        }
        return ok(
            ["subject": view.subject, "thread_id": view.thread_id, "messages": messages],
            summary: view.subject.isEmpty ? "read thread" : "read \(quoted(view.subject))")
    }

    @MainActor
    private static func getUpdates(
        _ input: [String: JSONValue], cite: (ToolCitation) -> Void
    ) async throws -> ToolOutcome {
        var params = UpdatesParams()
        if let tier = string(input, "tier") { params.tier = Tier(rawValue: tier) }
        if let status = string(input, "status") { params.status = AttentionStatus(rawValue: status) }
        if let days = int(input, "since_days"), days > 0 {
            params.since = iso(Date().addingTimeInterval(-Double(days) * 86400))
        }
        params.limit = min(max(int(input, "limit") ?? 20, 1), 50)

        // PEEK IS ALWAYS TRUE ON THIS PATH. `/client/updates` stamps a
        // seen-ledger for the caller, because the UI showing a row IS the
        // surfacing event — but the agent reads far more than it repeats back,
        // and marking mail seen that the user never laid eyes on would empty
        // their sitrep on the agent's behalf. See APIClient.getUpdates.
        let page = try await APIClient.shared.getUpdates(params, peek: true)
        let rows = page.items.map { update -> [String: Any] in
            cite(
                ToolCitation(
                    threadId: update.thread_id,
                    // Attention rows carry no header subject — `one_line` is the
                    // triage summary, and it is the best title on offer here.
                    subject: update.one_line,
                    sender: update.from_name ?? update.sender,
                    date: update.surfaced_at ?? ""))
            return row([
                "message_id": update.id,
                "thread_id": update.thread_id,
                "from": update.from_name ?? update.sender,
                "one_line": update.one_line,
                "tier": update.tier.rawValue,
                "importance": update.importance,
                "deadline": update.deadline,
                "status": update.status.rawValue,
                "date": update.surfaced_at,
            ])
        }
        return ok(["updates": rows], summary: "checked the attention list")
    }

    /// How many cards one show_emails call may put on screen. Past this the
    /// tray is a list view, which is what the Emails page is for.
    private static let showCap = 8

    /// Put email threads in the transcript as clickable cards. EVERY field on a
    /// card is re-read from the daemon here: the model only picks WHICH threads
    /// to show, never what the cards say — a thread_id it invented (or lifted
    /// out of an injected message) simply fails to resolve and is reported
    /// back as unavailable instead of rendering whatever it claimed.
    ///
    /// Never throws: a batch where some ids resolve should still show those,
    /// so per-id failures collect instead of aborting the call.
    @MainActor
    private static func showEmails(
        _ input: [String: JSONValue], show: ([EmailCard]) -> Void
    ) async -> ToolOutcome {
        let ids = strings(input, "thread_ids")
        guard !ids.isEmpty else {
            return failure(
                "missing thread_ids — pass thread_ids from an earlier search or updates result",
                summary: "nothing to show")
        }
        var seen = Set<String>()
        let unique = ids.filter { seen.insert($0).inserted }
        var cards: [EmailCard] = []
        var unavailable: [String] = []
        for id in unique.prefix(showCap) {
            do {
                let view = try await APIClient.shared.getThread(id)
                // messages[0] is the NEWEST (the reader's j/k order) — the card
                // previews where the thread currently stands.
                guard let latest = view.messages.first else {
                    unavailable.append(id)
                    continue
                }
                cards.append(
                    EmailCard(
                        threadId: view.thread_id,
                        subject: view.subject.isEmpty ? "(no subject)" : view.subject,
                        sender: latest.from_name ?? latest.from_addr,
                        date: latest.received_at,
                        snippet: snippet(latest.content)))
            } catch {
                unavailable.append(id)
            }
        }
        guard !cards.isEmpty else {
            return failure(
                "none of those thread_ids resolved — use thread_ids exactly as an earlier "
                    + "result returned them",
                summary: "nothing to show")
        }
        show(cards)
        return ok(
            row([
                "shown": cards.count,
                "unavailable": unavailable.isEmpty ? nil : unavailable,
                "note": unique.count > showCap ? "capped at \(showCap) cards" : nil,
            ]),
            summary: cards.count == 1 ? "showed 1 email" : "showed \(cards.count) emails")
    }

    /// A card-sized preview off the top of a message body.
    private static func snippet(_ text: String, limit: Int = 180) -> String {
        let flat = text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        return flat.count <= limit ? flat : String(flat.prefix(limit)) + "…"
    }

    @MainActor
    private static func getRecords(_ input: [String: JSONValue]) async throws -> ToolOutcome {
        guard let kind = string(input, "kind") else {
            return failure("missing kind", summary: "records failed")
        }
        let days = int(input, "days")
        let rows: [[String: Any]]
        switch kind {
        case "shipments":
            let includeDelivered = bool(input, "include_delivered") ?? false
            rows = try await APIClient.shared.getShipments(includeDelivered: includeDelivered)
                .map { shipment in
                    // NO tracking_url: it is a link lifted out of email, and a
                    // model-emitted one is a prompt-injection lever (the same
                    // reason MarketingOffer carries none on the wire).
                    row([
                        "item": shipment.item_name,
                        "carrier": shipment.carrier.rawValue,
                        "status": shipment.status.rawValue,
                        "tracking_number": shipment.tracking_number,
                        "last_update": shipment.last_update,
                    ])
                }
        case "receipts":
            rows = try await APIClient.shared.getReceipts(days: days).map { receipt in
                row([
                    "message_id": receipt.message_id,
                    "thread_id": receipt.thread_id,
                    "from": receipt.from_name ?? receipt.from_addr,
                    "amount": receipt.amount,
                    "currency": receipt.currency,
                    "date": receipt.received_at,
                ])
            }
        case "calendar":
            let hours = int(input, "hours")
            rows = try await APIClient.shared.getCalendar(hours: hours).map { event in
                row([
                    "message_id": event.message_id,
                    "thread_id": event.thread_id,
                    "kind": event.kind.rawValue,
                    "title": event.event_title,
                    "starts_at": event.starts_at,
                    "organizer": event.organizer,
                ])
            }
        case "banking":
            rows = try await APIClient.shared.getBanking().map { record in
                row([
                    "message_id": record.message_id,
                    "thread_id": record.thread_id,
                    "kind": record.kind.rawValue,
                    "institution": record.institution,
                    "amount": record.amount,
                    "currency": record.currency,
                    "account_hint": record.account_hint,
                    "date": record.received_at,
                ])
            }
        case "marketing":
            rows = try await APIClient.shared.getMarketing(days: days).map { offer in
                row([
                    "message_id": offer.message_id,
                    "thread_id": offer.thread_id,
                    "from": offer.sender,
                    "subject": offer.subject,
                    "brand": offer.brand,
                    "offer": offer.offer,
                    "discount": offer.discount,
                    "code": offer.code,
                    "expires_at": offer.expires_at,
                ])
            }
        default:
            return failure("unknown record kind \(kind)", summary: "records failed")
        }
        return ok([kind: rows], summary: "checked \(kind)")
    }

    @MainActor
    private static func searchContacts(_ input: [String: JSONValue]) async throws -> ToolOutcome {
        guard let query = string(input, "query") else {
            return failure("empty query", summary: "contact lookup failed")
        }
        let hits = try await APIClient.shared.contacts(query)
        let rows = hits.map { hit in
            row([
                "addr": hit.addr,
                "name": hit.display_name,
                "sent_count": hit.sent_count,
                "last_sent_at": hit.last_sent_at,
            ])
        }
        return ok(["contacts": rows], summary: "looked up \(quoted(query))")
    }

    // MARK: - safe writes (local store only)

    @MainActor
    private static func setStatus(_ input: [String: JSONValue]) async throws -> ToolOutcome {
        guard let messageId = int(input, "message_id") else {
            return failure("missing message_id", summary: "status failed")
        }
        guard let raw = string(input, "status"), let status = AttentionStatus(rawValue: raw),
            status != .new
        else {
            return failure("status must be \"open\" or \"done\"", summary: "status failed")
        }
        let result = try await APIClient.shared.setStatus(messageId, status)
        return ok(
            ["status": result.status, "message_id": messageId],
            summary: "marked \(raw)")
    }

    @MainActor
    private static func createSenderRule(_ input: [String: JSONValue]) async throws -> ToolOutcome {
        guard let pattern = string(input, "match_pattern") else {
            return failure("missing match_pattern", summary: "rule failed")
        }
        guard let want = string(input, "want") else {
            return failure("missing want", summary: "rule failed")
        }
        let normalized = pattern.lowercased()
        guard validPattern(normalized) else {
            return failure(
                "match_pattern must be one address (\"news@stripe.com\") or one domain "
                    + "(\"*@stripe.com\"); the domain part takes no wildcards",
                summary: "rule failed")
        }
        // CREATE-ONLY FROM THE AGENT PATH. `POST /client/rules` is an upsert, so
        // a repeat pattern silently rewrites `want_text` and `disposition` in
        // place — the user's own "always surface these" flipped to "mute", with
        // no card, no undo, and a success reported to the model either way. The
        // agent may ADD a rule; changing one the human wrote is the human's.
        //
        // Fails CLOSED: if the existing rules can't be read, nothing is written.
        let existing: [SenderRule]
        do {
            existing = try await APIClient.shared.listRules()
        } catch {
            return failure(
                errText(error, "could not check the existing rules"), summary: "rule failed")
        }
        guard !existing.contains(where: { $0.match_pattern.lowercased() == normalized }) else {
            return failure(
                "a rule for that pattern exists; the user can edit it in Settings",
                summary: "rule already exists")
        }
        // Disposition is OMITTED so the daemon infers it from `want` — the
        // owner's own sentence decides, not the model's reading of it. `sweep`
        // is never set from here: a rule says what should happen NEXT, and mail
        // already in the bands is not the agent's to clear.
        let created = try await APIClient.shared.createRule(
            CreateRuleBody(match_pattern: normalized, want: want, disposition: nil))
        return ok(
            [
                "rule_id": created.rule_id,
                // The daemon's own verdict, which is the only place the caller
                // can learn what inference chose.
                "disposition": created.disposition?.rawValue ?? "unspecified",
                "scope": "passband triage only — no mail was moved",
            ],
            summary: "wrote a triage rule")
    }

    /// The shapes a triage rule may take: one address, or one domain.
    ///
    /// `glob_match` reads `*` as "any run, including empty", so a domain with a
    /// wildcard in it is a catch-all in the making: not just `*@*` but `*@*.*`
    /// — matched by every dotted domain, which is every sender that will ever
    /// arrive — one sentence in somebody else's email away from squelching the
    /// whole inbox, with no card in the way. So the domain takes NO wildcards
    /// at all: a wildcard local part over a concrete domain is the widest rule
    /// this path writes. Checked HERE because the server only validates
    /// non-empty.
    private static func validPattern(_ pattern: String) -> Bool {
        let parts = pattern.split(separator: "@", omittingEmptySubsequences: false)
        guard parts.count == 2 else { return false }
        let local = parts[0]
        let domain = parts[1]
        guard !local.isEmpty, !domain.isEmpty else { return false }
        return !domain.contains("*")
    }

    @MainActor
    private static func saveDraft(_ input: [String: JSONValue]) async throws -> ToolOutcome {
        guard let body = string(input, "body") else {
            return failure("missing body", summary: "draft failed")
        }
        let replyTo = int(input, "reply_to_message_id")
        // THE DRAFT SLOT BELONGS TO THE PERSON. Drafts are keyed by
        // (account, reply_to_message_id) — nil being THE one new-message slot —
        // and the PUT is an upsert, the same row the composers autosave into.
        // Writing over a half-composed message destroys it on screen AND on the
        // server, and there is no undo for a lost draft. The agent may fill an
        // empty slot; replacing what somebody typed is something they ask for.
        //
        // Fails CLOSED: a slot we cannot read is a slot we do not write.
        let existing: [DraftView]
        do {
            existing = try await APIClient.shared.listDrafts()
        } catch {
            return failure(
                errText(error, "could not check the saved drafts"), summary: "draft failed")
        }
        guard !existing.contains(where: { $0.reply_to_message_id == replyTo }) else {
            return failure(
                replyTo == nil
                    ? "a draft is already saved in the new-message slot; ask the user before "
                        + "replacing it"
                    : "a draft is already saved for that reply; ask the user before replacing it",
                summary: "a draft already exists there")
        }
        let draft = try await APIClient.shared.putDraft(
            replyToMessageId: replyTo, to: string(input, "to") ?? "",
            subject: string(input, "subject") ?? "", body: body)
        return ok(
            row(["draft_id": draft.id, "reply_to_message_id": draft.reply_to_message_id]),
            summary: "saved a draft")
    }

    // MARK: - gated writes (the mailbox)

    @MainActor
    private static func archiveMessage(
        _ input: [String: JSONValue], _ gate: ActionGate
    ) async -> ToolOutcome {
        let target: VerifiedTarget
        switch await verify(input, summary: "archive failed") {
        case .refused(let outcome): return outcome
        case .ok(let value): target = value
        }
        let action = PendingAction(
            id: UUID(), tool: .archiveMessage, verb: "Archive",
            verifiedSender: target.sender, verifiedSubject: target.subject)
        return await confirmed(action, gate) {
            let result = try await APIClient.shared.actionArchive(target.messageId)
            return (json(["status": result.status, "message_id": target.messageId]), "Archived")
        }
    }

    @MainActor
    private static func labelMessage(
        _ input: [String: JSONValue], _ gate: ActionGate
    ) async -> ToolOutcome {
        let add = strings(input, "add")
        let remove = strings(input, "remove")
        guard !add.isEmpty || !remove.isEmpty else {
            return failure("nothing to add or remove", summary: "label failed")
        }
        let target: VerifiedTarget
        switch await verify(input, summary: "label failed") {
        case .refused(let outcome): return outcome
        case .ok(let value): target = value
        }
        var parts: [String] = []
        if !add.isEmpty { parts.append("add \(add.joined(separator: ", "))") }
        if !remove.isEmpty { parts.append("remove \(remove.joined(separator: ", "))") }
        let action = PendingAction(
            id: UUID(), tool: .labelMessage, verb: "Relabel",
            detail: parts.joined(separator: " · "),
            verifiedSender: target.sender, verifiedSubject: target.subject)
        return await confirmed(action, gate) {
            let result = try await APIClient.shared.actionLabel(
                target.messageId, add: add, remove: remove)
            return (json(["status": result.status, "message_id": target.messageId]), "Relabeled")
        }
    }

    @MainActor
    private static func sendEmail(
        _ input: [String: JSONValue], _ gate: ActionGate
    ) async -> ToolOutcome {
        guard let body = string(input, "body") else {
            return failure("missing body", summary: "send failed")
        }
        let replyTo = int(input, "reply_to_message_id")
        let to = string(input, "to")
        let subject = string(input, "subject")
        guard replyTo != nil || to != nil else {
            return failure(
                "send_email needs either reply_to_message_id or to", summary: "send failed")
        }
        // A REPLY NAMES NO RECIPIENT — the daemon derives it from the parent —
        // so without this the highest-stakes card in the app would never state
        // who receives the mail. Resolving the parent here is what puts a real
        // address on it, and it comes from the daemon, not the model.
        var verified: VerifiedTarget?
        if let replyTo {
            guard let threadId = string(input, "reply_to_thread_id") else {
                return failure(
                    "a reply needs reply_to_thread_id alongside reply_to_message_id",
                    summary: "send failed")
            }
            switch await verify(messageId: replyTo, threadId: threadId, summary: "send failed") {
            case .refused(let outcome): return outcome
            case .ok(let value): verified = value
            }
        }
        let action = PendingAction(
            id: UUID(), tool: .sendEmail, verb: "Send",
            verifiedSender: verified?.sender, verifiedSubject: verified?.subject,
            replyToMessageId: replyTo, to: to, subject: subject, body: body)
        return await confirmed(action, gate) {
            // No override_guard, ever, from here: the outbound guard's whole job
            // is to stop a secret leaving, and a model that just wrote the body
            // is exactly the caller that must not get to wave it through. A 422
            // comes back to the model verbatim (the kinds are already redacted
            // server-side) so it can rewrite and ask again.
            let result = try await APIClient.shared.actionSend(
                body: body, replyToMessageId: replyTo, to: to, subject: subject)
            return (
                json(
                    row([
                        "status": result.status, "echo_message_id": result.echo_message_id,
                        "thread_id": result.thread_id,
                    ])), "Sent"
            )
        }
    }

    @MainActor
    private static func unsubscribeSender(
        _ input: [String: JSONValue], _ gate: ActionGate
    ) async -> ToolOutcome {
        let target: VerifiedTarget
        switch await verify(input, summary: "unsubscribe failed") {
        case .refused(let outcome): return outcome
        case .ok(let value): target = value
        }
        let action = PendingAction(
            id: UUID(), tool: .unsubscribeSender, verb: "Unsubscribe",
            verifiedSender: target.sender, verifiedSubject: target.subject)
        return await confirmed(action, gate) {
            let result = try await APIClient.shared.unsubscribe(messageId: target.messageId)
            // THE DAEMON NEVER FETCHES THE LINK. Opening it in the user's own
            // browser is the design: an unsubscribe is something the human did,
            // from their machine, with their cookies. `Opener` hands the shell
            // http(s) and nothing else, and logs at most the host.
            guard Opener.isHTTP(result.url) else {
                return (
                    json(["result": "no_http_link", "sender": result.sender]),
                    "No unsubscribe link"
                )
            }
            Opener.open(result.url)
            // The URL itself stays out of the result: it is mail-derived, it
            // routinely carries a per-recipient token, and the model has no use
            // for it now that the browser has it.
            return (
                json([
                    "result": "opened_in_browser", "sender": result.sender,
                    "method": result.method,
                ]), "Unsubscribe page opened"
            )
        }
    }

    /// The confirm ceremony, in ONE place. Only `.approved` reaches `perform`,
    /// which is the entire reason the `confirm: true` those calls send is a true
    /// statement. A decline is a legitimate answer, not a failure: it comes back
    /// with is_error FALSE so the model accepts it and moves on.
    @MainActor
    private static func confirmed(
        _ action: PendingAction, _ gate: ActionGate,
        perform: () async throws -> (result: String, done: String)
    ) async -> ToolOutcome {
        switch await gate.confirm(action) {
        case .declined:
            return ToolOutcome(
                content: json([
                    "result": "declined",
                    "detail": "The user declined this action. Do not retry it.",
                ]),
                isError: false, summary: "\(action.verb.lowercased()) declined")

        case .editedInComposer:
            return ToolOutcome(
                content: json([
                    "result": "declined",
                    "detail":
                        "The user moved this message into their composer to finish and send "
                        + "it themselves. Do not send it.",
                ]),
                isError: false, summary: "moved to the composer")

        case .approved:
            do {
                let (result, done) = try await perform()
                gate.settle(action.id, .executed(done))
                return ToolOutcome(content: result, isError: false, summary: done.lowercased())
            } catch {
                let text = errText(error, "\(action.verb.lowercased()) failed")
                gate.settle(action.id, .failed(text))
                return ToolOutcome(
                    content: errorJSON(text), isError: true,
                    summary: "\(action.verb.lowercased()) failed")
            }
        }
    }

    // MARK: - shaping

    private static func json(_ object: Any) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: object),
            let string = String(data: data, encoding: .utf8)
        else { return "{}" }
        return string
    }

    static func errorJSON(_ message: String) -> String { json(["error": message]) }

    private static func ok(_ object: [String: Any], summary: String) -> ToolOutcome {
        ToolOutcome(content: json(object), isError: false, summary: summary)
    }

    private static func failure(_ message: String, summary: String) -> ToolOutcome {
        ToolOutcome(content: errorJSON(message), isError: true, summary: summary)
    }

    /// Drop the nil fields. JSONSerialization REFUSES an Optional, so a row
    /// built with one in it would serialize as `{}` and lose everything.
    private static func row(_ fields: [String: Any?]) -> [String: Any] {
        fields.compactMapValues { $0 }
    }

    private static func iso(_ date: Date) -> String {
        ISO8601DateFormatter().string(from: date)
    }

    /// A chip-sized echo of what the model asked for.
    private static func quoted(_ text: String, limit: Int = 42) -> String {
        let flat = text.replacingOccurrences(of: "\n", with: " ").trimmed
        return flat.count <= limit ? "\"\(flat)\"" : "\"\(flat.prefix(limit))…\""
    }

    // MARK: - input readers

    private static func string(_ input: [String: JSONValue], _ key: String) -> String? {
        guard let value = input[key]?.stringValue?.trimmed, !value.isEmpty else { return nil }
        return value
    }

    /// Tolerates a NUMERIC STRING: models quote ids often enough that refusing
    /// `"4821"` would burn a whole turn teaching one not to.
    private static func int(_ input: [String: JSONValue], _ key: String) -> Int? {
        if let number = input[key]?.intValue { return number }
        if let text = input[key]?.stringValue { return Int(text.trimmed) }
        return nil
    }

    private static func bool(_ input: [String: JSONValue], _ key: String) -> Bool? {
        if let flag = input[key]?.boolValue { return flag }
        if let text = input[key]?.stringValue?.lowercased() {
            if text == "true" { return true }
            if text == "false" { return false }
        }
        return nil
    }

    /// An array of strings, tolerating a lone string for the same reason `int`
    /// tolerates a quoted number.
    private static func strings(_ input: [String: JSONValue], _ key: String) -> [String] {
        if let list = input[key]?.arrayValue {
            return list.compactMap { $0.stringValue?.trimmed }.filter { !$0.isEmpty }
        }
        if let single = string(input, key) { return [single] }
        return []
    }

    // MARK: - definitions

    /// The tool list the model is given, in the order it should reach for them.
    static let definitions: [Wire.ToolDef] = [
        Wire.ToolDef(
            name: Tool.searchMail.rawValue,
            description: """
                Search the user's mailbox by meaning AND keyword (hybrid recall). \
                Returns summaries only — sender, subject, date, a snippet, and a \
                thread_id — never full bodies. Auth/2FA messages are excluded. Reach \
                for this first when the question is about a specific message.
                """,
            input_schema: .init(
                properties: [
                    "query": .init(type: "string", description: "What to look for, phrased naturally."),
                    "limit": .init(type: "integer", description: "Max results (default 8, max 20)."),
                ],
                required: ["query"])),

        Wire.ToolDef(
            name: Tool.getThread.rawValue,
            description: """
                Read one thread's messages by thread_id (from a search or updates \
                result) when the snippet isn't enough. Returns the subject and each \
                message's id, sender, date, and text. The message ids it returns are \
                what the acting tools take — always pass a message_id together with \
                the thread_id it came from.
                """,
            input_schema: .init(
                properties: [
                    "thread_id": .init(
                        type: "string", description: "The thread_id from an earlier result.")
                ],
                required: ["thread_id"])),

        Wire.ToolDef(
            name: Tool.getUpdates.rawValue,
            description: """
                The triaged attention list — what Passband decided is worth the user's \
                time, newest first, with each row's tier, importance, deadline and \
                status. Use this for "what needs me", "what's overdue", "what came in \
                today"; use search_mail when looking for one particular message. \
                Reading this does NOT mark anything as seen.
                """,
            input_schema: .init(
                properties: [
                    "tier": .init(
                        type: "string",
                        description:
                            "Narrow to one tier. past_due and deadline are dated obligations.",
                        values: ["past_due", "deadline", "signal", "noise"]),
                    "status": .init(
                        type: "string", description: "Narrow to one lifecycle state.",
                        values: ["new", "open", "done"]),
                    "since_days": .init(
                        type: "integer", description: "Only rows from the last N days."),
                    "limit": .init(type: "integer", description: "Max rows (default 20, max 50)."),
                ],
                required: nil)),

        Wire.ToolDef(
            name: Tool.getRecords.rawValue,
            description: """
                Structured records Passband extracted from the mail, so you never have \
                to parse a receipt yourself: shipments (in flight, with carrier and \
                status), receipts (amounts), calendar (invites and reservations), \
                banking (statements, alerts, autopay), marketing (live offers and \
                codes). Ask for one kind at a time.
                """,
            input_schema: .init(
                properties: [
                    "kind": .init(
                        type: "string", description: "Which record set to read.",
                        values: ["shipments", "receipts", "calendar", "banking", "marketing"]),
                    "days": .init(
                        type: "integer",
                        description: "Look-back window for receipts and marketing."),
                    "hours": .init(
                        type: "integer", description: "Look-ahead window for calendar."),
                    "include_delivered": .init(
                        type: "boolean",
                        description: "Shipments only: include already-delivered parcels."),
                ],
                required: ["kind"])),

        Wire.ToolDef(
            name: Tool.searchContacts.rawValue,
            description: """
                Find someone the user has actually written to, ranked by how much and \
                how recently. Use it to turn a first name into an address before \
                drafting or sending, rather than guessing at one.
                """,
            input_schema: .init(
                properties: [
                    "query": .init(
                        type: "string", description: "A name or address fragment.")
                ],
                required: ["query"])),

        Wire.ToolDef(
            name: Tool.showEmails.rawValue,
            description: """
                Show email threads to the user as clickable cards right in the chat. \
                Use it whenever the answer IS a set of emails — "show me", "which \
                emails", "find the ones" — after locating them with search_mail or \
                get_updates. Pass thread_ids from those results, in the order to \
                show them (max 8). The app renders each card from its own data, so \
                don't also describe the emails in text; one line of framing is \
                plenty. Purely visual: it changes nothing and reads nothing new.
                """,
            input_schema: .init(
                properties: [
                    "thread_ids": .init(
                        type: "array",
                        description: "thread_ids from earlier results, in display order.",
                        items: .init())
                ],
                required: ["thread_ids"])),

        Wire.ToolDef(
            name: Tool.setStatus.rawValue,
            description: """
                Mark one attention row open or done. Local to Passband and reversible \
                from the app — it changes what the user's sitrep shows, it does not \
                touch Gmail and moves no mail.
                """,
            input_schema: .init(
                properties: [
                    "message_id": .init(
                        type: "integer", description: "The message_id from an earlier result."),
                    "status": .init(
                        type: "string", description: "The new state.", values: ["open", "done"]),
                ],
                required: ["message_id", "status"])),

        Wire.ToolDef(
            name: Tool.createSenderRule.rawValue,
            description: """
                Write a triage rule in the user's own words: which senders it matches, \
                and what they want to happen with them. Passband decides from `want` \
                whether that means always surface, mute, or filter. LOCAL TRIAGE ONLY \
                — it shapes what gets surfaced from now on, never the mailbox, and \
                nothing already delivered moves. Reversible on the Rules screen. \
                Creates only: if a rule for that pattern exists, this reports so rather \
                than changing it, and the user edits theirs in Settings.
                """,
            input_schema: .init(
                properties: [
                    "match_pattern": .init(
                        type: "string",
                        description:
                            "One address (\"news@stripe.com\") or one domain "
                            + "(\"*@stripe.com\"). The domain part takes no wildcards."),
                    "want": .init(
                        type: "string",
                        description:
                            "What the user wants, in their words: \"always show me these\", "
                            + "\"mute unless it mentions an outage\"."),
                ],
                required: ["match_pattern", "want"])),

        Wire.ToolDef(
            name: Tool.saveDraft.rawValue,
            description: """
                Save a draft for the user to finish. Local to Passband, never a Gmail \
                draft, never sent. THIS IS THE TOOL for "write me a reply to…" or \
                "draft something that says…" — send_email is only for when they \
                explicitly ask you to send. Each draft slot holds one draft: if one is \
                already saved there, this reports so rather than replacing it, and you \
                should ask the user before overwriting their own writing.
                """,
            input_schema: .init(
                properties: [
                    "reply_to_message_id": .init(
                        type: "integer",
                        description:
                            "The message this answers. Omit for a new-message draft."),
                    "to": .init(type: "string", description: "Recipient, for a new message."),
                    "subject": .init(type: "string", description: "Subject, for a new message."),
                    "body": .init(type: "string", description: "The draft body, in markdown."),
                ],
                required: ["body"])),

        Wire.ToolDef(
            name: Tool.archiveMessage.rawValue,
            description: """
                Archive one message in Gmail. Shows the user a confirmation card first \
                and does nothing until they approve it. Pass both the message_id and \
                the thread_id it came from: the card names the sender and subject \
                Passband reads back, and the message has to be in that thread.
                """,
            input_schema: .init(
                properties: [
                    "message_id": .init(
                        type: "integer", description: "The message_id from an earlier result."),
                    "thread_id": .init(
                        type: "string",
                        description: "The thread_id that message_id came back with."),
                ],
                required: ["message_id", "thread_id"])),

        Wire.ToolDef(
            name: Tool.labelMessage.rawValue,
            description: """
                Add or remove Gmail labels on one message (removing INBOX archives it). \
                Shows the user a confirmation card first and does nothing until they \
                approve it. Pass both the message_id and the thread_id it came from: \
                the card names the sender and subject Passband reads back, and the \
                message has to be in that thread.
                """,
            input_schema: .init(
                properties: [
                    "message_id": .init(
                        type: "integer", description: "The message_id from an earlier result."),
                    "thread_id": .init(
                        type: "string",
                        description: "The thread_id that message_id came back with."),
                    "add": .init(
                        type: "array", description: "Label names to add.",
                        items: .init()),
                    "remove": .init(
                        type: "array", description: "Label names to remove.",
                        items: .init()),
                ],
                required: ["message_id", "thread_id"])),

        Wire.ToolDef(
            name: Tool.sendEmail.rawValue,
            description: """
                Send mail as the user. Shows them the full message on a confirmation \
                card first and sends nothing until they approve it. Only use this when \
                the user asked you to SEND; when they asked you to write, use \
                save_draft. On a reply, pass reply_to_message_id AND \
                reply_to_thread_id, and leave `to` and `subject` out — the recipient \
                and Re: line are derived from the parent, and the card shows the user \
                the recipient Passband reads back from it. An outbound guard may refuse \
                a body carrying credentials; if it does, rewrite without them rather \
                than trying again unchanged.
                """,
            input_schema: .init(
                properties: [
                    "reply_to_message_id": .init(
                        type: "integer", description: "The message being answered."),
                    "reply_to_thread_id": .init(
                        type: "string",
                        description:
                            "Required when replying: the thread_id that message came back with."),
                    "to": .init(
                        type: "string", description: "Recipient address, for a new message."),
                    "subject": .init(
                        type: "string", description: "Subject, for a new message."),
                    "body": .init(
                        type: "string", description: "The message body, in markdown."),
                ],
                required: ["body"])),

        Wire.ToolDef(
            name: Tool.unsubscribeSender.rawValue,
            description: """
                Unsubscribe from the sender of one message: Passband finds their \
                unsubscribe link and opens it in the user's browser for them to \
                finish. Shows a confirmation card first and does nothing until they \
                approve it. Pass both the message_id and the thread_id it came from: \
                the card names the sender Passband reads back, and the message has to \
                be in that thread.
                """,
            input_schema: .init(
                properties: [
                    "message_id": .init(
                        type: "integer",
                        description: "A message from the sender to unsubscribe from."),
                    "thread_id": .init(
                        type: "string",
                        description: "The thread_id that message_id came back with."),
                ],
                required: ["message_id", "thread_id"])),
    ]
}
